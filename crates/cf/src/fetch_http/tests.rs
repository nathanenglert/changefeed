//! Offline tests for the Tier-1 HTTP fetcher (DESIGN §5.1, §12; ARCHITECTURE §4.5).
//!
//! ALL tests use a LOCAL wiremock origin — NEVER the real internet. The fetcher owns its own
//! current-thread tokio runtime and drives reqwest via `block_on`; the wiremock server runs on a
//! separate multi-thread runtime kept alive for the duration of each test (its background workers
//! keep serving while the fetcher blocks on its own runtime), so the synchronous `fetch()` seam
//! works without a nested-runtime panic. The clock is injected (`FixedNowMs`) so robots TTL and the
//! crawl-delay floor are deterministic.

use super::*;
use cf_core::fetch::{FetchClient, FetchOutcome, FetchRequest};
use cf_core::model::RenderMode;
use cf_core::CfError;
use wiremock::matchers::{header, header_exists, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A wiremock origin paired with the runtime that drives it. The runtime is multi-threaded so its
/// background workers keep the mock server responsive while the fetcher blocks on its OWN runtime.
struct TestServer {
    server: MockServer,
    rt: tokio::runtime::Runtime,
}

impl TestServer {
    fn start() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let server = rt.block_on(MockServer::start());
        Self { server, rt }
    }

    fn uri(&self) -> String {
        self.server.uri()
    }

    fn mount(&self, mock: Mock) {
        self.rt.block_on(self.server.register(mock));
    }
}

/// A fetcher with robots OFF and a frozen clock — the default for status-mapping tests that do not
/// exercise robots.txt.
fn fetcher_no_robots(ua: &str) -> HttpFetcher {
    HttpFetcher::builder()
        .user_agent(ua)
        .timeout_secs(5)
        .respect_robots(false)
        .clock(Box::new(FixedNowMs(1_000_000)))
        .build()
        .expect("fetcher")
}

fn req(url: String) -> FetchRequest {
    FetchRequest {
        url,
        etag: None,
        last_modified: None,
        auth: None,
    }
}

// ===============================================================================================
// 200 → body + ETag.
// ===============================================================================================

#[test]
fn get_200_returns_body_and_etag() {
    let srv = TestServer::start();
    let html = "<html><body><main><h1>Pricing</h1>\
                <p>Our plans start at a clear, server-rendered price so this page does not look \
                empty to the needs_render heuristic at all whatsoever.</p>\
                <p>Plenty of visible prose here to keep the text ratio well above five percent.</p>\
                </main></body></html>";
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/pricing"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", "\"abc123\"")
                    .insert_header("Last-Modified", "Wed, 21 Oct 2026 07:28:00 GMT")
                    .set_body_raw(html.as_bytes().to_vec(), "text/html"),
            ),
    );

    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    let out = fetcher.fetch(&req(format!("{}/pricing", srv.uri())));

    match out {
        FetchOutcome::Body {
            status,
            body,
            meta,
            final_url,
            ..
        } => {
            assert_eq!(status, 200);
            assert!(body.contains("Pricing"), "body must carry the fetched HTML");
            assert_eq!(meta.etag.as_deref(), Some("\"abc123\""), "ETag is captured");
            assert_eq!(
                meta.last_modified.as_deref(),
                Some("Wed, 21 Oct 2026 07:28:00 GMT")
            );
            assert_eq!(meta.status, 200);
            assert_eq!(meta.tier, Some(FetchTier::Http));
            assert!(final_url.ends_with("/pricing"));
        }
        other => panic!("expected Body, got {}", outcome_name(&other)),
    }
}

