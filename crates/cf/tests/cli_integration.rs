//! CLI integration tests (ARCHITECTURE.md §5.4) driving the real `cf` binary via `assert_cmd`
//! against the `tests/fixtures` pricing HTML. These assert the FULL §4.5 exit-code table, the
//! ABSOLUTE stdout=events / stderr=logs piping contract, `--peek`/`--no-store` re-emit semantics,
//! `--stdin --url`, `--min-salience` gating, the schema, and `score --dry-run` byte-determinism.
//!
//! Determinism: every invocation injects a fixed `obs` (`CF_FAKE_OBS`) and a stepped id
//! (`CF_FAKE_ID`) so the pipeline is clock/network-free and golden assertions are stable.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;

const FAKE_OBS: &str = "2026-06-02T14:30:11Z";
const URL: &str = "https://competitor.com/pricing";

fn fixtures() -> PathBuf {
    // crates/cf/tests/ -> repo root /tests/fixtures
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
}

fn read_fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join(name)).expect("fixture exists")
}

/// A fresh project dir with `changefeed.toml` declaring a `comp-pricing` target whose ignore rules
/// strip the volatile CSRF nonce + live-viewer counter (so the no-op short-circuits, §5.3 → §5.6).
fn project_with_pricing_target() -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("changefeed.toml"),
        r#"
[defaults]
min_salience = "low"

[[target]]
id = "comp-pricing"
url = "https://competitor.com/pricing"
archetype = "pricing"
select = [".PricingTable"]
ignore = [".live-counter", { attr = "data-csrf-nonce" }, { regex = "\\d+ viewing right now" }]
"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join(".changefeed")).unwrap();
    dir
}

/// Build a `cf` command rooted at `dir` with the injected fixed clock + id, jsonl output.
fn cf(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("cf").unwrap();
    cmd.current_dir(dir)
        .env("CF_FAKE_OBS", FAKE_OBS)
        .env("CF_FAKE_ID", "T")
        .env("CF_DIR", dir);
    cmd
}

/// Run `cf check comp-pricing --stdin --url <URL>` piping `fixture` HTML; return (exit, stdout).
fn check_stdin(dir: &Path, fixture: &str, extra: &[&str]) -> (i32, String) {
    let mut cmd = cf(dir);
    cmd.args(["check", "comp-pricing", "--stdin", "--url", URL, "--format", "jsonl"]);
    cmd.args(extra);
    let assert = cmd.write_stdin(read_fixture(fixture)).assert();
    let output = assert.get_output().clone();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

// ===========================================================================================
// init
// ===========================================================================================

#[test]
fn init_creates_config_and_store() {
    let dir = TempDir::new().unwrap();
    cf(dir.path()).arg("init").assert().success();
    assert!(dir.path().join("changefeed.toml").is_file());
    assert!(dir.path().join(".changefeed").is_dir());
}

// ===========================================================================================
// the §10 worked example: before -> after yields exit 10 + the price `val` event
// ===========================================================================================

#[test]
fn before_then_after_emits_modified_price_delta_exit_10() {
    let dir = project_with_pricing_target();
    // Baseline (first observation) — exit 11.
    let (code, _) = check_stdin(dir.path(), "pricing_before.html", &[]);
    assert_eq!(code, 11, "first observation is exit 11");

    // The genuine $49 -> $59 change — exit 10 with a jsonl event.
    let (code, stdout) = check_stdin(dir.path(), "pricing_after.html", &[]);
    assert_eq!(code, 10, "a material change is exit 10");

    // Exactly one event line, and it carries the price `val` delta a="$59/mo" b="$49/mo".
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "only the price event survives the strip rules: {stdout}");
    let ev: serde_json::Value = serde_json::from_str(lines[0]).expect("valid jsonl event");
    assert_eq!(ev["ct"], "modified");
    assert_eq!(ev["delta"]["enc"], "val");
    assert_eq!(ev["delta"]["a"], "$59/mo");
    assert_eq!(ev["delta"]["b"], "$49/mo");
    assert_eq!(ev["why"]["cat"], "price_increase");
    assert_eq!(ev["seg"][0]["role"], "price");
}

// ===========================================================================================
// no-op: before -> noop is exit 0 with EMPTY stdout (doc_hash-equal short-circuit)
// ===========================================================================================

