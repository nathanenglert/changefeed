//! `impl FetchClient` — reqwest+rustls Tier-1 (ARCHITECTURE.md §1, §13; DESIGN §5.1, §12). The only
//! async surface: a single conditional GET with `Accept-Encoding: gzip, br, zstd`, a pinned
//! realistic UA, ETag/`Last-Modified` revalidation (`If-None-Match`/`If-Modified-Since`), a 304
//! short-circuit (no body, no-change reason `http-304`), the 10s/3-redirect/5 MB budget, per-host
//! robots.txt (24 h TTL) + `Crawl-delay` floor, and the §4.5 exit-code mapping. `render=chromium`
//! and `needs_render` detection both map to exit 7 (no headless in the MVP).
//!
//! This is the IMPURE network boundary: it owns a current-thread tokio runtime so the synchronous
//! `FetchClient::fetch` seam (called by the otherwise-pure pipeline) can drive reqwest. The clock
//! used for robots TTL / crawl-delay is INJECTED (`NowMs`) so tests stay deterministic and offline.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use cf_core::fetch::{FetchClient, FetchMeta, FetchOutcome, FetchRequest};
use cf_core::model::{FetchTier, RenderMode};
use cf_core::CfError;
use futures_util::StreamExt;
use reqwest::redirect::Policy;
use reqwest::StatusCode;

/// The pinned default User-Agent (DESIGN §12: honest UA with a contact URL — no aggressive spoof).
pub const DEFAULT_USER_AGENT: &str = "changefeed/1.0 (+https://github.com/changefeed/changefeed)";

/// §5.1 / §12 budget constants.
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;
pub const MAX_REDIRECTS: usize = 3;
pub const MAX_BODY_BYTES: u64 = 5 * 1024 * 1024; // 5 MB

/// §12: robots.txt is cached per host with a 24 h TTL.
pub const ROBOTS_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// §5.1 `needs_render` heuristic cutoffs.
pub const RENDER_TEXT_RATIO: f64 = 0.05;
pub const RENDER_MIN_TEXT_CHARS: usize = 200;

/// An injectable wall clock (epoch millis). The production impl reads the system clock; tests inject
/// a frozen value so robots TTL and crawl-delay are deterministic and the suite never reads the real
/// clock. (The clock lives ONLY in the bin crate, §4.2.)
pub trait NowMs: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// Production clock: epoch millis from the system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemNowMs;

impl NowMs for SystemNowMs {
    fn now_ms(&self) -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// A frozen clock for tests (no real time).
#[derive(Clone, Copy, Debug)]
pub struct FixedNowMs(pub i64);

impl NowMs for FixedNowMs {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

/// Tier-1 HTTP fetcher (reqwest + rustls, webpki-roots). Owns a current-thread tokio runtime so the
/// synchronous `FetchClient` seam can drive async reqwest from the one-shot `cf check` path.
pub struct HttpFetcher {
    pub user_agent: String,
    pub timeout_secs: u64,
    pub respect_robots: bool,
    /// `render=chromium` → exit 7 unconditionally in the MVP; `render=auto` → exit 7 when the
    /// `needs_render` heuristic fires; `render=never` → never exit 7 (raw HTTP HTML is diffed as-is).
    pub render: RenderMode,
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
    clock: Box<dyn NowMs>,
    /// Per-host robots.txt cache (DESIGN §12, 24 h TTL). `Mutex` because `fetch(&self)`.
    robots: Mutex<HashMap<String, RobotsEntry>>,
}

/// A cached robots.txt parse, the (injected) epoch-millis it was fetched at (TTL anchor), and the
/// last in-process resource-fetch time for this host (the crawl-delay floor).
#[derive(Clone, Debug)]
struct RobotsEntry {
    robots: Robots,
    fetched_at_ms: i64,
    last_fetch_ms: Option<i64>,
}

/// The parts of a response we need, captured (and for 2xx, body-buffered) inside one `block_on` so
/// no reqwest `Response` outlives its async scope (see `do_get`).
struct HttpResponse {
    status: u16,
    final_url: String,
    etag: Option<String>,
    last_modified: Option<String>,
    retry_after: Option<u32>,
    /// `Ok(body)` for a read 2xx; `Err` on the 5 MB cap or a mid-stream transport error; `Ok("")`
    /// for non-2xx (the body is intentionally unread).
    body: Result<String, CfError>,
}

impl HttpFetcher {
    /// Build a fetcher with the default UA, the §5.1 10 s budget, robots respected, and the system
    /// clock. The current-thread runtime + reqwest client are constructed once and reused.
    pub fn new() -> Result<Self, CfError> {
        Self::builder().build()
    }