#[test]
fn request_sends_pinned_ua_and_accept_encoding() {
    let srv = TestServer::start();
    let html = "<html><body><main><p>The request matched only because it carried the pinned UA \
                and the gzip/br/zstd Accept-Encoding the §5.1 client always advertises, and this \
                paragraph is long enough to keep needs_render quiet.</p></main></body></html>";
    // The mock matches ONLY when both the pinned UA and an Accept-Encoding advertising gzip/br/zstd
    // are present — so a Body outcome PROVES the §5.1 headers were sent (no separate inspection
    // seam needed).
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/hdr"))
            .and(header("user-agent", DEFAULT_USER_AGENT))
            .and(header_regex("accept-encoding", "gzip"))
            .and(header_regex("accept-encoding", "br"))
            .and(header_regex("accept-encoding", "zstd"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes().to_vec(), "text/html")),
    );

    // Build with the production default UA (the deliverable's pinned `changefeed/1.0` + contact).
    let fetcher = HttpFetcher::builder()
        .respect_robots(false)
        .timeout_secs(5)
        .clock(Box::new(FixedNowMs(0)))
        .build()
        .expect("fetcher");
    assert_eq!(fetcher.user_agent, DEFAULT_USER_AGENT);
    assert!(
        DEFAULT_USER_AGENT.starts_with("changefeed/1.0") && DEFAULT_USER_AGENT.contains("https://"),
        "the default UA is the pinned changefeed/1.0 with a contact URL (§12)"
    );

    match fetcher.fetch(&req(format!("{}/hdr", srv.uri()))) {
        FetchOutcome::Body { status, .. } => assert_eq!(status, 200),
        other => panic!(
            "the GET must carry the pinned UA + gzip/br/zstd Accept-Encoding, got {}",
            outcome_name(&other)
        ),
    }
}

// ===============================================================================================
// Conditional re-request: If-None-Match → 304 short-circuit, no body.
// ===============================================================================================

#[test]
fn conditional_get_304_short_circuits_with_no_body() {
    let srv = TestServer::start();
    // The origin returns 304 ONLY when the client sends If-None-Match (the validator we carry).
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/doc"))
            .and(header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304).insert_header("ETag", "\"v1\"")),
    );

    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    let request = FetchRequest {
        url: format!("{}/doc", srv.uri()),
        etag: Some("\"v1\"".to_string()),
        last_modified: None,
        auth: None,
    };
    let out = fetcher.fetch(&request);

    match out {
        FetchOutcome::NotModified { meta } => {
            // The 304 path carries NO body field at all (the variant has none) — the short-circuit
            // is structural. The validator round-trips so the next conditional GET reuses it.
            assert_eq!(meta.status, 304);
            assert_eq!(meta.etag.as_deref(), Some("\"v1\""));
            assert_eq!(meta.tier, Some(FetchTier::Http));
        }
        other => panic!("expected NotModified (http-304), got {}", outcome_name(&other)),
    }
}

#[test]
fn conditional_get_sends_if_modified_since() {
    let srv = TestServer::start();
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/lm"))
            .and(header_exists("if-modified-since"))
            .respond_with(ResponseTemplate::new(304)),
    );

    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    let request = FetchRequest {
        url: format!("{}/lm", srv.uri()),
        etag: None,
        last_modified: Some("Wed, 21 Oct 2026 07:28:00 GMT".to_string()),
        auth: None,
    };
    match fetcher.fetch(&request) {
        FetchOutcome::NotModified { meta } => {
            assert_eq!(meta.status, 304);
            // The Last-Modified validator round-trips even when the 304 omits the header.
            assert_eq!(
                meta.last_modified.as_deref(),
                Some("Wed, 21 Oct 2026 07:28:00 GMT")
            );
        }
        other => panic!(
            "If-Modified-Since must drive the 304 path, got {}",
            outcome_name(&other)
        ),
    }
}

// ===============================================================================================
// 5xx → soft fetch error (exit 3).
// ===============================================================================================

#[test]
fn server_5xx_maps_to_soft_fetch_exit_3() {
    let srv = TestServer::start();
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/boom"))
            .respond_with(ResponseTemplate::new(503)),
    );

    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    match fetcher.fetch(&req(format!("{}/boom", srv.uri()))) {
        FetchOutcome::Error(e) => {
            assert!(matches!(e, CfError::SoftFetch(_)), "5xx is a soft fetch error");
            assert_eq!(e.exit_code() as u8, 3, "§4.5: soft fetch error is exit 3");
        }
        other => panic!("expected SoftFetch, got {}", outcome_name(&other)),
    }
}