#[test]
fn before_then_noop_is_exit_0_empty_stdout() {
    let dir = project_with_pricing_target();
    let (code, _) = check_stdin(dir.path(), "pricing_before.html", &[]);
    assert_eq!(code, 11);

    let (code, stdout) = check_stdin(dir.path(), "pricing_noop.html", &[]);
    assert_eq!(code, 0, "a volatile-only delta is no-change (exit 0)");
    assert!(stdout.is_empty(), "the no-op path prints NOTHING; got: {stdout:?}");
}

// ===========================================================================================
// idempotency: a default check, then a re-check of the same content, is exit 0
// ===========================================================================================

#[test]
fn default_check_then_recheck_is_idempotent_exit_0() {
    let dir = project_with_pricing_target();
    check_stdin(dir.path(), "pricing_before.html", &[]); // baseline (11)
    let (code, _) = check_stdin(dir.path(), "pricing_after.html", &[]);
    assert_eq!(code, 10, "the change advances the baseline");

    // Re-check the SAME after content: the baseline is now the after snapshot -> no change.
    let (code, stdout) = check_stdin(dir.path(), "pricing_after.html", &[]);
    assert_eq!(code, 0, "re-checking the now-current snapshot is a no-op");
    assert!(stdout.is_empty());
}

// ===========================================================================================
// --peek: prints the delta WITHOUT advancing the baseline; a second --peek prints the SAME delta
// ===========================================================================================

#[test]
fn peek_does_not_advance_baseline_and_is_recallable() {
    let dir = project_with_pricing_target();
    check_stdin(dir.path(), "pricing_before.html", &[]); // baseline

    let (c1, peek1) = check_stdin(dir.path(), "pricing_after.html", &["--peek"]);
    let (c2, peek2) = check_stdin(dir.path(), "pricing_after.html", &["--peek"]);
    assert_eq!(c1, 10);
    assert_eq!(c2, 10);
    assert!(!peek1.is_empty());
    assert_eq!(peek1, peek2, "a second --peek sees the SAME delta (baseline not advanced)");

    // A default check after peeking still detects the change (peek never persisted it).
    let (c3, _) = check_stdin(dir.path(), "pricing_after.html", &[]);
    assert_eq!(c3, 10, "the default check still emits — peek did not advance the baseline");
}

// ===========================================================================================
// --min-salience gating + --emit-subthreshold
// ===========================================================================================

/// A target whose only change is a low/medium-salience prose tweak (release-note reword).
fn project_with_notes_target() -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("changefeed.toml"),
        r#"
[[target]]
id = "notes"
url = "https://example.com/notes"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join(".changefeed")).unwrap();
    dir
}

