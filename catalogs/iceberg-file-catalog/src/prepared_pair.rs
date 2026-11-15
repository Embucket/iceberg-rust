//! Explicit local two-head append publication. SQL/cache snapshot integration is separate.

use super::*;
use iceberg_rust::catalog::commit::TableUpdate;
use iceberg_rust::spec::snapshot::Operation;
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path as NativePath, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use uuid::Uuid;

const JOURNAL: &str = ".iceberg-append-pair-v1";
const WITNESS: &str = ".iceberg-append-pair-required-v1";
const MAX_METADATA: usize = 1024 * 1024;
const MAX_JOURNAL: usize = 16 * 1024 * 1024;
const MAX_ARTIFACTS: usize = 4096;
const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARTIFACT_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const UNPREPARED: u8 = 0;
const HEALTHY: u8 = 1;
const PENDING: u8 = 2;
const FENCED: u8 = 3;

#[derive(Debug, Default)]
pub(crate) struct PairState(AtomicU8);

static STATES: OnceLock<StdMutex<HashMap<String, Weak<PairState>>>> = OnceLock::new();

pub(crate) fn state_for(key: &str) -> Arc<PairState> {
    let mut states = STATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if states.len() > 4096 {
        states.retain(|_, state| state.strong_count() != 0);
    }
    if let Some(state) = states.get(key).and_then(Weak::upgrade) {
        return state;
    }
    let state = Arc::new(PairState::default());
    states.insert(key.to_owned(), Arc::downgrade(&state));
    state
}

pub(crate) fn state_for_path(path: &str, builder: &ObjectStoreBuilder) -> Arc<PairState> {
    state_for(&state_key(path, builder))
}

pub(crate) fn state_key(path: &str, builder: &ObjectStoreBuilder) -> String {
    if let ObjectStoreBuilder::Filesystem(store) = builder {
        if let Ok(native) = store.path_to_filesystem(&ObjectPath::from(strip_prefix(path))) {
            let native = native.canonicalize().unwrap_or(native);
            return format!("file:{}", native.display());
        }
    }
    let bucket = Bucket::from_path(path)
        .map(|bucket| bucket.to_string())
        .unwrap_or_default();
    format!("{bucket}/{}", ObjectPath::from(strip_prefix(path)))
        .trim_end_matches('/')
        .to_owned()
}

fn refusal(message: impl Into<String>) -> IcebergError {
    IcebergError::NotSupported(message.into())
}
fn io(error: impl std::error::Error + Send + Sync + 'static) -> IcebergError {
    IcebergError::External(Box::new(error))
}
fn fenced() -> IcebergError {
    refusal("local prepared append pair requires explicit recovery")
}

/// Whether a failed publication may have made a durable commit decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PairOutcome {
    NotCommitted,
    DecisionUnknown,
    CommittedNeedsRecovery,
}

#[derive(Debug, thiserror::Error)]
#[error("prepared append pair {outcome:?}: {source}")]
pub struct PairPublishError {
    pub outcome: PairOutcome,
    #[source]
    pub source: IcebergError,
}

#[derive(Clone, Debug)]
struct Entry {
    identifier: Identifier,
    old_location: String,
    new_location: String,
    old_bytes: Vec<u8>,
    old_metadata: TableMetadata,
    commit: CommitTable,
    new_metadata: TableMetadata,
    new_bytes: Vec<u8>,
}

#[derive(Debug)]
struct NativeRoot {
    path: PathBuf,
    key: String,
    location: String,
    store: Arc<object_store::local::LocalFileSystem>,
}

impl NativeRoot {
    fn file(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }

    fn regular(&self, path: &NativePath) -> Result<File, IcebergError> {
        let metadata = std::fs::symlink_metadata(path).map_err(io)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(refusal("pair artifact must be a regular non-symlink file"));
        }
        let canonical = path.canonicalize().map_err(io)?;
        if !canonical.starts_with(&self.path) {
            return Err(refusal("pair artifact escapes its configured catalog"));
        }
        File::open(path).map_err(io)
    }

    fn read(&self, path: &NativePath, limit: usize) -> Result<Vec<u8>, IcebergError> {
        let mut file = self.regular(path)?;
        if file.metadata().map_err(io)?.len() > limit as u64 {
            return Err(refusal("prepared-pair file exceeds its byte bound"));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() > limit {
            return Err(refusal("prepared-pair file exceeds its byte bound"));
        }
        Ok(bytes)
    }

    fn sync_directory(&self, path: &NativePath) -> Result<(), IcebergError> {
        let metadata = std::fs::symlink_metadata(path).map_err(io)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || !path.canonicalize().map_err(io)?.starts_with(&self.path)
        {
            return Err(refusal("prepared-pair directory changed"));
        }
        File::open(path).map_err(io)?.sync_all().map_err(io)
    }

    // The root owner excludes external path swaps. This is not a hostile-writer API.
    fn temporary(&self, parent: &NativePath, bytes: &[u8]) -> Result<PathBuf, IcebergError> {
        self.sync_directory(parent)?;
        let path = parent.join(format!(".append-pair-{}.tmp", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| {
            let mut file = options.open(&path).map_err(io)?;
            file.write_all(bytes).map_err(io)?;
            file.sync_all().map_err(io)
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        Ok(path)
    }

    fn replace_control(
        &self,
        name: &str,
        bytes: &[u8],
        attempted: &mut bool,
    ) -> Result<(), IcebergError> {
        if bytes.len() > MAX_JOURNAL {
            return Err(refusal("prepared-pair journal exceeds its byte bound"));
        }
        let destination = self.file(name);
        if destination.try_exists().map_err(io)? {
            self.regular(&destination)?;
        }
        let temporary = self.temporary(&self.path, bytes)?;
        *attempted = true;
        if let Err(error) = std::fs::rename(&temporary, &destination) {
            let _ = std::fs::remove_file(&temporary);
            return Err(io(error));
        }
        self.sync_directory(&self.path)
    }

    fn install_head(&self, entry: &Entry, path: &NativePath) -> Result<(), IcebergError> {
        if path.try_exists().map_err(io)? {
            if self.read(path, MAX_METADATA)? == entry.new_bytes {
                self.regular(path)?.sync_all().map_err(io)?;
                return self.sync_directory(
                    path.parent()
                        .ok_or_else(|| refusal("missing head parent"))?,
                );
            }
            return Err(refusal("prepared-pair successor head has different bytes"));
        }
        let parent = path
            .parent()
            .ok_or_else(|| refusal("missing head parent"))?;
        let temporary = self.temporary(parent, &entry.new_bytes)?;
        let linked = std::fs::hard_link(&temporary, path);
        let cleanup = std::fs::remove_file(&temporary);
        if let Err(error) = linked {
            return Err(io(error));
        }
        cleanup.map_err(io)?;
        self.sync_directory(parent)
    }
}

/// Opaque candidate retaining actual catalog-wide write exclusion. Drop before
/// publish has no persistence effect. The native worker owns the guard after dispatch.
#[derive(Debug)]
pub struct PreparedAppendPair {
    catalog: Arc<FileCatalog>,
    _guard: tokio::sync::OwnedRwLockWriteGuard<()>,
    root: NativeRoot,
    operation_id: Uuid,
    entries: [Entry; 2],
    artifacts: Vec<PathBuf>,
    #[cfg(test)]
    hook: Option<Arc<TestHook>>,
}

/// Exact committed metadata images, before an embedding engine refreshes its own providers.
#[derive(Debug)]
pub struct CommittedPairReceipt {
    pub operation_id: Uuid,
    pub tables: [(Identifier, TableMetadata); 2],
}

/// Cooperating catalog-reader exclusion. The embedding engine must separately
/// cover cached providers, query snapshot construction and physical-plan lifetime.
pub struct FileCatalogReadScope {
    catalog: Arc<FileCatalog>,
    _guard: tokio::sync::OwnedRwLockReadGuard<()>,
}

impl FileCatalogReadScope {
    pub async fn load(&self, identifier: &Identifier) -> Result<Tabular, IcebergError> {
        self.catalog.check_pair_health()?;
        Arc::clone(&self.catalog)
            .load_tabular_under_scope(identifier)
            .await
    }
}

impl FileCatalog {
    fn pair_root(&self) -> Result<NativeRoot, IcebergError> {
        let ObjectStoreBuilder::Filesystem(store) = &self.object_store else {
            return Err(refusal(
                "prepared append pairs require a local filesystem catalog",
            ));
        };
        let path = store
            .path_to_filesystem(&self.catalog_root_path())
            .map_err(IcebergError::from)?;
        let path = path.canonicalize().map_err(io)?;
        Ok(NativeRoot {
            path,
            key: state_key(&self.path, &self.object_store),
            location: self.path.clone(),
            store: Arc::clone(store),
        })
    }

    fn pair_native(&self, location: &str) -> Result<PathBuf, IcebergError> {
        let ObjectStoreBuilder::Filesystem(store) = &self.object_store else {
            return Err(refusal(
                "prepared append pairs require a local filesystem catalog",
            ));
        };
        store
            .path_to_filesystem(&ObjectPath::from(strip_prefix(location)))
            .map_err(IcebergError::from)
    }

    pub(crate) fn check_pair_health(&self) -> Result<(), IcebergError> {
        match self.pair_state.0.load(Ordering::Acquire) {
            HEALTHY => Ok(()),
            UNPREPARED => {
                if !matches!(self.object_store, ObjectStoreBuilder::Filesystem(_)) {
                    self.pair_state.0.store(HEALTHY, Ordering::Release);
                    return Ok(());
                }
                let ObjectStoreBuilder::Filesystem(store) = &self.object_store else {
                    return Err(fenced());
                };
                let path = store
                    .path_to_filesystem(&self.catalog_root_path())
                    .map_err(IcebergError::from)?;
                for name in [WITNESS, JOURNAL] {
                    match std::fs::symlink_metadata(path.join(name)) {
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        _ => {
                            self.pair_state.0.store(FENCED, Ordering::Release);
                            return Err(fenced());
                        }
                    }
                }
                self.pair_state
                    .0
                    .compare_exchange(UNPREPARED, HEALTHY, Ordering::AcqRel, Ordering::Acquire)
                    .ok();
                if self.pair_state.0.load(Ordering::Acquire) == HEALTHY {
                    Ok(())
                } else {
                    Err(fenced())
                }
            }
            _ => Err(fenced()),
        }
    }

    pub async fn read_scope(self: &Arc<Self>) -> Result<FileCatalogReadScope, IcebergError> {
        Ok(FileCatalogReadScope {
            catalog: Arc::clone(self),
            _guard: self.mutation_scope().await?,
        })
    }

    /// Exactly two main-branch native append commits. Preparation validates fresh
    /// heads while excluding all ordinary catalog mutations; it publishes nothing.
    pub async fn prepare_append_pair(
        self: &Arc<Self>,
        commits: [CommitTable; 2],
    ) -> Result<PreparedAppendPair, IcebergError> {
        let guard = self.catalog_mutation_lock().write_owned().await;
        self.check_pair_health()?;
        let root = self.pair_root()?;
        if commits[0].identifier == commits[1].identifier {
            return Err(refusal("prepared append pair needs two distinct targets"));
        }
        let mut entries = Vec::new();
        let mut artifacts = HashSet::new();
        let mut artifact_metadata_bytes = 0_u64;
        for commit in commits {
            if commit.identifier.namespace().len() != 1
                || !self
                    .renamed_tables
                    .read()
                    .map_err(|_| refusal("pair rename cache is poisoned"))?
                    .is_empty()
            {
                return Err(refusal("prepared append pair requires plain local targets"));
            }
            let old_location = self.metadata_location(&commit.identifier).await?;
            let Tabular::Table(old_table) = Arc::clone(self)
                .load_tabular_under_scope(&commit.identifier)
                .await?
            else {
                return Err(refusal("prepared append pair requires Iceberg tables"));
            };
            let old = old_table.metadata();
            let expected_location =
                self.tabular_path(&commit.identifier.namespace()[0], commit.identifier.name());
            if old.table_uuid.is_nil()
                || old.location != expected_location
                || !check_table_requirements(&commit.requirements, old)
            {
                return Err(IcebergError::CommitConflict(commit.identifier.to_string()));
            }
            let [TableUpdate::AddSnapshot { snapshot }, TableUpdate::SetSnapshotRef {
                ref_name,
                snapshot_reference,
            }] = commit.updates.as_slice()
            else {
                return Err(refusal(
                    "prepared append pair accepts one native append snapshot only",
                ));
            };
            if ref_name != "main"
                || snapshot.summary().operation != Operation::Append
                || *snapshot.parent_snapshot_id() != old.current_snapshot_id
                || *snapshot.snapshot_id() != snapshot_reference.snapshot_id
            {
                return Err(refusal(
                    "prepared append pair accepts a current main-branch append only",
                ));
            }
            let mut next = old.clone();
            apply_table_updates(&mut next, commit.updates.clone())?;
            let new_bytes = serde_json::to_vec(&next)?;
            if new_bytes.len() > MAX_METADATA {
                return Err(refusal("prepared-pair metadata exceeds its byte bound"));
            }
            let old_path = self.pair_native(&old_location)?;
            let old_bytes = root.read(&old_path, MAX_METADATA)?;
            let new_location =
                new_filesystem_metadata_location(&next.location, &old_location, false)?;
            let candidate = Table::new(
                commit.identifier.clone(),
                Arc::clone(self) as Arc<dyn Catalog>,
                old_table.object_store(),
                next.clone(),
            )
            .await?;
            if let Some(snapshot) = next.current_snapshot(None)? {
                let path = self.pair_native(snapshot.manifest_list())?;
                add_metadata_artifact(&root, &mut artifacts, &mut artifact_metadata_bytes, path)?;
            }
            let manifests = candidate.manifests(None, None).await?;
            if manifests.len() > MAX_ARTIFACTS {
                return Err(refusal("prepared-pair manifest count exceeds its bound"));
            }
            for manifest in &manifests {
                add_metadata_artifact(
                    &root,
                    &mut artifacts,
                    &mut artifact_metadata_bytes,
                    self.pair_native(&manifest.manifest_path)?,
                )?;
            }
            let mut files = candidate.datafiles(&manifests, None, (None, None)).await?;
            while let Some((_, entry)) = files.try_next().await? {
                artifacts.insert(self.pair_native(entry.data_file().file_path())?);
                if artifacts.len() > MAX_ARTIFACTS {
                    return Err(refusal("prepared-pair artifact count exceeds its bound"));
                }
            }
            entries.push(Entry {
                identifier: commit.identifier.clone(),
                old_location,
                new_location,
                old_bytes,
                old_metadata: old.clone(),
                commit,
                new_metadata: next,
                new_bytes,
            });
        }
        let entries: [Entry; 2] = entries
            .try_into()
            .map_err(|_| refusal("prepared append pair requires two targets"))?;
        let mut prepared = PreparedAppendPair {
            catalog: Arc::clone(self),
            _guard: guard,
            root,
            operation_id: Uuid::new_v4(),
            entries,
            artifacts: artifacts.into_iter().collect(),
            #[cfg(test)]
            hook: None,
        };
        prepared.artifacts.sort();
        // Encode and bound all durable control bytes before entering the native worker.
        prepared.journal("commit")?;
        Ok(prepared)
    }

    /// Recover under catalog-wide write exclusion before exposing any table heads.
    /// A synchronous CatalogList handle with required pair controls remains fenced
    /// until an explicit asynchronous constructor/recovery establishes this receipt.
    pub async fn recover_append_pair(&self) -> Result<(), IcebergError> {
        let guard = self.catalog_mutation_lock().write_owned().await;
        if !matches!(self.object_store, ObjectStoreBuilder::Filesystem(_)) {
            self.pair_state.0.store(HEALTHY, Ordering::Release);
            return Ok(());
        }
        let ObjectStoreBuilder::Filesystem(store) = &self.object_store else {
            return Err(fenced());
        };
        let path = store
            .path_to_filesystem(&self.catalog_root_path())
            .map_err(IcebergError::from)?;
        if !path.try_exists().map_err(io)? {
            self.pair_state.0.store(HEALTHY, Ordering::Release);
            return Ok(());
        }
        let root = self.pair_root()?;
        let state = Arc::clone(&self.pair_state);
        let cache = Arc::clone(&self.cache);
        state.0.store(PENDING, Ordering::Release);
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let result = recover(&root, &cache);
            state.0.store(
                if result.is_ok() { HEALTHY } else { FENCED },
                Ordering::Release,
            );
            result
        })
        .await
        .map_err(io)?
    }
}

impl PreparedAppendPair {
    fn journal(&self, phase: &str) -> Result<Vec<u8>, IcebergError> {
        encode_journal(&self.root.key, self.operation_id, phase, &self.entries)
    }

    /// One non-cancellable blocking owner performs the decision and both installs.
    /// A cancelled waiter cannot release the catalog writer; an unacknowledged
    /// successful decision fences admission until explicit recovery.
    pub async fn publish(self) -> Result<CommittedPairReceipt, PairPublishError> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let state = Arc::clone(&self.catalog.pair_state);
        drop(tokio::task::spawn_blocking(move || {
            let mut owner = PublishOwner {
                prepared: self,
                finished: false,
            };
            let result = owner.publish();
            let healthy = result.is_ok()
                || matches!(&result, Err(error) if error.outcome == PairOutcome::NotCommitted);
            if sender.send(result).is_ok() && healthy {
                owner
                    .prepared
                    .catalog
                    .pair_state
                    .0
                    .store(HEALTHY, Ordering::Release);
            } else {
                owner
                    .prepared
                    .catalog
                    .pair_state
                    .0
                    .store(FENCED, Ordering::Release);
            }
            owner.finished = true;
        }));
        receiver.await.map_err(|error| {
            state.0.store(FENCED, Ordering::Release);
            PairPublishError {
                outcome: PairOutcome::DecisionUnknown,
                source: io(error),
            }
        })?
    }
}