    /// Start a configurable builder (UA / timeout / robots / render / clock).
    pub fn builder() -> HttpFetcherBuilder {
        HttpFetcherBuilder::default()
    }

    fn from_builder(b: HttpFetcherBuilder) -> Result<Self, CfError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CfError::SoftFetch(format!("tokio runtime: {e}")))?;

        // reqwest client: rustls TLS, gzip/br/zstd decoding (the Cargo features), the redirect cap,
        // and the connect+read timeout budget. `gzip(true)`/`brotli(true)`/`zstd(true)` also send
        // the `Accept-Encoding: gzip, br, zstd` request header (§5.1).
        let client = reqwest::Client::builder()
            .user_agent(b.user_agent.clone())
            .gzip(true)
            .brotli(true)
            .zstd(true)
            .redirect(Policy::limited(MAX_REDIRECTS))
            .connect_timeout(Duration::from_secs(b.timeout_secs))
            .timeout(Duration::from_secs(b.timeout_secs))
            .build()
            .map_err(|e| CfError::SoftFetch(format!("http client: {e}")))?;

        Ok(Self {
            user_agent: b.user_agent,
            timeout_secs: b.timeout_secs,
            respect_robots: b.respect_robots,
            render: b.render,
            client,
            runtime,
            clock: b.clock,
            robots: Mutex::new(HashMap::new()),
        })
    }

