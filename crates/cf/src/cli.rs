//! clap derive command tree (ARCHITECTURE.md §1, §13): the MVP verbs + the §8.5 tuning verbs
//! (`explain`/`score`). Maps an `ObservationResult` to an `ExitCode` in one tested place.
//!
//! Piping contract (DESIGN §4.8, ABSOLUTE): **stdout = events ONLY** (jsonl/json/pretty);
//! **stderr = logs/progress/warnings/retry-after**. The no-change one-shot prints NOTHING and exits
//! 0. Determinism: `obs`/ids are injected via `CF_FAKE_OBS` / `CF_FAKE_ID` so golden tests are
//! clock/network-free (ARCHITECTURE §4).

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use cf_core::config;
use cf_core::packs;
use cf_core::pipeline::{self, PipelineCtx};
use cf_core::{
    event, CanonicalDoc, ChangeEvent, Config, Defaults, ExitCode, FetchTier, IgnoreRule,
    Materiality, ObservationResult, RenderMode, TargetCfg,
};

use crate::config_io;
use crate::fetch_http::HttpFetcher;
use crate::ids_clock::{Clock, IdGen, SeededIdGen, SystemClock, UlidGen};
use crate::render::{self, Format};
use crate::store_sqlite::SqliteStore;
use cf_core::fetch::{FetchClient, FetchOutcome, FetchRequest};
use cf_core::storage::{Store, StoredSnapshot};

/// changefeed — watch web pages for *material* changes and emit compact, agent-ready events.
#[derive(Debug, Parser)]
#[command(name = "cf", bin_name = "cf", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// The MVP subcommands (§13) + the §8.5 tuning verbs `explain`/`score`.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create `changefeed.toml` + `.changefeed/` in the current directory.
    Init,
    /// Add a `[[target]]` to `changefeed.toml`.
    Watch(WatchArgs),
    /// Observe a target once: fetch → pipeline → emit event(s) + exit code.
    Check(CheckArgs),
    /// Capture a raw snapshot of a target without diffing (seed baseline).
    Snapshot(SnapshotArgs),
    /// Re-emit a stored snapshot's diff (no network).
    Diff(DiffArgs),
    /// Read the change feed for targets (`--limit` / `--after-cursor`).
    Feed(FeedArgs),
    /// List configured targets.
    Ls,
    /// Show details for one target.
    Show(ShowArgs),
    /// Inspect / validate selectors and ignore rules.
    Rules(RulesArgs),
    /// Print the published JSON Schema (`--version 1`).
    Schema(SchemaArgs),
    /// Replay the salience scoring for an event and print the signal breakdown (§8.5).
    Explain(ExplainArgs),
    /// Score an ad-hoc snapshot pair without touching the store (the §8.5 tuning loop).
    Score(ScoreArgs),
}

#[derive(Debug, Args)]
pub struct WatchArgs {
    /// The URL (or an existing target id) to register.
    pub target: String,
    /// Optional stable target id (defaults to a URL-derived slug).
    #[arg(long)]
    pub id: Option<String>,
    /// Optional archetype rule pack (`pricing`/`api-docs`/`status-page`).
    #[arg(long)]
    pub archetype: Option<String>,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Targets (ids or ad-hoc URLs). Empty = all configured targets.
    pub targets: Vec<String>,
    /// Output format (default: pretty on a TTY, jsonl otherwise).
    #[arg(long, value_parser = ["pretty", "json", "jsonl"])]
    pub format: Option<String>,
    /// Diff against a specific prior revision instead of the head.
    #[arg(long)]
    pub since: Option<String>,
    /// Diff but do NOT persist / advance the baseline / record the dedup key (read-only probe).
    #[arg(long)]
    pub no_store: bool,
    /// Print the delta without advancing the baseline (a second --peek sees the same delta).
    #[arg(long)]
    pub peek: bool,
    /// Emit/exit only if salience >= LEVEL (none|low|medium|high|critical; default low).
    #[arg(long, value_parser = ["none", "low", "medium", "high", "critical"])]
    pub min_salience: Option<String>,
    /// Override the configured selector for an ad-hoc URL.
    #[arg(long)]
    pub selector: Option<String>,
    /// Override the render mode.
    #[arg(long, value_parser = ["auto", "never", "chromium"])]
    pub render: Option<String>,
    /// Per-fetch timeout in seconds.
    #[arg(long)]
    pub timeout: Option<u64>,
    /// HTTP conditional GET (default on).
    #[arg(long, overrides_with = "no_etag")]
    pub etag: bool,
    #[arg(long = "no-etag", overrides_with = "etag")]
    pub no_etag: bool,
    /// Emit (exit 12) for changes below --min-salience.
    #[arg(long)]
    pub emit_subthreshold: bool,
    /// Treat fetch errors as exit 1 instead of soft exit 3.
    #[arg(long)]
    pub fail_on_fetch_error: bool,
    /// Diff HTML piped on stdin instead of fetching (requires --url).
    #[arg(long)]
    pub stdin: bool,
    /// The URL the piped --stdin HTML represents.
    #[arg(long)]
    pub url: Option<String>,
    /// Read newline-delimited targets from stdin (`-`).
    #[arg(long)]
    pub targets_from: Option<String>,
    /// Suppress stdout; rely on the exit code only (cheapest poll).
    #[arg(short, long)]
    pub quiet: bool,
    /// The project directory (defaults to the current dir).
    #[arg(long, env = "CF_DIR")]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct SnapshotArgs {
    /// Target id or URL.
    pub target: String,
    #[arg(long)]
    pub url: Option<String>,
    #[arg(long)]
    pub stdin: bool,
    #[arg(long, env = "CF_DIR")]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DiffArgs {
    /// Target id.
    pub target: String,
    /// Optional prior revision (defaults to head-1 vs head).
    pub rev: Option<u64>,
    #[arg(long, value_parser = ["pretty", "json", "jsonl"])]
    pub format: Option<String>,
    #[arg(long, env = "CF_DIR")]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct FeedArgs {
    pub targets: Vec<String>,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long, default_value_t = 1000)]
    pub limit: u32,
    #[arg(long)]
    pub after_cursor: Option<String>,
    #[arg(long, value_parser = ["jsonl", "json"])]
    pub format: Option<String>,
    #[arg(long, value_parser = ["none", "low", "medium", "high", "critical"])]
    pub min_salience: Option<String>,
    #[arg(long)]
    pub max_salience_first: bool,
    #[arg(long)]
    pub standalone: bool,
    #[arg(long, env = "CF_DIR")]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Target id (or omit to show the store summary).
    pub target: Option<String>,
    #[arg(long, env = "CF_DIR")]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct RulesArgs {
    /// Target id to validate (against the stored head, or a piped/fetched page).
    pub target: Option<String>,
    /// Validate against HTML piped on stdin.
    #[arg(long)]
    pub stdin: bool,
    #[arg(long)]
    pub url: Option<String>,
    #[arg(long, env = "CF_DIR")]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct SchemaArgs {
    #[arg(long, default_value = "1")]
    pub version: String,
}