// ===============================================================================================
// timeout → soft fetch error (exit 3).
// ===============================================================================================

#[test]
fn timeout_maps_to_soft_fetch_exit_3() {
    let srv = TestServer::start();
    // Respond after a delay longer than the fetcher's 1 s timeout.
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("ok")
                    .set_delay(std::time::Duration::from_secs(3)),
            ),
    );

    let fetcher = HttpFetcher::builder()
        .user_agent("changefeed-test/1.0")
        .timeout_secs(1)
        .respect_robots(false)
        .clock(Box::new(FixedNowMs(0)))
        .build()
        .expect("fetcher");

    match fetcher.fetch(&req(format!("{}/slow", srv.uri()))) {
        FetchOutcome::Error(e) => {
            assert!(matches!(e, CfError::SoftFetch(_)), "timeout is a soft fetch error");
            assert_eq!(e.exit_code() as u8, 3);
        }
        other => panic!("expected SoftFetch (timeout), got {}", outcome_name(&other)),
    }
}

// ===============================================================================================
// DNS / connect failure → soft fetch error (exit 3). No real network: an unbound local port.
// ===============================================================================================

#[test]
fn connect_failure_maps_to_soft_fetch_exit_3() {
    // 127.0.0.1:1 is reserved/unbound — a connect attempt fails immediately, locally (no internet).
    let fetcher = HttpFetcher::builder()
        .user_agent("changefeed-test/1.0")
        .timeout_secs(2)
        .respect_robots(false)
        .clock(Box::new(FixedNowMs(0)))
        .build()
        .expect("fetcher");

    match fetcher.fetch(&req("http://127.0.0.1:1/never".to_string())) {
        FetchOutcome::Error(e) => {
            assert!(matches!(e, CfError::SoftFetch(_)));
            assert_eq!(e.exit_code() as u8, 3);
        }
        other => panic!("expected SoftFetch (connect), got {}", outcome_name(&other)),
    }
}

// ===============================================================================================
// 429 with Retry-After: 30 → exit 6, retry_after = 30.
// ===============================================================================================

#[test]
fn rate_limit_429_parses_retry_after_30_exit_6() {
    let srv = TestServer::start();
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "30")),
    );

    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    match fetcher.fetch(&req(format!("{}/throttled", srv.uri()))) {
        FetchOutcome::Error(e) => {
            match &e {
                CfError::RateLimit { retry_after } => {
                    assert_eq!(*retry_after, Some(30), "Retry-After: 30 parses into crawl.retry_after");
                }
                _ => panic!("expected RateLimit, got {e:?}"),
            }
            assert_eq!(e.exit_code() as u8, 6, "§4.5: 429 is exit 6");
        }
        other => panic!("expected RateLimit, got {}", outcome_name(&other)),
    }
}

#[test]
fn rate_limit_429_without_retry_after_is_still_exit_6() {
    let srv = TestServer::start();
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/throttled"))
            .respond_with(ResponseTemplate::new(429)),
    );

    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    match fetcher.fetch(&req(format!("{}/throttled", srv.uri()))) {
        FetchOutcome::Error(CfError::RateLimit { retry_after }) => {
            assert_eq!(retry_after, None, "missing Retry-After ⇒ None, not a fabricated value");
        }
        other => panic!("expected RateLimit, got {}", outcome_name(&other)),
    }
}

// ===============================================================================================
// 401 → auth (exit 5). And 403 likewise.
// ===============================================================================================

#[test]
fn auth_401_maps_to_exit_5() {
    let srv = TestServer::start();
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/secret"))
            .respond_with(ResponseTemplate::new(401)),
    );

    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    match fetcher.fetch(&req(format!("{}/secret", srv.uri()))) {
        FetchOutcome::Error(e) => {
            assert!(matches!(e, CfError::Auth(401)), "401 is an auth error carrying the code");
            assert_eq!(e.exit_code() as u8, 5, "§4.5: 401 is exit 5");
        }
        other => panic!("expected Auth, got {}", outcome_name(&other)),
    }
}