struct PublishOwner {
    prepared: PreparedAppendPair,
    finished: bool,
}
impl Drop for PublishOwner {
    fn drop(&mut self) {
        if !self.finished {
            self.prepared
                .catalog
                .pair_state
                .0
                .store(FENCED, Ordering::Release);
        }
    }
}
impl PublishOwner {
    fn publish(&mut self) -> Result<CommittedPairReceipt, PairPublishError> {
        let prepared = &self.prepared;
        let root = &prepared.root;
        let mut outcome = PairOutcome::NotCommitted;
        let result = (|| {
            validate_entries(root, &prepared.entries, false)?;
            let mut total = 0_u64;
            for path in &prepared.artifacts {
                let file = root.regular(path)?;
                total = total
                    .checked_add(file.metadata().map_err(io)?.len())
                    .ok_or_else(|| refusal("prepared-pair artifact byte overflow"))?;
                if total > MAX_ARTIFACT_BYTES {
                    return Err(refusal("prepared-pair artifact bytes exceed their bound"));
                }
                file.sync_all().map_err(io)?;
                root.sync_ancestors(path.parent().ok_or_else(|| refusal("artifact parent"))?)?;
            }
            prepare_witness(root)?;
            let ready = prepared.journal("ready")?;
            let mut attempted = false;
            root.replace_control(JOURNAL, &ready, &mut attempted)?;
            prepared
                .catalog
                .pair_state
                .0
                .store(PENDING, Ordering::Release);
            #[cfg(test)]
            checkpoint(&prepared.hook, TestStage::BeforeDecision)?;
            let decision = prepared.journal("commit")?;
            let mut decision_attempted = false;
            let decision_result = root.replace_control(JOURNAL, &decision, &mut decision_attempted);
            if decision_attempted {
                outcome = PairOutcome::DecisionUnknown;
            }
            decision_result?;
            outcome = PairOutcome::CommittedNeedsRecovery;
            #[cfg(test)]
            checkpoint(&prepared.hook, TestStage::AfterDecision)?;
            for (index, entry) in prepared.entries.iter().enumerate() {
                let path = prepared.catalog.pair_native(&entry.new_location)?;
                root.install_head(entry, &path)?;
                #[cfg(test)]
                if index == 0 {
                    checkpoint(&prepared.hook, TestStage::AfterFirstHead)?;
                }
                #[cfg(not(test))]
                let _ = index;
            }
            let complete = prepared.journal("complete")?;
            root.replace_control(JOURNAL, &complete, &mut false)?;
            publish_cache(&prepared.catalog.cache, &prepared.entries)?;
            Ok(CommittedPairReceipt {
                operation_id: prepared.operation_id,
                tables: prepared
                    .entries
                    .clone()
                    .map(|entry| (entry.identifier, entry.new_metadata)),
            })
        })();
        result.map_err(|source| {
            // Before decision no target changed. Persist an explicit terminal abort;
            // if that acknowledgement fails, require recovery rather than absence fallback.
            if outcome == PairOutcome::NotCommitted && root.file(WITNESS).exists() {
                if let Ok(aborted) = prepared.journal("aborted") {
                    if root.replace_control(JOURNAL, &aborted, &mut false).is_err() {
                        outcome = PairOutcome::DecisionUnknown;
                    }
                } else {
                    outcome = PairOutcome::DecisionUnknown;
                }
            }
            PairPublishError { outcome, source }
        })
    }
}