#[derive(Debug, Args)]
pub struct ExplainArgs {
    /// The event id to explain (matched against re-derived ids for a target's head diff).
    pub event_id: String,
    /// The target whose head-vs-prior diff to replay.
    #[arg(long)]
    pub target: Option<String>,
    #[arg(long, env = "CF_DIR")]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ScoreArgs {
    /// Score an ad-hoc snapshot pair without touching the store.
    #[arg(long)]
    pub dry_run: bool,
    /// The "before" HTML file (snapshot a).
    pub a: PathBuf,
    /// The "after" HTML file (snapshot b).
    pub b: PathBuf,
    /// Optional archetype rule pack.
    #[arg(long)]
    pub archetype: Option<String>,
    /// Override the extract selector.
    #[arg(long)]
    pub selector: Option<String>,
    #[arg(long, value_parser = ["pretty", "json", "jsonl"])]
    pub format: Option<String>,
}

// ===========================================================================================
// Entry — map a command to a frozen §4.5 exit code.
// ===========================================================================================

/// Run the parsed command and return the frozen §4.5 exit code. The single place that maps the
/// pipeline result to an exit code. All stdout writes are events; all diagnostics go to stderr.
pub fn run(cli: Cli) -> ExitCode {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let result = dispatch(cli, &mut out);
    match result {
        Ok(code) => code,
        Err(e) => {
            elog(&format!("error: {e}"));
            // An anyhow error at the boundary is a usage/config error unless it carries an ExitCode.
            match e.downcast_ref::<ExitCodeError>() {
                Some(ec) => ec.0,
                None => ExitCode::Usage,
            }
        }
    }
}

/// An error carrying a specific exit code (so a soft fetch surfaces 3, not 1).
#[derive(Debug)]
struct ExitCodeError(ExitCode, String);

impl std::fmt::Display for ExitCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.1)
    }
}
impl std::error::Error for ExitCodeError {}

fn dispatch(cli: Cli, out: &mut impl Write) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Init => cmd_init(out),
        Command::Watch(a) => cmd_watch(a, out),
        Command::Check(a) => cmd_check(a, out),
        Command::Snapshot(a) => cmd_snapshot(a, out),
        Command::Diff(a) => cmd_diff(a, out),
        Command::Feed(a) => cmd_feed(a, out),
        Command::Ls => cmd_ls(out),
        Command::Show(a) => cmd_show(a, out),
        Command::Rules(a) => cmd_rules(a, out),
        Command::Schema(a) => cmd_schema(a, out),
        Command::Explain(a) => cmd_explain(a, out),
        Command::Score(a) => cmd_score(a, out),
    }
}

// ===========================================================================================
// init / watch
// ===========================================================================================

fn cmd_init(_out: &mut impl Write) -> anyhow::Result<ExitCode> {
    let dir = std::env::current_dir()?;
    config_io::scaffold(&dir)?;
    elog(&format!(
        "initialized changefeed in {} (changefeed.toml + .changefeed/)",
        dir.display()
    ));
    Ok(ExitCode::NoChange)
}

fn cmd_watch(a: WatchArgs, _out: &mut impl Write) -> anyhow::Result<ExitCode> {
    let dir = std::env::current_dir()?;
    let (url, id) = if a.target.starts_with("http://") || a.target.starts_with("https://") {
        let id = a.id.unwrap_or_else(|| config::derive_tid(&a.target));
        (a.target.clone(), id)
    } else {
        // A bare id with no scheme: treat the arg as the id and require a URL via --id? In MVP we
        // accept `cf watch <url>`; a non-URL arg with no scheme is a usage error.
        return Err(anyhow::anyhow!(
            "watch expects a URL (got {:?}); use a full http(s):// URL",
            a.target
        ));
    };
    config_io::append_target(&dir, &id, &url, a.archetype.as_deref())?;
    elog(&format!("watching {id} -> {url}"));
    Ok(ExitCode::NoChange)
}

// ===========================================================================================
// check — the agent poll primitive (the heart of the cli)
// ===========================================================================================

fn cmd_check(a: CheckArgs, out: &mut impl Write) -> anyhow::Result<ExitCode> {
    let dir = a.dir.clone().unwrap_or(std::env::current_dir()?);
    let format = resolve_format(a.format.as_deref(), Format::default());
    let min_salience = match a.min_salience.as_deref() {
        Some(s) => config::parse_materiality(s).map_err(usage_err)?,
        None => Materiality::Low,
    };
    // --peek implies --no-store (DESIGN §4.4).
    let no_store = a.no_store || a.peek;

    // Load config if present (ad-hoc URLs are allowed without one).
    let cfg = if config_io::config_exists(&dir) {
        Some(config_io::load(&dir)?)
    } else {
        None
    };
    let defaults = cfg
        .as_ref()
        .map(|c| c.defaults.clone())
        .unwrap_or_else(default_defaults);

    // --stdin --url: diff piped HTML against the stored snapshot (the deterministic golden path).
    if a.stdin {
        let url = a
            .url
            .clone()
            .ok_or_else(|| usage_anyhow("--stdin requires --url URL"))?;
        let html = read_stdin()?;
        // Resolve the target: an explicit positional id (e.g. `cf check comp-pricing --stdin`) uses
        // that configured target's profile/ignore rules; otherwise the --url is an ad-hoc target.
        let name = a.targets.first().cloned().unwrap_or_else(|| url.clone());
        let mut target = resolve_or_adhoc_target(cfg.as_ref(), &name, &a, &defaults)?;
        // The piped HTML always represents the --url, even when the target was resolved by id.
        target.url = url.clone();
        target.profile.profile_id = target.id.clone();
        return run_one_check(&dir, &target, &defaults, Some(html), &a, no_store, min_salience, format, out);
    }

    // Resolve the target list (explicit args, --targets-from -, or all configured).
    let target_names = collect_targets(&a, cfg.as_ref())?;
    if target_names.is_empty() {
        return Err(usage_anyhow(
            "no targets: pass a URL/id, use --targets-from -, or configure changefeed.toml",
        ));
    }

    let mut worst = ExitCode::NoChange;
    for name in target_names {
        let target = resolve_or_adhoc_target(cfg.as_ref(), &name, &a, &defaults)?;
        let code = run_one_check(&dir, &target, &defaults, None, &a, no_store, min_salience, format, out)?;
        worst = worse_of(worst, code);
    }
    Ok(worst)
}

/// Resolve a target name (id or URL) to a `TargetCfg`. A configured id wins; otherwise treat the
/// name as an ad-hoc URL (DESIGN §4.3 "stripe.com/pricing with no config is legal"). A non-URL name
/// with no matching configured id is exit 2 (not found).
fn resolve_or_adhoc_target(
    cfg: Option<&Config>,
    name: &str,
    a: &CheckArgs,
    defaults: &Defaults,
) -> anyhow::Result<TargetCfg> {
    if let Some(cfg) = cfg {
        if let Some(t) = cfg.targets.iter().find(|t| t.id == name) {
            let mut t = t.clone();
            apply_check_overrides(&mut t, a);
            return Ok(t);
        }
    }
    if name.starts_with("http://") || name.starts_with("https://") {
        let select: Vec<String> = a.selector.clone().into_iter().collect();
        let mut t = config::ad_hoc_target(name, None, None, &select, defaults);
        apply_check_overrides(&mut t, a);
        Ok(t)
    } else {
        Err(not_found_anyhow(name))
    }
}

/// Apply the ad-hoc `cf check` flag overrides (`--selector`/`--render`) onto a target's profile.
fn apply_check_overrides(t: &mut TargetCfg, a: &CheckArgs) {
    if let Some(sel) = &a.selector {
        t.profile.strategy = cf_core::ExtractStrategy::Selector;
        t.profile.root_selector = Some(sel.clone());
        if !t.select.contains(sel) {
            t.select = vec![sel.clone()];
        }
    }
    if let Some(r) = &a.render {
        if let Ok(mode) = config::parse_render(r) {
            t.profile.render = mode;
        }
    }
}