fn check_notes(dir: &Path, fixture: &str, extra: &[&str]) -> (i32, String) {
    let mut cmd = cf(dir);
    cmd.args([
        "check",
        "notes",
        "--stdin",
        "--url",
        "https://example.com/notes",
        "--format",
        "jsonl",
    ]);
    cmd.args(extra);
    let out = cmd
        .write_stdin(read_fixture(fixture))
        .assert()
        .get_output()
        .clone();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn min_salience_high_gates_a_subhigh_change_to_exit_0() {
    let dir = project_with_notes_target();
    let (code, _) = check_notes(dir.path(), "lowsal_before.html", &[]);
    assert_eq!(code, 11);

    // The reword scores below `high`; with --min-salience high and no --emit-subthreshold it is
    // exit 0 with empty stdout. --no-store keeps the baseline so the assertion is repeatable.
    let (code, stdout) = check_notes(
        dir.path(),
        "lowsal_after.html",
        &["--min-salience", "high", "--no-store"],
    );
    assert_eq!(code, 0, "a sub-high change with --min-salience high is exit 0");
    assert!(stdout.is_empty());
}

#[test]
fn emit_subthreshold_surfaces_exit_12() {
    let dir = project_with_notes_target();
    check_notes(dir.path(), "lowsal_before.html", &[]); // baseline

    let (code, stdout) = check_notes(
        dir.path(),
        "lowsal_after.html",
        &["--min-salience", "high", "--emit-subthreshold", "--no-store"],
    );
    assert_eq!(code, 12, "--emit-subthreshold reports the sub-threshold change as exit 12");
    // Exit 12 is an exit-code signal; cf check prints no event for it (the envelope is daemon-only).
    assert!(stdout.is_empty());
}

// ===========================================================================================
// exit-code table edges: unknown flag (1), unknown target (2)
// ===========================================================================================

#[test]
fn unknown_flag_is_usage_exit_1() {
    let dir = project_with_pricing_target();
    let code = cf(dir.path())
        .args(["check", "comp-pricing", "--totally-bogus-flag"])
        .assert()
        .get_output()
        .status
        .code()
        .unwrap_or(-1);
    assert_eq!(code, 1, "a bad flag is exit 1 (usage), not clap's default 2");
}

#[test]
fn unknown_target_is_not_found_exit_2() {
    let dir = project_with_pricing_target();
    let code = cf(dir.path())
        .args(["check", "no-such-target"])
        .assert()
        .get_output()
        .status
        .code()
        .unwrap_or(-1);
    assert_eq!(code, 2, "an unknown target id is exit 2");
}

// ===========================================================================================
// schema --version 1 prints valid JSON Schema
// ===========================================================================================

#[test]
fn schema_version_1_prints_valid_json_schema() {
    let dir = TempDir::new().unwrap();
    let out = cf(dir.path())
        .args(["schema", "--version", "1"])
        .assert()
        .success()
        .get_output()
        .clone();
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("schema output is valid JSON");
    assert_eq!(json["$schema"], "https://json-schema.org/draft/2020-12/schema");
    // The MCP $refs point at these $defs (§4.10).
    for def in ["seg", "ct", "delta", "why", "followup"] {
        assert!(json["$defs"].get(def).is_some(), "schema missing $defs/{def}");
    }
}

#[test]
fn schema_unknown_version_is_usage_exit_1() {
    let dir = TempDir::new().unwrap();
    let code = cf(dir.path())
        .args(["schema", "--version", "99"])
        .assert()
        .get_output()
        .status
        .code()
        .unwrap_or(-1);
    assert_eq!(code, 1);
}

// ===========================================================================================
// the ABSOLUTE piping contract: stdout = events ONLY, warnings go to stderr
// ===========================================================================================

#[test]
fn stdout_carries_only_events_warnings_go_to_stderr() {
    let dir = project_with_pricing_target();
    check_stdin(dir.path(), "pricing_before.html", &[]); // baseline

    let out = cf(dir.path())
        .args(["check", "comp-pricing", "--stdin", "--url", URL, "--format", "jsonl"])
        .write_stdin(read_fixture("pricing_after.html"))
        .assert()
        .get_output()
        .clone();

    // stdout: every non-empty line parses as a JSON event (no logs/warnings leak in).
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|_| panic!("non-event on stdout: {line}"));
        assert_eq!(v["v"], "1", "stdout line is a changefeed/v1 event");
    }
    // The baseline run logged to stderr (e.g. nothing required on stdout for exit 11 jsonl prints
    // the envelope, but warnings/log lines NEVER appear on stdout).
    assert!(!stdout.contains("WARNING"));
    assert!(!stdout.contains("error:"));
}

#[test]
fn rules_warnings_go_to_stderr_not_stdout() {
    // A selector that matches ZERO nodes must WARN on stderr, never on stdout.
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("changefeed.toml"),
        r#"
[[target]]
id = "empty"
url = "https://example.com/x"
select = [".does-not-exist-anywhere"]
"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join(".changefeed")).unwrap();

    let out = cf(dir.path())
        .args(["rules", "empty", "--stdin", "--url", "https://example.com/x"])
        .write_stdin(b"<html><body><h1>Hello</h1></body></html>".to_vec())
        .assert()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("matched blocks: 0"), "the node count goes to stdout: {stdout}");
    assert!(
        stderr.contains("ZERO nodes"),
        "the zero-node WARNING goes to stderr: {stderr}"
    );
    assert!(!stdout.contains("WARNING"), "warnings never pollute stdout");
}

// ===========================================================================================
// score --dry-run is byte-identical across two runs (determinism, injected fixed id/obs)
// ===========================================================================================