    /// The §4.5 outcome of one observation. Tier-1 HTTP only:
    /// * `render=chromium` (or a fired `needs_render` heuristic under `auto`) → `RenderNeeded`
    ///   (exit 7); `render=never` bypasses the heuristic and diffs the raw HTML;
    /// * robots-disallowed (when `respect_robots`) → `Robots` (exit 4), crawl-delay floor honored;
    /// * 304 → `NotModified` (no body, no-change reason `http-304`);
    /// * 200 → `Body { final_url, etag, body, … }`;
    /// * 401/403 → `Auth` (5); 429 → `RateLimit { retry_after }` (6); 5xx/DNS/TLS/timeout → `SoftFetch` (3).
    fn fetch_inner(&self, req: &FetchRequest) -> FetchOutcome {
        // (0) render=chromium is unconditional exit 7 in the MVP — there is no headless tier.
        if self.render == RenderMode::Chromium {
            return FetchOutcome::Error(CfError::RenderNeeded);
        }

        let host = match host_of(&req.url) {
            Some(h) => h,
            None => {
                return FetchOutcome::Error(CfError::SoftFetch(format!(
                    "malformed url: {}",
                    req.url
                )))
            }
        };

        // (1) robots.txt politeness (DESIGN §12). Fetched + cached per host (24 h TTL); a
        // disallowed path is exit 4 when respect_robots is on.
        if self.respect_robots {
            if let Ok(robots) = self.robots_for(&host, &req.url) {
                if !robots.allowed(&self.user_agent, &path_of(&req.url)) {
                    return FetchOutcome::Error(CfError::Robots);
                }
                // Crawl-delay is honored as a FLOOR on inter-request spacing (§12). With no
                // persisted per-host timestamp at this seam, a positive crawl-delay on a host we
                // have already fetched within the window gates the request (exit 6).
                if let Some(delay) = robots.crawl_delay_secs(&self.user_agent) {
                    if let Some(retry) = self.crawl_delay_gate(&host, delay) {
                        return FetchOutcome::Error(CfError::RateLimit {
                            retry_after: Some(retry),
                        });
                    }
                }
            }
            // A robots.txt we could not fetch is treated permissively (DESIGN): we proceed.
        }

        // (2) the single conditional GET. The send AND the capped body read run inside ONE
        // `block_on` (`do_get`): a reqwest `Response` holds connection resources registered with
        // the runtime's reactor, so polling/dropping it across two separate `block_on` calls on a
        // current-thread runtime panics ("no reactor running"). Keeping it in one async scope is
        // both correct and lets us enforce the 5 MB cap before buffering the whole body.
        let started = self.clock.now_ms();
        let result = self.runtime.block_on(self.do_get(req));
        let elapsed_ms = (self.clock.now_ms() - started).max(0) as u32;

        let resp = match result {
            Ok(r) => r,
            Err(e) => return FetchOutcome::Error(e),
        };

        let code = resp.status;

        // (3) status → outcome (§4.5).
        if code == StatusCode::NOT_MODIFIED.as_u16() {
            // 304 short-circuits the entire pipeline: no body, no diff, no new snapshot (§12).
            return FetchOutcome::NotModified {
                meta: FetchMeta {
                    etag: resp.etag.or_else(|| req.etag.clone()),
                    last_modified: resp.last_modified.or_else(|| req.last_modified.clone()),
                    tier: Some(FetchTier::Http),
                    status: code,
                    ms: Some(elapsed_ms),
                },
            };
        }
        if code == StatusCode::UNAUTHORIZED.as_u16() || code == StatusCode::FORBIDDEN.as_u16() {
            return FetchOutcome::Error(CfError::Auth(code));
        }
        if code == StatusCode::TOO_MANY_REQUESTS.as_u16() {
            return FetchOutcome::Error(CfError::RateLimit {
                retry_after: resp.retry_after,
            });
        }
        if (500..600).contains(&code) {
            return FetchOutcome::Error(CfError::SoftFetch(format!("origin returned {code}")));
        }
        if !(200..300).contains(&code) {
            // Other non-2xx (e.g. a 4xx that is not auth/rate-limit) is a soft fetch error: there
            // is no body to diff and the baseline must not advance.
            return FetchOutcome::Error(CfError::SoftFetch(format!("unexpected status {code}")));
        }

        // (4) success: the body was already read under the 5 MB cap inside `do_get`.
        let body = match resp.body {
            Ok(b) => b,
            Err(e) => return FetchOutcome::Error(e),
        };

        // (5) needs_render heuristic (§5.1). The MVP has no headless tier, so a JS-empty page is
        // DETECTED and reported as exit 7 rather than producing a bogus empty diff — UNLESS the
        // caller set render="never", which is the documented escape hatch: "trust the raw HTTP
        // HTML as-is, skip detection". Content-rich pages that merely look JS-empty to the cheap
        // heuristic (e.g. an SPA-style shell whose real content IS in the static HTML) are then
        // diffed instead of erroring. `render="never"` is precisely what the §4.5 exit-7 row and
        // the chromium-path advice tell users to set to suppress this.
        if self.render != RenderMode::Never && needs_render(&body) {
            return FetchOutcome::Error(CfError::RenderNeeded);
        }

        FetchOutcome::Body {
            url: req.url.clone(),
            final_url: resp.final_url,
            status: code,
            body,
            meta: FetchMeta {
                etag: resp.etag,
                last_modified: resp.last_modified,
                tier: Some(FetchTier::Http),
                status: code,
                ms: Some(elapsed_ms),
            },
        }
    }