impl NativeRoot {
    fn native(&self, location: &str) -> Result<PathBuf, IcebergError> {
        self.store
            .path_to_filesystem(&ObjectPath::from(strip_prefix(location)))
            .map_err(IcebergError::from)
    }

    fn paths(&self, entry: &Entry) -> Result<(PathBuf, PathBuf), IcebergError> {
        let identifier = &entry.identifier;
        if identifier.namespace().len() != 1
            || !plain_component(&identifier.namespace()[0])
            || !plain_component(identifier.name())
        {
            return Err(refusal(
                "prepared-pair identifier is outside the local subset",
            ));
        }
        let location = format!(
            "{}/{}/{}",
            self.location.trim_end_matches('/'),
            identifier.namespace()[0],
            identifier.name()
        );
        if entry.new_metadata.location != location
            || entry.old_metadata.location != location
            || entry.new_metadata.table_uuid.is_nil()
            || entry.new_metadata.table_uuid != entry.old_metadata.table_uuid
        {
            return Err(refusal("prepared-pair table identity or location changed"));
        }
        let old = self.native(&entry.old_location)?;
        let parent = self.native(&format!("{location}/metadata"))?;
        if old.parent() != Some(parent.as_path()) {
            return Err(refusal(
                "prepared-pair predecessor head is outside its target",
            ));
        }
        let expected = new_filesystem_metadata_location(&location, &entry.old_location, false)?;
        if entry.new_location != expected {
            return Err(refusal(
                "prepared-pair successor path is not derived from its predecessor",
            ));
        }
        Ok((old, self.native(&expected)?))
    }
}