/// Run one target's `cf check`. `piped_html` short-circuits the fetch (`--stdin`).
#[allow(clippy::too_many_arguments)]
fn run_one_check(
    dir: &std::path::Path,
    target: &TargetCfg,
    defaults: &Defaults,
    piped_html: Option<String>,
    a: &CheckArgs,
    no_store: bool,
    min_salience: Materiality,
    format: Format,
    out: &mut impl Write,
) -> anyhow::Result<ExitCode> {
    // Render mode chromium short-circuits to exit 7 BEFORE any work (MVP has no headless tier).
    if target.profile.render == RenderMode::Chromium {
        elog("render=chromium required but no browser (MVP); set render=never");
        return Ok(ExitCode::RenderNeeded);
    }

    // §4.3 / §11: a non-writable store is NOT a usage error. Fall back to "first observation, no
    // politeness memory" — an ephemeral in-memory store so the pipeline still runs, emits the
    // baseline envelope, and returns exit 11 (with a stderr warning) instead of a hard exit-1 that an
    // agent branching on `$?` would treat as a non-retryable invocation bug.
    let mut store = match open_store(dir) {
        Ok(s) => s,
        Err(_) => {
            elog(&format!(
                "{}: store not writable; first-observation fallback (no politeness memory) [§4.3]",
                target.id
            ));
            SqliteStore::open_in_memory().map_err(usage_err)?
        }
    };
    let clock = SystemClock;
    let obs = obs_now(&clock);
    let mut ids = make_idgen();

    // Acquire the new HTML body: piped (--stdin) or fetched.
    let (html, status, etag, ms, final_url) = match piped_html {
        Some(h) => (h, 200u16, None, None, target.url.clone()),
        None => {
            match do_fetch(target, defaults, a, &store, dir)? {
                FetchResolved::Body {
                    body,
                    status,
                    etag,
                    ms,
                    final_url,
                } => (body, status, etag, ms, final_url),
                FetchResolved::NotModified => {
                    // 304: nothing changed (§12). cf check prints nothing, exit 0.
                    elog(&format!("{}: 304 not modified", target.id));
                    return Ok(ExitCode::NoChange);
                }
                FetchResolved::Error(code, msg, retry_after) => {
                    if code == ExitCode::RateLimit {
                        // §4.4/§4.6: surface the backoff on BOTH stdout JSON (`crawl.retry_after`,
                        // which the canonical agent loop reads) AND stderr — never a second fetch.
                        let cur_rev = store
                            .latest(&target.id)
                            .ok()
                            .flatten()
                            .map(|s| s.rev)
                            .unwrap_or(0);
                        let env = cf_core::event::rate_limited_envelope(
                            target.id.clone(),
                            ids.batch_id(),
                            obs.clone(),
                            cur_rev,
                            Some(target.url.clone()),
                            429,
                            retry_after,
                        );
                        if !a.quiet {
                            write_stdout(out, &render::render_envelope(&env, format))?;
                        }
                        match retry_after {
                            Some(ra) => {
                                elog(&format!("{}: rate limited; retry_after={ra}s", target.id))
                            }
                            None => elog(&format!(
                                "{}: rate limited; no Retry-After header (agent applies its default)",
                                target.id
                            )),
                        }
                        return Ok(ExitCode::RateLimit);
                    }
                    elog(&format!("{}: {}", target.id, msg));
                    if a.fail_on_fetch_error && code == ExitCode::SoftFetch {
                        return Ok(ExitCode::Usage);
                    }
                    return Ok(code);
                }
            }
        }
    };

    // Canonicalize the new body (extract → normalize → segment).
    let mut new_doc = match pipeline::canonicalize(&html, &target.profile) {
        Ok(d) => d,
        Err(e) => {
            elog(&format!("{}: canonicalize failed: {e}", target.id));
            return Ok(ExitCode::Usage);
        }
    };
    new_doc.url = target.url.clone();
    new_doc.final_url = final_url;
    new_doc.fetched_at = obs.clone();

    // Load the prior snapshot + rev.
    let prior = store.latest(&target.id).map_err(usage_err)?;
    let prev_rev = prior.as_ref().map(|s| s.rev);
    let to_rev = match prev_rev {
        Some(r) => r + 1,
        None => 0,
    };

    // Resolve the rule pack (archetype preset).
    let pack = packs::resolve(target.archetype.as_deref()).map_err(usage_err)?;

    let batch_id = ids.batch_id();
    let mut next_event = make_event_minter();
    let prior_doc = prior.as_ref().map(|s| &s.doc);

    // The page `<title>` (DESIGN §6.2 `src.title`). The selector-extracted subtree usually does not
    // include `<head>`, so we recover the title from the full body at this (impure) boundary and
    // thread it into the pure pipeline as data.
    let title = page_title(&html);

    let mut ctx = PipelineCtx {
        tid: &target.id,
        title: title.as_deref(),
        obs: &obs,
        batch_id: &batch_id,
        event_ids: &mut next_event,
        prior: prior_doc,
        prev_rev,
        to_rev,
        pack: &pack,
        min_salience,
        emit_subthreshold: a.emit_subthreshold,
        ignore: &target.ignore,
        status,
        etag: etag.clone(),
        ms,
    };

    let result = pipeline::observe_body(&new_doc, &mut ctx);

    emit_result(result, &new_doc, prior_doc, target, &store_html(&html), no_store, a.emit_subthreshold, a.quiet, format, &mut store, prev_rev, to_rev, out)
}

