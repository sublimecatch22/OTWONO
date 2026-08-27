//! The cluster cache (ADR-0015).
//!
//! A bounded, encrypted, content-addressed slice of disk that a node contributes so its
//! neighbours can serve each other instead of each fetching from origin. It is a cache and
//! nothing more: no ledger, no chain, no consensus, no accounting between neighbours.
//!
//! # Why this is a second store, not a flag on the first
//!
//! `/var/lib/otwono/store` holds what the user put there. It is theirs, and nothing may
//! evict it. `/var/lib/otwono/cache` holds bytes the node picked up on their neighbours'
//! behalf, and every one of them is disposable by definition. Two directories keeps that
//! distinction structural rather than a boolean somebody has to remember to check — a
//! cached object cannot be mistaken for the user's own copy, and eviction cannot reach the
//! user's data because it does not have a path to it.
//!
//! Both are [`Store`]s, so chunking, content addressing, encryption at rest and digest
//! verification are the same code in both places.
//!
//! # What may be cached
//!
//! `Public` and `Replicated`, and nothing else, ever. The check is here rather than at the
//! callers, because "the caller checked" is how a fifth caller leaks.
//!
//! # The budget is not this module's decision
//!
//! It arrives as a number from `FeatureGates::cluster_cache_bytes`, which the
//! capability policy engine derives from the tier and the storage axis (CLAUDE.md §2.6).
//! This module enforces it; it does not choose it.
//!
//! # Holding is publishing
//!
//! A node that serves a chunk tells its neighbours it holds that chunk. Over time, what a
//! household is interested in is partly inferable from what its node serves. Restricting
//! the cache to public and replicated content bounds that; it does not remove it, and the
//! UI has to say so before an operator opts in.

use crate::cas::{Store, StoreError};
use crate::chunk::ChunkRef;
use crate::object::{ContentId, Object};
use crate::Visibility;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const DEFAULT_CACHE_DIR: &str = "/var/lib/otwono/cache";

/// Free space the cache will never consume, whatever its budget says.
///
/// A cache that fills the disk is a fault, not a feature
/// (`CLUSTER-CACHE.md` §3). The audit log and the fetch spool have to keep working
/// on a node whose cache is full, and on an SBC with an 8 GB eMMC the budget alone is not
/// enough of a guarantee — the user's own data grows underneath it.
pub const RESERVE_FLOOR_BYTES: u64 = 256 * 1024 * 1024;

/// The accounting file. JSON rather than a database: it is small, it is read once at
/// startup, and a cache index that a person can read with `cat` is worth more here than
/// one that is fast.
const INDEX_FILE: &str = "meta.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheEntry {
    pub content_id: String,
    pub size_bytes: u64,
    /// Unix milliseconds. Bumped on every read, which is what makes eviction LRU.
    pub last_access_ms: u64,
    /// A pinned object is never evicted. Pinning is how an operator says "keep this
    /// available to the street even if nobody here has asked for it lately".
    #[serde(default)]
    pub pinned: bool,
    /// When this node's promise to hold a replica runs out (ADR-0026).
    ///
    /// Deliberately separate from `pinned` rather than reusing it. Both mean "do not
    /// evict", but an operator's pin is indefinite and a replica's hold expires — folding
    /// them together would make a TTL sweep silently unpin something a person chose to
    /// keep. Absent on everything that is not a replica.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replica_expires_ms: Option<u64>,
}

impl CacheEntry {
    /// Whether this entry may be evicted to make room, at `now_ms`.
    ///
    /// Two independent reasons to keep it, and either is enough: an operator pinned it, or
    /// this node promised to hold it as a replica and that promise has not run out.
    pub fn is_evictable(&self, now_ms: u64) -> bool {
        !self.pinned && !self.holds_a_live_replica(now_ms)
    }

    pub fn holds_a_live_replica(&self, now_ms: u64) -> bool {
        self.replica_expires_ms.is_some_and(|at| now_ms < at)
    }
}

/// What the cache holds, and how many objects reference each chunk.
///
/// The refcount is not an optimization. Chunks are shared between objects by design, so
/// evicting one object must not delete chunks another still needs — without counting,
/// eviction silently corrupts whatever else referenced them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheIndex {
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub objects: BTreeMap<String, CacheEntry>,
    #[serde(default)]
    pub chunk_refs: BTreeMap<String, u32>,
}

pub const INDEX_SCHEMA_VERSION: &str = "1.0.0";

impl CacheIndex {
    pub fn used_bytes(&self) -> u64 {
        self.objects.values().map(|e| e.size_bytes).sum()
    }
}