#[test]
fn auth_403_maps_to_exit_5() {
    let srv = TestServer::start();
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/forbidden"))
            .respond_with(ResponseTemplate::new(403)),
    );

    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    match fetcher.fetch(&req(format!("{}/forbidden", srv.uri()))) {
        FetchOutcome::Error(CfError::Auth(403)) => {}
        other => panic!("expected Auth(403), got {}", outcome_name(&other)),
    }
}

// ===============================================================================================
// robots.txt Disallow blocks the path → exit 4 when respect_robots.
// ===============================================================================================

#[test]
fn robots_disallow_blocks_path_exit_4() {
    let srv = TestServer::start();
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("User-agent: *\nDisallow: /private\n"),
            ),
    );
    // The resource itself would 200, but robots must short-circuit BEFORE the GET.
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/private/data"))
            .respond_with(ResponseTemplate::new(200).set_body_string("should never be read")),
    );

    let fetcher = HttpFetcher::builder()
        .user_agent("changefeed-test/1.0")
        .timeout_secs(5)
        .respect_robots(true)
        .clock(Box::new(FixedNowMs(1_000_000)))
        .build()
        .expect("fetcher");

    match fetcher.fetch(&req(format!("{}/private/data", srv.uri()))) {
        FetchOutcome::Error(e) => {
            assert!(matches!(e, CfError::Robots), "disallowed path is a robots block");
            assert_eq!(e.exit_code() as u8, 4, "§4.5: robots block is exit 4");
        }
        other => panic!("expected Robots, got {}", outcome_name(&other)),
    }
}

#[test]
fn robots_allows_unlisted_path() {
    let srv = TestServer::start();
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("User-agent: *\nDisallow: /private\n"),
            ),
    );
    let html = "<html><body><main><p>This public page has plenty of visible prose so the \
                needs_render heuristic stays well clear of firing on a perfectly normal page.</p>\
                <p>More than two hundred characters of real content keeps the ratio comfortable.</p>\
                </main></body></html>";
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/public"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes().to_vec(), "text/html")),
    );

    let fetcher = HttpFetcher::builder()
        .user_agent("changefeed-test/1.0")
        .timeout_secs(5)
        .respect_robots(true)
        .clock(Box::new(FixedNowMs(1_000_000)))
        .build()
        .expect("fetcher");

    match fetcher.fetch(&req(format!("{}/public", srv.uri()))) {
        FetchOutcome::Body { status, .. } => assert_eq!(status, 200),
        other => panic!("a robots-allowed path must fetch, got {}", outcome_name(&other)),
    }
}

#[test]
fn respect_robots_false_ignores_disallow() {
    let srv = TestServer::start();
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string("User-agent: *\nDisallow: /\n")),
    );
    let html = "<html><body><main><p>Even though robots disallows everything, an explicit \
                respect_robots=false opt-out fetches anyway — and this paragraph is long enough \
                that the needs_render heuristic does not fire on it at all.</p></main></body></html>";
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/anything"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes().to_vec(), "text/html")),
    );

    let fetcher = HttpFetcher::builder()
        .user_agent("changefeed-test/1.0")
        .timeout_secs(5)
        .respect_robots(false) // explicit, logged opt-out (§12)
        .clock(Box::new(FixedNowMs(1_000_000)))
        .build()
        .expect("fetcher");

    match fetcher.fetch(&req(format!("{}/anything", srv.uri()))) {
        FetchOutcome::Body { status, .. } => assert_eq!(status, 200),
        other => panic!("respect_robots=false must bypass robots, got {}", outcome_name(&other)),
    }
}

// ===============================================================================================
// Crawl-delay honored as a floor → too-soon second fetch is exit 6.
// ===============================================================================================