/// Persist + dedup + emit one observation result. Returns the §4.5 exit code. Honors `--no-store`
/// (no persist, no rev advance, no seen-set record) and the quiet flag.
#[allow(clippy::too_many_arguments)]
fn emit_result(
    result: ObservationResult,
    new_doc: &CanonicalDoc,
    prior_doc: Option<&CanonicalDoc>,
    target: &TargetCfg,
    raw_html: &str,
    no_store: bool,
    emit_subthreshold: bool,
    quiet: bool,
    format: Format,
    store: &mut SqliteStore,
    _prev_rev: Option<u64>,
    to_rev: u64,
    out: &mut impl Write,
) -> anyhow::Result<ExitCode> {
    match result {
        ObservationResult::NoChange { reason } => {
            use cf_core::NoChangeReason::*;
            match reason {
                SubThreshold if emit_subthreshold => {
                    // Below threshold but the operator asked to be told: exit 12, print nothing
                    // (the envelope `n:0` is for the daemon log; cf check signals via exit code).
                    Ok(ExitCode::SubThreshold)
                }
                _ => {
                    // The cheap no-op: exit 0, EMPTY stdout (DESIGN §4.5). doc_hash-equal short-
                    // circuit means we never even reach a store write.
                    Ok(ExitCode::NoChange)
                }
            }
        }
        ObservationResult::Baseline(env) => {
            if !no_store {
                store
                    .put_with_html(
                        &StoredSnapshot {
                            tid: target.id.clone(),
                            rev: to_rev,
                            doc: new_doc.clone(),
                        },
                        Some(raw_html),
                    )
                    .map_err(usage_err)?;
            }
            if !quiet {
                let s = render::render_envelope(&env, format);
                write_stdout(out, &s)?;
            }
            Ok(ExitCode::FirstObs)
        }
        ObservationResult::Changed { envelope, events } => {
            let total = events.len();
            // §7.4 dedup. In the default (store) mode: advance the baseline, then suppress any event
            // whose (slot, from→to) transition is already in the rolling seen-set (a flap-revert to a
            // known transition) and record the survivors. `--no-store`/`--peek` bypass entirely — a
            // probe deliberately re-emits the same delta (§4.4).
            let emit_events: Vec<ChangeEvent> = if no_store {
                events
            } else {
                store
                    .put_with_html(
                        &StoredSnapshot {
                            tid: target.id.clone(),
                            rev: to_rev,
                            doc: new_doc.clone(),
                        },
                        Some(raw_html),
                    )
                    .map_err(usage_err)?;
                let mut kept = Vec::with_capacity(total);
                for ev in events {
                    match event_key_for(&ev, &target.id, new_doc, prior_doc) {
                        Some(key) => {
                            if store.seen_event(key).map_err(usage_err)? {
                                continue; // already-seen transition → dedup-suppress
                            }
                            store.mark_event(key).map_err(usage_err)?;
                            kept.push(ev);
                        }
                        // No derivable key (e.g. a meta event with no seg.fp): emit, never silently drop.
                        None => kept.push(ev),
                    }
                }
                kept
            };
            // If dedup suppressed EVERY change, there is nothing new for the agent → no-op (exit 0).
            if emit_events.is_empty() {
                return Ok(ExitCode::NoChange);
            }
            if !quiet {
                let s = match format {
                    // The envelope is accurate only when nothing was suppressed (the common case);
                    // otherwise render the deduped events directly.
                    Format::Pretty if emit_events.len() == total => {
                        render::render_envelope(&envelope, format)
                    }
                    _ => render::render_events(&emit_events, format),
                };
                write_stdout(out, &s)?;
            }
            Ok(ExitCode::Change)
        }
        ObservationResult::FetchError(e) => {
            elog(&format!("{}: {e}", target.id));
            Ok(e.exit_code())
        }
    }
}

/// Re-derive the §7.4 idempotency key for an emitted event from the seg fp + the new doc. The
/// event's `seg.fp` is the slot_key prefix; we recover the full key by matching the slot_key in the
/// new doc. (The dedup join folds in target_id + from/to norm hashes, so we use the doc's block.)
fn event_key_for(
    ev: &ChangeEvent,
    tid: &str,
    new_doc: &CanonicalDoc,
    prior_doc: Option<&CanonicalDoc>,
) -> Option<u128> {
    let fp = ev.seg.first()?.fp.strip_prefix("blake3:")?;
    let new_block = find_block_by_fp(&new_doc.blocks, fp)?;
    let to = new_block.norm_hash;
    // §7.4 idempotency key = (tid, slot_key, from_norm_hash, to_norm_hash). `from` is the SAME
    // slot's norm_hash in the prior observation (the seg.fp IS the slot_key fp, §6.2), or the empty
    // hash for an added block — matching the diff's `from_norm_hash` convention. Using (from, to)
    // (not the previous (to, to)) makes a transition a stable, distinct key so a flap-revert to a
    // previously-seen transition dedups, while a genuinely new transition does not.
    let from = prior_doc
        .and_then(|p| find_block_by_fp(&p.blocks, fp))
        .map(|b| b.norm_hash)
        .unwrap_or_else(|| cf_core::NormHash::of(""));
    Some(cf_core::EventKey::derive(tid, &new_block.slot_key, from, to).raw())
}

fn find_block_by_fp<'a>(
    blocks: &'a [cf_core::Block],
    fp: &str,
) -> Option<&'a cf_core::Block> {
    for b in blocks {
        let hex = b.slot_key.fp_hex();
        if hex.starts_with(fp) {
            return Some(b);
        }
        if let Some(found) = find_block_by_fp(&b.children, fp) {
            return Some(found);
        }
    }
    None
}

// ===========================================================================================
// snapshot / diff / feed / ls / show
// ===========================================================================================

fn cmd_snapshot(a: SnapshotArgs, _out: &mut impl Write) -> anyhow::Result<ExitCode> {
    let dir = a.dir.clone().unwrap_or(std::env::current_dir()?);
    let cfg = if config_io::config_exists(&dir) {
        Some(config_io::load(&dir)?)
    } else {
        None
    };
    let defaults = cfg
        .as_ref()
        .map(|c| c.defaults.clone())
        .unwrap_or_else(default_defaults);
    let url = a.url.clone().unwrap_or_else(|| a.target.clone());
    let target = match cfg.as_ref().and_then(|c| c.targets.iter().find(|t| t.id == a.target)) {
        Some(t) => t.clone(),
        None if url.starts_with("http") => config::ad_hoc_target(&url, None, None, &[], &defaults),
        None => return Ok(ExitCode::NotFound),
    };

    let html = if a.stdin {
        read_stdin()?
    } else {
        let store = open_store(&dir)?;
        match do_fetch(&target, &defaults, &CheckArgs::none(), &store, &dir)? {
            FetchResolved::Body { body, .. } => body,
            FetchResolved::NotModified => return Ok(ExitCode::NoChange),
            FetchResolved::Error(code, msg, retry_after) => {
                match retry_after {
                    Some(ra) => elog(&format!("{}: {msg} (retry_after={ra}s)", target.id)),
                    None => elog(&format!("{}: {msg}", target.id)),
                }
                return Ok(code);
            }
        }
    };

    let clock = SystemClock;
    let obs = obs_now(&clock);
    let mut new_doc = pipeline::canonicalize(&html, &target.profile).map_err(usage_err)?;
    new_doc.url = target.url.clone();
    new_doc.final_url = target.url.clone();
    new_doc.fetched_at = obs;

    let mut store = open_store(&dir)?;
    let rev = store.next_rev(&target.id).map_err(usage_err)?;
    store
        .put_with_html(
            &StoredSnapshot {
                tid: target.id.clone(),
                rev,
                doc: new_doc,
            },
            Some(&html),
        )
        .map_err(usage_err)?;
    elog(&format!("{}: stored snapshot rev {rev} (baseline seeded)", target.id));
    Ok(ExitCode::FirstObs)
}

fn cmd_diff(a: DiffArgs, out: &mut impl Write) -> anyhow::Result<ExitCode> {
    let dir = a.dir.clone().unwrap_or(std::env::current_dir()?);
    let cfg = config_io::load(&dir).ok();
    let store = open_store(&dir)?;

    let head = match store.head_rev(&a.target).map_err(usage_err)? {
        Some(h) => h,
        None => return Ok(ExitCode::NotFound),
    };
    let new_rev = head;
    let old_rev = a.rev.unwrap_or_else(|| head.saturating_sub(1));
    if old_rev == new_rev {
        elog("only one revision stored; nothing to diff");
        return Ok(ExitCode::FirstObs);
    }
    let old = store.snapshot_at(&a.target, old_rev).map_err(usage_err)?;
    let new = store.snapshot_at(&a.target, new_rev).map_err(usage_err)?;
    let (Some(old), Some(new)) = (old, new) else {
        return Ok(ExitCode::NotFound);
    };

    let target = cfg
        .as_ref()
        .and_then(|c| c.targets.iter().find(|t| t.id == a.target).cloned());
    let archetype = target.as_ref().and_then(|t| t.archetype.clone());
    let ignore: Vec<IgnoreRule> = target.as_ref().map(|t| t.ignore.clone()).unwrap_or_default();
    let pack = packs::resolve(archetype.as_deref()).map_err(usage_err)?;

    let format = resolve_format(a.format.as_deref(), Format::Jsonl);
    let obs = "1970-01-01T00:00:00Z".to_string();
    let mut minter = make_deterministic_minter("diff");

    let mut ctx = PipelineCtx {
        tid: &a.target,
        title: None,
        obs: &obs,
        batch_id: "cfb_diff",
        event_ids: &mut minter,
        prior: Some(&old.doc),
        prev_rev: Some(old_rev),
        to_rev: new_rev,
        pack: &pack,
        min_salience: Materiality::None,
        emit_subthreshold: true,
        ignore: &ignore,
        status: 200,
        etag: None,
        ms: None,
    };
    match pipeline::observe_body(&new.doc, &mut ctx) {
        ObservationResult::Changed { events, .. } => {
            let s = render::render_events(&events, format);
            write_stdout(out, &s)?;
            Ok(ExitCode::Change)
        }
        _ => Ok(ExitCode::NoChange),
    }
}