fn plain_component(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && value.len() <= 255
        && chars.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | ' ')
        })
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                DIGITS[(byte >> 4) as usize] as char,
                DIGITS[(byte & 15) as usize] as char,
            ]
        })
        .collect()
}
fn unhex(value: &str) -> Result<Vec<u8>, IcebergError> {
    if value.len() > MAX_METADATA * 2 || value.len() % 2 != 0 {
        return Err(refusal(
            "prepared-pair predecessor encoding exceeds its bound",
        ));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |value: u8| match value {
                b'0'..=b'9' => Some(value - b'0'),
                b'a'..=b'f' => Some(value - b'a' + 10),
                _ => None,
            };
            let high = digit(pair[0])
                .ok_or_else(|| refusal("invalid prepared-pair predecessor encoding"))?;
            let low = digit(pair[1])
                .ok_or_else(|| refusal("invalid prepared-pair predecessor encoding"))?;
            Ok(high * 16 + low)
        })
        .collect()
}

fn encode_journal(
    key: &str,
    operation_id: Uuid,
    phase: &str,
    entries: &[Entry; 2],
) -> Result<Vec<u8>, IcebergError> {
    let entries = entries
        .iter()
        .map(|entry| {
            json!({
                "identifier": entry.identifier,
                "old_location": entry.old_location,
                "new_location": entry.new_location,
                "old_hex": hex(&entry.old_bytes),
                "old_metadata": entry.old_metadata,
                "commit": entry.commit,
                "new_json": String::from_utf8_lossy(&entry.new_bytes),
            })
        })
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec(
        &json!({ "version": 1, "catalog": key, "operation_id": operation_id.to_string(), "phase": phase, "entries": entries }),
    )?;
    if bytes.len() > MAX_JOURNAL {
        return Err(refusal("prepared-pair journal exceeds its byte bound"));
    }
    Ok(bytes)
}

