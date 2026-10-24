/*!
Process-wide cache for immutable Iceberg manifest bytes.

Manifest-list and manifest files are content-unique per snapshot — Iceberg
never rewrites a file in place; new snapshots get new paths — so bytes cached
by path never go stale and need no invalidation. Caching them removes the
per-scan object-store round-trips (`manifest list GET + one GET per
manifest`) that otherwise repeat on every scan of the same snapshot.

Keys are the original file URIs (scheme + bucket + path), NOT store-relative
paths: the cache is process-wide, and two object stores can hold different
bytes at the same relative path (`s3://a/x` vs `s3://b/x`). The key carries no
store identity or credentials, so bytes one caller fetched serve every other
caller of the same URI, whether it hits the cache later or waits on the fill
below. This assumes a URI names the same bytes for every reader in the
process and that any reader of a URI may see them, as with one set of
credentials per bucket. Mixing credential scopes for the same URIs (for
example per-table vended credentials) would need the scope in the key.

The cache is byte-capped LRU. Capacity comes from `ICEBERG_MANIFEST_CACHE_MB`
(read once per process; default 128 MiB; `0` disables caching entirely).
Entries larger than the cap are never inserted.

Concurrent cold fills of one URI share a single fetch ([`get_or_fetch`]): the
first caller starts the fetch on its own task, and later callers wait on a
per-URI gate and then re-check the cache. Because the fill runs on its own
task it finishes whether or not the caller that started it keeps polling, so
a partly consumed or abandoned scan stream never holds up other readers of
the same file, and a cancelled caller does not cancel the fill. If the fill
fails or its bytes cannot be cached, the waiters fetch for themselves. If the
fill ends without leaving bytes for a waiter to find (its task panicked or
its runtime shut down, or the LRU evicted the bytes again before the waiter
re-checked), the waiters elect one of them to fill again. One caller's error
never reaches the others and is never cached. A gate exists only while a fill
of its URI is in flight, and the number of gated URIs is capped; past the cap
a fill runs ungated, as it does when caching is disabled or no Tokio runtime
is available. Waiting is the right trade here because these reads happen
during scan planning, where every caller needs the bytes before it can make
progress. The Parquet footer cache in `datafusion_iceberg` makes the opposite
choice: its contended loads bypass rather than wait.
*/

use bytes::Bytes;
use lru::LruCache;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use tokio::sync::watch;
use tracing::Instrument;

use crate::error::Error;

const DEFAULT_CAP_MB: usize = 128;

/// Most URIs with a gated fill in flight at once. A scan reads at most
/// `concurrent_manifest_ops` manifests at a time, so this binds only when many
/// cold scans of distinct snapshots overlap; fills past it run ungated, as if
/// no other caller were filling the same URI.
const MAX_GATES: usize = 4096;

struct ByteCappedCache {
    entries: LruCache<String, Bytes>,
    total_bytes: usize,
    cap_bytes: usize,
}

impl ByteCappedCache {
    fn new(cap_bytes: usize) -> Self {
        Self {
            entries: LruCache::unbounded(),
            total_bytes: 0,
            cap_bytes,
        }
    }

    /// Insert `bytes` under `path`, evicting least-recently-used entries
    /// until the byte cap holds. Oversized payloads are skipped. Returns
    /// whether the bytes were inserted.
    fn insert(&mut self, path: &str, bytes: &Bytes) -> bool {
        let len = bytes.len();
        if len > self.cap_bytes {
            return false;
        }
        if let Some(previous) = self.entries.put(path.to_string(), bytes.clone()) {
            self.total_bytes = self.total_bytes.saturating_sub(previous.len());
        }
        self.total_bytes += len;
        while self.total_bytes > self.cap_bytes {
            match self.entries.pop_lru() {
                Some((_, evicted)) => {
                    self.total_bytes = self.total_bytes.saturating_sub(evicted.len());
                }
                None => break,
            }
        }
        true
    }
}

static CACHE: LazyLock<ManifestCache> = LazyLock::new(|| {
    let cap_mb = std::env::var("ICEBERG_MANIFEST_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_CAP_MB);
    ManifestCache::new(cap_mb.saturating_mul(1024 * 1024), MAX_GATES)
});

/// Bytes for `path` from the cache, or from the future `fetch` builds on a
/// miss, inserting them on success. Concurrent misses for the same `path`
/// share one fetch: the first caller runs it on its own task and later
/// callers wait for that fill and use its cached bytes. If the fill fails or
/// its bytes cannot be cached, the waiters fetch for themselves, concurrently;
/// if it ends without leaving bytes in the cache, one waiter fills again.
/// `fetch` is called only by a caller that fetches, and its error is returned
/// to that caller alone. A panic in the fetch reaches the caller that
/// started it.
pub(crate) async fn get_or_fetch<M, F>(path: &str, fetch: M) -> Result<Bytes, Error>
where
    M: FnOnce() -> F,
    F: Future<Output = Result<Bytes, Error>> + Send + 'static,
{
    CACHE.get_or_fetch(path, fetch).await
}

/// Process-wide counters of manifest-list and manifest cache fills, counted
/// from process start. Waiters served by another caller's fill number
/// `waits - waiter_refills`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ManifestCacheFillStats {
    /// Object-store fetches started on a cache miss, including fetches that
    /// failed or whose caller was cancelled.
    pub fills: u64,
    /// Misses that found a fill of the same URI in flight and waited for it.
    pub waits: u64,
    /// Waiters that fetched themselves because the fill they waited for
    /// failed or could not cache its bytes, or because it ended without
    /// leaving bytes in the cache (its task panicked or was dropped, or the
    /// bytes were evicted before the waiter re-checked). Included in `fills`.
    pub waiter_refills: u64,
}