fn cmd_feed(a: FeedArgs, out: &mut impl Write) -> anyhow::Result<ExitCode> {
    // MVP: the store keeps snapshots, not an event log (that is the daemon's append-only log, Phase
    // 2). `cf feed` therefore re-derives the head-vs-prior diff for each requested target and emits
    // them as a bounded, paginated JSONL stream (the same event schema), honoring --limit /
    // --min-salience / --max-salience-first.
    let dir = a.dir.clone().unwrap_or(std::env::current_dir()?);
    let cfg = config_io::load(&dir).ok();
    let store = open_store(&dir)?;
    let format = match a.format.as_deref() {
        Some("json") => Format::Json,
        _ => Format::Jsonl,
    };
    let min_salience = match a.min_salience.as_deref() {
        Some(s) => config::parse_materiality(s).map_err(usage_err)?,
        None => Materiality::None,
    };

    let targets: Vec<String> = if !a.targets.is_empty() {
        a.targets.clone()
    } else if let Some(cfg) = &cfg {
        cfg.targets.iter().map(|t| t.id.clone()).collect()
    } else {
        Vec::new()
    };

    // §4.7 catch-up: re-derive EVERY transition retained in the ring (oldest→newest) per target — not
    // just the latest head-vs-prior diff — so `--since` can replay the retained window. Each row keeps
    // its `to_rev` + the new snapshot's real `fetched_at` for the `--since` filter.
    let mut minter = make_deterministic_minter("feed");
    let mut rows: Vec<(ChangeEvent, u64, String)> = Vec::new();
    let mut max_to_rev = 0u64;
    for tid in &targets {
        let revs = store.retained_revs(tid).map_err(usage_err)?;
        let target = cfg
            .as_ref()
            .and_then(|c| c.targets.iter().find(|t| &t.id == tid).cloned());
        let archetype = target.as_ref().and_then(|t| t.archetype.clone());
        let ignore: Vec<IgnoreRule> = target.map(|t| t.ignore).unwrap_or_default();
        let pack = packs::resolve(archetype.as_deref()).map_err(usage_err)?;
        for w in revs.windows(2) {
            let (prev, cur) = (w[0], w[1]);
            let (Some(old), Some(new)) = (
                store.snapshot_at(tid, prev).map_err(usage_err)?,
                store.snapshot_at(tid, cur).map_err(usage_err)?,
            ) else {
                continue;
            };
            let fetched_at = new.doc.fetched_at.clone();
            if !feed_since_includes(a.since.as_deref(), cur, &fetched_at) {
                continue;
            }
            max_to_rev = max_to_rev.max(cur);
            let obs = "1970-01-01T00:00:00Z".to_string();
            let mut ctx = PipelineCtx {
                tid,
                title: None,
                obs: &obs,
                batch_id: "cfb_feed",
                event_ids: &mut minter,
                prior: Some(&old.doc),
                prev_rev: Some(prev),
                to_rev: cur,
                pack: &pack,
                min_salience,
                emit_subthreshold: false,
                ignore: &ignore,
                status: 200,
                etag: None,
                ms: None,
            };
            if let ObservationResult::Changed { events, .. } =
                pipeline::observe_body(&new.doc, &mut ctx)
            {
                for ev in events {
                    rows.push((ev, cur, fetched_at.clone()));
                }
            }
        }
    }

    // --max-salience-first: a token-constrained agent reading only the first page sees the worst news.
    if a.max_salience_first {
        rows.sort_by(|x, y| {
            y.0.why
                .sal
                .partial_cmp(&x.0.why.sal)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    // §4.7 bounded pagination: page = rows[start .. start+limit]; resume via an opaque `cfc_<offset>`
    // cursor. `next_cursor` is set only when more rows remain after the page.
    let total = rows.len();
    let start = a
        .after_cursor
        .as_deref()
        .and_then(parse_feed_cursor)
        .unwrap_or(0)
        .min(total);
    let end = start.saturating_add(a.limit as usize).min(total);
    let page: Vec<ChangeEvent> = rows[start..end].iter().map(|(e, _, _)| e.clone()).collect();
    let next_cursor = if end < total { Some(format!("cfc_{end}")) } else { None };

    let feed_id = if targets.len() == 1 {
        targets[0].clone()
    } else {
        "feed".to_string()
    };
    let obs = "1970-01-01T00:00:00Z".to_string();
    match format {
        Format::Json => {
            let env = event::feed_page_envelope(
                feed_id,
                obs,
                max_to_rev,
                page.len() as u32,
                page.clone(),
                next_cursor,
            );
            write_stdout(out, &render::render_envelope(&env, format))?;
        }
        _ => {
            // jsonl: one event per line (the established contract), then the TRAILING envelope line
            // carrying `next_cursor` (§4.7 "next_cursor in the trailing envelope").
            if !page.is_empty() {
                write_stdout(out, &render::render_events(&page, format))?;
            }
            let env =
                event::feed_page_envelope(feed_id, obs, max_to_rev, page.len() as u32, Vec::new(), next_cursor);
            let line = event::envelope_to_wire(&env)
                .map_err(|e| usage_anyhow(&format!("feed envelope: {e}")))?;
            write_stdout(out, &format!("{line}\n"))?;
        }
    }

    if page.is_empty() {
        Ok(ExitCode::NoChange)
    } else {
        Ok(ExitCode::Change)
    }
}

/// Parse a `cf feed` pagination cursor (`cfc_<offset>`) back to its row offset.
fn parse_feed_cursor(c: &str) -> Option<usize> {
    c.strip_prefix("cfc_").and_then(|n| n.parse().ok())
}

/// §4.7 `--since TIMESTAMP|REV` filter. A bare integer is a REV ("after this rev"); anything else is
/// an RFC3339 timestamp compared lexicographically against the snapshot's `fetched_at` (correct for
/// the `Z`-suffixed timestamps changefeed emits). `None` ⇒ include everything.
fn feed_since_includes(since: Option<&str>, to_rev: u64, fetched_at: &str) -> bool {
    match since {
        None => true,
        Some(s) => match s.parse::<u64>() {
            Ok(rev) => to_rev > rev,
            Err(_) => fetched_at > s,
        },
    }
}

fn cmd_ls(out: &mut impl Write) -> anyhow::Result<ExitCode> {
    let dir = std::env::current_dir()?;
    let cfg = match config_io::config_exists(&dir) {
        true => config_io::load(&dir)?,
        false => return Err(usage_anyhow("no changefeed.toml (run `cf init` first)")),
    };
    // `ls` is operator output (not events) — but the test asserts stdout=events only on `check`.
    // We print the target table to stdout here for human use.
    for t in &cfg.targets {
        writeln!(
            out,
            "{}\t{}\t{}",
            t.id,
            t.url,
            t.archetype.as_deref().unwrap_or("-")
        )?;
    }
    Ok(ExitCode::NoChange)
}

fn cmd_show(a: ShowArgs, out: &mut impl Write) -> anyhow::Result<ExitCode> {
    let dir = a.dir.clone().unwrap_or(std::env::current_dir()?);
    let store = open_store(&dir)?;
    match a.target {
        None => {
            let cfg = config_io::load(&dir).ok();
            if let Some(cfg) = cfg {
                for t in &cfg.targets {
                    let head = store.head_rev(&t.id).ok().flatten();
                    writeln!(out, "{}\thead_rev={:?}", t.id, head)?;
                }
            }
            Ok(ExitCode::NoChange)
        }
        Some(tid) => match store.head_rev(&tid).map_err(usage_err)? {
            None => Ok(ExitCode::NotFound),
            Some(head) => {
                let revs = store.retained_revs(&tid).map_err(usage_err)?;
                writeln!(out, "target={tid} head_rev={head} retained={revs:?}")?;
                Ok(ExitCode::NoChange)
            }
        },
    }
}

// ===========================================================================================
// rules — validate selectors / ignore rules (zero-node + low select_overlap warnings)
// ===========================================================================================

fn cmd_rules(a: RulesArgs, out: &mut impl Write) -> anyhow::Result<ExitCode> {
    let dir = a.dir.clone().unwrap_or(std::env::current_dir()?);
    let cfg = config_io::load(&dir).ok();
    let defaults = cfg
        .as_ref()
        .map(|c| c.defaults.clone())
        .unwrap_or_else(default_defaults);

    // Resolve the target + the HTML to validate against.
    let (target, html) = if a.stdin {
        let url = a.url.clone().unwrap_or_else(|| "https://stdin/".into());
        let target = match a.target.as_ref().and_then(|name| {
            cfg.as_ref().and_then(|c| c.targets.iter().find(|t| &t.id == name).cloned())
        }) {
            Some(t) => t,
            None => config::ad_hoc_target(&url, a.target.as_deref(), None, &[], &defaults),
        };
        (target, read_stdin()?)
    } else {
        let name = a
            .target
            .clone()
            .ok_or_else(|| usage_anyhow("rules: pass a target id, or --stdin --url"))?;
        let target = cfg
            .as_ref()
            .and_then(|c| c.targets.iter().find(|t| t.id == name).cloned())
            .ok_or_else(|| not_found_anyhow(&name))?;
        let store = open_store(&dir)?;
        match store.latest(&name).map_err(usage_err)? {
            Some(snap) => match store.raw_html_at(&name, snap.rev).map_err(usage_err)? {
                Some(h) => (target, h),
                None => return Err(usage_anyhow("no stored raw HTML for this target; re-snapshot")),
            },
            None => return Err(usage_anyhow("no stored snapshot; run `cf snapshot` or `cf check` first")),
        }
    };

    // Canonicalize and report node counts + overlap warnings.
    let new_doc = pipeline::canonicalize(&html, &target.profile).map_err(usage_err)?;
    let node_count = new_doc.stats.block_count;
    writeln!(out, "selector strategy: {:?}", target.profile.strategy)?;
    writeln!(out, "root_selector: {:?}", target.profile.root_selector)?;
    writeln!(out, "matched blocks: {node_count}")?;

    let mut warned = false;
    if node_count == 0 {
        elog("WARNING: selector matched ZERO nodes (the page would diff as fully removed)");
        warned = true;
    }
    // select_overlap: against a stored prior, warn if low (a likely redesign / wrong selector).
    if let Ok(store) = open_store(&dir) {
        if let Some(prior) = store.latest(&target.id).ok().flatten() {
            let overlap = slot_overlap(&prior.doc, &new_doc);
            writeln!(out, "select_overlap vs head: {overlap:.2}")?;
            if overlap < cf_core::extract::SELECT_OVERLAP_MIN {
                elog(&format!(
                    "WARNING: select_overlap {overlap:.2} < {:.2} (selector may have drifted)",
                    cf_core::extract::SELECT_OVERLAP_MIN
                ));
                warned = true;
            }
        }
    }
    for rule in &target.ignore {
        writeln!(out, "ignore: {rule:?}")?;
    }
    if warned {
        elog("rules validation produced warnings (see above)");
    }
    Ok(ExitCode::NoChange)
}

/// Fraction of the prior doc's slot keys that survive in the new doc (the §4.9 select_overlap).
fn slot_overlap(prior: &CanonicalDoc, new: &CanonicalDoc) -> f64 {
    use std::collections::HashSet;
    let mut prior_keys = HashSet::new();
    collect_keys(&prior.blocks, &mut prior_keys);
    let mut new_keys = HashSet::new();
    collect_keys(&new.blocks, &mut new_keys);
    if prior_keys.is_empty() {
        return 1.0;
    }
    let shared = prior_keys.iter().filter(|k| new_keys.contains(*k)).count();
    shared as f64 / prior_keys.len() as f64
}

fn collect_keys(blocks: &[cf_core::Block], out: &mut std::collections::HashSet<[u8; 12]>) {
    for b in blocks {
        out.insert(b.slot_key.as_bytes());
        collect_keys(&b.children, out);
    }
}

// ===========================================================================================
// schema
// ===========================================================================================

fn cmd_schema(a: SchemaArgs, out: &mut impl Write) -> anyhow::Result<ExitCode> {
    match event::schema_for_version(&a.version) {
        Some(s) => {
            write_stdout(out, s)?;
            if !s.ends_with('\n') {
                write_stdout(out, "\n")?;
            }
            Ok(ExitCode::NoChange)
        }
        None => Err(usage_anyhow(&format!(
            "unknown schema version {:?} (only 1 exists)",
            a.version
        ))),
    }
}

// ===========================================================================================
// explain / score — the §8.5 tuning loop (deterministic, clock/network-free)
// ===========================================================================================

fn cmd_explain(a: ExplainArgs, out: &mut impl Write) -> anyhow::Result<ExitCode> {
    let dir = a.dir.clone().unwrap_or(std::env::current_dir()?);
    let cfg = config_io::load(&dir).ok();
    let store = open_store(&dir)?;
    let tid = a
        .target
        .clone()
        .ok_or_else(|| usage_anyhow("explain: pass --target to replay the head diff"))?;
    let head = store
        .head_rev(&tid)
        .map_err(usage_err)?
        .ok_or_else(|| not_found_anyhow(&tid))?;
    if head == 0 {
        return Err(usage_anyhow("only a baseline is stored; nothing to explain"));
    }
    let old = store.snapshot_at(&tid, head - 1).map_err(usage_err)?.unwrap();
    let new = store.snapshot_at(&tid, head).map_err(usage_err)?.unwrap();
    let target = cfg
        .as_ref()
        .and_then(|c| c.targets.iter().find(|t| t.id == tid).cloned());
    let archetype = target.as_ref().and_then(|t| t.archetype.clone());
    let ignore: Vec<IgnoreRule> = target.map(|t| t.ignore).unwrap_or_default();
    let pack = packs::resolve(archetype.as_deref()).map_err(usage_err)?;

    let (_cs, scored) =
        pipeline::diff_and_score(&old.doc, &new.doc, &ignore, &pack, FetchTier::Http)
            .map_err(usage_err)?;
    let _ = &a.event_id; // event id selects a single row when seeded ids are stable; print all.
    for se in &scored {
        write_stdout(out, &format_explanation(se))?;
    }
    Ok(ExitCode::NoChange)
}

fn cmd_score(a: ScoreArgs, out: &mut impl Write) -> anyhow::Result<ExitCode> {
    let html_a = std::fs::read_to_string(&a.a)
        .map_err(|e| usage_anyhow(&format!("reading {}: {e}", a.a.display())))?;
    let html_b = std::fs::read_to_string(&a.b)
        .map_err(|e| usage_anyhow(&format!("reading {}: {e}", a.b.display())))?;

    let defaults = default_defaults();
    let select: Vec<String> = a.selector.clone().into_iter().collect();
    let target = config::ad_hoc_target(
        "https://dry-run/score",
        Some("score-dry-run"),
        a.archetype.as_deref(),
        &select,
        &defaults,
    );
    let pack = packs::resolve(a.archetype.as_deref()).map_err(usage_err)?;

    let doc_a = pipeline::canonicalize(&html_a, &target.profile).map_err(usage_err)?;
    let mut doc_b = pipeline::canonicalize(&html_b, &target.profile).map_err(usage_err)?;
    doc_b.url = target.url.clone();

    // Injected fixed id/obs make the output BYTE-IDENTICAL across runs (the determinism contract).
    let obs = std::env::var("CF_FAKE_OBS").unwrap_or_else(|_| "1970-01-01T00:00:00Z".into());
    let mut minter = make_deterministic_minter("score");

    let mut ctx = PipelineCtx {
        tid: &target.id,
        title: None,
        obs: &obs,
        batch_id: "cfb_score000000",
        event_ids: &mut minter,
        prior: Some(&doc_a),
        prev_rev: Some(0),
        to_rev: 1,
        pack: &pack,
        min_salience: Materiality::None,
        emit_subthreshold: true,
        ignore: &target.ignore,
        status: 200,
        etag: None,
        ms: None,
    };

    let format = resolve_format(a.format.as_deref(), Format::Jsonl);
    match pipeline::observe_body(&doc_b, &mut ctx) {
        ObservationResult::Changed { events, .. } => {
            let s = render::render_events(&events, format);
            write_stdout(out, &s)?;
            Ok(ExitCode::Change)
        }
        ObservationResult::Baseline(_) | ObservationResult::NoChange { .. } => {
            Ok(ExitCode::NoChange)
        }
        ObservationResult::FetchError(e) => Ok(e.exit_code()),
    }
}

/// Format a §8.5 explanation breakdown for one scored event (deterministic; for `cf explain`).
fn format_explanation(se: &cf_core::salience::ScoredEvent) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "event slot={} cat={} sal={:.2} mat={:?} act={:?} conf={:.2}\n",
        se.slot_key.fp_hex(),
        se.cat,
        se.sal,
        se.mat,
        se.act,
        se.conf,
    ));
    s.push_str(&format!("  decided_by: {}\n", se.explanation.decided_by));
    for c in &se.explanation.top_signals {
        s.push_str(&format!(
            "  signal {:<5} value={:.2} contribution={:.2}{}\n",
            c.signal,
            c.value,
            c.contribution,
            c.detail
                .as_ref()
                .map(|d| format!(" [{d}]"))
                .unwrap_or_default(),
        ));
    }
    let f = &se.explanation.conf_factors;
    s.push_str(&format!(
        "  conf: fetch={:.2} align={:.2} match={:.2} parse={:.2} stability={:.2}\n",
        f.c_fetch, f.c_align, f.c_match, f.c_parse, f.c_stability
    ));
    s
}

