//! End-to-end integration suite for the canonical agent contract (DESIGN §4.5, §6, §10; ARCHITECTURE
//! §5.3/§5.4). Drives the real `cf` binary via `assert_cmd` against the repo's `tests/fixtures`
//! pricing HTML and asserts the WHOLE agent surface an integrator relies on:
//!
//!   * the §10 worked example: `pricing_before` → `pricing_after` ⇒ exit 10 with the exact event
//!     shape (`ct=modified`, `delta.enc=val a="$59/mo" b="$49/mo"`, `why.cat=price_increase`,
//!     `why.mat=high`, `followup.act=re_run_downstream`, high `conf`, byte size in the §6.1 band);
//!   * the no-op short-circuit (volatile-only delta ⇒ exit 0, EMPTY stdout);
//!   * first observation ⇒ exit 11 with the baseline envelope (`baseline:true`, `from_rev` absent);
//!   * `--peek` re-emits the same delta without advancing the baseline; the default `check` advances
//!     it (a re-run is exit 0);
//!   * `--min-salience` gating (sub-threshold ⇒ exit 0; with `--emit-subthreshold` ⇒ exit 12);
//!   * the exit-code-table edges: bad flag ⇒ exit 1, unknown target ⇒ exit 2;
//!   * `cf schema --version 1` emits a valid JSON Schema AND a produced event validates against it;
//!   * `cf init` scaffolds `changefeed.toml` + `.changefeed/`;
//!   * DETERMINISM: the deterministic `cf check --stdin` golden path is BYTE-IDENTICAL across two
//!     runs (fixed injected id/obs) — the core is reproducible.
//!
//! Every invocation injects a fixed `obs` (`CF_FAKE_OBS`) and a stepped id (`CF_FAKE_ID`) so the
//! pipeline is clock/network-free and the byte assertions are stable.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

const FAKE_OBS: &str = "2026-06-02T14:30:11Z";
const URL: &str = "https://competitor.com/pricing";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
}

fn read_fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join(name)).expect("fixture exists")
}

/// A fresh project dir with a `comp-pricing` target whose ignore rules strip the volatile CSRF
/// nonce + live-viewer counter, so a volatile-only delta short-circuits at `doc_hash` (§5.3 → §5.6).
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

/// A `cf` command rooted at `dir` with a fixed injected clock + stepped id and jsonl output.
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
    let out = cmd.write_stdin(read_fixture(fixture)).assert().get_output().clone();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Parse the single jsonl event line emitted by a change run.
fn one_event(stdout: &str) -> Value {
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "expected exactly one event line, got: {stdout}");
    serde_json::from_str(lines[0]).expect("event line is valid JSON")
}

// ===========================================================================================
// The §10 worked example — the canonical agent contract event.
// ===========================================================================================

#[test]
fn section_10_worked_example_full_event_contract() {
    let dir = project_with_pricing_target();

    // First observation stores a baseline → exit 11 (the §10 setup step).
    let (code, _) = check_stdin(dir.path(), "pricing_before.html", &[]);
    assert_eq!(code, 11, "first observation is exit 11");

    // The Pro plan moves $49 → $59: the poll that matters → exit 10.
    let (code, stdout) = check_stdin(dir.path(), "pricing_after.html", &[]);
    assert_eq!(code, 10, "a material price change is exit 10");

    let ev = one_event(&stdout);

    // §6.2 top-level shape.
    assert_eq!(ev["v"], "1");
    assert_eq!(ev["src"]["url"], URL);
    assert_eq!(ev["src"]["tid"], "comp-pricing");
    // src.title comes from the page <title> (wired at the cli boundary — the selector subtree omits <head>).
    assert_eq!(ev["src"]["title"], "Pricing — Competitor");
    assert_eq!(ev["obs"], FAKE_OBS);

    // ct = modified.
    assert_eq!(ev["ct"], "modified");

    // delta: val encoding, a = after, b = before (a first), the genuine $49 → $59 move.
    assert_eq!(ev["delta"]["enc"], "val");
    assert_eq!(ev["delta"]["a"], "$59/mo");
    assert_eq!(ev["delta"]["b"], "$49/mo");

    // why: price_increase category, HIGH materiality (the cross-target routing label), salience present.
    assert_eq!(ev["why"]["cat"], "price_increase");
    assert_eq!(ev["why"]["mat"], "high", "a price-tier rise is mat=high under the pricing pack bands");
    let sal = ev["why"]["sal"].as_f64().expect("sal is a number");
    assert!((0.70..=0.89).contains(&sal), "sal {sal} should fall in the HIGH band [0.70, 0.89]");
    // The summary describes the percentage move.
    assert!(
        ev["why"]["summary"].as_str().unwrap().contains("20.4%"),
        "summary states the 20.4% move: {}",
        ev["why"]["summary"]
    );

    // followup: the pricing pack routes a high-materiality price change to re_run_downstream.
    assert_eq!(ev["followup"]["act"], "re_run_downstream");

    // conf: high — a clean, anchored, well-typed price change (§6.6 product of five factors).
    let conf = ev["conf"].as_f64().expect("conf is a number");
    assert!(conf >= 0.85, "conf {conf} should be high (>= 0.85) for a clean anchored price change");

    // seg: the affected segment addresses the Pro Plan price cell.
    assert_eq!(ev["seg"][0]["role"], "price");
    assert!(ev["seg"][0]["fp"].as_str().unwrap().starts_with("blake3:"));

    // prov: http tier, this observation's snapshot hash, the pricing pack stamp.
    assert_eq!(ev["prov"]["m"], "http");
    assert_eq!(ev["prov"]["status"], 200);
    assert!(ev["prov"]["hash"].as_str().unwrap().starts_with("blake3:"));
    assert!(
        ev["prov"]["pack"].as_str().unwrap().starts_with("pricing@b3:"),
        "prov.pack stamps the pricing pack + content hash"
    );

    // §6.1 byte-size target: the minified event is ~792 B (the doc-wide figure). The deterministic
    // network-free --stdin golden has no fetched etag (legitimately absent), so it sits at the low
    // edge of the band; the fully-fetched event (with prov.etag/ms) lands squarely at ~720-760 B.
    let line = stdout.lines().find(|l| !l.is_empty()).unwrap();
    let bytes = line.len();
    assert!(
        (680..=900).contains(&bytes),
        "the minified event is {bytes} B; the §6.1 ~792 B target band is ~700-900 B. event: {line}"
    );

    // Presence-as-signal (§6.1 rule 4): no field serializes as null.
    assert!(!line.contains("null"), "no field may serialize as null: {line}");
}