/// Snapshot of the process-wide [`ManifestCacheFillStats`].
pub fn manifest_cache_fill_stats() -> ManifestCacheFillStats {
    CACHE.stats()
}

/// Manifest bytes by URI, with per-URI gates around their fills and fill
/// counters.
struct ManifestCache {
    /// `None` when caching is disabled.
    bytes: Option<Mutex<ByteCappedCache>>,
    /// One entry per URI with a fill in flight, holding the completion signal
    /// its waiters subscribe to.
    in_flight: Mutex<HashMap<String, watch::Receiver<bool>>>,
    max_gates: usize,
    fills: AtomicU64,
    waits: AtomicU64,
    waiter_refills: AtomicU64,
}

/// A caller's place in the fills of one URI.
enum Role {
    /// No fill was in flight: this caller fills.
    Leader(Leader),
    /// Another caller is filling. The receiver changes to `true` when that
    /// fill ended without caching bytes, and closes when the fill ends.
    Waiter(watch::Receiver<bool>),
    /// The gate table is full: fill without coordination.
    Ungated,
}

/// Leadership of one URI's fill, owned by the task that runs it. Dropping it
/// ends the fill, whether it inserted, failed or its task was dropped: the
/// entry is removed and the completion signal closes, waking every waiter at
/// once. Waking them one at a time (a mutex hand-off) would chain each waiter
/// behind the previous one's task being polled.
struct Leader {
    cache: &'static ManifestCache,
    path: String,
    done: watch::Sender<bool>,
}

impl ManifestCache {
    /// A cache holding up to `cap_bytes` (`0` disables caching) with at most
    /// `max_gates` gated fills in flight.
    fn new(cap_bytes: usize, max_gates: usize) -> Self {
        Self {
            bytes: (cap_bytes > 0).then(|| Mutex::new(ByteCappedCache::new(cap_bytes))),
            in_flight: Mutex::new(HashMap::new()),
            max_gates,
            fills: AtomicU64::new(0),
            waits: AtomicU64::new(0),
            waiter_refills: AtomicU64::new(0),
        }
    }

    /// Cached bytes for `path`, touching its LRU position.
    fn get(&self, path: &str) -> Option<Bytes> {
        let mut guard = self.bytes.as_ref()?.lock().ok()?;
        guard.entries.get(path).cloned()
    }

    /// Insert `bytes` under `path`; see [`ByteCappedCache::insert`].
    fn put(&self, path: &str, bytes: &Bytes) -> bool {
        self.bytes
            .as_ref()
            .and_then(|cache| cache.lock().ok())
            .is_some_and(|mut guard| guard.insert(path, bytes))
    }

    fn table(&self) -> MutexGuard<'_, HashMap<String, watch::Receiver<bool>>> {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn enter(&'static self, path: &str) -> Role {
        let mut table = self.table();
        if let Some(done) = table.get(path) {
            return Role::Waiter(done.clone());
        }
        if table.len() >= self.max_gates {
            return Role::Ungated;
        }
        let (done, subscribe) = watch::channel(false);
        table.insert(path.to_owned(), subscribe);
        Role::Leader(Leader {
            cache: self,
            path: path.to_owned(),
            done,
        })
    }

    async fn get_or_fetch<M, F>(&'static self, path: &str, fetch: M) -> Result<Bytes, Error>
    where
        M: FnOnce() -> F,
        F: Future<Output = Result<Bytes, Error>> + Send + 'static,
    {
        if let Some(bytes) = self.get(path) {
            return Ok(bytes);
        }
        // Without a cache a waiter could never be served by another fill, and
        // without a runtime the fill cannot run on its own task.
        if self.bytes.is_none() || tokio::runtime::Handle::try_current().is_err() {
            return self.fetch(path, fetch()).await;
        }
        let mut waited = false;
        loop {
            match self.enter(path) {
                Role::Leader(leader) => {
                    // A fill may have completed between the lookup and taking
                    // the lead.
                    if let Some(bytes) = self.get(path) {
                        return Ok(bytes);
                    }
                    if waited {
                        self.waiter_refills.fetch_add(1, Ordering::Relaxed);
                    }
                    self.fills.fetch_add(1, Ordering::Relaxed);
                    return leader.fill(fetch()).await;
                }
                Role::Waiter(mut done) => {
                    if !waited {
                        waited = true;
                        self.waits.fetch_add(1, Ordering::Relaxed);
                    }
                    // Resolves once the fill marks itself uncached or drops
                    // its sender.
                    let _ = done.changed().await;
                    if let Some(bytes) = self.get(path) {
                        return Ok(bytes);
                    }
                    if *done.borrow() {
                        self.waiter_refills.fetch_add(1, Ordering::Relaxed);
                        return self.fetch(path, fetch()).await;
                    }
                    // The fill ended without leaving bytes to find: take the
                    // lead, or wait for whichever waiter took it first.
                }
                Role::Ungated => return self.fetch(path, fetch()).await,
            }
        }
    }

    /// Fetch and insert without coordinating with other callers.
    async fn fetch<F>(&self, path: &str, fetch: F) -> Result<Bytes, Error>
    where
        F: Future<Output = Result<Bytes, Error>>,
    {
        self.fills.fetch_add(1, Ordering::Relaxed);
        let bytes = fetch.await?;
        self.put(path, &bytes);
        Ok(bytes)
    }

    fn stats(&self) -> ManifestCacheFillStats {
        ManifestCacheFillStats {
            fills: self.fills.load(Ordering::Relaxed),
            waits: self.waits.load(Ordering::Relaxed),
            waiter_refills: self.waiter_refills.load(Ordering::Relaxed),
        }
    }
}