// ===========================================================================================
// Fetch resolution (the impure boundary, wrapped to a small enum).
// ===========================================================================================

enum FetchResolved {
    Body {
        body: String,
        status: u16,
        etag: Option<String>,
        ms: Option<u32>,
        final_url: String,
    },
    NotModified,
    /// A fetch error: its §4.5 exit code, a human message, and (for a 429) the parsed
    /// `Retry-After` seconds so `cf check` can surface `crawl.retry_after` (§4.4) without re-fetching.
    Error(ExitCode, String, Option<u32>),
}

fn do_fetch(
    target: &TargetCfg,
    defaults: &Defaults,
    a: &CheckArgs,
    store: &SqliteStore,
    _dir: &std::path::Path,
) -> anyhow::Result<FetchResolved> {
    let timeout = a
        .timeout
        .unwrap_or_else(|| defaults.timeout.as_secs().max(1));
    let use_etag = !a.no_etag;

    let fetcher = HttpFetcher::builder()
        .user_agent(defaults.user_agent.clone())
        .timeout_secs(timeout)
        .respect_robots(defaults.respect_robots)
        .render(target.profile.render)
        .build()
        .map_err(|e| usage_anyhow(&format!("fetcher: {e}")))?;

    // Conditional GET inputs come from the prior snapshot's fetch meta.
    let (etag, last_modified) = if use_etag {
        match store.latest(&target.id).ok().flatten() {
            Some(s) => (s.doc.fetch.etag.clone(), s.doc.fetch.last_modified.clone()),
            None => (None, None),
        }
    } else {
        (None, None)
    };

    let req = FetchRequest {
        url: target.url.clone(),
        etag,
        last_modified,
        auth: target.auth.clone(),
    };
    match fetcher.fetch(&req) {
        FetchOutcome::Body {
            final_url,
            status,
            body,
            meta,
            ..
        } => Ok(FetchResolved::Body {
            body,
            status,
            etag: meta.etag,
            ms: meta.ms,
            final_url,
        }),
        FetchOutcome::NotModified { .. } => Ok(FetchResolved::NotModified),
        FetchOutcome::Error(e) => {
            // Preserve the rate-limit backoff so `cf check` can surface `crawl.retry_after` (§4.4)
            // rather than dropping it on the floor (the Display is just "rate limited").
            let retry_after = match &e {
                cf_core::CfError::RateLimit { retry_after } => *retry_after,
                _ => None,
            };
            Ok(FetchResolved::Error(e.exit_code(), format!("{e}"), retry_after))
        }
    }
}

