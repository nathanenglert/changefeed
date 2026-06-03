//! Fetch boundary (ARCHITECTURE.md §1, §6.8): the `FetchClient` TRAIT + data types live here in
//! pure core. The impure reqwest+rustls Tier-1 implementation lives in `cf::fetch_http`. NO http
//! impl in this file — that keeps the purity wall intact and makes every stage callable with a
//! mock fetch in tests.

use crate::model::FetchTier;
use crate::CfError;

/// Conditional-GET validators carried across observations for the 304 short-circuit (§12).
#[derive(Clone, Debug, Default)]
pub struct FetchMeta {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub tier: Option<FetchTier>,
    pub status: u16,
    pub ms: Option<u32>,
}

/// The result of a fetch attempt handed to the pure pipeline.
pub enum FetchOutcome {
    /// 200-class body to extract.
    Body {
        url: String,
        final_url: String,
        status: u16,
        body: String,
        meta: FetchMeta,
    },
    /// 304 Not Modified — the cheapest observation. No body, no diff, no new snapshot (§12).
    NotModified { meta: FetchMeta },
    /// Transient/hard failure mapped to a typed error (exit 3/4/5/6/7).
    Error(CfError),
}

/// What the cli must supply to perform one fetch (conditional GET inputs).
#[derive(Clone, Debug, Default)]
pub struct FetchRequest {
    pub url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// §4.11 credentials (header/cookie/basic), already `${ENV}`-expanded at the cli boundary. The
    /// impure fetch impl attaches these to the request; they are NEVER persisted or logged (§12).
    pub auth: Option<crate::model::AuthCfg>,
}

/// The impure fetch seam. Implemented in `cf::fetch_http` (reqwest+rustls); mocked in tests.
pub trait FetchClient {
    fn fetch(&self, req: &FetchRequest) -> FetchOutcome;
}