impl Leader {
    /// Run `fetch` to completion on its own task, insert its bytes and end
    /// the fill, returning the fetch's result. The task owns the leadership,
    /// so waiters never depend on this caller being polled: a caller that
    /// stops polling or is dropped leaves the fill running.
    async fn fill<F>(self, fetch: F) -> Result<Bytes, Error>
    where
        F: Future<Output = Result<Bytes, Error>> + Send + 'static,
    {
        let task = tokio::spawn(
            async move {
                let result = fetch.await;
                let cached = result
                    .as_ref()
                    .is_ok_and(|bytes| self.cache.put(&self.path, bytes));
                if !cached {
                    // Failed or uncacheable: waiters fetch for themselves,
                    // concurrently. Electing one of them while the rest wait
                    // again would repeat a persistent error (object stores
                    // already retry transient ones) or an uncacheable fetch
                    // once per waiter, in sequence.
                    self.done.send_replace(true);
                }
                // Dropped only after the insert, so woken waiters find the
                // bytes.
                drop(self);
                result
            }
            .in_current_span(),
        );
        match task.await {
            Ok(result) => result,
            Err(error) => match error.try_into_panic() {
                Ok(panic) => std::panic::resume_unwind(panic),
                Err(error) => Err(error.into()),
            },
        }
    }
}

impl Drop for Leader {
    fn drop(&mut self) {
        // The entry goes before the sender (a field, dropped after this body)
        // closes, so a waiter woken by a fill that left no bytes can take the
        // lead.
        self.cache.table().remove(&self.path);
    }
}

#[cfg(test)]
mod tests {
    // The process-wide cache reads its capacity from the environment once per
    // process, so tests of the cache and gate mechanics build their own
    // instances. Tests that drive the real read paths go through the
    // process-wide cache and skip when the environment disables it.
    use super::*;

    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use std::time::Duration;