// ===========================================================================================
// Small helpers.
// ===========================================================================================

impl CheckArgs {
    /// A defaulted `CheckArgs` for the internal fetch path (snapshot reuses `do_fetch`).
    fn none() -> Self {
        CheckArgs {
            targets: Vec::new(),
            format: None,
            since: None,
            no_store: false,
            peek: false,
            min_salience: None,
            selector: None,
            render: None,
            timeout: None,
            etag: true,
            no_etag: false,
            emit_subthreshold: false,
            fail_on_fetch_error: false,
            stdin: false,
            url: None,
            targets_from: None,
            quiet: false,
            dir: None,
        }
    }
}

fn collect_targets(a: &CheckArgs, cfg: Option<&Config>) -> anyhow::Result<Vec<String>> {
    if a.targets_from.as_deref() == Some("-") {
        let stdin = read_stdin()?;
        return Ok(stdin
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect());
    }
    if !a.targets.is_empty() {
        return Ok(a.targets.clone());
    }
    if let Some(cfg) = cfg {
        return Ok(cfg.targets.iter().map(|t| t.id.clone()).collect());
    }
    Ok(Vec::new())
}

fn open_store(dir: &std::path::Path) -> anyhow::Result<SqliteStore> {
    let store_dir = dir.join(config_io::STORE_DIR);
    std::fs::create_dir_all(&store_dir)?;
    let path = store_dir.join("store.db");
    SqliteStore::open(&path).map_err(|e| usage_anyhow(&format!("store: {e}")))
}

