//! `cf-core` — the PURE changefeed pipeline (ARCHITECTURE.md §1, §4).
//!
//! This crate declares NO `reqwest`/`tokio`/`rusqlite`/`zstd`/`rand` dependency and uses `time`
//! only for civil-date parsing (NO clock). Reading a clock, opening a socket, or touching disk in
//! a pure stage is a BUILD ERROR, not a review note (the compiler-enforced purity wall).
//!
//! Each pipeline stage is one module with a pure signature `fn(input, &Profile) -> Result<Out,
//! CfError>`. `fetch` and `storage` live here as *traits + data types*; their impure
//! implementations live in the `cf` bin crate.

// Re-export the value-bearing dependency crates so the `cf` bin (the storage/serde boundary) can
// reconstruct `TypedValue::Date`/`Number` without re-declaring — and pin — these versions itself.
// These types appear in the FROZEN public contract (`TypedValue`), so re-exporting them is just
// surfacing the API's own dependency, not widening it.
pub use blake3;
pub use rust_decimal;
pub use time;

pub mod config;
pub mod diff;
pub mod event;
pub mod extract;
pub mod fetch;
pub mod model;
pub mod normalize;
pub mod packs;
pub mod salience;
pub mod segment;
pub mod storage;

pub use model::{
    Action, AnchorScheme, AuthCfg, Base, Block, BlockId, BlockType, CanonicalDoc, ChangeEvent,
    ChangeType, Config, Crawl, Defaults, Delta, DiffOp, DocHash, DocStats, Duration, EventId,
    EventKey, ExitCode, ExtractStrategy, FeedEnvelope, FetchTier, Followup, IdiffOp, IgnoreRule,
    Materiality, NoChangeReason, NormHash, ObservationResult, Profile, Prov, RenderMode, Seg,
    SinkCfg, SlotKey, SourceMode, Src, TargetCfg, TypedValue, Why,
};

pub mod pipeline;

use thiserror::Error;

/// Typed errors in core (`thiserror`); the cli adds context (`anyhow`) and maps to exit codes.
#[derive(Error, Debug)]
pub enum CfError {
    #[error("usage/config: {0}")]
    Usage(String), // -> 1
    #[error("target not found: {0}")]
    NotFound(String), // -> 2
    #[error("fetch failed: {0}")]
    SoftFetch(String), // -> 3
    #[error("blocked by robots.txt")]
    Robots, // -> 4
    #[error("auth failure: {0}")]
    Auth(u16), // -> 5
    #[error("rate limited")]
    RateLimit { retry_after: Option<u32> }, // -> 6
    #[error("render required, no browser")]
    RenderNeeded, // -> 7
}

impl CfError {
    /// Maps a typed core error to its frozen §4.5 exit code.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            CfError::Usage(_) => ExitCode::Usage,
            CfError::NotFound(_) => ExitCode::NotFound,
            CfError::SoftFetch(_) => ExitCode::SoftFetch,
            CfError::Robots => ExitCode::Robots,
            CfError::Auth(_) => ExitCode::Auth,
            CfError::RateLimit { .. } => ExitCode::RateLimit,
            CfError::RenderNeeded => ExitCode::RenderNeeded,
        }
    }
}

/// Boundary-injected dependencies for one observation. The pure pipeline reads `obs`/`base`
/// data values from here; it never reads a clock, RNG, network, or disk itself (§4.2).
///
/// The fetched body is provided by the caller (cli), so `run_observation` stays a linear sequence
/// of pure transforms.
pub struct ObservationCtx<'a> {
    /// RFC3339 observation timestamp, injected at the cli boundary.
    pub obs: &'a str,
    /// Pre-formatted `cfe_<ULID>` / `cfb_<ULID>` id source, injected at the cli boundary.
    pub batch_id: &'a str,
    /// The prior `CanonicalDoc` for this target, if any (loaded from the `Store` by the cli).
    pub prior: Option<&'a CanonicalDoc>,
    /// Monotonic per-`tid` revision counter (loaded from the `Store` by the cli).
    pub prev_rev: Option<u64>,
}

/// Pipeline orchestration (ARCHITECTURE.md §1): the two FETCH-level short-circuits — HTTP 304
/// (no body, no diff) and a hard fetch error — map directly to `ObservationResult` here. A 200-class
/// `Body` carries no pack/tid/min-salience, so the full pure spine (extract → … → event) runs in
/// [`pipeline::observe_body`], which the cli calls after canonicalizing the body (it owns the pack,
/// target id, ignore rules, and threshold). This keeps the cheap no-op path structurally zero-write.
pub fn run_observation(
    input: &fetch::FetchOutcome,
    _profile: &Profile,
    _ctx: &ObservationCtx<'_>,
) -> Option<ObservationResult> {
    match input {
        // 304 Not Modified — the cheapest observation (§12): no body, no diff, no new snapshot.
        fetch::FetchOutcome::NotModified { .. } => Some(ObservationResult::NoChange {
            reason: model::NoChangeReason::Http304,
        }),
        // A transient/hard fetch failure maps to its typed error (exit 3/4/5/6/7).
        fetch::FetchOutcome::Error(e) => Some(ObservationResult::FetchError(clone_err(e))),
        // A 200-class body needs the full pipeline context — handled by `pipeline::observe_body`.
        fetch::FetchOutcome::Body { .. } => None,
    }
}

/// Clone a `CfError` (it is not `Clone` because of `thiserror`'s default derive set).
fn clone_err(e: &CfError) -> CfError {
    match e {
        CfError::Usage(s) => CfError::Usage(s.clone()),
        CfError::NotFound(s) => CfError::NotFound(s.clone()),
        CfError::SoftFetch(s) => CfError::SoftFetch(s.clone()),
        CfError::Robots => CfError::Robots,
        CfError::Auth(c) => CfError::Auth(*c),
        CfError::RateLimit { retry_after } => CfError::RateLimit {
            retry_after: *retry_after,
        },
        CfError::RenderNeeded => CfError::RenderNeeded,
    }
}