    use futures::stream::BoxStream;
    use futures::{FutureExt, StreamExt, TryStreamExt};
    use iceberg_rust_spec::{
        manifest::{partition_value_schema, ManifestEntry},
        manifest_list::{manifest_list_schema_v2, Content, ManifestListEntry},
        spec::{
            schema::SchemaBuilder,
            snapshot::{Snapshot, SnapshotBuilder, Summary},
            table_metadata::{FormatVersion, TableMetadata, TableMetadataBuilder},
            types::{PrimitiveType, StructField, Type},
        },
    };
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult, Result as OsResult,
    };

    use crate::table::{datafiles, manifest::ManifestWriter, manifest_list::read_snapshot};

    const CALLERS: usize = 8;

    /// A cache instance that lives for the rest of the test process, as the
    /// fill tasks it spawns require.
    fn leaked(cap_bytes: usize, max_gates: usize) -> &'static ManifestCache {
        Box::leak(Box::new(ManifestCache::new(cap_bytes, max_gates)))
    }

    /// Whether the process-wide cache is disabled (`ICEBERG_MANIFEST_CACHE_MB=0`),
    /// in which case the read-path tests have nothing to share and skip.
    fn global_cache_disabled() -> bool {
        let disabled = CACHE.bytes.is_none();
        if disabled {
            eprintln!("skipped: the process-wide manifest cache is disabled");
        }
        disabled
    }

    #[test]
    fn uri_keys_do_not_collide_across_stores() {
        // Same store-relative path under two buckets must stay two entries,
        // and the stripped form must not be a key at all.
        let cache = ManifestCache::new(1024, MAX_GATES);
        cache.put(
            "s3://bucket-a/db/tbl/metadata/x-m0.avro",
            &Bytes::from_static(b"bytes-a"),
        );
        cache.put(
            "s3://bucket-b/db/tbl/metadata/x-m0.avro",
            &Bytes::from_static(b"bytes-b"),
        );
        assert_eq!(
            cache
                .get("s3://bucket-a/db/tbl/metadata/x-m0.avro")
                .as_deref(),
            Some(b"bytes-a".as_slice())
        );
        assert_eq!(
            cache
                .get("s3://bucket-b/db/tbl/metadata/x-m0.avro")
                .as_deref(),
            Some(b"bytes-b".as_slice())
        );
        assert!(cache.get("/db/tbl/metadata/x-m0.avro").is_none());
    }

    #[test]
    fn hit_and_miss() {
        let mut cache = ByteCappedCache::new(1024);
        assert!(cache.insert("a", &Bytes::from(vec![1u8; 10])));
        assert!(cache.entries.get("a").is_some());
        assert!(cache.entries.get("b").is_none());
    }

    #[test]
    fn byte_cap_evicts_lru() {
        let mut cache = ByteCappedCache::new(100);
        cache.insert("a", &Bytes::from(vec![0u8; 60]));
        cache.insert("b", &Bytes::from(vec![0u8; 60]));
        // "a" evicted to fit "b".
        assert!(cache.entries.get("a").is_none());
        assert!(cache.entries.get("b").is_some());
        assert!(cache.total_bytes <= 100);
    }

    #[test]
    fn oversized_payload_skipped() {
        let mut cache = ByteCappedCache::new(50);
        assert!(!cache.insert("big", &Bytes::from(vec![0u8; 51])));
        assert!(cache.entries.get("big").is_none());
        assert_eq!(cache.total_bytes, 0);
    }

    #[test]
    fn replacing_entry_updates_accounting() {
        let mut cache = ByteCappedCache::new(100);
        cache.insert("a", &Bytes::from(vec![0u8; 40]));
        cache.insert("a", &Bytes::from(vec![0u8; 20]));
        assert_eq!(cache.total_bytes, 20);
    }

    #[test]
    fn disabled_cache_stores_nothing() {
        let cache = ManifestCache::new(0, MAX_GATES);
        assert!(!cache.put("a", &Bytes::from_static(b"a")));
        assert!(cache.get("a").is_none());
    }

    // Concurrent cold fills. The tests run on the current-thread runtime and
    // hold fetches until they release them, so every interleaving they
    // assert on is fixed.

    /// How a [`HoldingStore`] answers the first GET it receives.
    #[derive(Debug, Clone, Copy)]
    enum FirstGet {
        Serve,
        Fail,
    }

    /// In-memory store that counts GETs as they start and holds them until
    /// the test releases them: the first GET and later GETs separately, so a
    /// test can settle the first caller's fill while the rest stay parked.
    /// Only the first GET may be failed.
    #[derive(Debug)]
    struct HoldingStore {
        inner: Arc<InMemory>,
        gets: AtomicUsize,
        hold_first: watch::Receiver<bool>,
        hold_rest: watch::Receiver<bool>,
        first: FirstGet,
    }

    /// Releases a [`HoldingStore`]'s held GETs.
    struct Holds {
        first: watch::Sender<bool>,
        rest: watch::Sender<bool>,
    }

    impl Holds {
        fn release_first(&self) {
            self.first.send_replace(true);
        }

        fn release_rest(&self) {
            self.rest.send_replace(true);
        }

        fn release_all(&self) {
            self.release_first();
            self.release_rest();
        }
    }

    impl HoldingStore {
        fn new(inner: Arc<InMemory>, first: FirstGet) -> (Arc<Self>, Holds) {
            let (first_tx, hold_first) = watch::channel(false);
            let (rest_tx, hold_rest) = watch::channel(false);
            let store = Arc::new(Self {
                inner,
                gets: AtomicUsize::new(0),
                hold_first,
                hold_rest,
                first,
            });
            let holds = Holds {
                first: first_tx,
                rest: rest_tx,
            };
            (store, holds)
        }

        fn gets(&self) -> usize {
            self.gets.load(Ordering::SeqCst)
        }
    }

    impl std::fmt::Display for HoldingStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "HoldingStore({})", self.inner)
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for HoldingStore {
        async fn put_opts(
            &self,
            location: &ObjectPath,
            payload: PutPayload,
            opts: PutOptions,
        ) -> OsResult<PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &ObjectPath,
            opts: PutMultipartOptions,
        ) -> OsResult<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &ObjectPath,
            options: GetOptions,
        ) -> OsResult<GetResult> {
            let call = self.gets.fetch_add(1, Ordering::SeqCst);
            let mut hold = if call == 0 {
                self.hold_first.clone()
            } else {
                self.hold_rest.clone()
            };
            hold.wait_for(|released| *released)
                .await
                .expect("test keeps the hold senders alive");
            if call == 0 && matches!(self.first, FirstGet::Fail) {
                return Err(object_store::Error::Generic {
                    store: "HoldingStore",
                    source: "injected first-GET failure".into(),
                });
            }
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, OsResult<ObjectPath>>,
        ) -> BoxStream<'static, OsResult<ObjectPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(&self, prefix: Option<&ObjectPath>) -> BoxStream<'static, OsResult<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(&self, prefix: Option<&ObjectPath>) -> OsResult<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &ObjectPath,
            to: &ObjectPath,
            options: CopyOptions,
        ) -> OsResult<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    fn test_metadata() -> TableMetadata {
        let schema = SchemaBuilder::default()
            .with_schema_id(0)
            .with_struct_field(StructField {
                id: 1,
                name: "id".to_string(),
                required: true,
                field_type: Type::Primitive(PrimitiveType::Long),
                doc: None,
                initial_default: None,
                write_default: None,
            })
            .build()
            .unwrap();
        TableMetadataBuilder::default()
            .format_version(FormatVersion::V2)
            .location("s3://manifest-cache-tests/table".to_string())
            .current_schema_id(0)
            .schemas(HashMap::from_iter([(0, schema)]))
            .build()
            .unwrap()
    }

    /// An empty manifest list stored at a URI unique to `tag`.
    async fn manifest_list_fixture(tag: &str) -> (Arc<InMemory>, Snapshot) {
        let uri = format!("s3://manifest-cache-tests/table/metadata/snap-{tag}.avro");
        let snapshot = SnapshotBuilder::default()
            .with_snapshot_id(1)
            .with_sequence_number(1)
            .with_timestamp_ms(1)
            .with_manifest_list(uri.clone())
            .with_summary(Summary::default())
            .with_schema_id(0)
            .build()
            .unwrap();
        let bytes = apache_avro::Writer::new(manifest_list_schema_v2(), Vec::new())
            .into_inner()
            .unwrap();
        let store = Arc::new(InMemory::new());
        store
            .put(
                &ObjectPath::from(format!("table/metadata/snap-{tag}.avro").as_str()),
                bytes.into(),
            )
            .await
            .unwrap();
        (store, snapshot)
    }

    /// `count` empty data manifests in one store, at URIs unique to `tag`.
    async fn manifests_fixture(
        tag: &str,
        count: usize,
        metadata: &TableMetadata,
    ) -> (Arc<InMemory>, Vec<ManifestListEntry>) {
        let partition_fields = metadata.current_partition_fields().unwrap();
        let schema = ManifestEntry::schema(
            &partition_value_schema(&partition_fields).unwrap(),
            &metadata.format_version,
        )
        .unwrap();
        let store = Arc::new(InMemory::new());
        let mut manifests = Vec::with_capacity(count);
        for index in 0..count {
            let uri = format!("s3://manifest-cache-tests/table/metadata/{tag}-m{index}.avro");
            let entry = ManifestWriter::new(&uri, 1, &schema, metadata, Content::Data)
                .unwrap()
                .finish(store.clone())
                .await
                .unwrap();
            manifests.push(entry);
        }
        (store, manifests)
    }

    /// One empty data manifest stored at a URI unique to `tag`.
    async fn manifest_fixture(
        tag: &str,
        metadata: &TableMetadata,
    ) -> (Arc<InMemory>, ManifestListEntry) {
        let (store, mut manifests) = manifests_fixture(tag, 1, metadata).await;
        (store, manifests.remove(0))
    }

    async fn read_list(
        store: Arc<HoldingStore>,
        snapshot: Arc<Snapshot>,
        metadata: Arc<TableMetadata>,
    ) -> Result<usize, Error> {
        let entries = read_snapshot(&snapshot, &metadata, store)
            .await?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(entries.len())
    }

    async fn read_manifests(
        store: Arc<HoldingStore>,
        manifests: &[ManifestListEntry],
    ) -> Result<usize, Error> {
        let entries = datafiles(store, manifests, None, (None, None))
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        Ok(entries.len())
    }

    async fn read_manifest(
        store: Arc<HoldingStore>,
        manifest: Arc<ManifestListEntry>,
    ) -> Result<usize, Error> {
        read_manifests(store, std::slice::from_ref(&*manifest)).await
    }

    /// Let every spawned task run until it parks. The tests run on the
    /// current-thread runtime, so a round of yields polls each task in turn.
    async fn settle() {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
    }

    async fn join_all<T>(tasks: Vec<tokio::task::JoinHandle<T>>) -> Vec<T> {
        let mut results = Vec::with_capacity(tasks.len());
        for task in tasks {
            results.push(task.await.expect("task panicked"));
        }
        results
    }

    /// No gate is left behind for `path` once its fills are done.
    fn assert_idle(path: &str) {
        assert!(
            !CACHE.table().contains_key(path),
            "idle gate left for {path}"
        );
    }

    /// A fetch that waits until `open` turns true, then fails or yields
    /// bytes, counting itself in `fetches` when it starts.
    async fn held_fetch(
        mut open: watch::Receiver<bool>,
        fail: bool,
        fetches: Arc<AtomicUsize>,
    ) -> Result<Bytes, Error> {
        fetches.fetch_add(1, Ordering::SeqCst);
        open.wait_for(|open| *open).await.unwrap();
        if fail {
            Err(Error::InvalidFormat("injected fetch failure".to_owned()))
        } else {
            Ok(Bytes::from_static(b"manifest"))
        }
    }

    #[tokio::test]
    async fn gates_count_fills_and_leave_no_idle_entry() {
        let cache = leaked(1024, MAX_GATES);
        let fetches = Arc::new(AtomicUsize::new(0));

        // Shared fill: one fetch serves every caller.
        let path = "s3://manifest-cache-tests/gates/shared-m0.avro";
        let (open_tx, open) = watch::channel(false);
        let callers = (0..CALLERS)
            .map(|_| cache.get_or_fetch(path, || held_fetch(open.clone(), false, fetches.clone())));
        let (results, ()) = futures::join!(futures::future::join_all(callers), async {
            settle().await;
            assert_eq!(cache.table().len(), 1, "all callers on one gate");
            assert_eq!(fetches.load(Ordering::SeqCst), 1, "only the leader fetches");
            open_tx.send_replace(true);
        });
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert_eq!(
            cache.stats(),
            ManifestCacheFillStats {
                fills: 1,
                waits: CALLERS as u64 - 1,
                waiter_refills: 0,
            }
        );
        assert!(cache.table().is_empty(), "gate removed once idle");

        // Warm: served from the cache without touching the gates.
        let warm = cache
            .get_or_fetch(path, || held_fetch(open.clone(), true, fetches.clone()))
            .await
            .unwrap();
        assert_eq!(warm.as_ref(), b"manifest");
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert_eq!(cache.stats().fills, 1);

        // Failed fill: every waiter fetches for itself, all at once.
        let path = "s3://manifest-cache-tests/gates/failed-m0.avro";
        let (first_tx, first) = watch::channel(false);
        let (rest_tx, rest) = watch::channel(false);
        let callers = (0..CALLERS).map(|call| {
            let open = if call == 0 {
                first.clone()
            } else {
                rest.clone()
            };
            let fetches = fetches.clone();
            cache.get_or_fetch(path, move || held_fetch(open, call == 0, fetches))
        });
        let (results, ()) = futures::join!(futures::future::join_all(callers), async {
            settle().await;
            first_tx.send_replace(true);
            settle().await;
            assert_eq!(fetches.load(Ordering::SeqCst), 1 + CALLERS);
            rest_tx.send_replace(true);
        });
        assert!(results[0].is_err());
        assert!(results[1..].iter().all(Result::is_ok), "{results:?}");
        assert_eq!(fetches.load(Ordering::SeqCst), 1 + CALLERS);
        assert_eq!(
            cache.stats(),
            ManifestCacheFillStats {
                fills: 1 + CALLERS as u64,
                waits: 2 * (CALLERS as u64 - 1),
                waiter_refills: CALLERS as u64 - 1,
            }
        );
        assert!(cache.table().is_empty(), "gate removed once idle");
    }

    #[tokio::test]
    async fn full_gate_table_fills_ungated() {
        let cache = leaked(1024, 1);
        let Role::Leader(occupied) =
            cache.enter("s3://manifest-cache-tests/gates/occupied-m0.avro")
        else {
            panic!("the first fill leads");
        };
        let path = "s3://manifest-cache-tests/gates/overflow-m0.avro";
        assert!(
            matches!(cache.enter(path), Role::Ungated),
            "the gate table is full"
        );
        let bytes = cache
            .get_or_fetch(path, || async { Ok(Bytes::from_static(b"overflow")) })
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), b"overflow");
        assert_eq!(cache.get(path).as_deref(), Some(b"overflow".as_slice()));
        assert_eq!(cache.stats().fills, 1);
        assert_eq!(cache.table().len(), 1);
        drop(occupied);
        assert!(cache.table().is_empty());
    }

    #[tokio::test]
    async fn disabled_cache_fills_ungated() {
        // With nothing to share, concurrent callers each fetch at once and no
        // gate is taken.
        let cache = leaked(0, MAX_GATES);
        let fetches = Arc::new(AtomicUsize::new(0));
        let path = "s3://manifest-cache-tests/gates/disabled-m0.avro";
        let (open_tx, open) = watch::channel(false);
        let callers = (0..CALLERS)
            .map(|_| cache.get_or_fetch(path, || held_fetch(open.clone(), false, fetches.clone())));
        let (results, ()) = futures::join!(futures::future::join_all(callers), async {
            settle().await;
            assert_eq!(fetches.load(Ordering::SeqCst), CALLERS, "no caller waits");
            assert!(cache.table().is_empty(), "no gate taken");
            open_tx.send_replace(true);
        });
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(
            cache.stats(),
            ManifestCacheFillStats {
                fills: CALLERS as u64,
                waits: 0,
                waiter_refills: 0,
            }
        );
        assert!(cache.get(path).is_none());
    }

    #[tokio::test]
    async fn evicted_fill_hands_over() {
        // The cache holds one entry, so caching another URI after the fill
        // inserted evicts its bytes before the woken waiters re-check: one
        // waiter fills again and serves the rest.
        let cache = leaked(b"manifest".len(), MAX_GATES);
        let fetches = Arc::new(AtomicUsize::new(0));
        let path = "s3://manifest-cache-tests/gates/evicted-m0.avro";
        let (open_tx, open) = watch::channel(false);
        let callers = (0..CALLERS)
            .map(|_| cache.get_or_fetch(path, || held_fetch(open.clone(), false, fetches.clone())));
        // `join!` polls the control branch before the callers on every wake,
        // so it sees the fill's insert before any waiter re-checks the cache.
        let ((), results) = futures::join!(
            async {
                settle().await;
                open_tx.send_replace(true);
                while cache.get(path).is_none() {
                    tokio::task::yield_now().await;
                }
                assert!(cache.put(
                    "s3://manifest-cache-tests/gates/evictor-m0.avro",
                    &Bytes::from_static(b"evictor!"),
                ));
                assert!(cache.get(path).is_none(), "the fill's bytes were evicted");
            },
            futures::future::join_all(callers)
        );
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            2,
            "one refill after the eviction"
        );
        assert_eq!(
            cache.stats(),
            ManifestCacheFillStats {
                fills: 2,
                waits: CALLERS as u64 - 1,
                waiter_refills: 1,
            }
        );
        assert!(cache.table().is_empty(), "gate removed once idle");
    }

    #[tokio::test]
    async fn abandoned_caller_does_not_stall_waiters() {
        // The first caller starts the fill and is then never polled again.
        let cache = leaked(1024, MAX_GATES);
        let fetches = Arc::new(AtomicUsize::new(0));
        let path = "s3://manifest-cache-tests/gates/abandoned-m0.avro";
        let (open_tx, open) = watch::channel(false);
        let mut abandoned =
            Box::pin(cache.get_or_fetch(path, || held_fetch(open.clone(), false, fetches.clone())));
        assert!(futures::poll!(&mut abandoned).is_pending());
        let waiters = futures::future::join_all((1..CALLERS).map(|_| {
            cache.get_or_fetch(path, || held_fetch(open.clone(), false, fetches.clone()))
        }));
        let (results, ()) = futures::join!(
            async {
                tokio::time::timeout(Duration::from_secs(10), waiters)
                    .await
                    .expect("waiters must not depend on the first caller being polled")
            },
            async {
                settle().await;
                open_tx.send_replace(true);
            }
        );
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "served by the first fill"
        );
        assert!(
            cache.table().is_empty(),
            "the fill ended without its caller"
        );
        // The abandoned caller still gets the bytes when polled again.
        assert_eq!(abandoned.await.unwrap().as_ref(), b"manifest");
    }

    #[tokio::test]
    async fn panicked_fill_hands_over() {
        // The first fetch panics: its caller sees the panic, and one waiter
        // fills again and serves the rest.
        let cache = leaked(1024, MAX_GATES);
        let fetches = Arc::new(AtomicUsize::new(0));
        let path = "s3://manifest-cache-tests/gates/panicked-m0.avro";
        let (open_tx, open) = watch::channel(false);
        let leader = tokio::spawn({
            let (open, fetches) = (open.clone(), fetches.clone());
            cache.get_or_fetch(path, move || async move {
                let fetched = held_fetch(open, false, fetches).await;
                if fetched.is_ok() {
                    panic!("injected fetch panic");
                }
                fetched
            })
        });
        settle().await;
        let waiters = (1..CALLERS)
            .map(|_| {
                let (open, fetches) = (open.clone(), fetches.clone());
                tokio::spawn(cache.get_or_fetch(path, move || held_fetch(open, false, fetches)))
            })
            .collect::<Vec<_>>();
        settle().await;
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "waiters wait for the fill"
        );
        open_tx.send_replace(true);
        assert!(
            leader.await.unwrap_err().is_panic(),
            "the panic reaches the caller that started the fill"
        );
        for result in join_all(waiters).await {
            assert_eq!(result.unwrap().as_ref(), b"manifest");
        }
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            2,
            "one refill after the panic"
        );
        assert_eq!(cache.stats().waiter_refills, 1);
        assert!(cache.table().is_empty(), "gate removed once idle");
    }

    #[tokio::test]
    async fn concurrent_fills_fetch_once() {
        if global_cache_disabled() {
            return;
        }
        let metadata = Arc::new(test_metadata());

        // Manifest list.
        let (inner, snapshot) = manifest_list_fixture("concurrent-fills").await;
        let (store, holds) = HoldingStore::new(inner, FirstGet::Serve);
        let snapshot = Arc::new(snapshot);
        let tasks = (0..CALLERS)
            .map(|_| tokio::spawn(read_list(store.clone(), snapshot.clone(), metadata.clone())))
            .collect::<Vec<_>>();
        settle().await;
        holds.release_all();
        for result in join_all(tasks).await {
            assert_eq!(result.unwrap(), 0);
        }
        assert_eq!(
            store.gets(),
            1,
            "one manifest-list GET for {CALLERS} cold readers"
        );
        assert_idle(snapshot.manifest_list());

        // Manifest file.
        let (inner, manifest) = manifest_fixture("concurrent-fills", &metadata).await;
        let (store, holds) = HoldingStore::new(inner, FirstGet::Serve);
        let manifest = Arc::new(manifest);
        let tasks = (0..CALLERS)
            .map(|_| tokio::spawn(read_manifest(store.clone(), manifest.clone())))
            .collect::<Vec<_>>();
        settle().await;
        holds.release_all();
        for result in join_all(tasks).await {
            assert_eq!(result.unwrap(), 0);
        }
        assert_eq!(
            store.gets(),
            1,
            "one manifest GET for {CALLERS} cold readers"
        );
        assert_idle(&manifest.manifest_path);
    }

    /// Spawn `CALLERS` readers against a store whose first GET fails: waiters
    /// must hold off while the leader fetches, then each fetch for itself once
    /// it failed (concurrently, none queued behind another waiter), and only
    /// the leader sees the error.
    async fn assert_leader_failure_isolated<F, Fut>(store: &HoldingStore, holds: Holds, read: F)
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<usize, Error>> + Send + 'static,
    {
        let tasks = (0..CALLERS)
            .map(|_| tokio::spawn(read()))
            .collect::<Vec<_>>();
        settle().await;
        assert_eq!(store.gets(), 1, "waiters wait for the leader's fetch");
        holds.release_first();
        settle().await;
        assert_eq!(
            store.gets(),
            CALLERS,
            "after the leader failed every waiter fetches at once"
        );
        holds.release_rest();
        let results = join_all(tasks).await;
        assert!(results[0].is_err(), "the leader sees its own failure");
        assert!(
            results[1..].iter().all(Result::is_ok),
            "the leader's failure must not reach waiters: {results:?}"
        );
        // The failure was not cached; the waiters' bytes serve later readers.
        read().await.unwrap();
        assert_eq!(store.gets(), CALLERS);
    }

    #[tokio::test]
    async fn leader_failure_lets_waiter_fill() {
        if global_cache_disabled() {
            return;
        }
        let metadata = Arc::new(test_metadata());

        // Manifest list.
        let (inner, snapshot) = manifest_list_fixture("leader-failure").await;
        let (store, holds) = HoldingStore::new(inner, FirstGet::Fail);
        let snapshot = Arc::new(snapshot);
        assert_leader_failure_isolated(&store, holds, || {
            read_list(store.clone(), snapshot.clone(), metadata.clone())
        })
        .await;
        assert_idle(snapshot.manifest_list());

        // Manifest file.
        let (inner, manifest) = manifest_fixture("leader-failure", &metadata).await;
        let (store, holds) = HoldingStore::new(inner, FirstGet::Fail);
        let manifest = Arc::new(manifest);
        assert_leader_failure_isolated(&store, holds, || {
            read_manifest(store.clone(), manifest.clone())
        })
        .await;
        assert_idle(&manifest.manifest_path);
    }

    /// Spawn a leader, queue `CALLERS - 1` waiters behind it and abort the
    /// leader mid-fetch: its fill keeps running and serves every waiter.
    async fn assert_cancelled_leader_keeps_fill<F, Fut>(store: &HoldingStore, holds: Holds, read: F)
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<usize, Error>> + Send + 'static,
    {
        let leader = tokio::spawn(read());
        settle().await;
        assert_eq!(store.gets(), 1, "the leader's fill is parked in its GET");
        let waiters = (1..CALLERS)
            .map(|_| tokio::spawn(read()))
            .collect::<Vec<_>>();
        settle().await;
        assert_eq!(store.gets(), 1, "waiters queue behind the fill");
        leader.abort();
        assert!(leader.await.unwrap_err().is_cancelled());
        settle().await;
        assert_eq!(store.gets(), 1, "the fill outlives its caller");
        holds.release_all();
        for result in join_all(waiters).await {
            assert_eq!(result.unwrap(), 0);
        }
        assert_eq!(
            store.gets(),
            1,
            "the cancelled caller's GET served everyone"
        );
    }

    #[tokio::test]
    async fn cancelled_leader_keeps_fill_running() {
        if global_cache_disabled() {
            return;
        }
        let metadata = Arc::new(test_metadata());

        // Manifest list.
        let (inner, snapshot) = manifest_list_fixture("cancelled-leader").await;
        let (store, holds) = HoldingStore::new(inner, FirstGet::Serve);
        let snapshot = Arc::new(snapshot);
        assert_cancelled_leader_keeps_fill(&store, holds, || {
            read_list(store.clone(), snapshot.clone(), metadata.clone())
        })
        .await;
        assert_idle(snapshot.manifest_list());

        // Manifest file.
        let (inner, manifest) = manifest_fixture("cancelled-leader", &metadata).await;
        let (store, holds) = HoldingStore::new(inner, FirstGet::Serve);
        let manifest = Arc::new(manifest);
        assert_cancelled_leader_keeps_fill(&store, holds, || {
            read_manifest(store.clone(), manifest.clone())
        })
        .await;
        assert_idle(&manifest.manifest_path);
    }

    #[tokio::test]
    async fn stalled_scan_stream_does_not_block_other_readers() {
        // A reader that pulls from its manifest stream once and then neither
        // polls nor drops it still has fetches in flight; other readers of the
        // same manifests, even on the same task, must not wait on it.
        if global_cache_disabled() {
            return;
        }
        let metadata = test_metadata();
        let (inner, manifests) = manifests_fixture("stalled-stream", 2, &metadata).await;
        let (store, holds) = HoldingStore::new(inner, FirstGet::Serve);
        let mut stalled = Box::pin(
            datafiles(store.clone(), &manifests, None, (None, None))
                .await
                .unwrap(),
        );
        assert!(
            stalled.next().now_or_never().is_none(),
            "the stalled reader's GETs are held"
        );
        settle().await;
        assert_eq!(store.gets(), manifests.len());
        holds.release_all();
        let entries = tokio::time::timeout(
            Duration::from_secs(10),
            read_manifests(store.clone(), &manifests),
        )
        .await
        .expect("a stalled reader must not block other readers")
        .unwrap();
        assert_eq!(entries, 0);
        assert_eq!(
            store.gets(),
            manifests.len(),
            "served by the stalled reader's fills"
        );
        for manifest in &manifests {
            assert_idle(&manifest.manifest_path);
        }
        drop(stalled);
    }
}