    /// Send the conditional GET and, for a success response, read the capped body — all in ONE
    /// async scope. Validators become `If-None-Match` / `If-Modified-Since`. On a success status the
    /// body is buffered under the 5 MB cap; on any other status the body is left unread (we only
    /// need its headers).
    async fn do_get(&self, req: &FetchRequest) -> Result<HttpResponse, CfError> {
        let mut builder = self.client.get(&req.url);
        if let Some(etag) = &req.etag {
            builder = builder.header(reqwest::header::IF_NONE_MATCH, etag.as_str());
        }
        if let Some(lm) = &req.last_modified {
            builder = builder.header(reqwest::header::IF_MODIFIED_SINCE, lm.as_str());
        }
        // §4.11 — attach credentials (already `${ENV}`-expanded; never logged, §12). `browser` auth
        // requires the Phase-2 render tier, which is rejected with exit 7 before any fetch.
        if let Some(auth) = &req.auth {
            use cf_core::AuthCfg::*;
            match auth {
                Header { headers } => {
                    for (k, v) in headers {
                        builder = builder.header(k.as_str(), v.as_str());
                    }
                }
                Cookie { cookies } => {
                    builder = builder.header(reqwest::header::COOKIE, cookies.as_str());
                }
                Basic { username, password } => {
                    builder = builder.basic_auth(username, Some(password));
                }
                Browser => {}
            }
        }
        let resp = builder.send().await.map_err(|e| classify_transport(&e))?;

        let status = resp.status();
        let code = status.as_u16();
        let final_url = resp.url().to_string();
        let etag = header_str(resp.headers(), reqwest::header::ETAG);
        let last_modified = header_str(resp.headers(), reqwest::header::LAST_MODIFIED);
        let retry_after = parse_retry_after(resp.headers());

        // Only buffer the body for a 2xx (the only path that needs it). Reading is fallible (the
        // 5 MB cap / a mid-stream transport error), so it is stored as a Result.
        let body = if status.is_success() {
            read_capped(resp, MAX_BODY_BYTES).await
        } else {
            Ok(String::new())
        };

        Ok(HttpResponse {
            status: code,
            final_url,
            etag,
            last_modified,
            retry_after,
            body,
        })
    }

    /// Fetch (or read from the 24 h cache) the robots.txt for a host.
    fn robots_for(&self, host: &str, url: &str) -> Result<Robots, CfError> {
        let now = self.clock.now_ms();
        if let Some(entry) = self.robots.lock().unwrap().get(host) {
            if now - entry.fetched_at_ms < ROBOTS_TTL_MS {
                return Ok(entry.robots.clone());
            }
        }
        // Cache miss / stale: fetch /robots.txt from the same origin. The send AND body read run
        // inside ONE `block_on` (same reactor-lifetime constraint as `do_get`).
        let robots_url = robots_url_for(url)
            .ok_or_else(|| CfError::SoftFetch("cannot derive robots.txt url".into()))?;
        let robots = self.runtime.block_on(async {
            match self.client.get(&robots_url).send().await {
                Ok(r) if r.status().is_success() => {
                    let text = r.text().await.unwrap_or_default();
                    Robots::parse(&text)
                }
                // Missing (404) / server error / transport error → permissive (allow all), still
                // cached so we do not hammer robots.txt within the TTL.
                _ => Robots::allow_all(),
            }
        });
        self.robots.lock().unwrap().insert(
            host.to_string(),
            RobotsEntry {
                robots: robots.clone(),
                fetched_at_ms: now,
                last_fetch_ms: None,
            },
        );
        Ok(robots)
    }

    /// Crawl-delay floor gate. Records the per-host last-fetch time and, if a prior fetch occurred
    /// within `delay_secs`, returns the remaining wait in whole seconds (exit 6). The store-backed
    /// cross-invocation timestamp is the cli's job (§12); this is the in-process floor.
    fn crawl_delay_gate(&self, host: &str, delay_secs: u32) -> Option<u32> {
        let now = self.clock.now_ms();
        let mut guard = self.robots.lock().unwrap();
        let entry = guard.get_mut(host)?;
        let interval_ms = delay_secs as i64 * 1000;
        match entry.last_fetch_ms {
            Some(last) => {
                let elapsed = now - last;
                if elapsed < interval_ms {
                    // Too soon — surface the remaining wait (rounded up), do NOT advance the stamp.
                    Some((((interval_ms - elapsed) + 999) / 1000) as u32)
                } else {
                    entry.last_fetch_ms = Some(now);
                    None
                }
            }
            None => {
                entry.last_fetch_ms = Some(now);
                None
            }
        }
    }
}

impl Default for HttpFetcher {
    fn default() -> Self {
        Self::new().expect("default HttpFetcher")
    }
}

impl FetchClient for HttpFetcher {
    fn fetch(&self, req: &FetchRequest) -> FetchOutcome {
        self.fetch_inner(req)
    }
}

/// Builder for [`HttpFetcher`].
pub struct HttpFetcherBuilder {
    user_agent: String,
    timeout_secs: u64,
    respect_robots: bool,
    render: RenderMode,
    clock: Box<dyn NowMs>,
}