#[test]
fn crawl_delay_floor_gates_second_fetch_exit_6() {
    let srv = TestServer::start();
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("User-agent: *\nCrawl-delay: 30\nAllow: /\n"),
            ),
    );
    let html = "<html><body><main><p>A normal content page with more than two hundred characters \
                of visible prose so the needs_render heuristic never fires here, leaving the \
                crawl-delay floor as the only gate under test.</p></main></body></html>";
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes().to_vec(), "text/html")),
    );

    // A frozen clock: both fetches see the same instant, so the second is inside the 30 s floor.
    let fetcher = HttpFetcher::builder()
        .user_agent("changefeed-test/1.0")
        .timeout_secs(5)
        .respect_robots(true)
        .clock(Box::new(FixedNowMs(5_000_000)))
        .build()
        .expect("fetcher");

    // First fetch passes (records the per-host last-fetch time).
    match fetcher.fetch(&req(format!("{}/page", srv.uri()))) {
        FetchOutcome::Body { status, .. } => assert_eq!(status, 200),
        other => panic!("first fetch should pass, got {}", outcome_name(&other)),
    }
    // Second fetch within the (frozen) 30 s window is gated as exit 6 with the remaining wait.
    match fetcher.fetch(&req(format!("{}/page", srv.uri()))) {
        FetchOutcome::Error(CfError::RateLimit { retry_after }) => {
            assert_eq!(retry_after, Some(30), "the full crawl-delay remains (frozen clock)");
        }
        other => panic!(
            "second fetch inside the crawl-delay floor must be exit 6, got {}",
            outcome_name(&other)
        ),
    }
}

// ===============================================================================================
// Redirect chain followed; final_url recorded.
// ===============================================================================================

#[test]
fn redirect_chain_followed_records_final_url() {
    let srv = TestServer::start();
    let base = srv.uri();
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", format!("{base}/middle")),
            ),
    );
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/middle"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", format!("{base}/end")),
            ),
    );
    let html = "<html><body><main><h1>Arrived</h1><p>The redirect chain landed here with plenty \
                of visible prose so the needs_render heuristic does not fire on the final page.</p>\
                </main></body></html>";
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/end"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(html.as_bytes().to_vec(), "text/html")),
    );

    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    match fetcher.fetch(&req(format!("{base}/start"))) {
        FetchOutcome::Body { final_url, status, body, .. } => {
            assert_eq!(status, 200);
            assert!(final_url.ends_with("/end"), "final_url is the landed URL, got {final_url}");
            assert!(body.contains("Arrived"));
        }
        other => panic!("expected Body after redirects, got {}", outcome_name(&other)),
    }
}

#[test]
fn redirect_over_cap_is_soft_fetch_error() {
    let srv = TestServer::start();
    let base = srv.uri();
    // A 5-hop chain exceeds the 3-redirect cap → reqwest aborts → soft fetch error (exit 3).
    for i in 0..6 {
        let next = format!("{base}/hop{}", i + 1);
        srv.mount(
            Mock::given(method("GET"))
                .and(path(format!("/hop{i}")))
                .respond_with(ResponseTemplate::new(302).insert_header("Location", next)),
        );
    }

    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    match fetcher.fetch(&req(format!("{base}/hop0"))) {
        FetchOutcome::Error(e) => {
            assert!(matches!(e, CfError::SoftFetch(_)));
            assert_eq!(e.exit_code() as u8, 3);
        }
        other => panic!("a chain over the 3-redirect cap must fail, got {}", outcome_name(&other)),
    }
}

// ===============================================================================================
// Oversized body aborted at the 5 MB cap.
// ===============================================================================================

#[test]
fn oversized_body_aborted_at_5mb_cap() {
    let srv = TestServer::start();
    // 6 MB of visible text — over the 5 MB cap.
    let big = "x".repeat(6 * 1024 * 1024);
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/huge"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(big.into_bytes(), "text/html")),
    );

    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    match fetcher.fetch(&req(format!("{}/huge", srv.uri()))) {
        FetchOutcome::Error(e) => {
            assert!(matches!(e, CfError::SoftFetch(_)), "over-cap body is aborted as a soft error");
            assert!(
                e.to_string().contains("cap"),
                "the error must name the body cap, got {e}"
            );
        }
        other => panic!("expected the 5 MB cap to abort, got {}", outcome_name(&other)),
    }
}