fn decode_journal(
    root: &NativeRoot,
    bytes: &[u8],
) -> Result<(Uuid, String, [Entry; 2]), IcebergError> {
    let value: Value = serde_json::from_slice(bytes)?;
    if value.as_object().map(|map| map.len()) != Some(5)
        || value["version"] != 1
        || value["catalog"] != root.key
    {
        return Err(refusal(
            "prepared-pair journal has a foreign root or version",
        ));
    }
    let operation = Uuid::parse_str(
        value["operation_id"]
            .as_str()
            .ok_or_else(|| refusal("missing prepared-pair operation"))?,
    )
    .map_err(io)?;
    if operation.is_nil() {
        return Err(refusal("nil prepared-pair operation"));
    }
    let phase = value["phase"]
        .as_str()
        .ok_or_else(|| refusal("missing prepared-pair phase"))?
        .to_owned();
    if !matches!(phase.as_str(), "ready" | "commit" | "complete" | "aborted") {
        return Err(refusal("unknown prepared-pair phase"));
    }
    let raw_entries = value["entries"]
        .as_array()
        .ok_or_else(|| refusal("missing prepared-pair entries"))?;
    if raw_entries.len() != 2 {
        return Err(refusal("prepared-pair journal needs two entries"));
    }
    let mut entries = Vec::new();
    for raw in raw_entries {
        if raw.as_object().map(|map| map.len()) != Some(7) {
            return Err(refusal("invalid prepared-pair entry"));
        }
        let new_json = raw["new_json"]
            .as_str()
            .ok_or_else(|| refusal("missing prepared-pair successor"))?;
        if new_json.len() > MAX_METADATA {
            return Err(refusal("prepared-pair metadata exceeds its byte bound"));
        }
        let entry = Entry {
            identifier: serde_json::from_value(raw["identifier"].clone())?,
            old_location: raw["old_location"]
                .as_str()
                .ok_or_else(|| refusal("missing predecessor location"))?
                .to_owned(),
            new_location: raw["new_location"]
                .as_str()
                .ok_or_else(|| refusal("missing successor location"))?
                .to_owned(),
            old_bytes: unhex(
                raw["old_hex"]
                    .as_str()
                    .ok_or_else(|| refusal("missing predecessor bytes"))?,
            )?,
            old_metadata: serde_json::from_value(raw["old_metadata"].clone())?,
            commit: serde_json::from_value(raw["commit"].clone())?,
            new_metadata: serde_json::from_str(new_json)?,
            new_bytes: new_json.as_bytes().to_vec(),
        };
        root.paths(&entry)?;
        validate_transition(&entry)?;
        entries.push(entry);
    }
    let entries: [Entry; 2] = entries
        .try_into()
        .map_err(|_| refusal("two prepared-pair entries"))?;
    if entries[0].identifier == entries[1].identifier
        || entries[0].new_metadata.table_uuid == entries[1].new_metadata.table_uuid
    {
        return Err(refusal("prepared-pair targets are not distinct"));
    }
    Ok((operation, phase, entries))
}