impl Default for HttpFetcherBuilder {
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_USER_AGENT.to_string(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            respect_robots: true,
            render: RenderMode::Auto,
            clock: Box::new(SystemNowMs),
        }
    }
}

impl HttpFetcherBuilder {
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }
    pub fn timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }
    pub fn respect_robots(mut self, yes: bool) -> Self {
        self.respect_robots = yes;
        self
    }
    pub fn render(mut self, mode: RenderMode) -> Self {
        self.render = mode;
        self
    }
    pub fn clock(mut self, clock: Box<dyn NowMs>) -> Self {
        self.clock = clock;
        self
    }
    pub fn build(self) -> Result<HttpFetcher, CfError> {
        HttpFetcher::from_builder(self)
    }
}

// ===============================================================================================
// Body read under the 5 MB cap.
// ===============================================================================================

/// Stream the (decompressed) response body, aborting if it exceeds `cap` bytes (§5.1). reqwest's
/// gzip/br/zstd layers mean `chunk` yields decoded bytes, so the cap is on the *content* size.
async fn read_capped(resp: reqwest::Response, cap: u64) -> Result<String, CfError> {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| classify_transport(&e))?;
        if buf.len() as u64 + chunk.len() as u64 > cap {
            return Err(CfError::SoftFetch(format!(
                "body exceeds {cap}-byte cap (§5.1)"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    // Lossy is fine: extraction works on text and a non-UTF-8 page is degenerate for our purposes.
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

// ===============================================================================================
// needs_render heuristic (§5.1).
// ===============================================================================================

/// `needs_render` heuristic (DESIGN §5.1). Fires when the page is JS-empty:
/// (1) `visible_text_len / html_byte_len < 0.05` AND absolute visible text `< 200` chars; OR
/// (2) a known hydration root is present but empty (`<div id="root">`, `__next`, `<app-root>`).
/// The MVP has no headless tier, so a fired heuristic is reported as exit 7 (render needed).
pub fn needs_render(html: &str) -> bool {
    let html_bytes = html.len();
    if html_bytes == 0 {
        return false;
    }
    let visible = visible_text_len(html);
    let ratio = visible as f64 / html_bytes as f64;
    if ratio < RENDER_TEXT_RATIO && visible < RENDER_MIN_TEXT_CHARS {
        return true;
    }
    empty_hydration_root(html)
}

/// Approximate visible-text length: strip `<script>`/`<style>` content and all remaining tags, then
/// count the non-whitespace-collapsed characters. Deliberately cheap — it is a heuristic, not a
/// parser.
fn visible_text_len(html: &str) -> usize {
    let stripped = strip_invisible(html);
    let mut count = 0usize;
    let mut prev_ws = true;
    for c in stripped.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                count += 1; // collapse runs to one space
            }
            prev_ws = true;
        } else {
            count += 1;
            prev_ws = false;
        }
    }
    count
}

/// Remove `<script>`…`</script>` / `<style>`…`</style>` bodies and every tag, leaving text only.
fn strip_invisible(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Skip a whole <script>/<style> element (content is not visible text).
            if let Some(end_tag) = skip_element(&lower, i, "script") {
                i = end_tag;
                continue;
            }
            if let Some(end_tag) = skip_element(&lower, i, "style") {
                i = end_tag;
                continue;
            }
            // Otherwise skip just this tag.
            if let Some(close) = html[i..].find('>') {
                i += close + 1;
            } else {
                break;
            }
        } else {
            let ch_len = utf8_char_len(bytes[i]);
            out.push_str(&html[i..(i + ch_len).min(html.len())]);
            i += ch_len;
        }
    }
    out
}

/// If `lower[at..]` opens `<tag …>`, return the index just past its matching `</tag>` (or the end).
fn skip_element(lower: &str, at: usize, tag: &str) -> Option<usize> {
    let open = format!("<{tag}");
    if !lower[at..].starts_with(&open) {
        return None;
    }
    // Confirm it is the tag and not a prefix (e.g. `<scriptx`): next char is `>`, space, or `/`.
    let after = at + open.len();
    let ok = lower[after..]
        .chars()
        .next()
        .map(|c| c == '>' || c.is_whitespace() || c == '/')
        .unwrap_or(false);
    if !ok {
        return None;
    }
    let close = format!("</{tag}>");
    match lower[after..].find(&close) {
        Some(rel) => Some(after + rel + close.len()),
        None => Some(lower.len()), // unterminated element — skip to EOF
    }
}