#[test]
fn body_just_under_cap_is_returned() {
    let srv = TestServer::start();
    // ~1 KB of real prose — comfortably under the cap and over the needs_render floor.
    let small = format!(
        "<html><body><main>{}</main></body></html>",
        "<p>real visible content paragraph</p>".repeat(40)
    );
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(small.into_bytes(), "text/html")),
    );

    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    match fetcher.fetch(&req(format!("{}/ok", srv.uri()))) {
        FetchOutcome::Body { status, .. } => assert_eq!(status, 200),
        other => panic!("a small body must be returned, got {}", outcome_name(&other)),
    }
}

// ===============================================================================================
// JS-empty page (text ratio < 0.05) → needs_render → exit 7.
// ===============================================================================================

#[test]
fn js_empty_page_triggers_needs_render_exit_7() {
    let srv = TestServer::start();
    // A heavy <script> payload with almost no visible text: a classic SPA shell. The text ratio is
    // far below 0.05 and absolute visible text is < 200 chars.
    let big_script = "var data = [".to_string() + &"0,".repeat(20_000) + "0];";
    let spa = format!(
        "<html><head><script>{big_script}</script></head>\
         <body><div id=\"root\"></div></body></html>"
    );
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/spa"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(spa.into_bytes(), "text/html")),
    );

    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    match fetcher.fetch(&req(format!("{}/spa", srv.uri()))) {
        FetchOutcome::Error(e) => {
            assert!(matches!(e, CfError::RenderNeeded), "a JS-empty page needs render");
            assert_eq!(e.exit_code() as u8, 7, "§4.5: render-needed is exit 7 (MVP, no headless)");
        }
        other => panic!("expected RenderNeeded, got {}", outcome_name(&other)),
    }
}

#[test]
fn empty_hydration_root_triggers_needs_render_exit_7() {
    let srv = TestServer::start();
    // Enough boilerplate to push the ratio above 0.05, but the known hydration root is EMPTY —
    // heuristic (2) must still fire.
    let filler = "<p>nav link</p>".repeat(60);
    let next = format!(
        "<html><body><nav>{filler}</nav><div id=\"__next\"></div></body></html>"
    );
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/next"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(next.into_bytes(), "text/html")),
    );

    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    match fetcher.fetch(&req(format!("{}/next", srv.uri()))) {
        FetchOutcome::Error(CfError::RenderNeeded) => {}
        other => panic!(
            "an empty __next hydration root must trigger needs_render, got {}",
            outcome_name(&other)
        ),
    }
}

// ===============================================================================================
// render=never bypasses the needs_render heuristic — the documented escape hatch for the §4.5
// exit-7 row. A JS-empty shell that WOULD be exit 7 under `auto` is diffed as raw HTML instead.
// ===============================================================================================

#[test]
fn render_never_bypasses_needs_render_and_returns_body() {
    let srv = TestServer::start();
    // The SAME script-heavy SPA shell that `js_empty_page_triggers_needs_render_exit_7` proves is
    // exit 7 under the default (auto) render mode: text ratio far below 0.05 AND an empty `#root`.
    let big_script = "var data = [".to_string() + &"0,".repeat(20_000) + "0];";
    let spa = format!(
        "<html><head><script>{big_script}</script></head>\
         <body><div id=\"root\"></div></body></html>"
    );
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/spa"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(spa.into_bytes(), "text/html")),
    );

    // Identical to `fetcher_no_robots`, but with render=never — the only difference under test.
    let fetcher = HttpFetcher::builder()
        .user_agent("changefeed-test/1.0")
        .timeout_secs(5)
        .respect_robots(false)
        .render(RenderMode::Never)
        .clock(Box::new(FixedNowMs(1_000_000)))
        .build()
        .expect("fetcher");

    match fetcher.fetch(&req(format!("{}/spa", srv.uri()))) {
        FetchOutcome::Body { status, body, meta, .. } => {
            assert_eq!(status, 200, "render=never must NOT short-circuit to exit 7");
            assert!(
                body.contains("id=\"root\""),
                "the raw HTTP HTML is returned verbatim for diffing, not discarded"
            );
            assert_eq!(meta.tier, Some(FetchTier::Http));
        }
        other => panic!(
            "render=never must diff the raw HTML, not return RenderNeeded — got {}",
            outcome_name(&other)
        ),
    }
}