// ===========================================================================================
// No-op: a volatile-only delta is exit 0 with EMPTY stdout (doc_hash-equal short-circuit).
// ===========================================================================================

#[test]
fn noop_volatile_only_change_is_exit_0_empty_stdout() {
    let dir = project_with_pricing_target();
    let (code, _) = check_stdin(dir.path(), "pricing_before.html", &[]);
    assert_eq!(code, 11);

    // pricing_noop differs from before ONLY in the stripped nonce + viewer counter.
    let (code, stdout) = check_stdin(dir.path(), "pricing_noop.html", &[]);
    assert_eq!(code, 0, "a volatile-only delta is no-change (exit 0)");
    assert!(stdout.is_empty(), "the no-op path prints NOTHING; got: {stdout:?}");
}

// ===========================================================================================
// First observation ⇒ exit 11 with the baseline envelope (baseline:true, from_rev:null/absent).
// ===========================================================================================

#[test]
fn first_observation_emits_baseline_envelope_exit_11() {
    let dir = project_with_pricing_target();
    let (code, stdout) = check_stdin(dir.path(), "pricing_before.html", &[]);
    assert_eq!(code, 11, "first observation is exit 11");

    // The baseline envelope is one jsonl line (§6.8 first-observation contract).
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "exit 11 prints one baseline envelope line: {stdout}");
    let env: Value = serde_json::from_str(lines[0]).expect("baseline envelope is valid JSON");

    assert_eq!(env["v"], "1");
    assert_eq!(env["feed"], "comp-pricing");
    // baseline:true with from_rev null tells the agent "first time seen, nothing to diff."
    assert_eq!(env["crawl"]["baseline"], true);
    assert!(
        env["crawl"].get("from_rev").is_none() || env["crawl"]["from_rev"].is_null(),
        "from_rev is null/absent on the baseline: {}",
        env["crawl"]
    );
    assert_eq!(env["crawl"]["n"], 0, "the baseline carries zero events");
    assert!(env["events"].as_array().unwrap().is_empty(), "baseline events[] is empty");
    // The envelope never serializes a null (presence-as-signal): from_rev is OMITTED, not null.
    assert!(!lines[0].contains("null"), "the baseline envelope leaked a null: {}", lines[0]);
}

// ===========================================================================================
// --peek re-emits the SAME delta without advancing the baseline; default check advances it.
// ===========================================================================================

#[test]
fn peek_re_emits_same_delta_default_check_advances_baseline() {
    let dir = project_with_pricing_target();
    check_stdin(dir.path(), "pricing_before.html", &[]); // baseline (11)

    // Two consecutive peeks see the SAME delta (the baseline is never advanced).
    let (c1, peek1) = check_stdin(dir.path(), "pricing_after.html", &["--peek"]);
    let (c2, peek2) = check_stdin(dir.path(), "pricing_after.html", &["--peek"]);
    assert_eq!(c1, 10);
    assert_eq!(c2, 10);
    assert!(!peek1.is_empty());
    assert_eq!(peek1, peek2, "a second --peek re-emits the SAME delta (baseline not advanced)");

    // The default check after peeking still emits (peek never persisted) ...
    let (c3, _) = check_stdin(dir.path(), "pricing_after.html", &[]);
    assert_eq!(c3, 10, "the default check still emits — peek did not advance the baseline");

    // ... and now that the default check DID advance it, the same content is a no-op.
    let (c4, stdout4) = check_stdin(dir.path(), "pricing_after.html", &[]);
    assert_eq!(c4, 0, "re-checking the now-current snapshot is exit 0");
    assert!(stdout4.is_empty());
}

