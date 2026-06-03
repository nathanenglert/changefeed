//! Storage boundary (ARCHITECTURE.md §1, §9.8): the `Store` TRAIT + data types live here in pure
//! core. The impure rusqlite(WAL)+zstd fixed-ring implementation lives in `cf::store_sqlite`. NO
//! rusqlite/zstd impl in this file — preserves the purity wall and lets stages run against a
//! tmp-dir / in-memory mock store in tests.

use crate::model::CanonicalDoc;
use crate::CfError;

/// A stored snapshot record (one ring entry, §9.8).
#[derive(Clone, Debug)]
pub struct StoredSnapshot {
    pub tid: String,
    pub rev: u64,
    pub doc: CanonicalDoc,
}

/// The impure storage seam. Implemented in `cf::store_sqlite`; mocked in tests.
///
/// MVP model (§9.8): one previous `CanonicalDoc` per target as a single `zstd-19` blob + raw HTML
/// blob, fixed-ring "keep last N", no CAS / no packfiles / no GC. The `doc_hash`/304
/// short-circuits write ZERO bytes.
pub trait Store {
    /// The most-recent stored snapshot for a target, if any.
    fn latest(&self, tid: &str) -> Result<Option<StoredSnapshot>, CfError>;

    /// Persist a new snapshot, evicting the oldest of the retained ring. Returns the new `rev`.
    fn put(&mut self, snapshot: &StoredSnapshot) -> Result<u64, CfError>;

    /// Whether an idempotency `event_key` has already been emitted (§7.4 seen-set).
    fn seen_event(&self, event_key: u128) -> Result<bool, CfError>;

    /// Record an emitted idempotency `event_key`.
    fn mark_event(&mut self, event_key: u128) -> Result<(), CfError>;
}