fn validate_transition(entry: &Entry) -> Result<(), IcebergError> {
    if entry.commit.identifier != entry.identifier
        || !check_table_requirements(&entry.commit.requirements, &entry.old_metadata)
    {
        return Err(refusal(
            "prepared-pair commit does not match its predecessor",
        ));
    }
    let [TableUpdate::AddSnapshot { snapshot }, TableUpdate::SetSnapshotRef {
        ref_name,
        snapshot_reference,
    }] = entry.commit.updates.as_slice()
    else {
        return Err(refusal("prepared-pair commit is not a single append"));
    };
    if ref_name != "main"
        || snapshot.summary().operation != Operation::Append
        || *snapshot.parent_snapshot_id() != entry.old_metadata.current_snapshot_id
        || *snapshot.snapshot_id() != snapshot_reference.snapshot_id
    {
        return Err(refusal("prepared-pair commit is not a current main append"));
    }
    let mut expected = entry.old_metadata.clone();
    apply_table_updates(&mut expected, entry.commit.updates.clone())?;
    expected.last_updated_ms = entry.new_metadata.last_updated_ms;
    if expected != entry.new_metadata {
        return Err(refusal(
            "prepared-pair successor is not the exact prepared append",
        ));
    }
    Ok(())
}

fn validate_entries(
    root: &NativeRoot,
    entries: &[Entry; 2],
    decided: bool,
) -> Result<(), IcebergError> {
    for entry in entries {
        validate_transition(entry)?;
        let (old, new) = root.paths(entry)?;
        let parent = old.parent().ok_or_else(|| refusal("head parent"))?;
        let marker = parent.join("table-uuid");
        if root.read(&marker, 16)? != entry.new_metadata.table_uuid.as_bytes() {
            return Err(refusal("prepared-pair physical UUID changed"));
        }
        if root.read(&old, MAX_METADATA)? != entry.old_bytes {
            return Err(refusal("prepared-pair predecessor bytes changed"));
        }
        root.regular(&old)?.sync_all().map_err(io)?;
        root.regular(&marker)?.sync_all().map_err(io)?;
        root.sync_ancestors(parent)?;
        let old_version = parse_version(&entry.old_location)?;
        let new_version = parse_version(&entry.new_location)?;
        for item in std::fs::read_dir(parent).map_err(io)? {
            let item = item.map_err(io)?;
            let path = item.path();
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                if let Ok(version) = parse_version(name) {
                    if version > old_version
                        && (!decided
                            || version != new_version
                            || path != new
                            || root.read(&path, MAX_METADATA)? != entry.new_bytes)
                    {
                        return Err(refusal("prepared-pair head advanced outside its decision"));
                    }
                }
            }
        }
    }
    Ok(())
}

fn witness_bytes(root: &NativeRoot) -> Result<Vec<u8>, IcebergError> {
    Ok(serde_json::to_vec(
        &json!({"version":1,"catalog":root.key}),
    )?)
}
fn prepare_witness(root: &NativeRoot) -> Result<(), IcebergError> {
    let expected = witness_bytes(root)?;
    let path = root.file(WITNESS);
    if path.try_exists().map_err(io)? {
        if root.read(&path, 4096)? != expected {
            return Err(refusal("prepared-pair required witness changed"));
        }
        return Ok(());
    }
    let temporary = root.temporary(&root.path, &expected)?;
    let result = std::fs::hard_link(&temporary, &path);
    let cleanup = std::fs::remove_file(&temporary);
    result.map_err(io)?;
    cleanup.map_err(io)?;
    root.sync_directory(&root.path)
}