// ===============================================================================================
// render=chromium → exit 7 unconditionally (no headless tier in the MVP), no fetch performed.
// ===============================================================================================

#[test]
fn render_chromium_maps_to_exit_7_without_fetching() {
    // No server needed: render=chromium short-circuits BEFORE any network I/O. The URL is a dead
    // local port to prove no GET is attempted.
    let fetcher = HttpFetcher::builder()
        .user_agent("changefeed-test/1.0")
        .timeout_secs(5)
        .respect_robots(false)
        .render(RenderMode::Chromium)
        .clock(Box::new(FixedNowMs(0)))
        .build()
        .expect("fetcher");

    match fetcher.fetch(&req("http://127.0.0.1:1/whatever".to_string())) {
        FetchOutcome::Error(e) => {
            assert!(matches!(e, CfError::RenderNeeded));
            assert_eq!(e.exit_code() as u8, 7);
        }
        other => panic!("render=chromium must be exit 7, got {}", outcome_name(&other)),
    }
}

// ===============================================================================================
// Pure-unit tests for the needs_render heuristic and robots parser (no server).
// ===============================================================================================

#[test]
fn needs_render_unit_matrix() {
    // A rich, server-rendered page does NOT need render.
    let rich = format!(
        "<html><body><main>{}</main></body></html>",
        "<p>Substantial visible content that a server rendered fully.</p>".repeat(20)
    );
    assert!(!needs_render(&rich), "a content-rich page must not flag needs_render");

    // A script-only shell DOES need render (ratio < 0.05 AND < 200 visible chars).
    let shell = format!(
        "<html><head><script>{}</script></head><body><div id=\"root\"></div></body></html>",
        "a=1;".repeat(5000)
    );
    assert!(needs_render(&shell), "a JS shell must flag needs_render");

    // An empty page (0 bytes) is not flagged (degenerate, avoids div-by-zero).
    assert!(!needs_render(""));

    // The hydration-root heuristic fires even when the ratio is fine.
    let next_empty = format!(
        "<html><body><nav>{}</nav><div id=\"__next\"></div></body></html>",
        "<a>menu</a>".repeat(80)
    );
    assert!(needs_render(&next_empty), "empty __next root flags needs_render");

    // A populated hydration root is NOT flagged.
    let next_full = "<html><body><div id=\"__next\"><main><p>Hydrated server content that is \
                     long enough to keep the visible-text ratio comfortably above the cutoff and \
                     well past two hundred characters of real prose.</p></main></div></body></html>";
    assert!(!needs_render(next_full), "a populated __next root must not flag");
}

#[test]
fn robots_parser_longest_match_wins() {
    let robots = Robots::parse(
        "User-agent: *\n\
         Disallow: /private\n\
         Allow: /private/public\n\
         Crawl-delay: 5\n",
    );
    assert!(!robots.allowed("changefeed/1.0", "/private/data"));
    assert!(
        robots.allowed("changefeed/1.0", "/private/public/page"),
        "the more-specific Allow wins over the shorter Disallow"
    );
    assert!(robots.allowed("changefeed/1.0", "/"));
    assert_eq!(robots.crawl_delay_secs("changefeed/1.0"), Some(5));
}

#[test]
fn robots_empty_disallow_allows_everything() {
    // `Disallow:` (empty) is the canonical "allow all" group.
    let robots = Robots::parse("User-agent: *\nDisallow:\n");
    assert!(robots.allowed("changefeed/1.0", "/anything"));
    assert!(robots.allowed("changefeed/1.0", "/deep/path"));
}

