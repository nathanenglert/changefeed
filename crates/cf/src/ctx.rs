//! `Ctx { store, clock, ids, fetch }` — the dependency-injection container (ARCHITECTURE.md §1,
//! §4.2). All impurity is injected here so tests can supply a frozen clock, seeded id gen, recorded
//! HTML, and a tmp-dir store.

use cf_core::fetch::FetchClient;
use cf_core::storage::Store;

use crate::ids_clock::{Clock, IdGen};

/// The injected impurity surface for one process run.
pub struct Ctx<S: Store, F: FetchClient, C: Clock, I: IdGen> {
    pub store: S,
    pub fetch: F,
    pub clock: C,
    pub ids: I,
}