/// Wall-clock milliseconds, for `last_access_ms`.
///
/// A third copy of four lines that `otwono-identity` and `otwono-permd` also carry. Adding
/// a dependency between three otherwise-unrelated crates to share a clock would be the
/// worse trade; consolidating them is a workspace-wide refactor, not part of this.
///
/// It is a *wall* clock, so a clock that jumps backwards makes recently-used objects look
/// old. The consequence is a worse eviction order for one interval, which is the right
/// severity for a cache — nothing here is correctness-critical, unlike a token expiry.
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct Cache {
    store: Store,
    budget_bytes: u64,
    index: Mutex<CacheIndex>,
    index_path: PathBuf,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("root", &self.store.root())
            .field("budget_bytes", &self.budget_bytes)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum CacheError {
    /// The label forbids it. Never "the caller should have known" — this is the boundary.
    NotCacheable(Visibility),
    /// Larger than the whole budget. No amount of eviction would make room.
    LargerThanBudget {
        size: u64,
        budget: u64,
    },
    /// The disk is too near full, whatever the budget permits.
    NoSpace {
        need: u64,
        free: u64,
    },
    /// Caching is off on this machine: the policy engine set the budget to zero.
    Disabled,
    Store(StoreError),
    Io(String),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::NotCacheable(v) => write!(
                f,
                "{v} content is never placed in the cluster cache; only public and \
                 replicated content may be"
            ),
            CacheError::LargerThanBudget { size, budget } => write!(
                f,
                "{size} bytes cannot be cached in a {budget}-byte budget, however much is evicted"
            ),
            CacheError::NoSpace { need, free } => write!(
                f,
                "caching needs {need} bytes and only {free} are free above the reserve floor"
            ),
            CacheError::Disabled => write!(
                f,
                "this machine contributes no cluster cache; the capability policy set \
                 its budget to zero"
            ),
            CacheError::Store(e) => write!(f, "{e}"),
            CacheError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CacheError {}

impl Cache {
    /// Open a cache at `root` with a budget the policy engine chose.
    ///
    /// A damaged index is rebuilt as empty rather than refused. The alternative — failing
    /// to start because the accounting file is corrupt — takes a node's whole mesh down
    /// over disposable data, and every object in the cache can be fetched again.
    pub fn open(store: Store, budget_bytes: u64) -> Result<Cache, CacheError> {
        let index_path = store.root().join(INDEX_FILE);
        store.ensure_layout().map_err(CacheError::Store)?;
        let index = match std::fs::read_to_string(&index_path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CacheIndex::default(),
            Err(e) => return Err(CacheError::Io(format!("{}: {e}", index_path.display()))),
        };
        Ok(Cache {
            store,
            budget_bytes,
            index: Mutex::new(index),
            index_path,
        })
    }

    /// Open an encrypted cache at the conventional path.
    pub fn at(
        root: impl AsRef<Path>,
        key: crate::StorageKey,
        budget_bytes: u64,
    ) -> Result<Cache, CacheError> {
        Cache::open(Store::encrypted(root, key), budget_bytes)
    }

    pub fn budget_bytes(&self) -> u64 {
        self.budget_bytes
    }

    pub fn enabled(&self) -> bool {
        self.budget_bytes > 0
    }

    pub fn used_bytes(&self) -> u64 {
        self.index.lock().expect("cache index poisoned").used_bytes()
    }

    pub fn len(&self) -> usize {
        self.index.lock().expect("cache index poisoned").objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn root(&self) -> &Path {
        self.store.root()
    }

    /// Whether the cache already holds the object with this hex content id.
    ///
    /// Takes the text form because callers on the wire path have one and would otherwise
    /// parse it back into a `ContentId` only to ask this question.
    pub fn contains_hex(&self, hex: &str) -> bool {
        self.index
            .lock()
            .expect("cache index poisoned")
            .objects
            .contains_key(hex)
    }

    pub fn contains(&self, id: &ContentId) -> bool {
        self.index
            .lock()
            .expect("cache index poisoned")
            .objects
            .contains_key(&id.to_hex())
    }

    /// Everything the cache holds, most recently used first.
    pub fn entries(&self) -> Vec<CacheEntry> {
        let index = self.index.lock().expect("cache index poisoned");
        let mut all: Vec<CacheEntry> = index.objects.values().cloned().collect();
        all.sort_by_key(|e| std::cmp::Reverse(e.last_access_ms));
        all
    }

    /// Put bytes fetched from a peer into the cache.
    ///
    /// The label is checked first, before anything is written and before any eviction
    /// happens, so a refused insert leaves the cache exactly as it was.
    pub fn insert(&self, bytes: &[u8], visibility: Visibility, now_ms: u64) -> Result<Object, CacheError> {
        if !self.enabled() {
            return Err(CacheError::Disabled);
        }
        if !visibility.may_be_cached_for_peers() {
            return Err(CacheError::NotCacheable(visibility));
        }
        let size = bytes.len() as u64;
        if size > self.budget_bytes {
            return Err(CacheError::LargerThanBudget {
                size,
                budget: self.budget_bytes,
            });
        }
        self.ensure_disk_room(size)?;
        self.evict_to_fit(size, now_ms)?;

        let object = self
            .store
            .put_bytes(bytes, visibility)
            .map_err(CacheError::Store)?;

        let mut index = self.index.lock().expect("cache index poisoned");
        let hex = object.content_id.to_hex();
        // Re-inserting something already held is a touch, not a second copy. Content
        // addressing means the chunks are already the same files.
        if let Some(entry) = index.objects.get_mut(&hex) {
            entry.last_access_ms = now_ms;
        } else {
            for chunk in &object.chunks {
                *index.chunk_refs.entry(chunk.blake3.clone()).or_insert(0) += 1;
            }
            index.objects.insert(
                hex.clone(),
                CacheEntry {
                    content_id: hex,
                    size_bytes: object.size_bytes,
                    last_access_ms: now_ms,
                    pinned: false,
                    replica_expires_ms: None,
                },
            );
        }
        Self::persist(&index, &self.index_path)?;
        Ok(object)
    }

    /// Read a cached object, and count the read as a use.
    pub fn get(&self, id: &ContentId, now_ms: u64) -> Result<Vec<u8>, CacheError> {
        let hex = id.to_hex();
        {
            let index = self.index.lock().expect("cache index poisoned");
            if !index.objects.contains_key(&hex) {
                return Err(CacheError::Store(StoreError::NotFound(hex)));
            }
        }
        let object = self.store.get_object(id).map_err(CacheError::Store)?;
        let bytes = self.store.read_object(&object).map_err(CacheError::Store)?;

        let mut index = self.index.lock().expect("cache index poisoned");
        if let Some(entry) = index.objects.get_mut(&hex) {
            entry.last_access_ms = now_ms;
        }
        Self::persist(&index, &self.index_path)?;
        Ok(bytes)
    }

    /// Describe a cached object without reading its bytes, and without counting as a use.
    ///
    /// Answering a peer's `content.manifest` is not the operator using the object, and
    /// letting a peer keep something alive in this node's cache by asking about it would
    /// hand the eviction policy to strangers.
    pub fn stat(&self, id: &ContentId) -> Result<Object, CacheError> {
        if !self.contains(id) {
            return Err(CacheError::Store(StoreError::NotFound(id.to_hex())));
        }
        self.store.get_object(id).map_err(CacheError::Store)
    }

    /// Read one chunk of a cached object **without** counting it as a use.
    ///
    /// This is the path that answers a peer. Local reads go through [`Cache::get`] and do
    /// count; a peer's request must not, or the eviction policy belongs to strangers.
    ///
    /// The trade-off is real and worth naming: a node's cache therefore keeps what its own
    /// household fetched in preference to what the street keeps asking for, which is
    /// slightly backwards for a *cluster* cache. Letting peers drive eviction is the
    /// larger hazard, so this is where it sits until there is a reason to move it.
    pub fn chunk(&self, r: &ChunkRef) -> Result<Vec<u8>, CacheError> {
        self.store.get_chunk(r).map_err(CacheError::Store)
    }

    /// Read a whole cached object without counting it as a use.
    pub fn read_for_peer(&self, object: &Object) -> Result<Vec<u8>, CacheError> {
        self.store.read_object(object).map_err(CacheError::Store)
    }

    /// Take a copy of an offered object and promise to hold it (ADR-0026).
    ///
    /// The holder side of pull replication. Both rules are applied here so no caller can
    /// remember one and forget the other: the **owner's** `max_size_bytes`, and this node's
    /// **own** remaining budget. Refusing is the normal outcome on a small node and is not
    /// an error worth escalating — `Ok(false)` means "not this one".
    ///
    /// A replica is a cache entry with an expiry rather than a separate store. It reuses the
    /// budget, the encryption at rest, the refcounted chunks and the serving path that
    /// already exist, and the only thing it adds is a reason not to evict it yet.
    pub fn take_replica(
        &self,
        bytes: &[u8],
        policy: &crate::object::Replication,
        now_ms: u64,
    ) -> Result<Option<Object>, CacheError> {
        let size = bytes.len() as u64;
        let remaining = self.budget_bytes.saturating_sub(self.used_bytes());
        if !policy.a_holder_may_take(size, remaining) {
            return Ok(None);
        }
        // REPLICATED and nothing else. insert() enforces this too; naming it here as well
        // means a caller reading this function does not have to go and check.
        let object = self.insert(bytes, Visibility::Replicated, now_ms)?;
        let ttl_ms = (policy.ttl_days as u64).saturating_mul(24 * 60 * 60 * 1000);
        let expires = now_ms.checked_add(ttl_ms);
        {
            let mut index = self.index.lock().expect("cache index poisoned");
            if let Some(entry) = index.objects.get_mut(&object.content_id.to_hex()) {
                // None from checked_add means an expiry beyond what a u64 can express,
                // which is "effectively never" rather than "already expired" -- the same
                // direction Replication::is_expired takes, for the same reason.
                entry.replica_expires_ms = expires.or(Some(u64::MAX));
            }
            Self::persist(&index, &self.index_path)?;
        }
        Ok(Some(object))
    }

    /// Drop the hold on replicas whose promise has run out (ADR-0026).
    ///
    /// The objects are **not deleted** — they become ordinary cache entries, evictable when
    /// the budget needs room. A node that fetched something recently should not lose it the
    /// instant a TTL lapses, and letting the cache's own LRU decide is both cheaper and
    /// kinder than a second deletion policy.
    ///
    /// Returns how many holds were released.
    pub fn expire_replicas(&self, now_ms: u64) -> Result<usize, CacheError> {
        let mut released = 0;
        {
            let mut index = self.index.lock().expect("cache index poisoned");
            for entry in index.objects.values_mut() {
                if entry.replica_expires_ms.is_some() && !entry.holds_a_live_replica(now_ms) {
                    entry.replica_expires_ms = None;
                    released += 1;
                }
            }
            if released > 0 {
                Self::persist(&index, &self.index_path)?;
            }
        }
        Ok(released)
    }

    /// How many replicas this node is currently holding.
    pub fn replicas_held(&self, now_ms: u64) -> usize {
        self.index
            .lock()
            .expect("cache index poisoned")
            .objects
            .values()
            .filter(|e| e.holds_a_live_replica(now_ms))
            .count()
    }

    /// Pin or unpin an object. A pinned object is never evicted.
    pub fn set_pinned(&self, id: &ContentId, pinned: bool) -> Result<bool, CacheError> {
        let mut index = self.index.lock().expect("cache index poisoned");
        let Some(entry) = index.objects.get_mut(&id.to_hex()) else {
            return Ok(false);
        };
        entry.pinned = pinned;
        Self::persist(&index, &self.index_path)?;
        Ok(true)
    }

    /// Drop one object, and any chunk no other cached object still references.
    pub fn remove(&self, id: &ContentId) -> Result<u64, CacheError> {
        let mut index = self.index.lock().expect("cache index poisoned");
        let freed = self.remove_locked(&mut index, &id.to_hex())?;
        Self::persist(&index, &self.index_path)?;
        Ok(freed)
    }

    /// Empty the cache.
    ///
    /// "Serving is carrying": an operator stores bytes they did not choose one at a time,
    /// so a purge must always be one action away (`CLUSTER-CACHE.md` §6). Pinned
    /// objects go too — a purge that left things behind would not be one.
    pub fn purge(&self) -> Result<u64, CacheError> {
        let mut index = self.index.lock().expect("cache index poisoned");
        let ids: Vec<String> = index.objects.keys().cloned().collect();
        let mut freed = 0;
        for id in ids {
            freed += self.remove_locked(&mut index, &id)?;
        }
        Self::persist(&index, &self.index_path)?;
        Ok(freed)
    }

    /// Evict least-recently-used objects until `need` more bytes fit inside the budget.
    ///
    /// Pinned objects and live replicas are skipped. If everything left is held and it
    /// still does not fit, the insert is refused rather than the budget quietly exceeded.
    pub fn evict_to_fit(&self, need: u64, now_ms: u64) -> Result<u64, CacheError> {
        let mut index = self.index.lock().expect("cache index poisoned");
        let mut freed = 0;
        loop {
            if index.used_bytes() + need <= self.budget_bytes {
                break;
            }
            // Oldest first. Ties break on the content id so eviction is deterministic —
            // a cache that evicts differently on two nodes with the same history is a cache
            // whose behaviour cannot be tested.
            let victim = index
                .objects
                .values()
                .filter(|e| e.is_evictable(now_ms))
                .min_by(|a, b| {
                    a.last_access_ms
                        .cmp(&b.last_access_ms)
                        .then_with(|| a.content_id.cmp(&b.content_id))
                })
                .map(|e| e.content_id.clone());
            let Some(victim) = victim else {
                return Err(CacheError::LargerThanBudget {
                    size: need,
                    budget: self.budget_bytes.saturating_sub(index.used_bytes()),
                });
            };
            freed += self.remove_locked(&mut index, &victim)?;
        }
        Self::persist(&index, &self.index_path)?;
        Ok(freed)
    }

    fn remove_locked(&self, index: &mut CacheIndex, hex: &str) -> Result<u64, CacheError> {
        let Some(entry) = index.objects.remove(hex) else {
            return Ok(0);
        };
        let Some(id) = ContentId::from_hex(hex) else {
            return Ok(entry.size_bytes);
        };
        // Read the record before deleting it: it names the chunks whose counts to drop.
        if let Ok(object) = self.store.get_object(&id) {
            for chunk in &object.chunks {
                let remaining = match index.chunk_refs.get_mut(&chunk.blake3) {
                    Some(n) => {
                        *n = n.saturating_sub(1);
                        *n
                    }
                    None => 0,
                };
                if remaining == 0 {
                    index.chunk_refs.remove(&chunk.blake3);
                    let path = self.store.chunk_path(&chunk.blake3);
                    if let Err(e) = std::fs::remove_file(&path) {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            return Err(CacheError::Io(format!("{}: {e}", path.display())));
                        }
                    }
                }
            }
        }
        let path = self.store.object_path(&id);
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(CacheError::Io(format!("{}: {e}", path.display())));
            }
        }
        Ok(entry.size_bytes)
    }

    /// Refuse to write when the filesystem is near full, whatever the budget allows.
    fn ensure_disk_room(&self, need: u64) -> Result<(), CacheError> {
        let stat = rustix::fs::statvfs(self.store.root())
            .map_err(|e| CacheError::Io(format!("{}: {e}", self.store.root().display())))?;
        let free = stat.f_bavail.saturating_mul(stat.f_frsize);
        if free < need.saturating_add(RESERVE_FLOOR_BYTES) {
            return Err(CacheError::NoSpace { need, free });
        }
        Ok(())
    }

    fn persist(index: &CacheIndex, path: &Path) -> Result<(), CacheError> {
        let mut out = index.clone();
        out.schema_version = INDEX_SCHEMA_VERSION.to_string();
        let text = serde_json::to_string_pretty(&out)
            .map_err(|e| CacheError::Io(format!("{}: {e}", path.display())))?;
        let parent = path.parent().unwrap_or(Path::new("."));
        let staging = parent.join(format!(".meta-{}.json", std::process::id()));
        std::fs::write(&staging, text).map_err(|e| CacheError::Io(format!("{}: {e}", staging.display())))?;
        std::fs::rename(&staging, path).map_err(|e| CacheError::Io(format!("{}: {e}", path.display())))
    }

    /// Does the cache hold this exact chunk? What a peer's want-list is answered from.
    pub fn has_chunk(&self, hex: &str) -> bool {
        self.index
            .lock()
            .expect("cache index poisoned")
            .chunk_refs
            .contains_key(hex)
    }

    /// Verify a chunk against the digest it is named by, before it is believed.
    ///
    /// The single security argument of ADR-0015, in one function: a peer that serves a
    /// chunk cannot alter it without this failing, so a source does not have to be trusted
    /// to be useful.
    pub fn verify_chunk(bytes: &[u8], expected_hex: &str) -> bool {
        ChunkRef::of(bytes).hex() == expected_hex
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StorageKey;

    fn tmpdir(name: &str) -> PathBuf {
        // A process-wide counter, not a name and a length: three tests once shared a path
        // because their payloads happened to be the same size (defect 30).
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "otwono-cache-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn cache(name: &str, budget: u64) -> Cache {
        Cache::at(tmpdir(name), StorageKey::generate(), budget).expect("cache opens")
    }

    fn data(len: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        // Mixed, not `seed | 1`: that maps 2 and 3 (and every other adjacent pair) to the
        // same stream, so two "different" fixtures come out byte-identical. Cost one
        // debugging round in the cache's LRU test.
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        while out.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn the_fixture_generator_gives_distinct_seeds_distinct_bytes() {
        // The bug this test exists for: `seed | 1` mapped 2 and 3 to one stream, so the LRU
        // test's third object was silently the same object as its second.
        let mut seen = std::collections::HashSet::new();
        for seed in 0..64u64 {
            assert!(
                seen.insert(data(4096, seed)),
                "seed {seed} repeats an earlier stream"
            );
        }
    }

    #[test]
    fn public_content_goes_in_and_comes_back_unchanged() {
        let c = cache("roundtrip", 1 << 20);
        let bytes = data(300 * 1024, 1);
        let object = c.insert(&bytes, Visibility::Public, 1).unwrap();
        assert!(c.contains(&object.content_id));
        assert_eq!(c.get(&object.content_id, 2).unwrap(), bytes);
    }

    #[test]
    fn replicated_content_is_cacheable_too() {
        let c = cache("replicated", 1 << 20);
        let o = c.insert(b"replicated", Visibility::Replicated, 1).unwrap();
        assert!(c.contains(&o.content_id));
    }

    #[test]
    fn private_content_cannot_enter_the_cache_by_any_path() {
        // The property ADR-0015 §5 exists for, checked negatively and with nothing written.
        let c = cache("private", 1 << 20);
        for label in [Visibility::Private, Visibility::Shared] {
            let err = c.insert(b"not for the street", label, 1).unwrap_err();
            assert!(matches!(err, CacheError::NotCacheable(v) if v == label), "{err}");
        }
        assert!(c.is_empty());
        assert_eq!(c.used_bytes(), 0);
        // And nothing reached the disk: no chunk files, no index entries.
        assert!(!c.has_chunk(&ChunkRef::of(b"not for the street").hex()));
    }

    #[test]
    fn a_zero_budget_caches_nothing() {
        // What a storage-constrained machine gets from the policy engine.
        let c = cache("disabled", 0);
        assert!(!c.enabled());
        assert!(matches!(
            c.insert(b"anything", Visibility::Public, 1),
            Err(CacheError::Disabled)
        ));
    }

    #[test]
    fn an_object_larger_than_the_whole_budget_is_refused_rather_than_evicting_everything() {
        let c = cache("toobig", 1024);
        let kept = c.insert(&data(512, 1), Visibility::Public, 1).unwrap();
        let err = c.insert(&data(4096, 2), Visibility::Public, 2).unwrap_err();
        assert!(matches!(err, CacheError::LargerThanBudget { .. }), "{err}");
        // The refusal must not have cost the cache what it already held.
        assert!(c.contains(&kept.content_id));
    }

    #[test]
    fn the_budget_holds_under_sustained_pressure() {
        // CLUSTER-CACHE.md §8: evict rather than fill the disk. Twenty inserts of
        // 64 KiB into a 256 KiB budget.
        let budget = 256 * 1024;
        let c = cache("pressure", budget);
        for i in 0..20u64 {
            c.insert(&data(64 * 1024, i + 1), Visibility::Public, i).unwrap();
            assert!(
                c.used_bytes() <= budget,
                "used {} over budget {budget} after {i} inserts",
                c.used_bytes()
            );
        }
        assert!(c.len() < 20, "nothing was ever evicted");
        assert!(c.len() >= 3, "eviction was far too eager: {} left", c.len());
    }

    #[test]
    fn eviction_takes_the_least_recently_used() {
        let c = cache("lru", 3 * 64 * 1024);
        let a = c.insert(&data(64 * 1024, 1), Visibility::Public, 10).unwrap();
        let b = c.insert(&data(64 * 1024, 2), Visibility::Public, 20).unwrap();
        let d = c.insert(&data(64 * 1024, 3), Visibility::Public, 30).unwrap();

        // Touch `a` so `b` becomes the oldest.
        c.get(&a.content_id, 40).unwrap();
        c.insert(&data(64 * 1024, 4), Visibility::Public, 50).unwrap();

        assert!(c.contains(&a.content_id), "a was touched and must survive");
        assert!(!c.contains(&b.content_id), "b was oldest and must go");
        assert!(c.contains(&d.content_id));
    }

    // --- replicas (ADR-0026) --------------------------------------------------------------

    fn policy(max_size: u64, ttl_days: u32) -> crate::object::Replication {
        crate::object::Replication {
            max_size_bytes: max_size,
            ttl_days,
            ..Default::default()
        }
    }

    #[test]
    fn a_replica_is_refused_by_the_owners_cap_or_by_this_nodes_budget() {
        // Two rules from two parties. Applying one and forgetting the other is the mistake
        // take_replica exists to prevent, so both refusals are pinned -- and a refusal is
        // Ok(None), because "too big for me" is the normal answer on a small node and not
        // an error worth escalating.
        let c = cache("replica-limits", 1_000);
        assert!(
            c.take_replica(&[0u8; 500], &policy(100, 30), 0)
                .unwrap()
                .is_none(),
            "over the owner's cap"
        );
        assert!(
            c.take_replica(&[0u8; 5_000], &policy(u64::MAX, 30), 0)
                .unwrap()
                .is_none(),
            "over this node's budget"
        );
        assert!(
            c.take_replica(&[0u8; 500], &policy(1_000, 30), 0)
                .unwrap()
                .is_some(),
            "within both"
        );
    }

    #[test]
    fn a_held_replica_is_not_evicted_to_make_room() {
        // The point of holding one. If budget pressure could evict a replica, this node's
        // promise would be worth nothing and durability would follow local demand.
        let c = cache("replica-hold", 2_000);
        let held = c
            .take_replica(&[7u8; 900], &policy(u64::MAX, 30), 1_000)
            .unwrap()
            .expect("taken");
        // Fill the rest and force pressure.
        for i in 0..4u8 {
            let _ = c.insert(&[i; 900], Visibility::Public, 2_000 + i as u64);
        }
        assert!(c.contains(&held.content_id), "a live replica was evicted");
    }

    #[test]
    fn an_expired_replica_becomes_ordinary_rather_than_being_deleted() {
        // ADR-0026: the hold lapses, the bytes stay. A node that fetched something recently
        // should not lose it the instant a TTL passes -- the cache's own LRU is a kinder and
        // cheaper decision than a second deletion policy.
        let c = cache("replica-expire", 10_000);
        let day = 24 * 60 * 60 * 1000u64;
        let o = c
            .take_replica(&[3u8; 100], &policy(u64::MAX, 1), 0)
            .unwrap()
            .expect("taken");
        assert_eq!(c.replicas_held(0), 1);

        assert_eq!(c.expire_replicas(day - 1).unwrap(), 0, "not yet");
        assert_eq!(c.expire_replicas(day).unwrap(), 1, "the hold lapses");
        assert!(c.contains(&o.content_id), "the object was deleted, not released");
        assert_eq!(c.replicas_held(day), 0);
        // And releasing twice is not counted twice.
        assert_eq!(c.expire_replicas(day + 1).unwrap(), 0);
    }

    #[test]
    fn an_expired_replica_can_then_be_evicted_like_anything_else() {
        // The other half of the previous test: released really means evictable, otherwise
        // "not deleted" would quietly mean "kept forever" and the budget would leak.
        let c = cache("replica-evict", 2_000);
        let day = 24 * 60 * 60 * 1000u64;
        let o = c
            .take_replica(&[9u8; 900], &policy(u64::MAX, 1), 0)
            .unwrap()
            .expect("taken");
        c.expire_replicas(day).unwrap();
        for i in 0..4u8 {
            let _ = c.insert(&[i; 900], Visibility::Public, day + 1 + i as u64);
        }
        assert!(!c.contains(&o.content_id), "a released replica was never evicted");
    }

    #[test]
    fn an_operators_pin_outlives_a_replica_ttl() {
        // Why the two are separate fields. Folding them together would make a TTL sweep
        // silently unpin something a person chose to keep.
        let c = cache("replica-pin", 10_000);
        let day = 24 * 60 * 60 * 1000u64;
        let o = c
            .take_replica(&[5u8; 100], &policy(u64::MAX, 1), 0)
            .unwrap()
            .expect("taken");
        assert!(c.set_pinned(&o.content_id, true).unwrap());
        c.expire_replicas(day).unwrap();

        let entry = c
            .entries()
            .into_iter()
            .find(|e| e.content_id == o.content_id.to_hex())
            .expect("still there");
        assert!(entry.pinned, "the operator's pin was cleared by a TTL sweep");
        assert!(
            entry.replica_expires_ms.is_none(),
            "the replica hold should have lapsed"
        );
        assert!(!entry.is_evictable(day), "a pinned object is still not evictable");
    }

    #[test]
    fn a_pinned_object_is_never_evicted() {
        let c = cache("pin", 2 * 64 * 1024);
        let keep = c.insert(&data(64 * 1024, 1), Visibility::Public, 10).unwrap();
        assert!(c.set_pinned(&keep.content_id, true).unwrap());
        for i in 0..6u64 {
            c.insert(&data(64 * 1024, i + 2), Visibility::Public, 20 + i)
                .unwrap();
        }
        assert!(c.contains(&keep.content_id), "a pinned object was evicted");
    }

    #[test]
    fn an_insert_that_only_pinned_objects_block_is_refused_not_forced() {
        let c = cache("allpinned", 64 * 1024);
        let pinned = c.insert(&data(64 * 1024, 1), Visibility::Public, 1).unwrap();
        c.set_pinned(&pinned.content_id, true).unwrap();
        let err = c.insert(&data(64 * 1024, 2), Visibility::Public, 2).unwrap_err();
        assert!(matches!(err, CacheError::LargerThanBudget { .. }), "{err}");
        assert!(c.contains(&pinned.content_id));
        assert!(c.used_bytes() <= c.budget_bytes());
    }

    #[test]
    fn a_peer_reading_an_object_does_not_keep_it_alive() {
        // stat() is what answers a peer; it must not count as a use, or the eviction policy
        // belongs to strangers.
        let c = cache("stat", 2 * 64 * 1024);
        let old = c.insert(&data(64 * 1024, 1), Visibility::Public, 10).unwrap();
        let new = c.insert(&data(64 * 1024, 2), Visibility::Public, 20).unwrap();
        c.stat(&old.content_id).unwrap();
        c.insert(&data(64 * 1024, 3), Visibility::Public, 30).unwrap();
        assert!(!c.contains(&old.content_id), "stat kept the oldest object alive");
        assert!(c.contains(&new.content_id));
    }

    #[test]
    fn evicting_one_object_does_not_break_another_that_shares_its_chunks() {
        // Content addressing means two objects can be built from overlapping chunks. Without
        // refcounting, evicting the first silently corrupts the second.
        let c = cache("shared", 4 * 1024 * 1024);
        let common = data(300 * 1024, 7);
        let mut second = common.clone();
        second.extend_from_slice(&data(64 * 1024, 8));

        let a = c.insert(&common, Visibility::Public, 10).unwrap();
        let b = c.insert(&second, Visibility::Public, 20).unwrap();
        let shared: Vec<&str> = a
            .chunks
            .iter()
            .filter(|x| b.chunks.iter().any(|y| y.blake3 == x.blake3))
            .map(|x| x.blake3.as_str())
            .collect();
        assert!(!shared.is_empty(), "the fixture must actually share chunks");

        c.remove(&a.content_id).unwrap();
        assert!(!c.contains(&a.content_id));
        assert_eq!(c.get(&b.content_id, 30).unwrap(), second, "b lost its chunks");
    }

    #[test]
    fn removing_the_last_holder_of_a_chunk_frees_it() {
        let c = cache("free", 4 * 1024 * 1024);
        let bytes = data(300 * 1024, 9);
        let o = c.insert(&bytes, Visibility::Public, 1).unwrap();
        let digest = o.chunks[0].blake3.clone();
        assert!(c.has_chunk(&digest));
        c.remove(&o.content_id).unwrap();
        assert!(!c.has_chunk(&digest));
        assert!(!c
            .root()
            .join("chunks")
            .join(&digest[0..2])
            .join(&digest[2..4])
            .join(&digest)
            .exists());
    }

    #[test]
    fn a_purge_leaves_nothing_behind_not_even_pinned_objects() {
        let c = cache("purge", 4 * 1024 * 1024);
        let pinned = c.insert(&data(64 * 1024, 1), Visibility::Public, 1).unwrap();
        c.set_pinned(&pinned.content_id, true).unwrap();
        c.insert(&data(64 * 1024, 2), Visibility::Public, 2).unwrap();
        let freed = c.purge().unwrap();
        assert!(freed > 0);
        assert!(c.is_empty());
        assert_eq!(c.used_bytes(), 0);
        assert!(!c.contains(&pinned.content_id));
    }

    #[test]
    fn re_inserting_something_already_held_is_a_touch_not_a_second_copy() {
        let c = cache("dup", 1 << 20);
        let bytes = data(64 * 1024, 3);
        let first = c.insert(&bytes, Visibility::Public, 10).unwrap();
        let used = c.used_bytes();
        let again = c.insert(&bytes, Visibility::Public, 99).unwrap();
        assert_eq!(first.content_id, again.content_id);
        assert_eq!(c.len(), 1);
        assert_eq!(c.used_bytes(), used);
        assert_eq!(c.entries()[0].last_access_ms, 99);
    }

    #[test]
    fn the_index_survives_a_reopen() {
        let dir = tmpdir("reopen");
        std::fs::create_dir_all(&dir).unwrap();
        // The same key both times, loaded the way a node loads it rather than held in a
        // variable — StorageKey is deliberately not Clone.
        let key_path = dir.join("cache.key");
        let bytes = data(64 * 1024, 5);
        let id = {
            let (key, generated) = StorageKey::load_or_generate(&key_path).unwrap();
            assert!(generated);
            let c = Cache::at(&dir, key, 1 << 20).unwrap();
            c.insert(&bytes, Visibility::Public, 7).unwrap().content_id
        };
        let (key, generated) = StorageKey::load_or_generate(&key_path).unwrap();
        assert!(!generated, "the second open must reuse the first key");
        let c = Cache::at(&dir, key, 1 << 20).unwrap();
        assert!(c.contains(&id));
        assert_eq!(c.get(&id, 8).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_index_reopens_empty_rather_than_refusing_to_start() {
        // Everything in the cache is disposable and re-fetchable. Failing to start over the
        // accounting file would take a node's mesh down for it.
        let dir = tmpdir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("cache.key");
        {
            let (key, _) = StorageKey::load_or_generate(&key_path).unwrap();
            let c = Cache::at(&dir, key, 1 << 20).unwrap();
            c.insert(b"something", Visibility::Public, 1).unwrap();
        }
        std::fs::write(dir.join(INDEX_FILE), "{ not json at all").unwrap();
        let (key, _) = StorageKey::load_or_generate(&key_path).unwrap();
        let c = Cache::at(&dir, key, 1 << 20).expect("must still open");
        assert!(c.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_node_with_no_budget_is_not_a_replica_holder_at_all() {
        // Not "a holder with zero room": the pass has to be able to tell "I do not do this"
        // from "I do this and am full", because only the first means make no traffic.
        let c = cache("holder-off", 0);
        assert!(ReplicaHolder::replica_room(&c, &[], 1_000).is_none());
    }

    #[test]
    fn a_holder_reports_its_room_and_what_it_already_has() {
        let c = cache("holder-room", 1 << 20);
        let bytes = data(4096, 77);
        let held = c
            .insert(&bytes, Visibility::Replicated, 1_000)
            .expect("insert")
            .content_id
            .to_hex();
        let absent = "00".repeat(32);

        let room = ReplicaHolder::replica_room(&c, &[held.clone(), absent.clone()], 1_000)
            .expect("this node replicates");
        assert_eq!(room.room_bytes, (1 << 20) - c.used_bytes());
        assert_eq!(
            room.already_held,
            vec![held],
            "the answer must name only what was asked about, and only what is there"
        );
        assert!(!room.already_held.contains(&absent));
        let _ = std::fs::remove_dir_all(c.root());
    }

    #[test]
    fn asking_for_room_releases_a_hold_that_has_run_out() {
        // The sweep rides this call rather than a timer (ADR-0026 §9). If it did not, a node
        // whose holds had all lapsed would report no room and quietly stop replicating.
        let c = cache("holder-sweep", 1 << 20);
        let policy = crate::object::Replication {
            ttl_days: 1,
            ..Default::default()
        };
        let taken = ReplicaHolder::take_replica(&c, &data(2048, 3), &policy, 1_000)
            .expect("take")
            .expect("taken");
        assert_eq!(c.replicas_held(1_000), 1);

        let a_day = 24 * 60 * 60 * 1000u64;
        let room = ReplicaHolder::replica_room(&c, std::slice::from_ref(&taken.content_id), 1_000 + a_day)
            .expect("still replicating");
        assert_eq!(c.replicas_held(1_000 + a_day), 0, "the hold did not lapse");
        // Lapsed is not deleted: the bytes stay as an ordinary evictable cache entry, so the
        // holder still reports having them and a peer need not re-offer.
        assert_eq!(room.already_held, vec![taken.content_id]);
        let _ = std::fs::remove_dir_all(c.root());
    }

    #[test]
    fn a_holder_refuses_what_will_not_fit_without_calling_it_an_error() {
        let c = cache("holder-full", 4096);
        let policy = crate::object::Replication::default();
        let outcome = ReplicaHolder::take_replica(&c, &data(64 * 1024, 9), &policy, 1_000)
            .expect("a refusal is not an error");
        assert!(outcome.is_none(), "took an object larger than the budget");
        assert!(c.is_empty());
        let _ = std::fs::remove_dir_all(c.root());
    }

    #[test]
    fn a_chunk_that_fails_its_digest_is_recognisable_as_wrong() {
        let bytes = data(1024, 11);
        let honest = ChunkRef::of(&bytes).hex();
        assert!(Cache::verify_chunk(&bytes, &honest));
        let mut tampered = bytes.clone();
        tampered[0] ^= 1;
        assert!(!Cache::verify_chunk(&tampered, &honest));
    }
}

/// Somewhere a replica can be put (ADR-0026 §10).
///
/// A replication pass has two halves in two processes: the link belongs to `otwono-netd`,
/// and the cache belongs to `otwono-stored`. `otwono-netd` must not open the cache itself —
/// two processes each holding their own in-memory copy of one on-disk index lose each
/// other's updates and double-count the same budget, which is CLAUDE.md §2.4's "never
/// through shared mutable state" with a file standing in for the shared global.
///
/// So the pass is written against this trait instead of against [`Cache`]. [`Cache`]
/// implements it directly, which is what the tests drive with no daemon running;
/// `otwono-netd` implements it over the control plane. The pass logic — what to take, in
/// what order, and which checks to run before keeping anything — exists once, above both.
///
/// The three methods are exactly the three moments a pass has, and no more: it asks before
/// it puts anything on the wire, it filters the offer page in one call rather than one call
/// per offer, and it takes at most one object.
pub trait ReplicaHolder {
    /// How many more bytes this node will promise to hold, and which of `candidates` it
    /// already has.
    ///
    /// `None` means this node does not replicate — a budget of zero, no cache at all, or an
    /// operator who said no. The pass makes no replication traffic in that case rather than
    /// asking and discarding the answer (ADR-0026 §9).
    ///
    /// Implementations sweep lapsed holds first, because the answer is about room and a hold
    /// that has run out is not occupying any.
    fn replica_room(&self, candidates: &[String], now_ms: u64) -> Option<ReplicaRoom>;

    /// Take the bytes and promise to hold them for the policy's term.
    ///
    /// `Ok(None)` is "not this one" and not an error: the budget may have filled between the
    /// question and the answer, or the object may be larger than it was advertised as.
    fn take_replica(
        &self,
        bytes: &[u8],
        policy: &crate::object::Replication,
        now_ms: u64,
    ) -> Result<Option<TakenReplica>, String>;
}

/// What a holder has room for, and what it need not be offered again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaRoom {
    /// Bytes still available under this node's budget. May be zero, which is a node that
    /// replicates in principle and has no room today — distinct from `None`, which is a node
    /// that does not replicate at all.
    pub room_bytes: u64,
    /// Which of the offered content ids this node already holds, in lowercase hex.
    pub already_held: Vec<String>,
}

/// One object a holder agreed to keep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakenReplica {
    pub content_id: String,
    pub size_bytes: u64,
}

impl ReplicaHolder for Cache {
    fn replica_room(&self, candidates: &[String], now_ms: u64) -> Option<ReplicaRoom> {
        // Best-effort: failing to release lapsed holds costs this node room it could have
        // had, which loses a replica rather than correctness.
        let _ = self.expire_replicas(now_ms);
        if !self.enabled() {
            return None;
        }
        Some(ReplicaRoom {
            room_bytes: self.budget_bytes().saturating_sub(self.used_bytes()),
            already_held: candidates
                .iter()
                .filter(|hex| self.contains_hex(hex))
                .cloned()
                .collect(),
        })
    }

    fn take_replica(
        &self,
        bytes: &[u8],
        policy: &crate::object::Replication,
        now_ms: u64,
    ) -> Result<Option<TakenReplica>, String> {
        match Cache::take_replica(self, bytes, policy, now_ms) {
            Ok(Some(object)) => Ok(Some(TakenReplica {
                content_id: object.content_id.to_hex(),
                size_bytes: object.size_bytes,
            })),
            Ok(None) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}