type Cache = RwLock<HashMap<Identifier, (String, TabularMetadata)>>;
fn publish_cache(cache: &Cache, entries: &[Entry; 2]) -> Result<(), IcebergError> {
    let mut cache = cache
        .write()
        .map_err(|_| refusal("prepared-pair cache is poisoned"))?;
    for entry in entries {
        cache.insert(
            entry.identifier.clone(),
            (
                entry.new_location.clone(),
                entry.new_metadata.clone().into(),
            ),
        );
    }
    Ok(())
}

fn recover(root: &NativeRoot, cache: &Cache) -> Result<(), IcebergError> {
    let witness = root.file(WITNESS).try_exists().map_err(io)?;
    let journal = root.file(JOURNAL).try_exists().map_err(io)?;
    if !witness && !journal {
        return Ok(());
    }
    if !witness || !journal {
        return Err(refusal("required prepared-pair controls are missing"));
    }
    if root.read(&root.file(WITNESS), 4096)? != witness_bytes(root)? {
        return Err(refusal("prepared-pair witness has a foreign root"));
    }
    let bytes = root.read(&root.file(JOURNAL), MAX_JOURNAL)?;
    let (operation, phase, entries) = decode_journal(root, &bytes)?;
    match phase.as_str() {
        // Completed/aborted decisions do not retain authority over later ordinary
        // appends or DROP/recreate. Their syntax/root binding must still validate.
        "complete" | "aborted" => Ok(()),
        "ready" => {
            validate_entries(root, &entries, false)?;
            root.replace_control(
                JOURNAL,
                &encode_journal(&root.key, operation, "aborted", &entries)?,
                &mut false,
            )
        }
        "commit" => {
            validate_entries(root, &entries, true)?;
            for entry in &entries {
                root.install_head(entry, &root.native(&entry.new_location)?)?;
            }
            root.replace_control(
                JOURNAL,
                &encode_journal(&root.key, operation, "complete", &entries)?,
                &mut false,
            )?;
            publish_cache(cache, &entries)
        }
        _ => Err(refusal("unknown prepared-pair recovery phase")),
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestStage {
    BeforeDecision,
    AfterDecision,
    AfterFirstHead,
}
#[cfg(test)]
#[derive(Debug)]
struct TestHook {
    stage: TestStage,
    fail: bool,
    crash: bool,
    pause: StdMutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
}
#[cfg(test)]
fn checkpoint(hook: &Option<Arc<TestHook>>, stage: TestStage) -> Result<(), IcebergError> {
    if let Some(hook) = hook.as_ref().filter(|hook| hook.stage == stage) {
        if let Some((entered, release)) = hook
            .pause
            .lock()
            .map_err(|_| refusal("pair test hook poisoned"))?
            .take()
        {
            let _ = entered.send(());
            release
                .recv_timeout(std::time::Duration::from_secs(10))
                .map_err(io)?;
        }
        if hook.crash {
            std::process::exit(86);
        }
        if hook.fail {
            return Err(io(std::io::Error::other(
                "injected prepared-pair worker failure",
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "prepared_pair_tests.rs"]
mod tests;

impl NativeRoot {
    fn sync_ancestors(&self, mut path: &NativePath) -> Result<(), IcebergError> {
        if !path.starts_with(&self.path) {
            return Err(refusal("artifact parent escapes its catalog"));
        }
        loop {
            self.sync_directory(path)?;
            if path == self.path {
                return Ok(());
            }
            path = path
                .parent()
                .ok_or_else(|| refusal("artifact parent escapes its catalog"))?;
        }
    }
}

fn add_metadata_artifact(
    root: &NativeRoot,
    artifacts: &mut HashSet<PathBuf>,
    total: &mut u64,
    path: PathBuf,
) -> Result<(), IcebergError> {
    if artifacts.insert(path.clone()) {
        let bytes = root.regular(&path)?.metadata().map_err(io)?.len();
        if bytes > MAX_METADATA as u64 {
            return Err(refusal("prepared-pair manifest exceeds its byte bound"));
        }
        *total = total
            .checked_add(bytes)
            .ok_or_else(|| refusal("prepared-pair manifest size overflow"))?;
        if *total > MAX_ARTIFACT_METADATA_BYTES || artifacts.len() > MAX_ARTIFACTS {
            return Err(refusal("prepared-pair manifests exceed their total bound"));
        }
    }
    Ok(())
}