#[test]
fn robots_specific_agent_group_beats_star() {
    let robots = Robots::parse(
        "User-agent: *\n\
         Disallow: /\n\
         \n\
         User-agent: changefeed\n\
         Disallow: /admin\n",
    );
    // Our UA matches the specific group (which only blocks /admin), not the catch-all Disallow: /.
    assert!(robots.allowed("changefeed/1.0", "/public"));
    assert!(!robots.allowed("changefeed/1.0", "/admin/panel"));
    // A different UA falls to the catch-all and is blocked everywhere.
    assert!(!robots.allowed("SomeOtherBot/2.0", "/public"));
}

#[test]
fn robots_wildcard_pattern() {
    let robots = Robots::parse("User-agent: *\nDisallow: /*.pdf$\n");
    assert!(!robots.allowed("changefeed/1.0", "/files/report.pdf"));
    assert!(robots.allowed("changefeed/1.0", "/files/report.html"));
}

/// §4.11 — header auth must actually be attached to the request. The mock only returns 200 when the
/// `Authorization` header matches the configured (already `${ENV}`-expanded) value; otherwise it
/// would 404. This proves the credential reaches the wire (the prior bug dropped it silently).
#[test]
fn header_auth_is_attached_to_the_request() {
    use cf_core::model::AuthCfg;
    let srv = TestServer::start();
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/secure"))
            .and(header("authorization", "Bearer s3cr3t"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("<html><body><p>ok</p></body></html>"),
            ),
    );
    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    let request = FetchRequest {
        url: format!("{}/secure", srv.uri()),
        etag: None,
        last_modified: None,
        auth: Some(AuthCfg::Header {
            headers: vec![("Authorization".to_string(), "Bearer s3cr3t".to_string())],
        }),
    };
    assert!(
        matches!(fetcher.fetch(&request), FetchOutcome::Body { status: 200, .. }),
        "the Authorization header must be sent (mock 200-gates on it)"
    );
}

/// §4.11 — basic auth produces a correct `Authorization: Basic <base64>` header.
#[test]
fn basic_auth_is_attached_to_the_request() {
    use cf_core::model::AuthCfg;
    let srv = TestServer::start();
    // base64("alice:secret") = YWxpY2U6c2VjcmV0
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/secure"))
            .and(header("authorization", "Basic YWxpY2U6c2VjcmV0"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("<html><body><p>ok</p></body></html>"),
            ),
    );
    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    let request = FetchRequest {
        url: format!("{}/secure", srv.uri()),
        etag: None,
        last_modified: None,
        auth: Some(AuthCfg::Basic {
            username: "alice".to_string(),
            password: "secret".to_string(),
        }),
    };
    assert!(
        matches!(fetcher.fetch(&request), FetchOutcome::Body { status: 200, .. }),
        "basic auth must produce the expected Authorization header"
    );
}

/// §4.11 (AS-2) — cookie auth must attach the verbatim `Cookie` header to the request. The mock
/// 200-gates on the cookie string, so a 200 proves it reached the wire.
#[test]
fn cookie_auth_is_attached_to_the_request() {
    use cf_core::model::AuthCfg;
    let srv = TestServer::start();
    srv.mount(
        Mock::given(method("GET"))
            .and(path("/secure"))
            .and(header("cookie", "session=abc123; csrf=xyz789"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("<html><body><p>ok</p></body></html>"),
            ),
    );
    let fetcher = fetcher_no_robots("changefeed-test/1.0");
    let request = FetchRequest {
        url: format!("{}/secure", srv.uri()),
        etag: None,
        last_modified: None,
        auth: Some(AuthCfg::Cookie {
            cookies: "session=abc123; csrf=xyz789".to_string(),
        }),
    };
    assert!(
        matches!(fetcher.fetch(&request), FetchOutcome::Body { status: 200, .. }),
        "the Cookie header must be sent (mock 200-gates on it)"
    );
}

// ---- helpers ----------------------------------------------------------------------------------

fn outcome_name(o: &FetchOutcome) -> String {
    match o {
        FetchOutcome::Body { status, .. } => format!("Body(status={status})"),
        FetchOutcome::NotModified { .. } => "NotModified".to_string(),
        FetchOutcome::Error(e) => format!("Error({e:?})"),
    }
}