/// Known SPA hydration roots present but empty (DESIGN §5.1 heuristic 2).
fn empty_hydration_root(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    const ROOTS: &[(&str, &str)] = &[
        ("<div id=\"root\"", "</div>"),
        ("<div id='root'", "</div>"),
        ("<div id=\"__next\"", "</div>"),
        ("<div id='__next'", "</div>"),
        ("<app-root", "</app-root>"),
    ];
    for (open, close) in ROOTS {
        if let Some(start) = lower.find(open) {
            // Find the end of the opening tag, then the matching close tag.
            if let Some(gt) = lower[start..].find('>') {
                let content_start = start + gt + 1;
                if let Some(crel) = lower[content_start..].find(close) {
                    let inner = &html[content_start..content_start + crel];
                    if inner.trim().is_empty() {
                        return true;
                    }
                }
            }
        }
    }
    false
}

// ===============================================================================================
// robots.txt — a small, self-contained parser (DESIGN §12).
// ===============================================================================================

/// A parsed robots.txt: per-user-agent allow/disallow rule sets + crawl-delays. Path matching uses
/// longest-match wins (the de-facto standard) with `*` wildcard and `$` end-anchor support.
#[derive(Clone, Debug)]
pub struct Robots {
    groups: Vec<RobotsGroup>,
}

#[derive(Clone, Debug, Default)]
struct RobotsGroup {
    agents: Vec<String>, // lowercased UA tokens this group applies to ("*" = all)
    rules: Vec<RobotsRule>,
    crawl_delay: Option<u32>,
}

#[derive(Clone, Debug)]
struct RobotsRule {
    allow: bool,
    pattern: String, // raw path pattern (may contain `*` and a trailing `$`)
}

impl Robots {
    /// A permissive robots (no rules) — used when robots.txt is absent / unreachable.
    pub fn allow_all() -> Self {
        Self { groups: Vec::new() }
    }

    /// Parse robots.txt content into grouped rules.
    pub fn parse(text: &str) -> Self {
        let mut groups: Vec<RobotsGroup> = Vec::new();
        let mut cur: Option<RobotsGroup> = None;
        // A blank line / a new User-agent after a rule starts a new group; consecutive
        // User-agent lines accumulate into one group (multi-agent group).
        let mut last_was_rule = false;

        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f.trim().to_ascii_lowercase(), v.trim().to_string()),
                None => continue,
            };
            match field.as_str() {
                "user-agent" => {
                    if last_was_rule {
                        // A new agent line after rules ⇒ close the current group.
                        if let Some(g) = cur.take() {
                            groups.push(g);
                        }
                        last_was_rule = false;
                    }
                    let g = cur.get_or_insert_with(RobotsGroup::default);
                    g.agents.push(value.to_ascii_lowercase());
                }
                "disallow" => {
                    let g = cur.get_or_insert_with(RobotsGroup::default);
                    g.rules.push(RobotsRule {
                        allow: false,
                        pattern: value,
                    });
                    last_was_rule = true;
                }
                "allow" => {
                    let g = cur.get_or_insert_with(RobotsGroup::default);
                    g.rules.push(RobotsRule {
                        allow: true,
                        pattern: value,
                    });
                    last_was_rule = true;
                }
                "crawl-delay" => {
                    let g = cur.get_or_insert_with(RobotsGroup::default);
                    if let Ok(secs) = value.parse::<f64>() {
                        g.crawl_delay = Some(secs.ceil() as u32);
                    }
                    last_was_rule = true;
                }
                _ => {}
            }
        }
        if let Some(g) = cur.take() {
            groups.push(g);
        }
        Self { groups }
    }

    /// The most-specific group applicable to `ua` (an exact token match beats `*`).
    fn group_for(&self, ua: &str) -> Option<&RobotsGroup> {
        let ua_l = ua.to_ascii_lowercase();
        let mut star: Option<&RobotsGroup> = None;
        for g in &self.groups {
            for a in &g.agents {
                if a == "*" {
                    star = Some(g);
                } else if ua_l.contains(a.as_str()) {
                    return Some(g);
                }
            }
        }
        star
    }

    /// Whether `path` is allowed for `ua` (longest-match wins; `Allow` beats `Disallow` on a tie).
    pub fn allowed(&self, ua: &str, path: &str) -> bool {
        let group = match self.group_for(ua) {
            Some(g) => g,
            None => return true, // no applicable group ⇒ allowed
        };
        let mut best: Option<(&RobotsRule, usize)> = None;
        for rule in &group.rules {
            if let Some(len) = pattern_match_len(&rule.pattern, path) {
                let better = match best {
                    None => true,
                    Some((br, blen)) => {
                        len > blen || (len == blen && rule.allow && !br.allow)
                    }
                };
                if better {
                    best = Some((rule, len));
                }
            }
        }
        match best {
            Some((rule, _)) => rule.allow,
            None => true,
        }
    }

    /// The crawl-delay (whole seconds) declared for `ua`'s group, if any.
    pub fn crawl_delay_secs(&self, ua: &str) -> Option<u32> {
        self.group_for(ua).and_then(|g| g.crawl_delay)
    }
}