#[test]
fn score_dry_run_is_byte_identical_across_runs() {
    let dir = TempDir::new().unwrap();
    let a = fixtures().join("pricing_before.html");
    let b = fixtures().join("pricing_after.html");

    let run = || {
        cf(dir.path())
            .args(["score", "--dry-run"])
            .arg(&a)
            .arg(&b)
            .args(["--archetype", "pricing", "--selector", ".PricingTable", "--format", "jsonl"])
            .assert()
            .get_output()
            .stdout
            .clone()
    };
    let r1 = run();
    let r2 = run();
    assert_eq!(r1, r2, "score --dry-run must be byte-identical across runs (§8.5 determinism)");
    assert!(!r1.is_empty(), "score --dry-run emits the scored events");

    // And it actually scores the price change.
    let s = String::from_utf8_lossy(&r1);
    assert!(s.contains(r#""a":"$59/mo""#));
    assert!(s.contains(r#""b":"$49/mo""#));
}

// ===========================================================================================
// snapshot seeds a baseline without diffing
// ===========================================================================================

#[test]
fn snapshot_seeds_a_baseline_without_emitting_events() {
    let dir = project_with_pricing_target();
    let out = cf(dir.path())
        .args(["snapshot", "comp-pricing", "--url", URL, "--stdin"])
        .write_stdin(read_fixture("pricing_before.html"))
        .assert()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(11), "snapshot reports first-obs (11)");
    assert!(out.stdout.is_empty(), "snapshot emits no events on stdout");

    // A subsequent check against the after content now diffs against the seeded baseline -> 10.
    let (code, _) = check_stdin(dir.path(), "pricing_after.html", &[]);
    assert_eq!(code, 10);
}

/// §4.7 — `cf feed` is paginated: `--limit` caps the page, the trailing envelope carries a
/// `next_cursor` when more remain, and `--after-cursor` resumes at the next event. (Before the fix
/// the cursor flags were inert and no `next_cursor` was ever emitted.)
#[test]
fn feed_paginates_with_next_cursor_and_after_cursor() {
    use serde_json::Value;
    let dir = project_with_pricing_target();
    let _ = cf(dir.path())
        .args(["snapshot", "comp-pricing", "--url", URL, "--stdin"])
        .write_stdin(read_fixture("pricing_before.html"))
        .assert();
    // Two retained transitions: rev0→1 (A→B) and rev1→2 (B→A).
    assert_eq!(check_stdin(dir.path(), "pricing_after.html", &[]).0, 10);
    assert_eq!(check_stdin(dir.path(), "pricing_before.html", &[]).0, 10);

    let feed = |args: &[&str]| -> Vec<String> {
        let mut a = vec!["feed", "comp-pricing", "--format", "jsonl"];
        a.extend_from_slice(args);
        let out = cf(dir.path()).args(a).assert().get_output().clone();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.to_string())
            .collect()
    };

    // Full feed (default limit) → every transition fits, so NO next_cursor.
    let full = feed(&[]);
    let full_env: Value = serde_json::from_str(full.last().unwrap()).unwrap();
    let total_events = full.len() - 1; // minus the trailing envelope line
    assert!(total_events >= 2, "the ring replays ≥2 transitions, got {total_events}");
    assert!(full_env["next_cursor"].is_null(), "a complete feed has no next_cursor: {}", full_env);

    // Page 1 (limit 1): exactly one event + a trailing envelope WITH next_cursor.
    let p1 = feed(&["--limit", "1"]);
    assert_eq!(p1.len(), 2, "page-1 = 1 event line + 1 trailing envelope: {p1:?}");
    let ev1: Value = serde_json::from_str(&p1[0]).unwrap();
    let env1: Value = serde_json::from_str(&p1[1]).unwrap();
    let cursor = env1["next_cursor"]
        .as_str()
        .expect("next_cursor present while more remain")
        .to_string();

    // Page 2 (resume): a DIFFERENT event than page 1 — proving --after-cursor advances.
    let p2 = feed(&["--limit", "1", "--after-cursor", &cursor]);
    let ev2: Value = serde_json::from_str(&p2[0]).unwrap();
    assert_ne!(ev1["id"], ev2["id"], "--after-cursor resumes at the next event, not the same one");
}

/// §7.4 — the idempotency seen-set must SUPPRESS a transition it has already emitted. A flap
/// `A→B→A→B` should emit the first `A→B`, the `B→A`, then dedup-suppress the SECOND `A→B` to exit 0.
/// (Before the fix `seen_event` was never consulted and the key was mis-derived, so nothing deduped.)
#[test]
fn seen_set_dedups_a_repeated_transition_to_exit_0() {
    let dir = project_with_pricing_target();
    // Seed baseline = before (state A).
    let _ = cf(dir.path())
        .args(["snapshot", "comp-pricing", "--url", URL, "--stdin"])
        .write_stdin(read_fixture("pricing_before.html"))
        .assert();
    // A→B: first time, emitted.
    let (c1, _) = check_stdin(dir.path(), "pricing_after.html", &[]);
    assert_eq!(c1, 10, "first A→B is a change");
    // B→A: a different (new) transition, emitted.
    let (c2, _) = check_stdin(dir.path(), "pricing_before.html", &[]);
    assert_eq!(c2, 10, "B→A is a new transition");
    // A→B AGAIN: this transition is already in the seen-set → dedup-suppressed → exit 0.
    let (c3, _) = check_stdin(dir.path(), "pricing_after.html", &[]);
    assert_eq!(c3, 0, "the repeated A→B transition is dedup-suppressed via the seen-set");
}

/// §4.3 / §11 — `cf check` with no writable store must fall back to "first observation, no politeness
/// memory" (exit 11) with a stderr warning, NOT a hard exit-1 usage error that an agent branching on
/// `$?` would treat as a non-retryable invocation bug.
#[cfg(unix)]
#[test]
fn unwritable_store_falls_back_to_first_observation_exit_11() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    // Read-only project dir → `.changefeed` cannot be created (the literal "no writable store").
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

    let output = Command::cargo_bin("cf")
        .unwrap()
        .current_dir(dir.path())
        .env("CF_FAKE_OBS", FAKE_OBS)
        .env("CF_FAKE_ID", "T")
        .env("CF_DIR", dir.path())
        .args(["check", URL, "--stdin", "--url", URL, "--format", "jsonl"])
        .write_stdin(read_fixture("pricing_before.html"))
        .assert()
        .get_output()
        .clone();

    // Restore perms so TempDir cleanup succeeds regardless of the assertion outcome.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

    let exit = output.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(exit, 11, "unwritable store → exit 11 fallback, not exit 1. stderr: {stderr}");
    let lc = stderr.to_lowercase();
    assert!(
        lc.contains("first-observation") || lc.contains("not writable") || stderr.contains("§4.3"),
        "a stderr warning must explain the fallback, got {stderr:?}"
    );
}

/// §4.4/§4.5/§4.6 — a 429 must exit 6 AND surface `crawl.retry_after` on BOTH stdout JSON (so the
/// canonical agent loop's `json.loads(stdout)["crawl"]["retry_after"]` / `jq '.crawl.retry_after'`
/// works) and stderr — without requiring a second fetch.
#[test]
fn rate_limit_429_surfaces_retry_after_on_stdout_and_stderr() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(async {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pricing"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "123"))
            .mount(&s)
            .await;
        s // /robots.txt is unmounted → 404 → crawl allowed
    });
    let url = format!("{}/pricing", server.uri());

    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".changefeed")).unwrap();
    let output = Command::cargo_bin("cf")
        .unwrap()
        .current_dir(dir.path())
        .env("CF_FAKE_OBS", FAKE_OBS)
        .env("CF_FAKE_ID", "T")
        .env("CF_DIR", dir.path())
        .args(["check", &url, "--format", "json"])
        .assert()
        .get_output()
        .clone();

    let exit = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert_eq!(exit, 6, "429 → exit 6 (rate-limited). stderr: {stderr}");
    // Whitespace-insensitive so the assertion holds for both compact (jsonl) and pretty (json) output.
    let compact: String = stdout.split_whitespace().collect();
    assert!(compact.contains("\"crawl\""), "stdout is a feed envelope, got {stdout:?}");
    assert!(
        compact.contains("\"retry_after\":123"),
        "stdout JSON carries crawl.retry_after=123, got {stdout:?}"
    );
    assert!(stderr.contains("123"), "retry_after is also on stderr, got {stderr:?}");

    drop(server);
    drop(rt);
}