// ===========================================================================================
// --min-salience gating: sub-threshold ⇒ exit 0; with --emit-subthreshold ⇒ exit 12.
// ===========================================================================================

/// A target whose only change is a low/medium-salience prose tweak (a release-note reword).
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
    let out = cmd.write_stdin(read_fixture(fixture)).assert().get_output().clone();
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
    assert!(stdout.is_empty(), "a gated sub-threshold change prints nothing: {stdout:?}");
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
// Exit-code table edges: bad flag ⇒ exit 1, unknown target ⇒ exit 2.
// ===========================================================================================

#[test]
fn bad_flag_is_usage_exit_1() {
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
    assert_eq!(code, 2, "an unknown (non-URL) target id is exit 2");
}

// ===========================================================================================
// cf schema --version 1 emits valid JSON Schema AND a produced event validates against it.
// ===========================================================================================

#[test]
fn schema_version_1_is_valid_and_a_produced_event_validates() {
    let dir = project_with_pricing_target();

    // 1. The schema is valid JSON Schema 2020-12 with the MCP $defs.
    let schema_out = cf(dir.path())
        .args(["schema", "--version", "1"])
        .assert()
        .success()
        .get_output()
        .clone();
    let schema_json: Value =
        serde_json::from_slice(&schema_out.stdout).expect("schema output is valid JSON");
    assert_eq!(schema_json["$schema"], "https://json-schema.org/draft/2020-12/schema");
    for def in ["seg", "ct", "delta", "why", "followup"] {
        assert!(schema_json["$defs"].get(def).is_some(), "schema missing $defs/{def}");
    }

    // 2. Produce a real event from the pipeline and validate it against the published schema.
    check_stdin(dir.path(), "pricing_before.html", &[]); // baseline
    let (code, stdout) = check_stdin(dir.path(), "pricing_after.html", &[]);
    assert_eq!(code, 10);
    let event = one_event(&stdout);

    let validator = jsonschema::validator_for(&schema_json).expect("schema compiles");
    let errors: Vec<String> = validator.iter_errors(&event).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "a produced event must validate against cf schema: {errors:?}");
    assert!(validator.is_valid(&event));
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
// cf init scaffolds changefeed.toml + .changefeed/.
// ===========================================================================================

#[test]
fn init_scaffolds_config_and_store_dir() {
    let dir = TempDir::new().unwrap();
    cf(dir.path()).arg("init").assert().success();
    assert!(dir.path().join("changefeed.toml").is_file(), "init writes changefeed.toml");
    assert!(dir.path().join(".changefeed").is_dir(), "init creates .changefeed/");
    // The scaffolded config parses (a subsequent `ls` does not error on it).
    cf(dir.path()).arg("ls").assert().success();
}

// ===========================================================================================
// DETERMINISM: the deterministic --stdin golden path is BYTE-IDENTICAL across two runs.
// ===========================================================================================

#[test]
fn cf_check_golden_event_is_byte_identical_across_runs() {
    // Run the WHOLE before→after pipeline twice, in two fresh stores, with the SAME injected fixed
    // id (CF_FAKE_ID=T) and obs (CF_FAKE_OBS). The emitted change event must be byte-for-byte equal
    // — proving the core (extract→…→event) is reproducible modulo the boundary-injected id/obs.
    let run_once = || {
        let dir = project_with_pricing_target();
        let (b, _) = check_stdin(dir.path(), "pricing_before.html", &[]);
        assert_eq!(b, 11);
        let (c, stdout) = check_stdin(dir.path(), "pricing_after.html", &[]);
        assert_eq!(c, 10);
        stdout
    };
    let r1 = run_once();
    let r2 = run_once();
    assert_eq!(r1, r2, "the same two snapshots must produce a byte-identical event (determinism)");
    assert!(!r1.is_empty());
    // And it is the genuine price event (not an empty/no-op).
    assert!(r1.contains(r#""a":"$59/mo""#));
    assert!(r1.contains(r#""b":"$49/mo""#));
}

#[test]
fn score_dry_run_is_byte_identical_across_runs() {
    // The §8.5 tuning loop (`cf score --dry-run before after`) is the other deterministic, fully
    // network-free path. Two runs must be byte-identical (seeded id, no ULID).
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
    assert_eq!(r1, r2, "score --dry-run must be byte-identical across runs (§8.5)");
    assert!(!r1.is_empty(), "score --dry-run emits the scored events");
    let s = String::from_utf8_lossy(&r1);
    assert!(s.contains(r#""a":"$59/mo""#));
    assert!(s.contains(r#""b":"$49/mo""#));
}
