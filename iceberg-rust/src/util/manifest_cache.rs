/*!
Process-wide cache for immutable Iceberg manifest bytes.

Manifest-list and manifest files are content-unique per snapshot — Iceberg
never rewrites a file in place; new snapshots get new paths — so bytes cached
by path never go stale and need no invalidation. Caching them removes the
per-scan object-store round-trips (`manifest list GET + one GET per
manifest`) that otherwise repeat on every scan of the same snapshot.

Keys are the original file URIs (scheme + bucket + path), NOT store-relative
paths: the cache is process-wide, and two object stores can hold different
bytes at the same relative path (`s3://a/x` vs `s3://b/x`).

The cache is byte-capped LRU. Capacity comes from `ICEBERG_MANIFEST_CACHE_MB`
(read once per process; default 128 MiB; `0` disables caching entirely).
Entries larger than the cap are never inserted.
*/

use bytes::Bytes;
use lru::LruCache;
use std::sync::{LazyLock, Mutex};

const DEFAULT_CAP_MB: usize = 128;

struct ByteCappedCache {
    entries: LruCache<String, Bytes>,
    total_bytes: usize,
    cap_bytes: usize,
}

static CACHE: LazyLock<Option<Mutex<ByteCappedCache>>> = LazyLock::new(|| {
    let cap_mb = std::env::var("ICEBERG_MANIFEST_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_CAP_MB);
    if cap_mb == 0 {
        return None;
    }
    Some(Mutex::new(ByteCappedCache {
        entries: LruCache::unbounded(),
        total_bytes: 0,
        cap_bytes: cap_mb.saturating_mul(1024 * 1024),
    }))
});

/// Cached bytes for `path`, touching its LRU position.
pub(crate) fn get(path: &str) -> Option<Bytes> {
    let cache = CACHE.as_ref()?;
    let mut guard = cache.lock().ok()?;
    guard.entries.get(path).cloned()
}

/// Insert `bytes` under `path`, evicting least-recently-used entries until
/// the byte cap holds. Oversized payloads are skipped.
pub(crate) fn put(path: &str, bytes: &Bytes) {
    let Some(cache) = CACHE.as_ref() else {
        return;
    };
    let Ok(mut guard) = cache.lock() else {
        return;
    };
    let len = bytes.len();
    if len > guard.cap_bytes {
        return;
    }
    if let Some(previous) = guard.entries.put(path.to_string(), bytes.clone()) {
        guard.total_bytes = guard.total_bytes.saturating_sub(previous.len());
    }
    guard.total_bytes += len;
    while guard.total_bytes > guard.cap_bytes {
        match guard.entries.pop_lru() {
            Some((_, evicted)) => {
                guard.total_bytes = guard.total_bytes.saturating_sub(evicted.len());
            }
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    // The static cache reads its capacity from the environment once per
    // process, so tests exercise the mechanics through a locally built
    // instance of the same structure.
    use super::*;

    fn build(cap_bytes: usize) -> ByteCappedCache {
        ByteCappedCache {
            entries: LruCache::unbounded(),
            total_bytes: 0,
            cap_bytes,
        }
    }

    fn put_in(cache: &mut ByteCappedCache, path: &str, bytes: &Bytes) {
        let len = bytes.len();
        if len > cache.cap_bytes {
            return;
        }
        if let Some(previous) = cache.entries.put(path.to_string(), bytes.clone()) {
            cache.total_bytes = cache.total_bytes.saturating_sub(previous.len());
        }
        cache.total_bytes += len;
        while cache.total_bytes > cache.cap_bytes {
            match cache.entries.pop_lru() {
                Some((_, evicted)) => {
                    cache.total_bytes = cache.total_bytes.saturating_sub(evicted.len());
                }
                None => break,
            }
        }
    }

    #[test]
    fn uri_keys_do_not_collide_across_stores() {
        // Same store-relative path under two buckets must stay two entries,
        // and the stripped form must not be a key at all. Exercises the real
        // process-wide cache (default cap, env unset in tests).
        put(
            "s3://bucket-a/db/tbl/metadata/x-m0.avro",
            &Bytes::from_static(b"bytes-a"),
        );
        put(
            "s3://bucket-b/db/tbl/metadata/x-m0.avro",
            &Bytes::from_static(b"bytes-b"),
        );
        assert_eq!(
            get("s3://bucket-a/db/tbl/metadata/x-m0.avro").as_deref(),
            Some(b"bytes-a".as_slice())
        );
        assert_eq!(
            get("s3://bucket-b/db/tbl/metadata/x-m0.avro").as_deref(),
            Some(b"bytes-b".as_slice())
        );
        assert!(get("/db/tbl/metadata/x-m0.avro").is_none());
    }

    #[test]
    fn hit_and_miss() {
        let mut cache = build(1024);
        put_in(&mut cache, "a", &Bytes::from(vec![1u8; 10]));
        assert!(cache.entries.get("a").is_some());
        assert!(cache.entries.get("b").is_none());
    }

    #[test]
    fn byte_cap_evicts_lru() {
        let mut cache = build(100);
        put_in(&mut cache, "a", &Bytes::from(vec![0u8; 60]));
        put_in(&mut cache, "b", &Bytes::from(vec![0u8; 60]));
        // "a" evicted to fit "b".
        assert!(cache.entries.get("a").is_none());
        assert!(cache.entries.get("b").is_some());
        assert!(cache.total_bytes <= 100);
    }

    #[test]
    fn oversized_payload_skipped() {
        let mut cache = build(50);
        put_in(&mut cache, "big", &Bytes::from(vec![0u8; 51]));
        assert!(cache.entries.get("big").is_none());
        assert_eq!(cache.total_bytes, 0);
    }

    #[test]
    fn replacing_entry_updates_accounting() {
        let mut cache = build(100);
        put_in(&mut cache, "a", &Bytes::from(vec![0u8; 40]));
        put_in(&mut cache, "a", &Bytes::from(vec![0u8; 20]));
        assert_eq!(cache.total_bytes, 20);
    }
}