/// If `pattern` matches a prefix of `path`, return the length of the *pattern's literal coverage*
/// (used for longest-match wins). Supports `*` (any run) and a trailing `$` (end-anchor). An empty
/// `Disallow:` pattern matches nothing (match len `None`); `/` matches everything.
fn pattern_match_len(pattern: &str, path: &str) -> Option<usize> {
    if pattern.is_empty() {
        return None; // empty Disallow ⇒ allow everything (no match)
    }
    let (pat, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    // Walk the pattern with `*` wildcards. We only need to know IF it matches a prefix and the
    // literal length consumed (for specificity scoring) — a simple greedy matcher suffices for the
    // shapes seen in real robots.txt.
    if !pat.contains('*') {
        let matches = if anchored {
            path == pat
        } else {
            path.starts_with(pat)
        };
        return if matches { Some(pat.len()) } else { None };
    }
    // Wildcard path: ensure each literal segment appears in order.
    let mut pos = 0usize;
    let mut literal_len = 0usize;
    let segments: Vec<&str> = pat.split('*').collect();
    let n = segments.len();
    for (i, seg) in segments.iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        literal_len += seg.len();
        if i == 0 {
            if !path[pos..].starts_with(seg) {
                return None;
            }
            pos += seg.len();
        } else {
            match path[pos..].find(seg) {
                Some(rel) => pos += rel + seg.len(),
                None => return None,
            }
        }
        let _ = n;
    }
    if anchored && pos != path.len() {
        return None;
    }
    Some(literal_len)
}

// ===============================================================================================
// Header / URL helpers.
// ===============================================================================================

fn header_str(headers: &reqwest::header::HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Parse `Retry-After` (delta-seconds form) into seconds (§12). HTTP-date form is not parsed (the
/// MVP backs off a default on the cli side); a non-numeric value yields `None`.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u32> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Classify a reqwest transport error into the §4.5 soft-fetch error (exit 3): DNS/TLS/timeout/
/// connect failures are all transient.
fn classify_transport(e: &reqwest::Error) -> CfError {
    let kind = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_redirect() {
        "redirect-loop"
    } else {
        "transport"
    };
    CfError::SoftFetch(format!("{kind}: {e}"))
}

fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
}

fn path_of(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => {
            let mut p = u.path().to_string();
            if let Some(q) = u.query() {
                p.push('?');
                p.push_str(q);
            }
            p
        }
        Err(_) => "/".to_string(),
    }
}

fn robots_url_for(url: &str) -> Option<String> {
    let u = reqwest::Url::parse(url).ok()?;
    let scheme = u.scheme();
    let host = u.host_str()?;
    let port = u
        .port()
        .map(|p| format!(":{p}"))
        .unwrap_or_default();
    Some(format!("{scheme}://{host}{port}/robots.txt"))
}

fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests;