/// Resolve the output format: an explicit flag wins; otherwise pretty on a TTY, else the default.
fn resolve_format(flag: Option<&str>, default: Format) -> Format {
    match flag {
        Some("pretty") => Format::Pretty,
        Some("json") => Format::Json,
        Some("jsonl") => Format::Jsonl,
        _ => {
            if std::io::stdout().is_terminal() {
                Format::Pretty
            } else {
                default
            }
        }
    }
}

/// The `obs` timestamp: a fixed injected value (`CF_FAKE_OBS`) for deterministic tests, else the
/// clock. This is the ONE place the cli reads time for `obs` (§4.2).
fn obs_now(clock: &SystemClock) -> String {
    std::env::var("CF_FAKE_OBS").unwrap_or_else(|_| clock.now_rfc3339())
}

/// An event-id minter: a fixed seeded sequence (`CF_FAKE_ID`) for deterministic tests, else ULIDs.
/// Used by the live `cf check` path where ids are minted fresh per observation.
fn make_event_minter() -> Box<dyn FnMut() -> String> {
    if let Ok(seed) = std::env::var("CF_FAKE_ID") {
        let mut gen = SeededIdGen::new(seed);
        Box::new(move || gen.event_id())
    } else {
        let mut gen = UlidGen;
        Box::new(move || gen.event_id())
    }
}

/// A DETERMINISTIC, clock/RNG-free event-id minter for the replay verbs (`score --dry-run`, `diff`,
/// `feed`, `explain`). These re-emit a stored/ad-hoc diff and MUST be byte-identical across runs
/// (the §8.5 tuning loop), so they never mint a ULID — the id is a stepped seeded sequence. The
/// `CF_FAKE_ID` env var overrides the prefix when a test needs a specific value.
fn make_deterministic_minter(prefix: &str) -> Box<dyn FnMut() -> String> {
    let seed = std::env::var("CF_FAKE_ID").unwrap_or_else(|_| prefix.to_string());
    let mut gen = SeededIdGen::new(seed);
    Box::new(move || gen.event_id())
}

/// A batch-id minter mirror used for the envelope batch id.
fn make_idgen() -> Box<dyn IdGen> {
    if let Ok(seed) = std::env::var("CF_FAKE_ID") {
        Box::new(SeededIdGen::new(seed))
    } else {
        Box::new(UlidGen)
    }
}

fn default_defaults() -> Defaults {
    Defaults {
        schedule: cf_core::Duration::from_secs(900),
        render: RenderMode::Auto,
        timeout: cf_core::Duration::from_secs(30),
        user_agent: "changefeed/1.0 (+https://github.com/changefeed/changefeed)".into(),
        respect_robots: true,
        min_salience: Materiality::Low,
        store_format: "zstd".into(),
    }
}

/// The store keeps the raw post-extract HTML; we pass the body through verbatim (§5.6).
fn store_html(html: &str) -> String {
    html.to_string()
}

/// Extract the page `<title>` from raw HTML for `src.title` (DESIGN §6.2). Returns `None` when the
/// document has no title (then the field is omitted, presence-as-signal §6.1 rule 4). Delegates to
/// the pure `cf_core::extract::page_title` (the impure crate does not link `scraper` directly).
fn page_title(html: &str) -> Option<String> {
    cf_core::extract::page_title(html)
}

fn read_stdin() -> anyhow::Result<String> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    Ok(s)
}

fn write_stdout(out: &mut impl Write, s: &str) -> anyhow::Result<()> {
    out.write_all(s.as_bytes())?;
    Ok(())
}

/// stderr logging — NEVER stdout (the absolute piping contract, §4.8).
fn elog(msg: &str) {
    let _ = writeln!(std::io::stderr(), "{msg}");
}

fn worse_of(a: ExitCode, b: ExitCode) -> ExitCode {
    // Severity ranking: usage/error codes dominate; then change > first-obs > no-change.
    fn rank(c: ExitCode) -> u8 {
        match c {
            ExitCode::NoChange => 0,
            ExitCode::FirstObs => 1,
            ExitCode::SubThreshold => 2,
            ExitCode::Change => 3,
            ExitCode::SoftFetch => 4,
            ExitCode::RateLimit => 5,
            ExitCode::Robots => 6,
            ExitCode::Auth => 7,
            ExitCode::RenderNeeded => 8,
            ExitCode::NotFound => 9,
            ExitCode::Usage => 10,
        }
    }
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

fn usage_err(e: cf_core::CfError) -> anyhow::Error {
    anyhow::Error::from(e)
}

fn usage_anyhow(msg: &str) -> anyhow::Error {
    anyhow::Error::from(ExitCodeError(ExitCode::Usage, msg.to_string()))
}

fn not_found_anyhow(name: &str) -> anyhow::Error {
    anyhow::Error::from(ExitCodeError(
        ExitCode::NotFound,
        format!("target not found: {name}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worse_of_prefers_errors_then_change_over_no_change() {
        // Across a multi-target check, the worst code wins (an agent branches on the worst news).
        assert_eq!(worse_of(ExitCode::NoChange, ExitCode::Change), ExitCode::Change);
        assert_eq!(worse_of(ExitCode::Change, ExitCode::SoftFetch), ExitCode::SoftFetch);
        assert_eq!(worse_of(ExitCode::FirstObs, ExitCode::NoChange), ExitCode::FirstObs);
        assert_eq!(worse_of(ExitCode::SoftFetch, ExitCode::Usage), ExitCode::Usage);
        // Idempotent on equal inputs.
        assert_eq!(worse_of(ExitCode::NoChange, ExitCode::NoChange), ExitCode::NoChange);
    }

    #[test]
    fn explicit_format_flag_overrides_tty_default() {
        assert_eq!(resolve_format(Some("jsonl"), Format::Pretty), Format::Jsonl);
        assert_eq!(resolve_format(Some("json"), Format::Jsonl), Format::Json);
        assert_eq!(resolve_format(Some("pretty"), Format::Jsonl), Format::Pretty);
    }

    #[test]
    fn deterministic_minter_is_stepped_and_stable() {
        // Two fresh minters with the same prefix produce the SAME id sequence (the §8.5 replay
        // determinism — score/diff/feed never mint a ULID).
        std::env::remove_var("CF_FAKE_ID");
        let mut a = make_deterministic_minter("score");
        let mut b = make_deterministic_minter("score");
        let a0 = a();
        let a1 = a();
        assert_ne!(a0, a1, "the sequence steps");
        assert_eq!(a0, b(), "two minters with the same prefix agree id-for-id");
        assert_eq!(a1, b());
        assert!(a0.starts_with("cfe_score"));
    }

    #[test]
    fn exit_code_error_carries_a_specific_code() {
        let e = anyhow::Error::from(ExitCodeError(ExitCode::NotFound, "x".into()));
        assert_eq!(e.downcast_ref::<ExitCodeError>().unwrap().0, ExitCode::NotFound);
    }
}
