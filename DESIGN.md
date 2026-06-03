# changefeed

**Status:** Final  ·  **Owner:** Nathan Englert  ·  **Date:** 2026-06-02

> **TL;DR.** `changefeed` (binary `cf`) turns noisy, frequently-changing web pages into structured **change events** for AI agents. For each observation of a watched URL it answers three questions in one compact payload: **(1) the delta** — what actually changed, computed by a deterministic, LLM-free diff over a normalized block-tree; **(2) why it matters** — a salience score + materiality label + change category; **(3) the suggested follow-up** — an action *hint*, never a command. A full single-change event is **~790 bytes / ~305 tokens** (measured, §6.1) — versus re-reading a ~9k-token page. The dominant case (poll → nothing material changed) costs the agent **one integer compare and zero tokens**, so its model never wakes for the ~95% of observations that are noise. Storage tracks **change size, not page size**: in the steady state a watched page costs **~60–120 bytes per *changed* observation** and **zero bytes per no-change poll**, via content-addressed dedup + zstd delta-chaining.

---

## Table of Contents

1. [Problem & Goals](#1-problem--goals)
2. [Design Principles](#2-design-principles)
3. [System Overview](#3-system-overview)
4. [CLI Surface & Agent Integration](#4-cli-surface--agent-integration)
5. [Extraction & Normalization](#5-extraction--normalization)
6. [Change Event Schema](#6-change-event-schema)
7. [Diff & Noise Suppression](#7-diff--noise-suppression)
8. [Salience & Follow-up](#8-salience--follow-up)
9. [Caching & Storage](#9-caching--storage)
10. [End-to-End Worked Example](#10-end-to-end-worked-example)
11. [Failure Modes & Edge Cases](#11-failure-modes--edge-cases)
12. [Security & Politeness](#12-security--politeness)
13. [Roadmap & Phasing](#13-roadmap--phasing)
14. [Open Questions](#14-open-questions)
15. [Appendix](#15-appendix)

---

## 1. Problem & Goals

### 1.1 The problem

AI agents increasingly need to *watch* the web, not just read it once. A research agent tracks a competitor's pricing page. A coding agent watches an API reference for breaking changes. A compliance agent watches a regulatory filings index. A sales agent watches job postings for hiring signals. In every case the agent does not care about the page — it cares about **what changed since last time**.

The default pattern for "did this page change?" is brutally wasteful. The agent re-fetches the page, pulls the previous version (or a summary) into context, and asks the model to diff them in-context and decide what matters. This breaks in four expensive ways:

1. **Token / cost waste.** A cleaned-up pricing or docs page is ~6k–12k tokens of readable text; the raw HTML is 50k–120k tokens of markup, inline CSS, SVG paths, and analytics. Diffing in-context means paying for *two* full copies plus reasoning overhead — on *every poll*, even when nothing changed.
2. **Latency.** Stuffing 20k–120k tokens into the prompt and asking for a careful diff adds seconds of time-to-first-token and seconds of generation, per page, per poll.
3. **Correctness.** LLMs are unreliable diffing engines on long inputs. A price that moved from `$29` to `$31` inside a 9k-token page is exactly the single-token change that gets lost in the middle of the context window — and the model also *hallucinates* changes that did not happen. A real diff is deterministic; an LLM "diff" is a probabilistic guess.
4. **Flapping noise.** Live pages mutate constantly for reasons no one cares about: CSRF tokens, session IDs, cache-buster query strings, rotating hero images, "127 people viewing this", relative timestamps, A/B-test class names, ad slots, CSP nonces. A naive byte-diff fires on every poll, and agents that act on raw diffs either get throttled or get muted by their operators.

The root cause: **the page is the wrong unit of work.** Agents are forced to reconstruct "the delta" from two full snapshots, in the most expensive, least reliable place possible — the context window.

### 1.2 The thesis: delta + why + follow-up

`changefeed` makes the **change event** the unit of work. For each observation of a watched URL it emits a single compact payload, computed *before* the agent's model ever runs:

- **(1) THE DELTA** — what changed, computed deterministically over a *normalized* block-tree (added / removed / modified / reordered). Not a vibe; a structured patch.
- **(2) WHY IT MATTERS** — a salience score and a classification (`price_increase`, `api_breaking`, `incident_open`, `cosmetic`…), so the agent can *skip* cosmetic churn without spending a single token of reasoning on it.
- **(3) SUGGESTED FOLLOW-UP** — what *kind* of action the change implies (`notify`, `re_run_downstream`, `escalate_human`, `ignore`). A hint, not a command — the agent stays in control but starts from a strong prior.

The agent reads **~305 tokens** of "here is exactly what changed and why" (a single price-change event, measured in §6.1) instead of ~20k tokens of "here are two pages, figure it out." (All per-event token figures in this doc use the one canonical event measured in §6.1; see that section for methodology.)

### 1.3 Quantifying the win (illustrative model, not measured)

The numbers below are an **illustrative back-of-envelope model**, not a measurement, and they rest on three explicit assumptions stated inline. Per observation of one ~9k-token (clean text) page, input tokens only:

| Approach | Input tokens / observation | Cost @ $3/Mtok |
|---|---|---|
| Naive: diff raw HTML in-context (2×) | ~120,000 | $0.0585 |
| Naive: diff cleaned text in-context (2× + reasoning) | ~19,500 | $0.0059 |
| **changefeed event (something changed)** | **~305** | **$0.00092** |
| **changefeed no-op (nothing material changed)** | **0** | **~$0** |

Versus the cleaned-text naive approach on a *changed* page that is a **98.4%** input-token reduction. **Assumption 1:** the naive baseline re-stuffs *both* full cleaned-text copies plus reasoning overhead on every poll. A *competent* naive implementation would cache the prior page's tokens via prompt caching, cutting the naive per-poll input cost roughly 5–10× and shrinking this multiple accordingly — the win is still large, but the headline figure here is the un-cached, adversarially-favorable case.

The bigger structural win is that **most polls find nothing**: changefeed does the deterministic diff itself and emits exit code `0` with empty stdout, so the agent's model never runs. Modeling a fleet of **500 pages polled every 15 min (96 obs/day = 48,000 obs/day)** with **Assumption 2: a 5% material-change rate** (so 2,400 material obs/day) and **Assumption 3: the remaining 95% are true no-ops the LLM never sees**:

- Naive cleaned-text diffing (un-cached): `48,000 × $0.0059 ≈ $283/day`. *(Earlier drafts quoted ~$2,808/day; that figure assumed raw-HTML diffing at $0.0585/obs and is dropped as unrealistic — no competent integrator diffs raw HTML.)*
- changefeed: `2,400 material obs × $0.00092 ≈ $2.20/day` of LLM input — a **>99.2%** reduction, before counting that the 45,600 no-op polls cost the model exactly zero.

The win compounds two moves: (a) replacing a 20k-token in-context diff with a ~305-token structured event, and (b) suppressing the ~95% of observations that are noise so the LLM never sees them. Latency and correctness improve for the same reasons — the model only wakes for real, pre-localized changes.

### 1.4 Goals

1. **Emit delta + why + follow-up as the primary output.** Every observation produces one compact, agent-consumable event. This is the product.
2. **Deterministic, reproducible diffs.** Same two snapshots → byte-identical event. No LLM in the core diff or salience path; LLM use is opt-in enrichment only. Diffs are auditable and testable.
3. **Cheap by default.** The common case (poll → no material change) costs near-zero tokens, CPU, and storage. Cost scales with *change volume*, not *page volume* or *poll frequency*.
4. **Noise resistance is a first-class feature**, not config polish. A flapping tool gets muted and abandoned.
5. **Lightweight storage.** Previous versions are cached as compactly as possible — content-addressed dedup, zstd compression, delta-chaining. History must not become the reason teams turn the tool off.
6. **Agent-first interface.** Structured JSON/NDJSON/MCP by default, designed to drop into an agent loop; a human renderer exists for debugging.
7. **Salience is configurable per watch.** A pricing analyst and an SRE care about different deltas on the same page; the materiality rules and selectors are per-watch tunable.

### 1.5 Non-goals

- **Not a general crawler.** It watches an explicit, user-declared set of URLs. It does not discover links, walk sitemaps, or spider a domain.
- **Not a data-extraction scraper.** It reports *change*, not *content*. If you want the current full dataset, scrape it; changefeed tells you when that dataset moved.
- **Not an uptime monitor.** It reports semantic change in *third-party* pages; it does not measure availability/latency of *your* services and page an on-call. (Watching a status page is a use case, not the mission.)
- **Not a headless-browser farm by default.** JS-rendering is an opt-in adapter, not the default path.
- **Not a notification/alerting product.** It emits events; routing them to Slack/email/PagerDuty is the integrator's job, trivially built on the event stream.

### 1.6 Target users

- **AI-agent developers** (primary) building autonomous loops that need a reliable "what changed" signal. They consume events via JSON/NDJSON/MCP.
- **The engineer building changefeed** — needs the diff/normalization/storage design pinned.
- Secondarily, **growth/competitive-intel, devrel, compliance, and SRE teams** who run the CLI directly and wire events into existing alerting.

---

## 2. Design Principles

- **The change event is the unit of work.** Everything in the system exists to produce *one event object well*. The page is never the deliverable.
- **Determinism over cleverness.** The core diff and salience scoring are real algorithms over a normalized tree — no LLM, no clock, no randomness. Reproducible, testable, auditable. Any LLM use is opt-in adjudication that can only re-label within a fixed taxonomy. (One deliberate exception, isolated and labeled: the *date-proximity enrichment* in §8.1 reads a clock; it lives outside the deterministic core and is stamped as non-reproducible-across-time.)
- **Cheap by default.** No-op polls are near-free in tokens, CPU, and storage. Cost tracks change volume, not fleet size or cadence.
- **Noise resistance is the product.** Normalization, volatility damping, and debounce are not optional polish; without them the tool flaps and gets muted.
- **Agent-first output.** Exit codes and JSONL are the load-bearing interface; pretty TTY output is a courtesy for humans.
- **Lightweight storage.** Content-addressed dedup + zstd + delta-chaining; history is cheap enough that nobody disables it to save disk.
- **You declare what you watch.** Explicit URLs and per-watch rules, never autonomous spidering — predictable scope, predictable cost.

### 2.1 The identity model (read this before §5, §7, §9)

Three reviewers flagged that "identity" is overloaded across the diff, stability, and storage layers. There are **three distinct identities** and every stage names exactly which it uses. They are *not* interchangeable:

| Identity | Defined in | Stable across observations? | Used by |
|---|---|---|---|
| **`block_id`** — within-an-observation node identity. A content hash; **changes by construction whenever the block's text changes.** | §5.4 | **No.** A `modify` *always* changes `block_id`. | Internal node references inside one `CanonicalDoc`; the `delta` join *within* a successful aligned pair. |
| **`slot_key`** — the cross-observation stable join key. Derived from `heading_breadcrumb ‖ type ‖ ordinal-within-section-of-type` — **never** from the mutable text and **never** from a DOM/CSS path. | §5.4 | **Yes**, by construction (it excludes text and layout). | The diff aligner's spine (§7.1), `seg.fp`/`seg.anchor` addressing (§6.2), and **all** flap-tracking, debounce, dedup, and auto-ignore joins (§7.4). |
| **`content_hash`** (`norm_hash`) — the normalized-text hash of a block. | §5.3 | Changes on any content edit (that is its job). | Detecting *whether* a block changed; the idempotency `event_key`. |

The earlier draft incorrectly described `block_id` as "the stable join key" and separately tracked stability off a "stable selector." Both are corrected: **`slot_key` is the one and only cross-observation join, used identically everywhere.** Path/CSS-based identity is rejected throughout (§5.4).

---

## 3. System Overview

`changefeed` is a single static **Rust** binary (`cf`). One observation flows through eight stages; the tier boundary at fetch is invisible to the *diff* downstream (the same canonical tree is produced either way), with one explicit exception — the fetch tier is recorded in `prov.m` and feeds the confidence model (§6.6). Two short-circuits (HTTP 304, then `doc_hash`-equal) abort the pipeline early at near-zero cost.

```
                          ┌──────────────────────────────────────────────────────────────┐
   cf check <target>      │                    one observation                            │
        │                 │                                                                │
        ▼                 ▼                                                                │
  ┌──────────┐   304?  ┌──────────┐   ┌───────────┐   ┌──────────┐   ┌────────┐   ┌──────────┐
  │  FETCH   │────────►│ EXTRACT  │──►│ NORMALIZE │──►│ SEGMENT  │──►│  DIFF  │──►│ CLASSIFY │
  │ http /   │  exit 0 │ readab./ │   │ strip     │   │ stable   │   │ align+ │   │ salience │
  │ headless │  no     │ selector │   │ volatile  │   │ slot_key │   │ noise- │   │ + follow │
  └────┬─────┘  store  └──────────┘   └─────┬─────┘   │ + types  │   │ suppress│  │   -up    │
       │ ETag/304               doc_hash==? │         └──────────┘   └───┬────┘   └────┬─────┘
       │ short-circuit          exit 0, no store                        │             │
       ▼                                                                ▼             ▼
  ┌──────────┐                                                    ┌──────────┐  ┌──────────┐
  │  STORE   │◄───────────────── snapshot (CAS + zstd delta) ─────│ persist  │  │  EMIT    │
  │ CAS pack │                                                    │ snapshot │  │ event    │
  │ +SQLite  │──────────────────────────────────────────────────►│ on change│  │ (JSONL/  │
  └──────────┘                                                    └──────────┘  │  MCP)    │
                                                                                └────┬─────┘
                                                                                     ▼
                                                                        exit code + stdout
                                                                        0=no-change · 10=change
```

- **Fetch** (§5.1): plain HTTP first (`reqwest`/`rustls`), escalating to pooled headless Chromium (`chromiumoxide`/CDP) only when a `needs_render` heuristic fires. Conditional GET (ETag/`If-None-Match`) short-circuits a `304` straight to exit `0` with no body, no diff, no stored snapshot.
- **Extract** (§5.2): Readability DOM-scoring by default, selector-strategy for structured pages.
- **Normalize** (§5.3): strip volatile attributes/text (nonces, csrf, framework IDs, cache-busters, relative timestamps, tracking params), NFC-normalize, canonicalize URLs.
- **Segment** (§5.4): flatten to typed blocks, each carrying a drift-resistant `slot_key` and a content `block_id`.
- **Diff** (§7): `slot_key`-anchor → LIS → similarity-fill alignment over the block sequence; intra-block Myers token diff; noise classification; (Phase 2) debounce + dedup. A `doc_hash`-equal check short-circuits before this runs.
- **Classify** (§8): seven deterministic salience signals (eight once the Phase-2 volatility signal lands) + noisy-OR combiner → `salience`/`materiality`; rule packs map category → follow-up action.
- **Store** (§9): one previous `CanonicalDoc` per target in MVP (single zstd blob); content-addressed dedup + delta-chaining is the Phase-2 fleet-scale optimization. Snapshots persist only when content changed.
- **Emit** (§4, §6): one `changefeed/v1` event per coherent change, as JSONL/JSON/pretty/MCP, plus the exit code.

Each stage is a pure function of `(input, Profile)`. The raw post-extract HTML is persisted (alongside the canonical tree) so segmentation can be re-derived offline (with a better algorithm) without re-fetching.

---

## 4. CLI Surface & Agent Integration

The binary installs as both `changefeed` and the short alias `cf`; examples use `cf`. Every choice here serves one rule:

> **An agent must learn what changed without re-reading the page, and branch on the result without parsing anything.** Exit codes and JSONL are the load-bearing interface.

### 4.1 Implementation language: Rust

`changefeed` is a single static binary (musl on Linux, native on macOS/Windows). Justification:

- **Zero-runtime deployment.** `curl -L .../cf -o /usr/local/bin/cf && chmod +x` and it works — no venv, no `node_modules`, no glibc roulette. The primary surface is *inside other people's agent loops and containers*, where we control nothing.
- **Storage owns the best compression bindings.** The `zstd` crate (libzstd) gives dictionary training (`--train`) and delta-against-prior (`--patch-from`) natively; `blake3` gives fast content-addressing.
- **Headless without bundling a browser.** Default fetches use `reqwest` + `rustls` (pure-Rust TLS). JS pages shell out to a *separately-installed* Chromium over CDP (`chromiumoxide`). We do **not** bundle Chromium — that turns a ~12 MB binary into ~150 MB. Render mode is opt-in per target; the binary degrades with a clear error (exit `7`) if no browser is found.
- **HTML parsing.** `scraper` (`html5ever`) for CSS extraction; `lol_html` (streaming rewriter) for cheap selector-scoped stripping on large pages.
- **Async daemon.** `tokio` lets one `cf daemon` politely poll hundreds of targets on independent schedules with bounded per-host concurrency.

Rejected: **Go** (weaker cgo-free zstd, thinner headless ecosystem); **Python**/**Node** (deployment friction is fatal for the "drop into a container" use case).

### 4.2 Command set

```
cf init                 scaffold changefeed.toml + .changefeed/ store
cf watch <url|target>   register/declare a target (writes to config)
cf check [targets...]   ONE-SHOT: fetch, diff vs last snapshot, emit event(s), exit-code-branch
cf daemon               long-running: poll all scheduled targets, stream events to sinks
cf diff <target> [REV]  re-emit a stored observation's diff (no network)
cf snapshot <target>    fetch + store WITHOUT diffing (seed / force baseline)
cf feed [targets...]    replay stored change events as JSONL (the "what did I miss" stream)
cf rules                test/validate selectors & ignore rules against a live or stored page
cf explain <event_id>   replay the salience scoring; print the signal breakdown
cf score --dry-run a b  score an ad-hoc snapshot pair without touching the store
cf gc                   prune the snapshot store per retention policy (Phase 2: mark-and-sweep)
cf pack / cf fsck       repack survivors / verify object integrity (Phase 2)
cf ls / cf show         inspect targets and the store
cf mcp                  run as an MCP (stdio) server exposing tools to an agent
cf login <target>       headed browser once to capture an authenticated session
cf schema --version 1   print the JSON Schema for the event/envelope
cf completion <shell>   emit shell completions
```

The two verbs that matter for agents are **`cf check`** (the poll primitive) and **`cf feed`** (the catch-up stream); everything else is operator ergonomics.

### 4.3 One-shot vs daemon: support both, default to one-shot

The **one-shot `cf check`** model is the default and the recommended integration: agent frameworks already own scheduling (cron, the agent loop, a workflow engine, an MCP host on a timer), it is stateless to invoke and trivially containerizable, and exit-code branching makes polling free. **`cf daemon`** exists for one process owning many targets with heterogeneous schedules, server-side ETag/crawl-delay state, and a single append-only event log to `--tail`. It reads the same `changefeed.toml` and writes to the configured sinks.

**State that one-shot `cf check` requires a store.** `cf check` is "stateless to *invoke*" but it is **not** stateless in effect: it reads the prior snapshot and per-host politeness timestamp from `.changefeed/` and writes the new snapshot back. An ad-hoc `cf check <url>` with no `changefeed.toml` creates/uses a `.changefeed/` in the current directory (or `$CF_STORE` / `$XDG_DATA_HOME/changefeed`); without a writable store it cannot honor crawl-delay or diff against a prior, and falls back to "first observation, no politeness memory" (exit `11`) with a stderr warning. Features that genuinely need cross-invocation *held* state — **debounce and the volatility model — are daemon-only (Phase 2)** and are explicitly disabled under one-shot `cf check` (see §7.4).

### 4.4 `cf check` — the agent poll primitive

```
cf check [TARGETS...] [flags]

  --format pretty|json|jsonl   output format (default: pretty on TTY, jsonl otherwise)
  --since REV|TIMESTAMP        diff against a specific prior revision, not just last
  --no-store                   diff but do NOT persist the new snapshot AND do NOT advance
                               the baseline or the dedup seen-set (pure read-only probe)
  --peek                       alias for the common agent pattern: diff against current head,
                               print the event, but do NOT advance the baseline (so a second
                               call sees the same delta). Implies --no-store.
  --min-salience LEVEL         emit/exit-signal only if salience >= LEVEL
                               (none|low|medium|high|critical; default: low)
  --selector CSS               override the configured selector for an ad-hoc URL
  --render auto|never|chromium override render mode
  --timeout SECS               per-fetch timeout (default 30)
  --etag / --no-etag           HTTP conditional GET (default on)
  --emit-subthreshold          emit (exit 12) for changes below --min-salience
  --fail-on-fetch-error        treat fetch errors as exit 1 instead of soft exit 3
  --stdin --url URL            diff HTML piped on stdin instead of fetching
  --targets-from -             read newline-delimited targets from stdin
  -q, --quiet                  suppress stdout, rely on exit code only (cheapest poll)
```

`cf check https://stripe.com/pricing` with no config is legal: it fetches, compares to the last stored snapshot (or stores a baseline and reports "first observation"), prints the event, and **sets the exit code**. This is the call an agent makes in a loop.

**`--no-store` and dedup semantics (resolved).** `--no-store` is a *pure read-only probe*: it neither persists the new snapshot, advances the per-target baseline/`rev`, nor records the idempotency `event_key` in the seen-set. Therefore two consecutive `--no-store` probes on the same fetched content **both** emit the same event (no dedup suppression) — which is exactly what an agent wants from a probe. The default (store-on-check) call advances the baseline *and* records the `event_key`, so a re-run on the now-current snapshot is a no-op (exit `0`). The retry-after value, when rate-limited, is emitted in the event/envelope JSON (`crawl.retry_after`) and on stderr — never requiring a second fetch to read it.

### 4.5 Exit codes — the cheap branch

This is the most important agent-facing feature. An agent polls and branches on `$?` **without parsing stdout at all**.

| Code | Meaning | Agent reaction |
|------|---------|----------------|
| `0`  | No change (re-observed, nothing material per ignore rules + `--min-salience`) | Do nothing. Cheapest path; no stdout to parse. |
| `10` | **Change detected** at/above `--min-salience` | Read stdout JSONL → act on the delta. |
| `11` | First observation (baseline stored, no prior to diff) | Note baseline; usually no action. Emits the minimal baseline envelope (§6.8). |
| `12` | Change detected but **below** `--min-salience` (only with `--emit-subthreshold`) | Optional logging. Without the flag, sub-threshold is exit `0`. |
| `1`  | Usage / config error (bad flag, malformed TOML) | Bug in invocation; do not retry. |
| `2`  | Target not found / unknown id | Fix config. |
| `3`  | Fetch failed (DNS, TLS, timeout, 5xx) — *soft* | Transient; retry with backoff. `--fail-on-fetch-error` promotes to `1`. |
| `4`  | Blocked by robots.txt | Do not retry without policy change. |
| `5`  | Auth failure (401/403) | Refresh credentials. |
| `6`  | Rate-limited / crawl-delay not yet elapsed (429 or politeness gate) | Back off; read `crawl.retry_after` from stderr/JSON. |
| `7`  | Render required but no browser available | Install Chromium or set `render="never"`. |

Design rule: **the no-change path is exit `0` with empty stdout**, so the dominant case costs the agent one integer compare. `10` is the only code that *requires* reading stdout.

**Exit-code stability policy (pinned).** Exit codes are **stable across all minor and patch versions** within a major version, and the `3`/`1` split (soft fetch vs hard usage error) is a permanent contract. A **major** version bump *may* reassign codes, but only with a documented migration table; codes `0`/`10`/`11` are frozen for all foreseeable majors because the entire agent-loop contract rests on them. **No-change vs failure is always distinguishable by exit code alone** (`0` vs `3`) — an agent never needs to read stdout to tell "nothing changed" from "I could not check" (this settles Open Question §14.1 on the `check` surface; the daemon additionally logs the `n:0` envelope for liveness).

### 4.6 The canonical agent loop

The exit-10 branch must consume the output of the **first** call — the default `cf check` already advanced the baseline, so a *second* `cf check` would diff the new snapshot against itself and emit nothing. Capture stdout once:

```bash
while sleep 900; do
  out=$(cf check stripe-pricing --min-salience medium --format jsonl)   # one fetch, one diff
  case $? in
    0)  : ;;                                       # no change — do nothing, zero tokens
    10) printf '%s\n' "$out" | your-agent ingest-change ;;   # reuse the captured event
    3)  sleep 60 ;;                                # transient fetch error, back off
    5)  refresh-credentials ;;                     # auth expired
    6)  sleep "$(printf '%s' "$out" | jq -r '.crawl.retry_after // 60')" ;;  # no second fetch
  esac
done
```

The Python form is the same shape — one invocation, read `stdout` only on exit `10`:

```python
r = subprocess.run(["cf","check","stripe-pricing","--format","json"], capture_output=True, text=True)
if r.returncode == 0:
    pass                                    # nothing changed; spend zero tokens
elif r.returncode == 10:
    event = json.loads(r.stdout)            # one compact object, not a whole page
    agent.handle(event["why"]["summary"], event["delta"], event["followup"])
elif r.returncode == 3:
    agent.schedule_retry(backoff=True)
elif r.returncode == 6:
    agent.schedule_retry(after=json.loads(r.stdout).get("crawl", {}).get("retry_after", 60))
```

If an agent genuinely needs a probe that does *not* advance the baseline (e.g. it will ingest the change through a separate authenticated fetch), use `cf check --peek` (§4.4), which prints the delta without advancing — a second `--peek` returns the same event.

### 4.7 `cf feed` — catch-up stream

```
cf feed [TARGETS...] [flags]
  --since TIMESTAMP|REV    replay all events after this point
  --tail                   block and stream new events as the daemon produces them
  --format jsonl|json      default jsonl
  --standalone             disable envelope-level dedup (full prov/src per event)
  --min-salience LEVEL
  --limit N                cap at N most-recent events (default 1000)
  --after-cursor CURSOR    resume after an opaque cursor (pagination)
  --max-salience-first     order high→low salience so a truncated read keeps the worst news
```

`cf feed --since 2026-06-01T00:00:00Z` answers "what changed while my agent was offline." With `--tail` it attaches to the daemon's append-only `.changefeed/events.jsonl` (tailed via inotify/kqueue) so a long-lived agent can `for await` the stream.

**Bounded catch-up contract.** Replaying a week across 500 targets can be thousands of events, so `cf feed` is **paginated and bounded by default**: it returns at most `--limit` events (default 1000) plus a `next_cursor` in the trailing envelope; the agent resumes with `--after-cursor`. With `--max-salience-first`, a token-constrained agent reading only the first page is guaranteed to see the highest-salience changes first. This gives the catch-up path the same hard upper bound on tokens that the per-event path has (§6.1).

### 4.8 Output formats & piping contracts

Three formats, **one event schema** (`changefeed/v1`, §6), identical across `check`, `feed`, daemon sinks, and the MCP tool — agents learn it once.

- **`--format jsonl`** (default for pipes): one self-contained event object per line.
- **`--format json`**: a single pretty-printed object (or `{"events":[...]}` for multiple targets).
- **`--format pretty`** (default on a TTY): colorized human diff with a salience badge and the suggested follow-up.

Piping rules are absolute:

- **stdout** = events only (JSONL/JSON/pretty). A consumer can `cf feed --tail | jq` forever without corruption.
- **stderr** = logs, progress, warnings, robots/rate-limit notices.
- **stdin**: `cf check --stdin --url https://x/y` diffs piped HTML against the stored snapshot — letting an agent that already fetched the page (via its own authenticated browser) use `cf` purely as a diff/salience engine. `cf check --targets-from -` reads newline-delimited targets for batch polling.

Pretty example:

```
● stripe-pricing  https://stripe.com/pricing            [HIGH] price_increase
  ~ Team plan      $20 / seat  →  $25 / seat
  + Enterprise SSO  — Contact us
  why: price_increase, keyword "per seat"   (sal 0.86)
  → suggested: re_run_downstream
  rev 42 (prev 41)  · stored +73 B
```

### 4.9 Config file: `changefeed.toml`

TOML, not YAML: watched targets are config-as-code humans review in PRs, and YAML's significant whitespace + type-coercion footguns (`no` → `false`, the Norway problem) are exactly wrong for a file full of URLs, selectors, and header values.

```toml
[defaults]
schedule       = "15m"                 # humantime duration; daemon-only
render         = "auto"                # auto | never | chromium
timeout        = "30s"
user_agent     = "changefeed/1.0 (+https://github.com/acme/changefeed)"
respect_robots = true
min_salience   = "low"
store_format   = "zstd-delta"          # zstd-delta | zstd | raw
fidelity       = "full"                # full | skeleton | hashes (see §9.1)

[[sink]]                               # daemon-mode event sinks
type = "jsonl"
path = ".changefeed/events.jsonl"      # append-only; tailed by `cf feed --tail`

[[sink]]
type    = "webhook"
url     = "https://hooks.internal/changefeed"
headers = { Authorization = "Bearer ${CF_WEBHOOK_TOKEN}" }
min_salience = "high"                  # only page-worthy changes hit the webhook

# ---- Targets ----

[[target]]
id        = "stripe-pricing"           # stable handle; becomes src.tid in events
url       = "https://stripe.com/pricing"
schedule  = "1h"
archetype = "pricing"                  # selects the salience rule pack + extract profile
select    = ["main .PricingTable", "section[data-pricing]"]
ignore = [
  ".cookie-banner",
  "[data-testid='live-visitor-count']",
  { selector = "time.last-updated" },           # ignore an element entirely
  { attr = "nonce" },                            # ignore churny attributes everywhere
  { regex = '\b\d{4}-\d{2}-\d{2}T[\d:.]+Z\b' }   # ignore ISO timestamps in text
]
salience_hints = { keywords = ["price", "per seat", "deprecated", "free tier"] }

[[target]]
id        = "openai-api-ref"
url       = "https://platform.openai.com/docs/api-reference"
archetype = "api-docs"
render    = "chromium"                 # JS-rendered SPA
select    = ["#content"]
[target.auth]
type    = "header"
headers = { Authorization = "Bearer ${OPENAI_DOCS_TOKEN}" }

[[target]]
id        = "competitor-status"
url       = "https://status.competitor.com"
archetype = "status-page"
schedule  = "2m"
select    = [".component-container"]
[target.auth]
type    = "cookie"
cookies = "session=${COMP_SESSION}; csrf=${COMP_CSRF}"
```

Secrets are never written literally; `${VAR}` is expanded from the environment (or `.changefeed/secrets.env`, loaded `0600`) at runtime and redacted from logs. `cf rules --explain stripe-pricing` dry-runs the selector/ignore pipeline against a live fetch and prints exactly which nodes survive into the diff.

**Selector resilience & partial-match safety.** A `select` is a CSS selector against the *original* DOM, evaluated identically on the Tier-1 and Tier-2 (post-render) HTML — so escalation does not change which subtree is kept. `cf rules` warns when a selector matches **zero** nodes (a likely redesign — §11) *and* when it matches a subtree whose stable-`slot_key` set overlaps the prior observation's by less than `select_overlap_min` (default 0.3), which catches the dangerous case where a redesign makes the selector match a *different* subtree and would otherwise produce a confident wrong diff. On low overlap we emit one low-`conf`, high-`mat` `content_edit` event (`followup: verify_llm`-eligible) rather than a silent garbage diff, and flag the target for operator review.

### 4.10 MCP / function-calling mapping

`cf mcp` runs an MCP server over stdio. Each CLI verb maps to one tool; `inputSchema`/`outputSchema` are JSON Schema 2020-12 with a `type:"object"` root, and the tool returns the **same event object** the CLI emits — the sub-schemas below are `$ref`s into the single published v1 schema (`cf schema --version 1`), so the "same object" promise is machine-verifiable by the host. The `changed` boolean is the MCP analog of the exit code — the agent branches on it before reading `delta`, and **the tool never returns the full page**.

```json
{
  "name": "changefeed_check",
  "description": "Re-observe a watched URL and return ONLY what changed since last time (delta, why it matters, suggested follow-up). Does NOT return the full page. Use this instead of fetching a page to see if it changed.",
  "inputSchema": {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {
      "target":       { "type": "string", "description": "Target id from changefeed.toml, or a full URL" },
      "min_salience": { "type": "string", "enum": ["none","low","medium","high","critical"], "default": "low" },
      "peek":         { "type": "boolean", "default": false, "description": "Do not advance the baseline (re-callable)" }
    },
    "required": ["target"]
  },
  "outputSchema": {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "properties": {
      "changed":     { "type": "boolean" },
      "materiality": { "type": "string", "enum": ["none","low","medium","high","critical"] },
      "seg":         { "$ref": "https://changefeed.dev/schema/v1.json#/$defs/seg" },
      "ct":          { "$ref": "https://changefeed.dev/schema/v1.json#/$defs/ct" },
      "delta":       { "$ref": "https://changefeed.dev/schema/v1.json#/$defs/delta" },
      "why":         { "$ref": "https://changefeed.dev/schema/v1.json#/$defs/why" },
      "followup":    { "$ref": "https://changefeed.dev/schema/v1.json#/$defs/followup" },
      "conf":        { "type": "number" }
    },
    "required": ["changed"]
  }
}
```

A `CallToolResult` carries the structured event in `structuredContent` and a one-line gloss in `content[0].text`. Exposed tools: `changefeed_check`, `changefeed_feed`, `changefeed_watch`, `changefeed_diff`, `changefeed_list`. Because `followup.act` in emitted events is the **8-value agent-facing enum** (never `verify_llm`, see §6.5), the `followup` `$ref` enumerates exactly those eight, so a host's exhaustive switch is complete.

### 4.11 Auth for gated pages

Per-target `[target.auth]` with four `type`s: `header` (Bearer/API-key, most common), `cookie` (verbatim cookie string), `basic` (`username`+`password` → `Authorization: Basic`), and `browser` (only with `render="chromium"`: a persisted Chromium storage-state captured once by `cf login <target>`, reused headless). All secret-bearing values are `${ENV}`-expanded, never persisted in the snapshot store, and redacted from logs.

---

## 5. Extraction & Normalization

This is the front half of `changefeed`: it turns a watched URL into a **canonical block-tree** (`changefeed.canonical/1`) — a stable, diffable, typed representation. Its one design goal — *that two observations of the same logical page produce block trees that line up block-for-block* — is an **engineering target with a measurable proxy, not a proven guarantee.** We define "lines up" operationally: the fraction of blocks paired by stable `slot_key` between two consecutive no-real-change observations should be ≥ 0.98 (the `align_rate` metric, logged per observation). When `align_rate` drops, the diff degrades to extra add/remove pairs — visible, bounded, and surfaced as low `conf`, never a silent wrong answer.

```
fetch  ──►  extract  ──►  normalize  ──►  segment  ──►  type  ──►  CanonicalDoc
(HTTP/     (Readability/   (whitespace,   (semantic    (price/    (diffable artifact
 headless)  selector,       URLs, strip    blocks +     date/      + raw snapshot,
            boilerplate)     volatile)      slot_keys)   number…)   content-addressed)
```

Each stage is a pure function of input + a per-source `Profile` (§5.7), individually cacheable and replayable.

### 5.1 Fetch: plain HTTP first, headless only on proof of need

Headless rendering is 50–200× more expensive than an HTTP GET, and most monitoring targets (status pages, changelogs, filings, most docs) ship meaningful server-rendered HTML. We do not pay for Chromium unless a page proves it needs JS.

**Tier 1 — `reqwest`.** Single GET with `Accept-Encoding: gzip, br, zstd`, a pinned realistic UA per profile, and ETag/`Last-Modified` revalidation. **A `304 Not Modified` short-circuits the entire pipeline** — no extract, no diff, zero new storage, emit `no-change` with reason `http-304`. Budget: 10 s connect+read, 3 redirects, 5 MB body cap.

**Escalation (Tier 1 → Tier 2)** if any `needs_render` heuristic fires: (1) content-emptiness ratio `visible_text_len / html_byte_len < 0.05` and absolute text `< 200` chars; (2) a known hydration root present but empty (`<div id="root">`, `<div id="__next">`, `<app-root>`); (3) meaningful content only in `__NEXT_DATA__` / `application/json` bootstrap; (4) profile override (`render = "always" | "never"`).

**Tier 2 — pooled headless Chromium (`chromiumoxide`/CDP).** A long-lived warm tab (process spawn is ~300 ms+, a warm tab is tens of ms). Navigate, wait for network-idle (≤2 in-flight for 500 ms) with a 15 s ceiling, or an optional `profile.wait_for` selector. Block image/font/media/analytics requests by `ResourceType` + hostname blocklist (we want the DOM, not pixels). Extract `document.documentElement.outerHTML` *after* hydration — **this post-render HTML becomes the "raw HTML" Stage 2 sees, so the diff downstream is identical regardless of tier.** (The tier still leaks into `prov.m` and thus the confidence model — §6.6 — which is intentional: a headless render is inherently a less certain observation.)

| Tier | Wall-clock | Memory |
|------|-----------|--------|
| HTTP 304 | ~50–150 ms | ~MB |
| HTTP 200 | ~100–400 ms | ~MB |
| Headless (warm tab) | ~1.5–4 s | ~80–150 MB/tab |

**Render-determinism: how two renders converge to one canonical tree.** The "deterministic diff *given a canonical tree*" guarantee depends on rendering converging. We do not hand-wave this; convergence is engineered in four explicit steps, and residual nondeterminism is treated as noise, not as a correct diff:

1. **Settle, don't snapshot-early.** We extract `outerHTML` only after network-idle *and* a stable-DOM check: poll `documentElement.outerHTML.length` every 200 ms and accept once it is unchanged for two consecutive polls (or the 15 s ceiling). This kills mid-hydration captures.
2. **Block the nondeterministic surfaces.** Ad slots, analytics, and recommendation widgets are blocked by the resource blocklist *and* by the boilerplate-class strip (§5.2), so client-side-randomized regions never enter the tree.
3. **Canonical ordering for unordered containers.** A `profile.unordered` selector list marks containers whose child order is not semantically meaningful (e.g. a masonry grid); their children are sorted by `slot_key` before segmentation, so render-to-render reshuffles become no-ops, not reorders.
4. **A/B and lazy-section residue → the volatility model (Phase 2).** Anything that survives 1–3 and still flaps run-to-run is caught by per-`slot_key` flap tracking / A/B canonicalization (§7.4) and lowered `conf` (headless baseline `conf` is capped, §6.6). We **bound** this: a target whose post-canonicalization `align_rate` (defined above) stays below 0.95 over 8 renders is flagged `render_unstable` for operator review rather than silently emitting flapping diffs.

The escalation decision is logged on every observation (`fetch.tier`, `fetch.escalation_reason`). A **direct-JSON fast path** (`profile.json_endpoint` + a JSONPath→typed-block map) skips render *and* HTML extraction entirely — the cheapest, most stable source when available, recommended for high-frequency price/inventory pages.

### 5.2 Extract: strip boilerplate, keep the main content

Default is the **Mozilla Readability** algorithm reimplemented over an `html5ever` DOM (text-density / link-density scoring, tag bonuses for `<article>`/`<main>`/`<section>`, penalties for `<nav>`/`<aside>`/`<footer>`). But extraction is **profile-pluggable**, because pure readability harms *structured* pages (it will happily delete a pricing table it mistakes for boilerplate). The profile picks a strategy:

- `readability` (default) — article/changelog/release-notes/docs prose.
- `selector` — `profile.root_selector` names the subtree to keep verbatim. **Recommended for the high-value structured sources** (pricing tables, status grids, inventory).
- `full` — keep `<body>` minus a hardcoded boilerplate blocklist.

Regardless of strategy we always hard-strip `<script>`, `<style>`, `<noscript>`, `<template>`, `<svg>`, `<iframe>`, comments, and boilerplate-class matches (`ad`, `promo`, `cookie`, `banner`, `newsletter`, `social-share`, `breadcrumb`). Output is a pruned DOM subtree, not a string.

### 5.3 Normalize: kill everything volatile before it can masquerade as a change

This is where we earn the "no false-positive diffs" promise. **Everything stripped here is removed *before* the block content hash (`norm_hash`) and `doc_hash` are computed** — so a page whose *only* run-to-run difference is a rotating session token, nonce, or relative timestamp produces an **identical `doc_hash`** and hits the no-store short-circuit (§5.6). (This is the linchpin the storage demo in §9 is measured against; see §9.2 for the corpus distinction.)

- **Text/encoding.** Decode entities; Unicode **NFC**; collapse whitespace (incl. NBSP, zero-width `U+200B/FEFF`) to a single ASCII space; trim. Code blocks are exempt (whitespace is load-bearing).
- **Attributes — strip the volatile set** (the highest-leverage anti-noise rule): nonces/csrf and any high-entropy token (`/^[A-Za-z0-9_-]{24,}$/`, Shannon entropy > 3.5 b/char); cache-busters (`?v=`, `?hash=`, fingerprinted `app.4f3a2b1.js` → `app.js`); framework churn (`data-reactid`, `data-v-*`, `data-svelte-*`, `id` matching `/^(:r[0-9a-z]+:|radix-|headlessui-|mui-|ember\d+)/`); relative-time text (`"3 minutes ago"`, `"as of 14:32 UTC"`) → placeholder `⟦TS⟧` unless the profile types that block as a `date` we care about.
- **URLs — canonicalize.** Resolve relative→absolute against the final base; lowercase scheme+host; drop default ports; sort query params; strip tracking params (`utm_*`, `gclid`, `fbclid`, `ref`, `_ga`); drop fragments. So `HTTP://Example.com:80/p?utm_source=x&b=2&a=1#top` → `http://example.com/p?a=1&b=2`. **The attribute-delta path in §7.2 compares the *post-canonicalization* `href`/`src` values**, so a `utm` swap is a non-event while a genuine CDN/host change is a real one.

The volatile-strip set is profile-extensible (`profile.strip_attrs`, `profile.strip_text`); we log `normalize.stripped_count` so a page that is 90% volatile (a mis-tuned profile) is visible.

### 5.4 Segment: stable semantic blocks, two identities

We flatten the clean DOM into an ordered **tree** of semantic blocks (containment is itself signal: a row moving between tables differs from a row edited in place). Boundaries are semantic units, not DOM nodes: `h1`–`h6`→`heading`, `<p>`→`paragraph`, `<li>`→`list_item`, `<tr>`→`table_row` under a `table` container, `<pre>`/`<code>`→`code`.

Each block carries **two distinct identities** (the model in §2.1):

- **`block_id`** — `blake3(slot_key ‖ ":" ‖ norm_text)[:12]` (base32). This is the *within-observation* node handle. **It changes whenever the block's text changes — by construction.** It is *not* a cross-observation join key, and we never treat it as one. (The earlier draft's claim that the content-anchored ID is "the stable join key" was wrong: a hash of the text cannot survive an edit. This is corrected; the cross-observation join is `slot_key` below.)
- **`slot_key`** — the **cross-observation stable join**, computed in priority order:
  1. **Explicit anchor** (most stable): a non-volatile `id` that survived normalization or a profile-declared key → `slot:anchor:<id>`. Docs (`#authentication`), changelogs (`#v2-4-0`), status components (`#component-api`) give these for free.
  2. **Structural slot** (the workhorse, used for **all** block types — generalized from the value-block case): `slot_key = slot:struct:blake3(heading_breadcrumb ‖ ":" ‖ type ‖ ":" ‖ ordinal_within_section_of_type)[:12]`. It deliberately **excludes the block's text and DOM/CSS path**, so editing a sentence, wrapping it in two divs, or reordering siblings of a *different* type leaves the key intact. The 2nd `paragraph` under "Pro Plan" is `slot:struct(Pricing›Pro Plan, paragraph, #1)` regardless of its text or where it sits in the DOM.

**Worked collision/edit example (two paragraphs in one section).** Section "Pro Plan" has two paragraphs, P1 "Best for growing teams." and P2 "Cancel anytime." Their keys are `…(Pro Plan, paragraph, #0)` and `…(Pro Plan, paragraph, #1)` — distinct by ordinal-within-section-of-type, so no collision. Edit P1 to "Best for scaling teams.":

| | before | after |
|---|---|---|
| P1 `slot_key` | `(Pro Plan, paragraph, #0)` | `(Pro Plan, paragraph, #0)` — **unchanged** |
| P1 `block_id` | `blake3(slot‖"Best for growing teams.")` | `blake3(slot‖"Best for scaling teams.")` — **changed** |
| P2 `slot_key` | `(Pro Plan, paragraph, #1)` | `(Pro Plan, paragraph, #1)` — **unchanged** |

The aligner joins by `slot_key` (P1↔P1, P2↔P2), sees P1's `norm_hash` changed and P2's did not, and emits exactly one `modified` op for P1. **The `block_id` change is expected and irrelevant** — alignment never joins on it. This is why scheme-2 IDs (the content `block_id`) changing on edit is a non-problem: the join key is `slot_key`, which is text-free.

**We reject path/CSS-based identity entirely.** A path ID (`body>div:nth-child(3)>main>div.cards>div:nth-child(2)>p`) breaks the instant the site adds a wrapper div or A/B-tests a layout. `slot_key` derives identity from *what section a block is in and what kind of block it is*, never *where it sits in the DOM*.

`anchored_by` records which `slot_key` scheme produced the key (`anchor` | `struct`), telling the diff engine how much to trust an exact match versus fall back to similarity. Churn survival: adding a wrapper div → keys unchanged → zero diff; moving a section → `reorder`, not delete+add; editing a sentence → `slot_key` stable, `norm_hash` differs → `modify`; A/B nav swap → stripped in Stage 2, invisible; price change → slot stable, value changes → `modify`.

### 5.5 Type: typed values inform salience

Each block carries a `type` because a changed *price* is treated very differently from a changed *paragraph*: `price` (currency + decimal), `date` (ISO/RFC/`<time datetime>`), `number`/`quantity`, `code`, `table`/`table_row`, `link`, `heading`, `text` (default). The payload carries the **parsed value** alongside raw text — `{"type":"price","raw":"$11.00","value":{"amount":1100,"currency":"USD","period":"mo"}}` — so the diff compares structured values (1100 vs 900) not strings, and salience computes `+22%`. Typing is best-effort and overridable per block via `profile.types`.

### 5.6 The canonical artifact: `CanonicalDoc`

Serialized as canonical JSON (sorted keys, no insignificant whitespace) and persisted (§9).

```json
{
  "schema": "changefeed.canonical/1",
  "url": "https://acme.example/pricing",
  "final_url": "https://acme.example/pricing",
  "fetched_at": "2026-06-02T14:00:00Z",
  "fetch": { "tier": "http", "status": 200, "escalation_reason": null,
             "etag": "W/\"a1b2\"", "content_hash": "blake3:8c1f…" },
  "profile_id": "acme-pricing",
  "doc_hash": "blake3:3e9a…",
  "blocks": [
    { "slot_key": "slot:anchor:plans", "block_id": "a1b2c3d4e5f6",
      "type": "heading", "level": 1, "text": "Plans", "children": [
        { "slot_key": "slot:struct:9k1m…", "block_id": "kq7x4m2a9b1c",
          "type": "heading", "level": 2, "text": "Pro Plan",
          "path_hint": "Plans › Pro Plan", "children": [
            { "slot_key": "slot:struct:p9w2…", "block_id": "p9w2hf0a7sd1",
              "type": "price", "text": "$11.00 / mo",
              "value": { "amount": 1100, "currency": "USD", "period": "mo" },
              "anchored_by": "struct" },
            { "slot_key": "slot:struct:tn4k…", "block_id": "tn4k1c8s2v0e",
              "type": "list_item", "text": "Unlimited projects",
              "anchored_by": "struct" }
          ] } ] }
  ],
  "stats": { "block_count": 4, "stripped_attrs": 37, "render_ms": 0, "bytes_raw": 50122 }
}
```

- **`doc_hash`** — blake3 over the canonical tree (`slot_key`s + types + `norm_text`/values, *excluding* `fetched_at`/`fetch`/`block_id`). Equal `doc_hash` ⇒ guaranteed no semantic change ⇒ emit `no-change` (reason `doc-hash-equal`) **without diffing and without storing a new snapshot**. This is the second cheap short-circuit after HTTP 304. *(Because §5.3 strips volatile tokens before this hash, a page whose only change is a rotating nonce is `doc_hash`-equal and costs zero bytes — see §9.2.)*
- **`slot_key`** — the drift-resistant, text-free cross-observation join key (§5.4). **This is the only join key the diff aligner uses.**
- **`block_id`** — the within-observation content handle; changes on edit; never a join key.
- **`type` + `value`** — diff compares `value` when present, else `norm_text`.
- We persist the **raw post-extract HTML** alongside (one blob) so segmentation can be re-derived offline when the algorithm improves, without re-fetching. (Storage cost of keeping both is accounted in §9.2.)

### 5.7 Multi-page sources & the Profile

A source is a *set* of URLs reduced to exactly **one** `CanonicalDoc` per observation before the diff engine sees it:

- **Numbered / `rel=next` pagination:** fetch up to `profile.max_pages` (default 5), concatenate block trees under a synthetic root, deriving each item's `slot_key` from a **page-invariant content anchor** (not the page number), so an item moving from page 2 to page 1 keeps its key and doesn't diff as churn.
- **Infinite scroll** (Tier 2 only): drive `Page.scrollBy` until height stabilizes or `max_scrolls` (default 10), then serialize.
- **"Latest-N" feeds** (changelogs, job boards): `mode = "feed"` — segment top-level items, key by content-anchor (title/permalink hash); new items yield `added`, delistings yield `removed`; cap retained history at `feed_window` so old entries don't diff as "removed" when they scroll off.

**Multi-fetch failure handling (resolved).** A multi-page observation is **all-or-nothing**: if any required page (1…`max_pages` up to the first `rel=next` that 404s legitimately) fails with a soft error (5xx/timeout/DNS), the *entire observation* aborts with exit `3` and **no snapshot is stored** — we never diff a truncated doc against a full prior (which would emit a giant false `removed`). A page that returns a *legitimate* empty/last result (`rel=next` absent, 404 past the last page) is a normal terminus, not a failure. Forced inter-page spacing under per-host crawl-delay (§12) is the cost of the observation; for sources where 5 pages × 1 s floor is too slow, set `max_pages` lower or use the `feed` mode (which only fetches page 1 and diffs the top-N items).

The **`Profile`** is the per-source tuning seam every stage reads:

```toml
[profile.acme-pricing]
render        = "auto"          # auto | always | never
wait_for      = ".pricing-grid"
strategy      = "selector"      # readability | selector | full
root_selector = "main .pricing"
strip_attrs   = ["data-experiment-id"]
strip_text    = ['updated .* ago']
unordered     = [".masonry-grid"]   # children sorted by slot_key (render-determinism, §5.1)
mode          = "page"          # page | feed | paginated | infinite
max_pages     = 1
[profile.acme-pricing.types]
".plan .amount" = "price"
```

Profiles ship with **archetype presets** (`pricing`, `docs`, `changelog`, `status`, `jobs`, `filing`) — pick an archetype, override a field or two. Unprofiled URLs fall back to HTTP-first / readability / page mode and still produce a usable doc.

---

## 6. Change Event Schema

This is the artifact every agent consumes. A change event answers three questions in a fixed order so an agent can branch without parsing prose: **what changed** (`delta`), **why it matters** (`why`), **what to do** (`followup`).

### 6.1 Design rules

1. **Agents pay per token.** Default wire serialization is minified JSON with short, stable keys. **The canonical single-field price-change event (Example 1 below) is 792 bytes minified, which tokenizes to ~305 tokens** (measured on the exact bytes of Example 1: 304 cl100k tokens, 307 o200k tokens). The byte count includes a handful of multibyte UTF-8 glyphs (`—`, `›`, `→`) that inflate bytes relative to ASCII; the token count is the figure that matters for cost. **This ~305-token figure is the one used everywhere in this doc** — §1.2, §1.3, and §10 all reference it. A `--pretty` flag exists for humans. (A leaner event with ASCII-only summary and without the optional `q`/`title`/`params` fields runs ~200 tokens; ~305 is the fully-populated upper-typical case.)
2. **Never embed the whole page.** `delta` carries only the changed span plus a few words of anchor context. Full snapshots live in the content store, referenced by hash.
3. **Bounded size.** Every event has a hard ceiling: `delta.enc=block` truncates each side to ≤600 chars (`atrunc:true`), `idiff` truncates equal runs to ±6 tokens of context, and a cascade-clustered event (§7.4) caps `child_deltas[]` at `max_children` (default 32) — beyond that it degrades to a single summary delta (`enc:"struct"`, §6.3) plus a `truncated:N` count and a store pointer. **No single event exceeds ~8 KB / ~2k tokens**, so the token-savings guarantee has a worst-case upper bound on a bad day.
4. **Every field earns its place or is omitted.** *Presence-as-signal*: optional fields are absent (never `null`); absent keys cost zero bytes.
5. **Addresses are portable across re-renders** (the dual address below).
6. **One event = one semantically-coherent change.** A pricing table where 3 tiers changed is 3 events sharing a `batch`, so an agent routes each independently and salience is per-change.

### 6.2 Top-level shape

```jsonc
{
  "v": "1",                       // schema major version, string
  "id": "cfe_01J9...",            // event id, k-sortable ULID, "cfe_" prefix
  "src": { ... },                 // source identity (url + target id)
  "obs": "2026-06-02T14:03:11Z",  // observed_at, RFC3339 UTC
  "base": { ... },               // baseline/previous snapshot reference
  "seg": [ ... ],                // affected segment(s) + addresses (array; usually len 1)
  "ct": "modified",               // change_type enum (frozen in v1)
  "delta": { ... },              // THE DELTA — compact before→after
  "why": { ... },                // WHY IT MATTERS
  "followup": { ... },           // SUGGESTED FOLLOW-UP
  "conf": 0.94,                   // confidence the event is real & correctly classified
  "prov": { ... }                // provenance (fetch method, hashes, etag)
}
```

Short keys (`ct`, `conf`, `obs`, `prov`, `seg`) appear in *every* event so their byte savings compound across a feed; `delta`/`why`/`followup` are spelled out because they are the payload an agent reasons about and clarity beats 4 saved bytes.

**`src`** — `{ url, tid, title? }`. `tid` (the stable handle from `cf watch --id`) is the join key across time; `url` may change (redirects, locale) while `tid` stays put.

**`base`** — `{ obs, snap, rev }`. `snap` is a `blake3:<hex>` pointer into the content store (an agent wanting the full prior text calls `cf show <tid>@<rev>`); `rev` is a monotonic counter per `tid`. We do **not** bump `rev` when the content hash is unchanged.

**`seg`** — affected segments. **One coordinate is authoritative for joining; the rest are for display/fuzzy-relocate only:**

```jsonc
"seg": [{
  "anchor": "Pro plan",        // AUTHORITATIVE join anchor: nearest stable heading/label of the slot_key
  "fp": "blake3:4b9c1e",        // AUTHORITATIVE: 12-hex prefix of the block's slot_key (the §5.4 cross-obs key)
  "label_path": "Pricing › Pro Plan › price",  // HUMAN-READABLE breadcrumb for display/debug ONLY — never a join key
  "role": "price"               // price|date|version|status|link|prose|code|table-cell|nav|meta
}]
```

The **authoritative, stable join key per segment is `seg.fp`** (the `slot_key` prefix from §5.4) together with `seg.anchor`. `label_path` (renamed from the old `path`, which read like a CSS path and invited misuse) is a human breadcrumb for rendering and debugging **only** — it must never be used as a join key, because §5.4 rejects path-based identity. An agent that needs to correlate a segment across events keys on `fp`.

`seg` is an array because *reordered* changes inherently span ≥2 positions; common case is length 1.

**`ct`** — closed enum (frozen in v1): `added | removed | modified | reordered | restyled`. `restyled` (content identical, only markup/whitespace changed) is emitted at low `conf`/salience because *sometimes* it matters (a "Deprecated" strikethrough); agents usually filter it with `--min-salience low`.

### 6.3 THE DELTA — compact before→after

The encoding is chosen per segment by a size heuristic, signaled by `delta.enc`. Throughout, **`a` = after, `b` = before** (`a` first, because the new value is what the agent acts on).

| `enc` | When | Shape |
|------|------|-------|
| `val` | short atomic values (price, version, status, date) | `{ "enc":"val", "a":"$79/mo", "b":"$49/mo" }` |
| `idiff` | localized prose change | `{ "enc":"idiff", "ops":[["=","Rate limit is "],["-","100"],["+","60"],["=","req/s."]] }` |
| `block` | large / low-overlap rewrite | `{ "enc":"block", "a":"…new (≤600c)…", "b":"…old…", "atrunc":true }` |
| `move` | reorder | `{ "enc":"move", "from":7, "to":1, "key":"Outage in us-east-1" }` |
| `struct` | a set-change too large for per-child events (table/list mass change) | `{ "enc":"struct", "added":N, "removed":M, "modified":K, "sample":[…≤8 child deltas…], "truncated":T }` |

`idiff.ops` is the standard diff-match-patch `[op, text]` tuple form (`=` keep / `-` delete / `+` insert), with equal runs truncated to 6 words of context. We pick `block` over `idiff` when the inline diff serializes to ≥ 0.7× the block form (a near-total rewrite shouldn't pay diff overhead). For `added`/`removed`, only `a` or only `b` is present. `struct` is the bounded fallback (rule 3, §6.1) when a cascade cluster exceeds `max_children`.

### 6.4 WHY IT MATTERS

```jsonc
"why": {
  "sal": 0.86,                 // salience 0..1 (2 decimals) — the continuous filter knob
  "mat": "high",               // materiality bucket: none|low|medium|high|critical
  "cat": "price_increase",     // change category (controlled vocab)
  "summary": "Pro monthly price rose 34% ($29→$39)."  // <=160 chars; references the concrete delta
}
```

(The inner prose field is named `summary`, not `why`, so the JSONPath is `why.summary` rather than the awkward `why.why`.) Both `sal` (continuous) and `mat` (discrete) are emitted because a label is not recoverable from a float without re-deciding cutoffs.

**Default bands (and how overrides change comparability):**

| `mat` | default `sal` range | meaning |
|-------|-------------|---------|
| `critical` | ≥ 0.90 | act now / page a human |
| `high` | 0.70–0.89 | act this cycle |
| `medium` | 0.40–0.69 | worth knowing, queue it |
| `low` | 0.15–0.39 | log it |
| `none` | < 0.15 | suppressed by default |

**Authoritative-label rule (resolved inconsistency).** Rule packs (§8.3) may move the band cutoffs per target (a pricing page is deliberately twitchier). Therefore: **`mat` is the authoritative, cross-target-comparable signal an agent should route on; `sal` is comparable *only within a single target's pack*, not across targets.** An agent must **not** hard-code the default `sal→mat` table and apply it across targets — gate on `mat` (or on `sal` only for one known target). The pack's effective cutoffs are stamped in the event's `prov.pack` (pack id + content hash) so any score is reproducible. The earlier draft's claim that the cutoffs are "fixed and documented" applied only to the *default* pack; that is corrected here.

`cat` controlled vocabulary (open; unknown → `content_edit`): `price_increase`, `price_decrease`, `plan_added`, `plan_removed`, `api_breaking`, `api_deprecation`, `api_addition`, `incident_open`, `incident_update`, `incident_resolved`, `version_bump`, `availability_out`, `availability_in`, `legal_filing`, `job_posted`, `job_removed`, `content_edit`, `cosmetic`.

### 6.5 SUGGESTED FOLLOW-UP

Advice for the agent, **never auto-executed**. The split between `why.cat` (what changed) and `followup.act` (what to do) is deliberate — the mapping is *policy*, overridable per agent.

```jsonc
"followup": {
  "act": "re_run_downstream",  // closed AGENT-FACING enum (exactly 8 values), exactly one
  "tgt": "pricing-watchers",   // optional logical target (channel/queue/pipeline id), agent-resolved
  "params": { "urgency": "high" },
  "q": "Did the annual price or feature list change too?"  // optional question to chase
}
```

`act` is a **closed 8-action agent-facing taxonomy**, ordered by escalating cost, frozen in v1: `ignore`, `notify`, `refetch_linked`, `reembed_kb`, `re_run_downstream`, `open_ticket`, `escalate_human`, `page_oncall`. **An emitted event NEVER contains any other value** — so an integrator writes one exhaustive `match` over these eight with no default branch and is correct forever within v1.

`verify_llm` is **not** an agent-facing action and is **not** in the `act` enum. It is an *internal scoring state* (§8.4/§8.6): when the heuristic is uncertain, the scorer routes to the optional LLM tier, which **always replaces it with one of the eight real actions before emission**. An agent will never receive `verify_llm`. (The earlier draft listed a "9-action" enum including `verify_llm`; that conflated an internal state with the wire contract and is corrected.)

### 6.6 Confidence & provenance

**`conf` is a computed score, not a vibe (now fully specified).** `conf ∈ [0,1]` answers *"are we sure this event is real and correctly typed?"* — orthogonal to salience (*does it matter?*). It is deterministic given the observation pair and is the product of five multiplicative factors, each in `[0,1]`:

```
conf = c_fetch · c_align · c_match · c_parse · c_stability
```

| factor | meaning | formula / values |
|---|---|---|
| `c_fetch` | tier reliability | `http/api = 1.00`, `rss = 0.95`, `headless = 0.85` (a render is inherently less certain) |
| `c_align` | how cleanly the block aligned | `1.0` if joined by explicit-anchor `slot_key`; `0.9` if by struct slot; for a similarity-matched pair, `= sim` (§7.1) |
| `c_match` | edit localization quality | `1.0` for a `val`/typed-value change; for prose, `min(1, overlap/τ_match)` so a near-total rewrite (low overlap) lowers conf |
| `c_parse` | type-parse certainty | `1.0` if the typed `value` parsed cleanly; `0.7` if the type was inferred ambiguously; `1.0` for untyped prose |
| `c_stability` | observation-level sanity | `0.6` if `align_rate < 0.9` for the whole doc (a suspected redesign/garbled fetch); `1.0` otherwise |

Worked: the Example-1 price change is `1.00 · 1.00 · 1.00 · 1.00 · 1.00`, but headless lowers `c_fetch` to `0.85` and an ambiguous reorder lowers `c_align` to `sim≈0.6`. A redesign with a huge low-overlap change gets `c_match·c_stability` near `0.6·0.6`, surfacing as a real-but-uncertain event the agent verifies. `c_align`/`c_match`/`c_stability` are all replayable offline from the stored snapshot pair (no clock, no network), satisfying the determinism guarantee. Agents commonly gate on both (`sal>0.7 && conf>0.6`); low `conf` is the primary trigger for the internal `verify_llm` state (§8.6).

**`prov`** — `{ m, hash, etag?, status, ms?, pack? }` where `m ∈ http|headless|api|rss`. `hash` (this observation's snapshot) becomes the *next* event's `base.snap`. `pack` carries the rule-pack id + content hash + scorer version so any score is reproducible.

### 6.7 Three full examples

**Example 1 — price change (the canonical case).** On the wire this is one minified line; **measured at 792 bytes ≈ 305 tokens** (the figure used throughout this doc).

```jsonc
{
  "v": "1",
  "id": "cfe_01J9Z4K7QH8M2N3P4R5S6T7V8W",
  "src": { "url": "https://acme.com/pricing", "tid": "acme-pricing", "title": "Pricing — Acme" },
  "obs": "2026-06-02T14:03:11Z",
  "base": { "obs": "2026-05-26T14:01:55Z", "snap": "blake3:9f2c5d…e1", "rev": 41 },
  "seg": [{ "anchor": "Pro plan", "fp": "blake3:4b9c1e", "label_path": "Pricing › Pro Plan › price", "role": "price" }],
  "ct": "modified",
  "delta": { "enc": "val", "a": "$39/mo", "b": "$29/mo" },
  "why": { "sal": 0.86, "mat": "high", "cat": "price_increase", "summary": "Pro monthly price rose 34% ($29→$39)." },
  "followup": { "act": "re_run_downstream", "tgt": "pricing-watchers", "params": { "urgency": "high" }, "q": "Did the annual price or feature list change too?" },
  "conf": 0.97,
  "prov": { "m": "http", "hash": "blake3:1c8a72…d4", "etag": "W/\"3f1-9aXc\"", "status": 200, "pack": "pricing@b3:2f1a" }
}
```

**Example 2 — docs / API breaking change.**

```jsonc
{
  "v": "1",
  "id": "cfe_01J9Z4M0XB2C4D6E8F0G2H4J6K",
  "src": { "url": "https://api.acme.com/docs/v2/users", "tid": "acme-api-users", "title": "Users API — Acme Docs" },
  "obs": "2026-06-02T14:03:12Z",
  "base": { "obs": "2026-05-30T09:00:00Z", "snap": "blake3:77af10…0a", "rev": 7 },
  "seg": [{ "anchor": "per_page", "fp": "blake3:c0de41", "label_path": "Users API › Parameters › per_page", "role": "table-cell" }],
  "ct": "modified",
  "delta": { "enc": "idiff", "ops": [
      ["=", "Default "], ["-", "100"], ["+", "25"], ["=", ", max "], ["-", "1000"], ["+", "100"], ["=", " per page."]
  ] },
  "why": { "sal": 0.93, "mat": "critical", "cat": "api_breaking",
           "summary": "per_page max cut 1000→100; clients paging at >100 will now error/truncate." },
  "followup": { "act": "open_ticket", "params": { "system": "jira", "priority": "P1" },
                "q": "Do any of our integration calls request per_page > 100?" },
  "conf": 0.85,
  "prov": { "m": "headless", "hash": "blake3:5b1e9c…77", "status": 200, "pack": "api-docs@b3:7c33" }
}
```

**Example 3 — status-page incident (added segment).**

```jsonc
{
  "v": "1",
  "id": "cfe_01J9Z4N9YA1B3C5D7E9F1G3H5J",
  "src": { "url": "https://status.acme.com", "tid": "acme-status", "title": "Acme Status" },
  "obs": "2026-06-02T14:03:09Z",
  "base": { "obs": "2026-06-02T13:58:09Z", "snap": "blake3:aa01…9c", "rev": 2207 },
  "seg": [{ "anchor": "Investigating", "fp": "blake3:7e1a02", "label_path": "Incidents › (latest)", "role": "status" }],
  "ct": "added",
  "delta": { "enc": "block", "a": "Investigating — Elevated API error rates in us-east-1. We are investigating reports of 5xx errors. Posted 14:02 UTC." },
  "why": { "sal": 0.95, "mat": "critical", "cat": "incident_open",
           "summary": "New unresolved incident: elevated 5xx in us-east-1 (Investigating)." },
  "followup": { "act": "page_oncall", "tgt": "oncall", "params": { "to": "platform-oncall" },
                "q": "Are our us-east-1 services seeing correlated errors?" },
  "conf": 0.99,
  "prov": { "m": "api", "hash": "blake3:c4d2…01", "etag": "\"2207\"", "status": 200, "pack": "status-page@b3:9aa1" }
}
```

### 6.8 The feed envelope, the no-change result & first-observation

One crawl of one target produces zero or more events, wrapped so crawl-level metadata is sent once:

```jsonc
{
  "v": "1", "feed": "acme-pricing", "batch": "cfb_01J9Z4K7QH…", "obs": "2026-06-02T14:03:11Z",
  "crawl": { "from_rev": 41, "to_rev": 42, "url": "https://acme.com/pricing",
             "m": "http", "status": 200, "ms": 812, "hash": "blake3:1c8a72…d4", "n": 3 },
  "events": [ /* events MAY omit prov.m/prov.status/src.url when equal to crawl-level values */ ],
  "next": "2026-06-02T20:03:11Z", "next_cursor": "cur_01J9…"
}
```

`cf feed --standalone` disables this dedup for consumers that process events out of context. A **no-change** crawl still emits a result *to the daemon event log* (silence is ambiguous), but a one-shot `cf check` does **not** print it (exit `0`, empty stdout — the cheap path):

```jsonc
{ "v":"1", "feed":"acme-pricing", "batch":"cfb_01J9…", "obs":"2026-06-02T14:03:11Z",
  "crawl": { "from_rev":41, "to_rev":41, "status":200, "m":"http", "etag_hit":true, "n":0 },
  "events": [] }
```

`to_rev == from_rev` with `n:0` is the unambiguous "nothing changed" signal; `etag_hit:true` records a ~0-byte 304 short-circuit. **Fetch failures are distinct**: `crawl.status ≥ 400` (or `crawl.err`) with `events:[]`. As pinned in §4.5, **an agent never needs this envelope to tell no-change from failure on `cf check` — the exit code (`0` vs `3`) is sufficient**; the envelope exists for the daemon log and `cf feed`.

**First-observation (exit `11`) contract.** `cf check` on exit `11` prints a minimal baseline envelope so the branch has a defined shape:

```jsonc
{ "v":"1", "feed":"acme-pricing", "batch":"cfb_01J9…", "obs":"2026-06-02T14:03:11Z",
  "crawl": { "from_rev":null, "to_rev":1, "status":200, "m":"http", "hash":"blake3:1c8a72…d4", "n":0, "baseline":true },
  "events": [] }
```

`baseline:true` with `from_rev:null` tells the agent "first time seen, nothing to diff."

### 6.9 Versioning

`v` is the schema major version (a string), on every event and envelope. **Additive changes never bump the major:** new optional fields, new open-vocab values (`why.cat`, `seg.role`) are minor; **consumers MUST ignore unknown fields and tolerate unknown enum values** (treat unknown `cat` as `content_edit`). **Closed enums (`ct`, the 8-value `followup.act`) are frozen within a major version**, so agents write default-less exhaustive switches. Exit codes follow the same compatibility policy (§4.5). The JSON Schema ships at `cf schema --version 1` and `changefeed.dev/schema/v1.json`, with the `$defs` the MCP `$ref`s point at (§4.10).

---

## 7. Diff & Noise Suppression

The heart of `changefeed`. Given `old_tree` and `new_tree` (canonical block-trees from §5), emit a clean set of `{added, removed, modified, reordered}` ops with **cosmetic noise suppressed**, and hand a scored-ready changeset to salience. Stages: **(0) ignore masking → (1) alignment → (2) intra-block diff → (3) noise classification → (4, Phase 2) stability/debounce/dedup.** Before any of this runs, the `doc_hash`-equal short-circuit (§5.6) skips the whole engine when nothing semantic changed. **All cross-observation joins in this section use `slot_key` (§2.1/§5.4)** — never `block_id`, never a path.

### 7.0 Ignore masking (cheap, runs first)

Masked blocks are excluded from alignment entirely. Rules come from per-target config (`ignore` in `changefeed.toml`, §4.9) and, in Phase 2, learned auto-rules (§7.4), applied identically with a `source: user|auto` tag (auto rules are always overridable via `cf rules`):

- `selector` matches the block → drops the block.
- `regex` matching only *part* of a block's text → **redacts** the matched span with sentinel `￼` before hashing (so "Last updated: 2026-06-02" stops flapping but the surrounding sentence still diffs).
- `attr` → strips the named attribute before the block hash is computed.

### 7.1 Block alignment (slot_key-anchor → LIS → similarity-fill)

We diff the **pre-order block sequence**, joining on `slot_key`, then reconcile positions to detect moves — full Zhang-Shasha tree-edit-distance is O(n²·depth²) and unnecessary. Three phases:

1. **Stable-key anchor matching.** Pair blocks whose `slot_key` is identical and appears exactly once on each side — a *certain* anchor. Two passes, O(B). Because `slot_key` is text-free (§5.4), **a block whose text was edited still anchors here** (its key is unchanged), so a `modified` block is matched in this phase, not pushed to similarity fallback — this corrects the earlier draft's claim that the *content* ID drove this phase. Similarity fallback (phase 3) is only for blocks whose `slot_key` itself changed (a section was renamed, an ordinal shifted) or genuinely new/removed blocks.
2. **LIS over anchors fixes ordering.** Patience-sort the anchors' new-positions and take the longest increasing subsequence (O(A log A)). Anchors on the LIS are the stable spine; anchors off it are matched-but-moved → candidate `reordered` ops — exactly `git`'s histogram/patience anchoring.
3. **Similarity fill for the residual.** Within each gap between spine anchors: first pair equal `norm_hash` blocks; then, for blocks whose `slot_key` changed, restrict candidates to the same block `type`, a position window `±W` (32), and a band from a MinHash/SimHash **LSH index** (so we never score all-pairs). Score:

   ```
   sim(a,b) = 0.55·token_jaccard(a,b) + 0.30·(1 − norm_levenshtein(a,b)) + 0.15·slot_affinity(a,b)
   ```

   A pair is accepted as `modified` iff `sim ≥ τ_match` (**0.62**; **0.75** for short `table_row`/`kv` types). Below threshold we report a clean `removed`+`added`. τ=0.62 reads a one-sentence rewrite (~70–85% overlap) as `modified` and a wholesale replacement (<50%) as add/remove.

**The LSH index is built per-diff, not persisted.** It is an in-memory MinHash signature table over the two trees' residual blocks, constructed in O(B) at diff time and discarded after — it never touches the storage layer. ("Index" here means a transient in-RAM structure, not an on-disk one.)

**Low-anchor archetypes (the honest worst case).** The structured archetypes the tool targets — pricing grids of 8 near-identical tier cards, status component lists, job boards — have *few globally-unique text anchors*. But `slot_key` is text-free, so they still anchor cleanly: the 8 tier cards have 8 *distinct* `slot:struct(Pricing›Tier-N, …)` keys by section/ordinal, and phase 1 resolves them directly **without needing unique text**. The pathological case is the **rename-everything redesign**, where many `slot_key`s change at once: then phases 1–2 resolve little and almost everything falls to LSH-banded similarity. Worst-case complexity is therefore **O(B) hashing + O(A log A) LIS + O(B·b) similarity** where `b` is the bounded LSH band size (a small constant, ≤ a few dozen candidates per residual block via window `±W` and the LSH band) — i.e. **O(B log B) in practice, never O(B²)**, because the window+LSH band caps candidates per block regardless of how few anchors survive. When the whole document is one low-anchor gap, `move_min` is moot (we don't emit reorders without anchors) and we fall back to set-diff within the gap.

A matched pair whose `slot_affinity < 0.5` is tagged `moved`. We only *emit* a standalone `reordered` event when the moved block is itself salient or a contiguous run ≥ `move_min` (3) blocks moved together.

**Performance posture (target, not measured).** A 5k-block page is *targeted* to diff in single-digit milliseconds on the high-anchor path and low-tens of milliseconds on the low-anchor (LSH-bounded) path; these are engineering targets to be confirmed by benchmark, not measured facts. The storage numbers in §9 are the only measured figures in this doc.

### 7.2 Intra-block text diff

For each `modified` pair we produce a compact structured delta. Tokenize into words + punctuation + whitespace-runs (whitespace tokens are diffed but **never reported**), run **Myers O(ND)** on the token sequences, coalesce adjacent edits into spans, and truncate context to **±6 tokens** with `…` elision. **Attribute deltas** (`href`/`src`/`datetime`) are emitted separately from text, and **compared on their post-normalization (canonicalized) values** (§5.3) — so a `utm`-only URL change is suppressed while a real host/CDN swap is reported. This maps to `delta.idiff`/`delta.val` (§6.3).

### 7.3 Noise classification

Each surviving op gets a `noise_score ∈ [0,1]` (1 = almost certainly cosmetic), passed to salience as a **soft negative feature**, not a hard gate. An op is tagged noise when: (1) whitespace/case-only after stripping reported whitespace tokens; (2) the only changed tokens are numbers matching volatile patterns (view counts, relative timestamps, residual cache-busters) AND the block is not a salient-numeric type (a number in a `kv` keyed `/price|cost|qty|stock|version/` is **never** noise; a bare number next to "viewing"/"online now"/"ago" is); (3) reorder-only with no content change; (4) add/remove of `media`/`link` blocks in regions Stage 4 marked volatile.

### 7.4 Stability model, debounce & dedup — **Phase 2, daemon-only**

This is what makes changefeed quiet *over time*. **It requires held cross-invocation state and is therefore a Phase-2, daemon-owned feature; one-shot `cf check` does NOT debounce and does NOT auto-learn volatility** (see the MVP-quietness note at the end of this section). In MVP, quietness comes from §5.3 normalization + static `ignore` rules + idempotency dedup only.

**Per-segment flap tracking keys on `slot_key`** (the §2.1 cross-observation key — *not* a CSS selector, *not* `block_id`). For each `slot_key` that has ever changed we persist a tiny record:

```
SegmentStat { slot_key:[u8;12], observations:u32, change_count:u32,
              flap_ewma:f32 (α=0.30), distinct_vals:HLL(p=10) (~1KB),
              last_values:ring<u64,8>, last_changed_at, first_seen_at }
```

~64 B + 1 KB HLL per tracked segment; a 5k-block page is typically a few hundred records — stored in a compact SQLite **stats sidecar**, so stability tracking costs bytes proportional to *changed segments × fetches*, not page size. A `slot_key` with `flap_ewma > 0.6` over `observations ≥ 8` is **auto-volatile** (conservative gates so a status page during an incident isn't silenced after two fetches). Split by distinct-value cardinality:

- **High flap + high HLL-distinct** (a counter/clock/nonce that slipped past §5.3): redact numeric spans and auto-ignore the `slot_key`, emitting a one-time `auto_ignore` notice.
- **High flap + low distinct** (ring shows cycling among 3–4 values → A/B rotation): **canonicalize** to the lexicographically-min representative and diff against that, so only a genuinely *new* (5th) variant fires.

**Debounce (daemon-only).** A changed segment's event is held in daemon memory for `--debounce` (default 2 fetches / 10 m). If it reverts to a previously-seen `norm_hash` within the window it's cancelled (a flap); if it settles it fires once describing old→final — collapsing `A→B→A→B→C` into one `A→C`. **Because hold-state lives only in the daemon, one-shot `cf check` emits immediately and never holds** (a `check` that observes a change returns `10` now, not after a debounce). An agent wanting debounce runs the daemon and reads `cf feed --tail`.

**Dedup (works in both modes).** (a) **Idempotency key** `event_key = xxh3(target_id ‖ slot_key ‖ from_norm_hash ‖ to_norm_hash)`, in a rolling per-target seen-set (N=1000) persisted in the store — re-running a default `cf check` on the same snapshot pair emits **zero** events (this is what `--no-store`/`--peek` deliberately bypass, §4.4). (b) **Cascade clustering** collapses co-located ops (8 re-rendered table rows under one `<table>`) into one event with `child_deltas[]` (capped at `max_children`, then `enc:"struct"`, §6.1/§6.3).

**Novelty window.** `novelty` (§7.5) is computed against the per-`slot_key` `last_values` ring (8 deep) *and* the `distinct_vals` HLL within the retention window (§9.4) — so a revert to a value last seen a month ago (still within retention) scores `novelty<1` and is recognized as a revert, not "brand-new"; a value evicted past retention scores `novelty=1`. The window is therefore exactly the snapshot retention window.

**MVP quietness expectation (stated plainly for integrators).** Without the Phase-2 stability model, MVP relies on §5.3 normalization (which already eliminates the dominant nonce/timestamp/counter false-positive classes *before hashing*) + static `ignore` rules + idempotency dedup. Expected residual flap in MVP: pages with *unanticipated* per-poll churn not covered by a static `ignore` rule (a novel A/B test, an un-typed live counter) will emit low-`mat` `content_edit`/`cosmetic` events that an agent filters with `--min-salience medium`. MVP is adoptable for the structured archetypes (pricing/docs/status/changelog) where normalization + archetype `ignore` packs cover the known churn; very twitchy bespoke pages benefit materially from Phase 2.

### 7.5 The changeset handed to salience

By the time bytes reach salience they are de-noised, de-duped, (Phase 2) debounced, and compacted:

```json
{
  "target_id": "stripe-pricing", "observed_at": "2026-06-02T14:03:11Z",
  "from_snapshot": "blake3:1a2b…", "to_snapshot": "blake3:9f0e…",
  "clusters": [{
    "cluster_id": "c1", "anchor_slot": "slot:struct:pro-price",
    "ops": ["modified"], "child_deltas": [ /* §7.2 text_delta objects */ ],
    "features": {
      "kind": "modified", "block_types": ["kv","table_row"],
      "numeric_change": {"field":"price","from":20,"to":25,"pct":0.25},
      "tokens_changed": 2, "magnitude": 0.07, "noise_score": 0.05,
      "segment_stability": 0.98,   // 1 − flap_ewma (Phase 2; defaults to 1.0 in MVP)
      "novelty": 1.0,              // §7.4 novelty window
      "moved": false
    }
  }],
  "suppressed": { "noise": 14, "debounced": 2, "auto_ignored_segments": 3 }
}
```

The decisive handoff features are **`segment_stability`** (Phase 2; the inverse of flap rate, defaulting to 1.0 in MVP since there is no learned volatility), **`novelty`**, **`magnitude`/`numeric_change`**, and **`noise_score`** (soft negative weight).

---

## 8. Salience & Follow-up

This layer answers questions (2) and (3): a deterministic `salience ∈ [0,1]` plus a discrete `materiality` label, and a single `action` from the closed agent-facing taxonomy. **The tool is fully useful with zero model calls** — every event gets a score and action from pure local computation. The LLM is an optional escalation tier (§8.6).

### 8.1 The deterministic signals (seven core; eighth is Phase-2)

Each signal is normalized to `[0,1]`, computed per block op from the changeset (§7.5). **The `input source` column states exactly which stage/tier produces each signal** — flagging the two signals reviewers caught as under-specified (`pos` and `date`).

| id | signal | input source | what it captures |
|----|--------|------|------------------|
| `type` | **block-type weight** (`w_type`) | block `type` (§5.5), all tiers | domain prior: `price=1.00, date=0.85, heading=0.80, code_span=0.70, table_row=0.75, list_item=0.55, link=0.50, paragraph=0.45, image_alt=0.20, nav=0.10, footer_boiler=0.05` (rule-pack overridable) |
| `mag` | **magnitude** (`w_mag`) | §7.2 token diff / typed `value`, all tiers | token-level (not char-level) edit ratio; text: `1 − exp(−3r)`; numeric: `clamp(\|after−before\|/max(\|before\|,ε),0,1)`. `$49→$79` = 0.61; `$49.00→$49.01` ≈ noise |
| `num` | **numeric/price delta** | typed `value` (§5.5), all tiers | fires on `number`/`currency`/`percent`; carries direction (`+`/`−`) so up vs down branch differently |
| `date` | **date delta (magnitude only in core)** | parsed `date` value (§5.5), all tiers | **Core, deterministic:** scored by the *size* of the date shift (e.g. a deadline moved by 60 days scores higher than 1 day), with **no reference to the current clock** — fully reproducible offline. The "how soon is the new date" weighting that needs `now()` is an **opt-in enrichment** (`salience.date_proximity=true`), explicitly outside the deterministic core and stamped `non_reproducible:true` in `explanation` when used (see §8.5). Default off. |
| `neg` | **negation / polarity flip** | §7.2 token diff dictionary, all tiers | meaning-inverting tokens (`no longer`, `deprecated`, `removed`, `unsupported`, `sold out`, `end-of-life`, `breaking`, `required`). A flip sets `w_neg=1.0` regardless of byte size |
| `pos` | **prominence** (`w_pos`) | see fallback note below | how visually prominent the changed block is |
| `kw` | **keyword rule-pack hit** | active pack regex set (§8.3), all tiers | block text matched against the pack's regex; a hit attaches the matched rule id for explainability |
| `vol` | **volatility damping** (`w_vol`) — **Phase 2 only** | per-`slot_key` flap EWMA (§7.4, daemon) | `w_vol = 1/(1+change_rate)`, applied **multiplicatively**. **Unavailable in MVP** (no learned volatility); MVP sets `w_vol=1.0`, so MVP ships **seven** scoring signals, not eight. The earlier draft's "eight signals in MVP" was wrong because `vol`'s input is the Phase-2 stability model. |

**`pos` input source and its honest weakness.** `pos` needs visual prominence, which only the **Tier-2 render** can supply directly (CDP layout → `viewport_rank = rank of the block's bounding box top, normalized`). **On the default Tier-1 HTTP path there is no layout**, so `pos` falls back to a structural proxy: `pos = 0.5·(1 − dom_depth/max_depth) + 0.5·(1 − preorder_index/block_count)` — i.e. shallow and early-in-document blocks score higher. We state plainly that this is a *weak* proxy (a visually-prominent block placed late in source order, common with CSS grid/flex reordering, is underweighted). `pos` is therefore a **secondary signal with low affinity** (`a_pos=0.5`, §8.2) so a bad proxy can never *alone* drive materiality; it only breaks ties among otherwise-comparable changes. When Tier-2 render is in use, the true `viewport_rank` replaces the proxy and `explanation` records which was used.

Token-level magnitude makes whitespace/attribute/minifier churn invisible.

### 8.2 Combining → score + materiality

Materiality is **disjunctive** — any one decisive signal (a price moved, a polarity flipped) must make the block material regardless of the quiet signals. So we use a **noisy-OR** combiner, with volatility damping applied multiplicatively:

```
raw_block   = 1 − Π_i (1 − a_i · s_i)          # noisy-OR over the 6 core scoring signals (type,mag,num,date,neg,kw)
                                               #   (pos is included as a 7th when present; see a_pos below)
block_score = raw_block · w_vol                # volatility damps, never boosts (w_vol=1.0 in MVP)
page_score  = 1 − Π_{b ∈ top_k} (1 − block_score_b)   # k=5
```

Default affinities `a_i`: `type 0.6, mag 0.7, num 0.9, date 0.85, neg 1.0, pos 0.5, kw 0.95` (pack-overridable). `neg=1.0` means a real polarity flip alone drives the score to critical. `page_score` bands into `materiality` per §6.4 (rule-pack-overridable; remember `mat` — not `sal` — is the cross-target-comparable label, §6.4).

**Latency posture.** The classify stage is *targeted* at well under a millisecond per event for the common case (a handful of clusters, regex set in the low hundreds), but this is an engineering target to confirm by benchmark, not a measured fact; the rule-pack regex pass over many changed blocks is the dominant term and is bounded by the cluster count, not page size.

### 8.3 Rule packs (declarative TOML, layered)

A rule pack is pure data — no code — resolved last-wins: **built-in archetype defaults → user archetype override → per-target pack**. Diffable, reviewable, community-shippable (`cf pack add github:changefeed/packs/sec-filings`). Shipped v1 archetypes (MVP ships `pricing`, `api-docs`, `status-page`; the rest land in Phase 3): `pricing`, `api-docs`, `release-notes`, `status-page`, `regulatory`, `job-posting`, `changelog`, `inventory`, `default`.

```toml
# packs/pricing.toml
extends = "default"
schema  = 1
[block_type_weight]
price = 1.00
table_row = 0.90
[materiality.bands]          # pricing is twitchy on purpose — these override the default cutoffs,
high = 0.55                  # which is why `mat` (not `sal`) is the cross-target signal (§6.4)
critical = 0.80
[[rule]]
id     = "plan.removed"
match  = "(?i)\\b(no longer available|discontinued|sunset|legacy plan)\\b"
weight = 1.0
action = "escalate_human"
sticky = true                # always escalate regardless of score banding
```

```toml
# packs/status-page.toml
[[rule]]
id     = "incident.open"
match  = "(?i)\\b(degraded|partial outage|major outage|investigating)\\b"
weight = 1.0
action = "page_oncall"
sticky = true
```

### 8.4 Mapping a delta → one action

Deterministic, first-match-wins, fully explainable, against the **8-action agent-facing taxonomy** (§6.5). The `verify_llm` *internal state* is resolved before emission (step 4 → §8.6) and never appears in an event:

1. **Sticky rule hit?** → use that rule's action (bypasses banding).
2. **Else any matched rule with an explicit action?** → take the **highest-cost** action among them.
3. **Else the band-default map** (per-pack overridable): `none→ignore, low→notify, medium→notify, high→re_run_downstream, critical→escalate_human`.
4. **Uncertainty gate (internal):** if `sal ∈ [verify.lo=0.45, verify.hi=0.75]` AND the chosen action's cost ≥ `re_run_downstream` AND `llm.enabled`, mark the *internal* state `verify_llm` and route to §8.6, **which returns one of the eight real actions**. If `llm.enabled=false` (the default, and always in MVP), this gate is inert and the step-3 action stands.

### 8.5 Determinism & explainability (non-negotiable)

Same delta in → same `sal`/`mat`/`act` out, on any machine, **offline** — the core scorer takes no clock, no network, no randomness. The **one** clock-using path, the optional `date_proximity` enrichment (§8.1), is *opt-in*, default-off, and any event whose score used it carries `explanation.non_reproducible:true` and the wall-clock it used, so its non-reproducibility is explicit rather than hidden. Rule-pack version (`schema` + content hash) and scorer version are stamped into every event (`prov.pack`), so any pure-core score is reproducible later. `cf explain <event_id>` replays the scoring as a table; `cf score --dry-run <prev> <curr>` is the tuning loop.

```json
"explanation": {
  "top_signals": [
    {"signal":"num","value":0.61,"block":"slot:struct:pro-price","detail":"49→79 (+61%)"},
    {"signal":"type","value":1.00,"block":"slot:struct:pro-price"},
    {"signal":"pos","value":0.88,"block":"slot:struct:pro-price","detail":"proxy:dom_depth (Tier-1)"}
  ],
  "matched_rules": ["price.tier.change"],
  "damped_by_volatility": 0.0,
  "decided_by": "rule",
  "non_reproducible": false
}
```

### 8.6 Optional LLM escalation (off by default, cheap, cached)

The LLM tier disambiguates deltas the heuristics can't confidently classify but that are expensive to mishandle. Off by default, gated three ways:

1. **Eligibility:** only deltas where `sal ∈ [0.45, 0.75]` AND heuristic action cost ≥ `re_run_downstream` AND `llm.enabled`. Pure noise and obvious criticals never call the model.
2. **Verdict cache:** keyed by `delta_hash = blake3(canonical(ops) ‖ pack_hash ‖ model_id)` (128-bit), in `verdicts.db` (SQLite). A flapping `$49↔$79` is classified **once**.
3. **Hard budget:** `llm.max_calls_per_run` / `llm.max_calls_per_day`; on exhaustion we **fail safe** to the heuristic action (never silently downgrade a critical).

The model never sees the whole page — only the changed blocks, the heuristic's guess, and the rule context. It *adjudicates*, returning a strict tiny contract (`{material, materiality, action, confidence, reason}`, ~40 tokens). The verdict's `action` is validated against the **8 agent-facing actions** (it can never be `verify_llm`); on parse failure or `confidence < 0.5` we discard it and keep the heuristic result. The LLM can only re-label within the taxonomy — it cannot invent actions, raise spend, or override a `sticky` rule. `decided_by` flips to `"llm"` and the `reason` is attached.

Net effect: a watcher polling 500 pages every 15 minutes makes **zero** model calls on the ~98% of polls that are no-change or obviously classified, and at most a handful of cached calls on the genuinely-ambiguous remainder.

---

## 9. Caching & Storage

The engine room. Everything above this layer is stateless transformation; this is the only thing that persists, and its footprint is the cost that grows with `(# URLs × change frequency × retention)`. **Note "change frequency," not "poll frequency": because of the §5.3-normalized `doc_hash` short-circuit, a no-change poll stores *zero bytes*.**

### 9.0 Two corpora — and which one the pipeline actually stores

Reviewers correctly caught that the headline numbers must be tied to a precisely-defined corpus. There are **two**, and we report both:

- **Corpus A — raw HTML upper bound (the worst case, pre-normalization).** 720 raw HTML versions of a synthetic 124 KB pricing page watched hourly for 30 days (89.56 MB raw), where each hour's HTML differs *only* by a rotating footer session token plus, at hour 300, one real price change. This is the corpus the compression table in §9.3 is measured on. **It is an upper bound, NOT what a deployed instance stores**, because §5.3 strips that footer token *before* hashing.
- **Corpus B — what the pipeline actually ingests (post-normalization).** After §5.3 normalization, the rotating footer token is gone from the canonical tree, so **719 of the 720 hours are `doc_hash`-equal and store ZERO bytes** (no diff, no snapshot — §5.6). The store contains the **initial baseline + exactly one changed version (the price change) = 2 stored `CanonicalDoc` versions**, plus the per-segment stats sidecar. The realistic 30-day footprint for this page is therefore **the baseline blob + one ~73-byte delta + the raw-HTML baseline blob**, on the order of **tens of KB total for the month**, dominated by the two full baselines, not by 720 versions.

The two corpora answer different questions: **Corpus A demonstrates the codec/dedup machinery's behavior when content genuinely changes every version** (the relevant model for a *truly* churning page — a changelog that adds an entry every hour, or a status page mid-incident); **Corpus B is what a steady page costs in production.** Every ratio below is labeled with its corpus. The earlier draft quoted Corpus-A ratios as if general — corrected here.

**Realistic-corpus caveat.** On a genuinely churning page (every poll adds real content), dedup ratios collapse toward 1× and the per-observation cost approaches the *new-content* size, not ~66 bytes. Corpus A's 75× dedup is an artifact of one block changing across 720 otherwise-identical versions; a page where every block is new every hour would store ~full size each time (still delta-compressed against the prior version, but with little to share). **The tool's storage promise is precisely "you pay for change, not for page" — so a page that genuinely changes a lot genuinely costs a lot, by design.**

### 9.0.1 Design principle: store the structure, address the content, chain the deltas

Three orthogonal ideas, each attacking a different axis of waste; they compose **multiplicatively** (this is the **Phase-2 fleet-scale** engine; MVP storage is the far simpler model in §9.8):

1. **Block decomposition** kills *intra-version* redundancy and gives the diff engine its primitive.
2. **Content-addressed storage (CAS)** kills *inter-version and inter-page* redundancy.
3. **Delta-chaining + a trained dictionary** kills the *residual* redundancy in the bytes we do store.

### 9.1 WHAT to store: the fidelity spectrum

| Tier | Stored per *changed* observation | Detect *that* it changed? | Content (text) diff? | Re-render full page? | Storage / changed obs |
|---|---|---|---|---|---|
| **T0 — hashes only** (`--fidelity=hashes`, ultralight) | one 16-byte page digest | yes | no | no | **16 B** |
| **T1 — skeleton + block hashes** | block-tree shape + per-block 16 B hash + slot_key/labels | yes, *and which block* | structural only | no | **~1–4 KB** |
| **T2 — skeleton + block values** | T1 + each block's text, in CAS | yes | full word/line diff | canonical text | **~50–300 B after dedup+delta** |
| **T3 — full raw HTML** | original response bytes | yes | full byte diff | exactly | **~60 B (noise) – ~2 KB (cold)** |

**The insight that makes the default cheap:** you store on *change*, not on *poll* (Corpus B). For a changed observation you need the *current* full content (so the next observation can diff against it) plus the *changed* blocks; unchanged blocks already resolve to existing CAS objects.

**DEFAULT = `--fidelity=full` backed by CAS** (T3 content as a T1 manifest + deduped delta-chained blocks). Drop to `--fidelity=skeleton` for enormous (>2 MB) or legally-un-cacheable pages, or `--fidelity=hashes` for ultralight (§9.7).

#### The canonical block-tree node (the unit of both diffing and CAS dedup)

Same `CanonicalDoc` block from §5.6, addressed for storage:

```jsonc
{
  "slot_key": "slot:struct:9f2c…",  // §5.4 cross-observation key (also the dedup-stable identity)
  "kind": "block",                 // block | list | leaf
  "tag": "section",
  "text": "Team\n$29/mo\n…",       // normalized visible text (NFC, collapsed ws — VOLATILE TOKENS ALREADY STRIPPED §5.3)
  "hash": "b3:9f2c…"               // BLAKE3(slot_key ‖ kind ‖ tag ‖ norm_text), truncated to 128 bits
}
```

Canonicalization (the §5.3 rules) is what makes the hashes *semantic*: because volatile tokens are stripped *before* the hash, two observations differing only by a nonce produce **identical** block hashes and an identical `doc_hash` (and store nothing — §5.6). The `slot_key` is what lets us recognize "the Team block moved from position 2 to 3" as a reorder, not a delete+add.

**Why BLAKE3 truncated to 128 bits (16-byte digests).** ~5–10× faster than SHA-256 and 16-byte digests halve every manifest and ref. 128 bits is collision-safe to a corpus of <2⁴⁰ blocks (birthday bound ~2⁶⁴, collision prob ~2⁻⁴⁸ at 2⁴⁰). **Cross-page blast radius (honest):** because blobs dedup *globally* across targets/domains, a large multi-tenant store crosses 2⁴⁰ blocks faster than a single watch, and a (vanishingly improbable) collision would mis-serve a *different* page's block. For stores expected to exceed ~2³⁸ blocks we support `--hash-bits 256` per store (config-time, immutable thereafter); `cf fsck` re-hashes and detects any corruption. The `b3:`/`b3-256:` prefix versions the scheme.

### 9.2 The git-like object model & the dedup win

Three content-addressed object types:

```
blob      := one canonicalized block's bytes              (addressed by BLAKE3-128)
manifest  := an ordered list of (slot_key, blob-hash)     for ONE observation (a snapshot)
ref       := watch-id -> head manifest hash + append-only version log
```

A **blob** is one block, stored once globally. A **manifest** is the recipe for one observation. A **ref** is the per-watch head plus the version log.

**Manifest size — corrected.** A manifest is `(slot_key, blob-hash)` per block = `(12 B + 16 B) = 28 B/block` *if slot_keys are stored inline*. For an 84-block page that is ~2.4 KB. But **manifests are themselves delta-chained against the prior manifest** (consecutive manifests share all-but-one entry), so the *stored* cost of a one-block change is one new entry's ~28 B plus the delta framing — single-digit dozens of bytes, not 2.4 KB. The earlier draft's "N×16 bytes" understated the per-manifest cost (it omitted the slot_key) and the per-*change* cost is the delta, not the full manifest. We store each `slot_key` once in an interned per-watch dictionary and reference it by a small integer in the manifest, so the recurring manifest cost is `(varint slot_id + 16 B hash)` ≈ 18 B/block before manifest-delta.

**Measured dedup on Corpus A** (raw HTML upper bound; *not* what Corpus B stores):

```
total block instances across 720 versions: 60,480
unique blocks (by content hash):               804
dedup ratio:                                  75.2×
unique block bytes (raw):                  266,051
```

**This 75.2× is a Corpus-A artifact:** of the 804 "unique" blocks, ~720 are footer-token variants that **§5.3 strips before hashing** — so on **Corpus B the unique-block count collapses to the handful of genuinely distinct blocks (the stable layout + the one changed price block)**, the footer token never enters the store, and there is no per-hour blob at all. Corpus A is the right model for a page whose blocks *genuinely* change every hour; Corpus B is the steady-page reality.

**What actually gets stored on a real one-block change (Corpus B):** one new blob (a few hundred bytes pre-compression) + one manifest delta (one changed `(slot_id, hash)` entry). Nothing else.

### 9.3 Compression — codec, dictionary, delta-chaining

Measured on **Corpus A** (89.56 MB raw — the upper-bound corpus where every version differs):

| Strategy | Total bytes | Ratio (vs Corpus A raw) |
|---|---:|---:|
| 720 raw `.html` files | 89,557,810 | 1× |
| Per-file `gzip -9` | 1,884,234 | 48× |
| Per-file `zstd -19` | 1,317,294 | 68× |
| Per-file `brotli -q11` | 1,100,576 | 81× |
| Per-file `zstd -19` + **trained dictionary** | 409,930 | **218×** |
| `git gc --aggressive` packfile | 71,317 | 1,256× |
| **Rebased delta chain** (full base every 24h) | 99,271 | 902× |
| **Full delta chain** (`zstd --patch-from`) | **47,608** | **1,881×** |

**Codec: zstd.** gzip is out (48×, no usable dictionary/patch tooling — kept only as interop export). brotli `-q11` beats zstd on *standalone* small text (81× vs 68×, via its built-in 120 KB web dictionary) but has **no first-class delta/patch mode and no custom-dictionary workflow** — its edge evaporates the moment we delta-chain. **zstd wins on the two features we exploit: `--train` custom dictionaries and `--patch-from` delta**, is fastest to decompress, and `libzstd` is a clean Rust dependency. **Decision: zstd everywhere, level 19 for cold blobs/bases, level 12 for hot deltas.**

**Trained dictionary — corrected to one number.** Compressing a 300-byte block in isolation wastes the window on a cold model. Measured: per-file zstd -19 went **1,317,294 → 409,930 bytes (3.21×, i.e. 218× vs raw)** by training a 112 KB dictionary on the block corpus. **(The earlier draft's table row of 522,570 bytes was a stale measurement that double-counted the 112 KB dictionary as stored-per-file rather than once-per-domain; the corrected single figure is 409,930, with the dictionary charged once at `objects/dict/<domain>.zdict`.)** Implementation: one dictionary **per watch-domain** (`zstd --train`, COVER), version-stamped and content-addressed.

**Delta-chaining — the real prize (Corpus A).** `zstd --patch-from=<prev>` produces a VCDIFF-like binary delta:

```
patch v0000 -> v0001 (footer token noise only):   64 bytes   [Corpus A: token NOT stripped]
patch v0150 -> v0151 (footer token noise only):   65 bytes   [Corpus A]
patch v0299 -> v0300 (THE PRICE CHANGE $29->$39): 73 bytes   [the ONE change Corpus B also stores]
patch v0301 (noise only):                          66 bytes   [Corpus A]
```

**Crucial corpus note (the central contradiction, resolved).** The 64–66 B "footer token noise" patches exist **only in Corpus A**, where the token is deliberately *not* normalized away, to demonstrate codec behavior under genuine per-version change. **In the deployed pipeline (Corpus B) those noise hours are `doc_hash`-equal and store nothing** — §5.3 strips the token before the hash, §5.6 short-circuits, zero bytes written. So the deployed cost of this page over 30 days is **the two full baselines + the single 73-byte real-change delta**, not 720 patches. *The thesis "storage is proportional to the size of the change, not the page" is made physical by the **73-byte** real-change patch; the noise patches are a measurement artifact of the upper-bound corpus, not a deployed cost.* We delta-chain at two granularities: **blob deltas** for large blocks that mutate in place, and **manifest deltas** for the mostly-identical recipes.

**Blob-vs-manifest delta strategy (resolved gap).** A changed block is stored as a **standalone CAS blob, delta-compressed (`--patch-from`) against the *immediately-prior version's blob for the same `slot_key`*** when that prior blob exists; a brand-new `slot_key` is stored as a full (dictionary-compressed) blob. This keeps CAS dedup ("identical block → one object") and per-slot delta-chaining compatible: identical blocks still collapse to one object (the delta of an unchanged block is never created because the block hash is unchanged and the manifest just re-references the existing object); only *changed* blocks produce a patch, and that patch is against the same slot's prior content (maximal overlap).

**Cross-page/cross-watch dedup (resolved gap).** A blob that is *byte-identical* across sibling watches is stored **once, full (or dictionary-compressed), not delta-chained across watches** — delta chains are per-watch (a chain crossing watches would couple their GC and rebasing). So the cross-page dedup win materializes for *identical* blocks (one object, referenced by multiple watches' manifests) but *changed* blocks chain only within their own watch. This is the correct trade: cross-watch identity is common for boilerplate (shared footers, legal text) and gives full dedup; cross-watch *near*-identity is rare and not worth coupling chains for.

**Chain length vs random access — periodic re-basing (Corpus A).** A pure chain is smallest (47.6 KB) but reconstructing the newest version replays every hop — worst case **~5 s to walk 719 hops**, unacceptable. Fix: every *K* observations (or when cumulative deltas exceed 50% of a full snapshot — git's heuristic), store a fresh full **base**. At **K=24**:

```
Rebased chain (base every 24h):  99,271 bytes,  ≤23 hops,  reconstruct ≤ ~0.16 s   [Corpus A]
Full chain:                       47,608 bytes,  up to 719 hops,  reconstruct ~5.05 s [Corpus A]
```

Re-basing doubles footprint but is still **902×** smaller than raw (Corpus A) and bounds reconstruction. **Decision: default `rebase_window K=24`, AND force a base whenever `cf` emits a material change event** — so every interesting version is a fast anchor and chain segments align with event boundaries. Chain direction is **forward (base → newer)** — the access pattern is "give me the latest" / "the version around event X," both near head/anchors. *(On Corpus B these reconstruction numbers are moot — there are only 2 versions — but the rebasing machinery matters for genuinely-churning pages, which look like Corpus A.)*

**Index overhead (honest, for the scaling claim).** The SQLite `objects` index row (~48 B: hash, type, pack, offset, len, base_hash, dict_id) and one `versions` row per *stored* observation (~64 B) are charged in the footprint. For Corpus B (2 stored versions) this is negligible. For the 2,000-URL fleet at fleet scale, index rows are charged per *stored change*, not per poll — so a steady fleet's index is small; a churning fleet's index grows with real changes (as it should).

### 9.4 Retention & GC (Phase 2: mark-and-sweep)

Because storage is content-addressed, GC is **git-style mark-and-sweep**. You never delete a blob by version; you delete *references* and sweep unreachable blobs. Per-watch retention:

```jsonc
"retention": {
  "keep_last_n": 200,            // always keep the 200 most-recent manifests
  "keep_window": "90d",          // and anything within 90 days
  "keep_on_event": true,         // NEVER prune a version that emitted a change event
  "keep_bases": true             // keep rebase anchors even if their window is pruned
}
```

`cf gc` algorithm: (1) **mark roots** — every ref head + every manifest satisfying retention (last-N ∪ window ∪ event-tagged ∪ base anchors); (2) **transitive mark** — from each live manifest mark its blobs; **from each delta-encoded blob mark its base blob; AND from each delta-encoded *manifest* mark its base manifest** (manifests are themselves chained, §9.3 — so a non-event manifest that is the base of a retained manifest is promoted-or-rebased, never orphaned); GC promotes an orphaned-but-needed base to full, or re-bases the survivor; (3) **sweep** unmarked blobs/manifests; (4) **compact** — optionally re-pack survivors and rebuild chains. `cf gc --dry-run` reports reclaimable bytes; `cf gc --aggressive` re-trains dictionaries, **re-compresses old blobs under the new dictionary** (see dictionary lifecycle below), and re-bases. **`keep_on_event: true` is the killer feature**: the agent's audit trail of *what changed and when* survives forever at the cost of only the changed blobs.

**Dictionary lifecycle under CAS immutability (resolved).** A blob compressed with dict v1 is undecodable under dict v2, so a dictionary is **never rewritten in place** and **never swept while any live blob references it**. Retraining (on 4× corpus growth) **mints a NEW content-addressed dict used only for new writes**; old dicts are *pinned* by the blobs that need them (including `keep_on_event` blobs retained forever). A dict is swept only when GC proves no live blob references it. `cf gc --aggressive` optionally **re-compresses old blobs under the newest dict** (a full read-decode-recompress rewrite) so an old dict can then be swept — this is the only way a dict goes away, and it is opt-in because it rewrites blobs. **The authoritative dict per blob is `objects.dict_id`** (a CAS blob must be decoded with the exact dict it was written with); `refs.dict_id` is *only* the default-for-new-writes and is never consulted on decode (see §9.5).

### 9.5 On-disk layout (Phase 2 store)

```
.changefeed/
├── config.toml                       # changefeed.toml (codec levels, default retention)
├── store.db                          # SQLite: index, refs, version log, stats sidecar, locks
├── secrets.env                       # 0600, ${ENV} secrets (never in the object store)
├── events.jsonl                      # daemon append-only event log (tailed by cf feed --tail)
├── objects/
│   ├── pack/
│   │   ├── 0001.cfpack                # append-only packfile of blobs + deltas
│   │   └── 0001.cfpack.idx            # hash -> (pack, offset, len, type, base-hash, dict)
│   ├── loose/b3/9f/9f2c…             # recently-written objects (git fanout) before packing
│   └── dict/acme.com.<b3>.zdict       # trained dictionary, content-addressed name
└── tmp/                               # staging for atomic writes
```

**Why SQLite for the index.** It must answer fast and transactionally: head for watch X, manifest for version 412, which versions are event-tagged (GC roots), which blobs are unreachable. Relational + ACID under concurrent writers; flat files can't reverse-lookup without a full scan. WAL gives safe concurrent reads.

```sql
CREATE TABLE objects(hash BLOB PRIMARY KEY, type INT, pack INT, offset INT,
                     len INT, base_hash BLOB, dict_id INT);          -- dict_id = AUTHORITATIVE decode dict
CREATE TABLE refs(watch_id TEXT PRIMARY KEY, head_manifest BLOB,
                  default_dict_id INT);                              -- default-for-NEW-writes ONLY
CREATE TABLE versions(watch_id TEXT, version INT, manifest BLOB, observed_at INT,
                      http_etag TEXT, event_id TEXT,                 -- event_id => GC root
                      PRIMARY KEY(watch_id, version));
CREATE INDEX versions_time ON versions(watch_id, observed_at);
```

**Dictionary scope is pinned to ONE authority.** Four granularities were stated loosely before; the resolution: **`objects.dict_id` is the single authoritative decode dict per blob** (a CAS blob can only be decoded with the dict it was written with). `refs.default_dict_id` is *only* the default chosen for new writes of that watch and is never consulted on read. There is no per-manifest dict (a manifest references blobs, each of which carries its own `dict_id`). Conceptually a dict is trained per watch-*domain*, but stored/decoded per-*blob*.

**Blobs do not go in SQLite** (BLOBs bloat the index/vacuum) — append-only packfiles referenced by `(pack, offset, len)`. **Integrity:** `cf fsck` verifies each object by **reconstructing** it (decompress if full; **apply the `--patch-from` delta over its reconstructed base — recursively, with the correct `dict_id`** — if a delta) and then re-hashing the reconstructed bytes against its address. (The earlier draft said "re-hashing its *decompressed* bytes"; for a delta blob the decompressed bytes are a *patch*, not the block, so reconstruction-then-hash is the correct check, and it is base- and dict-dependent.) Each packfile carries a trailing BLAKE3; `store.db` carries `PRAGMA integrity_check`.

**Write concurrency (resolved).** Two concurrent one-shot `cf check`s must not interleave appends to the same packfile and corrupt offsets. Resolution: **the packfile append occurs while holding the same exclusive write lock that guards the SQLite write transaction** — i.e. acquire the store write lock → append blob to the active packfile and `fsync` → commit the index row (which makes the blob visible) → release. A crash after the append but before commit leaves an unreferenced packfile tail the next GC sweeps. (Alternatively, with `store.writers=parallel`, each writer gets its own `NNNN.cfpack` segment so appends never share a file; the default is the single-lock model for simplicity.) Readers use WAL snapshots and never block; a coarse `flock` covers repacks.

### 9.6 Reconstruction (the read path)

To diff observation *V* against *V−1*, or answer `cf show <watch> --version 412`: (1) look up the manifest hash; (2) for each blob, decompress directly if full, else walk to its base (≤ K hops, bounded by re-basing) applying `zstd` patches **with the blob's recorded `dict_id`**; (3) assemble in manifest order → canonical tree → diff against the neighbor. Because manifests are explicit `(slot_key, blob-hash)` lists, **the diff engine compares two manifests' hash-lists in O(blocks) and materializes only the 1–2 blobs whose hashes differ** — it never reconstructs the full page to find the change. This is the storage layer directly serving the tool's core principle.

### 9.7 Ultralight mode — minimum viable state (Phase 3)

For watchers that must run with near-zero persistent state (embedded in an agent process, a Lambda, a $5 VPS watching 50k URLs), `cf watch --ultralight` keeps **no object store at all** — just a fixed-size per-watch record:

```jsonc
{
  "watch_id": "acme-pricing",
  "page_hash": "b3:9f2c…",            // 16 B: whole-page hash (detect ANY change)
  "block_hashes": ["b3:a1…", …],      // N×16 B: per-block hashes (detect WHICH block)
  "slot_keys":   ["slot:struct:…", …],// 12 B slot_key per block (name WHERE the change is)
  "ring": [ /* last 8 (block_idx, old_hash, new_hash, ts) tuples */ ]
}
```

Sizing (corrected): a 60-block page is `16 + 60×16 + 60×~16 (slot_keys, interned 12 B + framing) + 8×~40 (ring) ≈ 2.3 KB/watch` → **50,000 watches fit in ~115 MB of state** (the earlier 2.7 KB/135 MB figure used ~40 B example keys; interned 12-B `slot_key`s are smaller — but neither figure counts the on-transition full-fetch store, below).

**What ultralight can and CANNOT do (corrected — this is a real capability boundary).** Ultralight stores only one-way hashes. From a hash you **cannot** recover before-text *or* after-text. Therefore ultralight **cannot emit any §6.3 delta encoding** (`val`/`idiff`/`block` all require text on at least one side). Concretely:

- **It answers (1) THE DELTA only at *block granularity*: THAT a change occurred (`page_hash`), WHERE (`slot_key` + `block_hash`), and the KIND (added/removed/moved/hash-changed by comparing hash sets).** It does **NOT** answer the before→after text part of (1).
- It answers (3) FOLLOW-UP (the action taxonomy is computable from kind + slot type prior).
- For (2) WHY-with-text, it **hands off**: on a detected change it fires a **one-shot full-fidelity fetch+store**. **But that fetch captures only the AFTER state** — there is no stored prior blob in ultralight — so the **first** post-transition full event also has no before-text to diff against; it shows the after-content as an `added`/`block` and only the *second* full observation onward can show true before→after. The on-transition fetch's full snapshot lands in a small **companion full-store** (a normal §9 store rooted at `$CF_STORE/ultralight-spill/<watch_id>/`, retained for `keep_last_n=2` so the next change *can* diff); this is the one place ultralight is not stateless, and it is opt-out (`--no-spill` keeps pure block-granularity-only).

**Move detection — corrected justification.** Because `slot_key`s derive from content/heading-scope (§5.4), an inserted block shifts indices but leaves other blocks' content hashes and `slot_key`s intact, so moves are recognized as moves by comparing `(slot_key → block_hash)` maps across the two records. (The earlier draft credited a "Rabin/rolling block hash"; that solves boundary-finding in *un*segmented byte streams and is irrelevant here — blocks are already semantically segmented in §5.4, which is what gives insertion-shift invariance.)

**Ultralight steady-state, full-fidelity on transition** is the recommended profile for very-high-cardinality fleets, with the explicit caveat that the *first* event after any quiet period is after-only.

### 9.8 MVP storage (the simple model that ships first)

The entire §9.0.1–§9.6 CAS/packfile/dictionary/delta-chain/GC engine is a **Phase-2 fleet-scale optimization**. The MVP delivers 100% of the agent-facing value with a far simpler model, because the *thesis* (cheap no-op, deterministic delta) is delivered by the 304/`doc_hash` short-circuits and the diff — **none of which need delta-chaining, dictionary training, packfiles, or GC**:

- **Store exactly ONE previous `CanonicalDoc` per target**, as a single `zstd -19` blob keyed by `tid`, plus the raw post-extract HTML as a second `zstd -19` blob (for offline re-segmentation, §5.6).
- **Retention = "keep last N CanonicalDocs per target"** (default N=8), trivially correct, **no GC** — old blobs are overwritten/dropped by a fixed ring. The idempotency seen-set and a tiny version log live in `store.db`.
- **`doc_hash`-equal and 304 still short-circuit to zero writes** — the no-op-is-free promise holds in MVP unchanged.
- Per-changed-observation cost in MVP is ~one full `zstd` CanonicalDoc blob (a few KB for a typical page), *not* ~73 bytes — the byte-level delta numbers above are the Phase-2 win. MVP is "cheap enough" (a few KB × N-retained × #targets); Phase 2 is "cheap at fleet scale" (bytes-per-change).

This descope roughly halves MVP storage engineering for zero loss of agent-facing capability; CAS + dedup + delta-chaining + rebasing + dictionary training + mark-and-sweep GC all move to Phase 2 (§13).

---

## 10. End-to-End Worked Example

Watching a competitor's pricing page from `cf watch` through to the agent acting.

**Setup.**

```bash
cf init
cf watch https://competitor.com/pricing --id comp-pricing \
   --archetype pricing --select "main .PricingTable" --schedule 30m
```

This writes a `[[target]]` to `changefeed.toml`, selects the `pricing` rule pack and extract profile, and the first `cf check` stores a baseline → **exit 11** (prints the baseline envelope, §6.8).

**The no-op poll (the ~95% case).** Thirty minutes later an agent runs `cf check comp-pricing -q`:

1. **Fetch** sends `If-None-Match: W/"a1b2"`. The origin returns **`304 Not Modified`**. The pipeline short-circuits — no extract, no diff, no new snapshot.
2. **Exit `0`, empty stdout.** The agent's `case $?` hits `0)` and does nothing. Zero tokens, zero bytes. Even on a `200` with a rotating CSRF nonce as the *only* difference, §5.3 strips the nonce before hashing, the `doc_hash`-equal check (§5.6) aborts before the diff, and the storage layer writes nothing (Corpus B, §9.0).

**The poll that matters.** The Pro plan moves `$49 → $59`:

1. **Fetch** Tier 1 returns `200` (server-rendered HTML; no escalation). Body flows to extract.
2. **Extract** keeps `main .PricingTable` (selector strategy — readability would have risked deleting the table).
3. **Normalize** strips the rotating CSRF nonce and the "127 viewing" counter; the Pro price cell survives.
4. **Segment** assigns the price cell the `slot_key` `slot:struct(Pricing›Pro Plan, price, #0)` — stable across the value change — and types it `price` with `value:{amount:5900,currency:USD,period:"mo"}` (was `4900`). Its `block_id` changes (text changed) but the aligner never joins on `block_id`.
5. **Diff** anchors on `slot_key` (every block, including the edited price cell, matches in phase 1 because `slot_key` is text-free), finds the price slot's `value` changed, emits one `modified` op (`val` encoding), `numeric_change:{from:49,to:59,pct:0.204}`, `noise_score 0.02`. (MVP: `segment_stability` defaults to 1.0, `novelty` 1.0.)
6. **Classify.** Signals: `type=1.00` (price), `num=0.9·0.20`, `pos≈0.88` via Tier-1 dom-depth+order proxy (§8.1), `kw` hit on "price". Noisy-OR → `sal 0.86`; with `w_vol=1.0` (MVP) → `mat high` under the `pricing` pack's bands. The pack maps a price-tier change to `re_run_downstream`.
7. **Store** (MVP) writes one new `zstd` CanonicalDoc blob keyed by `comp-pricing`, dropping the oldest of the retained ring. (Phase 2 would write one ~300-byte changed blob + a manifest delta and force a rebase anchor — **~73 bytes** after delta-compression.)
8. **Emit** — exit `10`, this on stdout (minified, ~305 tokens — the §6.1 canonical size):

```json
{"v":"1","id":"cfe_01J9ZB…","src":{"url":"https://competitor.com/pricing","tid":"comp-pricing","title":"Pricing — Competitor"},"obs":"2026-06-02T14:30:11Z","base":{"obs":"2026-06-02T14:00:09Z","snap":"blake3:9f2a…","rev":47},"seg":[{"anchor":"Pro plan","fp":"blake3:4b9c1e","label_path":"Pricing › Pro Plan › price","role":"price"}],"ct":"modified","delta":{"enc":"val","a":"$59/mo","b":"$49/mo"},"why":{"sal":0.86,"mat":"high","cat":"price_increase","summary":"Pro monthly price rose 20.4% ($49→$59)."},"followup":{"act":"re_run_downstream","tgt":"pricing-watchers","params":{"urgency":"high"}},"conf":0.97,"prov":{"m":"http","hash":"blake3:1c80…","etag":"W/\"c3d4\"","status":200,"pack":"pricing@b3:2f1a"}}
```

**The agent acts.** `case $?` hits `10)`, the agent reads ~305 tokens of JSON (the captured `$out` from the single call, §4.6), sees `followup.act == "re_run_downstream"` with `tgt "pricing-watchers"`, and re-runs its own pricing model — *without ever loading the page into context*. On the next 47 no-op polls that day it spends zero tokens. The naive approach would have re-read the full ~9k-token page on all 48 polls.

---

## 11. Failure Modes & Edge Cases

| Failure | Behavior | Rationale |
|---|---|---|
| **Origin down / 5xx / DNS / TLS / timeout** | Exit `3` (soft), `crawl.status≥400` or `crawl.err`, no new snapshot. Daemon backs off exponentially with jitter; one-shot leaves backoff to the caller. | Distinct from "no change" (§6.8) by **exit code alone** (§4.5). `--fail-on-fetch-error` promotes to `1`. |
| **Multi-page partial failure** (page 3 of 5 502s) | **Whole observation aborts, exit `3`, no snapshot stored** — never diff a truncated doc against a full prior (§5.7). A legitimate `rel=next`-absent terminus is *not* a failure. | Prevents a giant false `removed`. |
| **Render needed, no browser** | Exit `7`, no event. | Render is opt-in; degrade loudly, not silently. |
| **Render nondeterminism** (SPA hydrates differently each load) | Convergence steps §5.1 (settle-wait, blocklist, `unordered` sort) absorb the bulk; residual flap → Phase-2 volatility model + lowered `conf` (`c_fetch=0.85` headless). A target whose `align_rate` stays <0.95 over 8 renders is flagged `render_unstable`. | The deterministic-diff guarantee holds *given a canonical tree*; we engineer convergence and **bound** residual nondeterminism rather than asserting it away. |
| **Whole-page redesign / selector matches a DIFFERENT subtree** | `select_overlap_min` check (§4.9): if the matched subtree's `slot_key` overlap with prior < 0.3, emit ONE low-`conf` (`c_stability=0.6`), high-`mat` `content_edit` and flag for operator review — not a silent confident wrong diff. Zero-node match → exit-3-style operator alert via `cf rules`. | A redesign is a real event worth surfacing once, at low confidence so the agent verifies. |
| **First observation** | Exit `11`, baseline stored, baseline envelope printed (§6.8). | No prior to diff against. |
| **Block edited** (text changed) | `slot_key` is text-free so the block still anchors in phase 1 (§7.1); `norm_hash` differs → `modified`. `block_id` changing is expected and irrelevant. | Joins never use the content-derived `block_id`. |
| **`slot_key` itself changed** (section renamed / ordinal shifted) | Falls to similarity fill (§7.1 phase 3); re-paired as `modified` if `sim ≥ τ_match`, else clean `removed`+`added`. | The only case that reaches similarity matching. |
| **Mass reorder of identical-shaped siblings** (5 lookalike plan cards) | `slot:struct` ordinals disambiguate by section/ordinal; if a profile declares a content discriminator (`slot_key_discriminator = ".plan-name"`) it is used instead of ordinal. *(Per-archetype default discriminators tracked in §14.)* | Ordinal-only slots are fragile when many same-type siblings move; a declared discriminator fixes it. |
| **Hash collision** (BLAKE3-128) | Birthday bound ~2⁶⁴; ~2⁻⁴⁸ at 2⁴⁰ blocks. Global cross-page dedup raises exposure at fleet scale (§9.1) → `--hash-bits 256` for very large stores. `cf fsck` re-hashes and detects corruption. | 128 bits is the safe floor for per-store scale; 256 for multi-tenant fleets. |
| **Delta-chain base corruption** | `cf fsck` pinpoints the unreconstructable version; GC re-bases survivors off the nearest intact anchor. | Re-basing every 24h + on-event bounds blast radius to ≤23 hops. |
| **Clock skew / out-of-order observations** | `rev` is a monotonic per-`tid` counter, not derived from `obs`; never bumped on unchanged content. | Ordering is by `rev`, not wall-clock. |
| **PDF / non-HTML filings** | Separate extractor path. *(Open question §14 — Phase 3.)* | Regulatory filings often arrive as PDF. |
| **Infinite scroll never stabilizes** | Capped at `max_scrolls` (10); serialize what we have, log the cap. | Bounds Tier-2 cost. |
| **Feed item scrolls off the window** | `feed_window` caps retained items so an old entry aging out does **not** diff as `removed`. | Distinguishes "delisted" from "fell off page N." |
| **Re-running default `cf check` on the same snapshot pair** | Idempotency `event_key` (§7.4) suppresses → exit `0`. `--no-store`/`--peek` deliberately bypass this so a probe re-emits. | Pipeline is idempotent; probes are explicitly not. |
| **`cf check` with no writable store** | Falls back to "first observation, no politeness memory" (exit `11`), stderr warning; cannot honor crawl-delay or diff. | Crawl-delay state needs a persisted per-host timestamp (§4.3). |

---

## 12. Security & Politeness

Politeness is **on by default** and per-host, because this tool is pointed at other people's servers at scale.

- **robots.txt** fetched and cached per host (24 h TTL). With `respect_robots = true` (default), a disallowed path returns exit `4` and is skipped; `Crawl-delay` is honored as a floor on inter-request spacing. `respect_robots = false` is per-target, explicit, and logged.
- **Per-host concurrency + min-interval.** The daemon holds one in-flight request per host and a configurable `min_interval` (default 1 s, or `Crawl-delay`, whichever is larger). One-shot `cf check` records a per-host last-fetch timestamp **in `.changefeed/store.db`** and refuses (exit `6`) if it would violate the interval, unless `--ignore-crawl-delay`. An ad-hoc check with no writable store cannot persist this timestamp and therefore cannot enforce crawl-delay across invocations (§4.3, §11) — a documented limitation of stateless ad-hoc use, not the configured path.
- **Conditional GET** (ETag/`Last-Modified` → `If-None-Match`/`If-Modified-Since`). A `304` is the cheapest possible observation — exit `0`, no body, no diff, no new snapshot — serving politeness *and* the storage-footprint constraint at once.
- **429 / Retry-After** surfaces as exit `6`; the seconds are emitted in `crawl.retry_after` (event/envelope JSON) and on stderr, so a loop sleeps exactly that long **without a second fetch** (§4.6). Daemon uses exponential backoff with jitter.
- **Honest User-Agent.** Default UA identifies the tool with a contact URL; we do not aggressively spoof.

**Secrets & credentials.** All secret-bearing config values are `${ENV}`-expanded at runtime, loaded from `.changefeed/secrets.env` (`0600`), **never written into the snapshot store**, and redacted from logs. The four auth types are `header`, `cookie`, `basic`, `browser`. Snapshots store normalized *content*, not raw response headers, so a stored blob can never leak a session token. (This is also why a session token never reaches the object store and the §9 storage math is about content, not headers.)

**Store integrity.** Every object is verifiable by **reconstructing and re-hashing** against its address (`cf fsck`, §9.5 — reconstruction, not bare decompression, for delta blobs); packfiles carry a trailing BLAKE3; SQLite carries `PRAGMA integrity_check`. Packfiles are immutable once sealed.

**Resource bounds.** 5 MB body cap, 10 s HTTP / 15 s render timeouts, `max_pages`/`max_scrolls` caps, bounded per-host concurrency — a single hostile or runaway target cannot exhaust the fetcher.

---

## 13. Roadmap & Phasing

### MVP (the thing that delivers the thesis)

- **Fetch:** Tier 1 HTTP only (`reqwest`/`rustls`) + ETag/304 short-circuit. No headless (`render="chromium"` → exit `7`).
- **Extract/Normalize:** `html5ever` + Readability and `selector` strategies; the **full volatile-strip normalization set (§5.3)** — non-negotiable; without it the tool flaps *and* the storage no-op-is-free promise fails.
- **Segment:** `slot_key` (anchor + struct) + `block_id`; `price`/`date`/`number`/`text`/`heading`/`table_row` typing.
- **Diff:** `slot_key`-anchor → LIS → similarity-fill; Myers intra-block diff; static ignore masking; idempotency dedup. *(Volatility/auto-learning and debounce are Phase 2 and daemon-only.)*
- **Salience:** the **seven** deterministic signals + noisy-OR (`vol` is Phase-2, `w_vol=1.0` in MVP); shipped packs `pricing`, `api-docs`, `status-page`. **LLM tier off / not built.** `conf` computed per §6.6.
- **Storage:** the **simple MVP model (§9.8)** — one previous `CanonicalDoc` per target as a single `zstd-19` blob + raw HTML blob, fixed-ring "keep last N", **no CAS, no packfiles, no GC**. `doc_hash`/304 short-circuits intact.
- **CLI:** `init`, `watch`, `check` (incl. `--peek`/`--no-store`), `snapshot`, `diff`, `feed` (incl. `--limit`/`--after-cursor`), `ls`, `show`, `rules`, `schema`. Exit codes, JSONL/pretty, stdin contracts. `changefeed.toml`.

### Phase 2 (production hardening + fleet-scale storage)

- **Headless tier** (`chromiumoxide`/CDP pool) + `needs_render` heuristic + render-determinism convergence (§5.1) + `cf login`/browser auth + direct-JSON fast path.
- **Stability model** (§7.4, daemon-only): per-`slot_key` flap EWMA, HLL distinct-value detection, A/B canonicalization, auto-ignore, **debounce**, and the `vol` salience signal (the eighth).
- **Fleet-scale storage engine (§9.0.1–§9.6):** CAS + BLAKE3-128 + zstd delta-chaining + per-domain trained dictionary + rebasing (K=24, on-event) + SQLite index + packfiles + mark-and-sweep `cf gc` + `cf pack`/`cf fsck`. This is the bytes-per-change optimization, needed at scale, not at first ship.
- **`cf daemon`** with schedules, sinks (jsonl/webhook), per-host politeness state, `cf feed --tail`.
- **MCP server** (`cf mcp`) and the five tools.
- **Cascade clustering** + the `reordered`/`move`/`struct` delta encodings.
- **Multi-page** modes (`feed`, `paginated`, `infinite`).

### Phase 3 (ecosystem & scale)

- **LLM escalation tier** (§8.6) with verdict cache and budgets.
- **Community rule packs** (`cf pack add github:…`) and the remaining archetypes (`regulatory`, `job-posting`, `changelog`, `inventory`, `release-notes`).
- **Ultralight mode** (§9.7) + the hybrid ultralight→full-fidelity-on-transition profile.
- **PDF / non-HTML extractor path** for regulatory filings.
- **Storage/cost analytics** and LLM cache hit-rate reporting.

---

## 14. Open Questions

1. **No-op liveness.** Resolved for the agent surface: `cf check` distinguishes no-change from failure by **exit code alone** (§4.5), so it emits nothing on no-change; the daemon still logs the `n:0` envelope for its own liveness. Remaining: should the daemon's `n:0` cadence be configurable (every poll vs every Nth) to bound the event-log size on huge fleets?
2. **Reorder addressing.** `delta.enc="move"` carries positional `from`/`to` + a `key`; should it also carry from/to `seg.fp` for moved blocks with a stable `slot_key`? Current design says positional+key is sufficient; revisit if agents need to correlate the moved block's other changes.
3. **Table/list set-diffs.** Resolved: cascade clustering emits per-row events up to `max_children`, then degrades to `delta.enc="struct"` (§6.3). Remaining: is `max_children=32` the right cutoff per archetype (a 200-row inventory table vs a 5-tier pricing grid)?
4. **Structural-slot stability under mass sibling reorder** (§11): per-archetype default `slot_key_discriminator`s (e.g. `.plan-name` for pricing, `[data-component]` for status) — ship them in the archetype packs, or require the user to declare?
5. **Diff similarity threshold calibration.** `τ_match=0.62` and the `0.55/0.30/0.15` sim weights are defended but not yet empirically tuned across archetypes; `cf score --dry-run` is the tuning loop.
6. **Follow-up vocabulary extensibility.** The 8-action agent-facing `act` enum is closed/frozen in v1. Confirm the `tgt`/`params` escape hatch covers all integrator needs so we never need a v2 just to add an action.
7. **Cross-store hash width policy.** §9.1 offers per-store `--hash-bits 256` for multi-tenant fleets crossing ~2³⁸ blocks. Should a shared/multi-tenant store *default* to 256-bit, accepting the manifest-size cost, rather than making operators reason about birthday bounds?
8. **Authenticated headless session reuse** at scale: credential storage and session refresh in the Chromium pool (`cf login` + storage-state) is sketched but unspecified for many concurrent gated targets.
9. **`align_rate` thresholds.** The 0.98 target / 0.95 `render_unstable` / 0.3 `select_overlap_min` gates (§5, §5.1, §4.9) are reasoned but not empirically validated; they are the calibration surface for "are these two trees the same logical page."

---

## 15. Appendix

### A. Glossary

| Term | Meaning |
|---|---|
| **Watch / target** | A user-declared URL + per-watch rules, identified by a stable `tid`. |
| **Observation** | One fetch+process of a target; produces either a no-change result or ≥1 change events. |
| **CanonicalDoc** | The normalized, typed block-tree (`changefeed.canonical/1`) the diff engine consumes. |
| **Block** | The smallest semantically-typed diff unit (heading, paragraph, table_row, price cell…). |
| **`block_id`** | **Within-observation** content handle; `blake3(slot_key ‖ norm_text)`; **changes on edit; never a join key.** |
| **`slot_key`** | **Cross-observation** stable join key; text-free (`anchor` or `struct(breadcrumb,type,ordinal)`); **the only key the aligner/stability/dedup join on.** |
| **`norm_hash`** | Hash of a block's normalized text; detects *whether* a block changed; basis of `event_key`. |
| **doc_hash** | BLAKE3 over the canonical tree (`slot_key`s + types + values; excludes provenance and `block_id`); equal ⇒ no semantic change ⇒ no store. |
| **Manifest** | An ordered list of `(slot_key, blob-hash)` describing one observation. |
| **Blob** | One canonicalized block's bytes, content-addressed, stored once globally. |
| **Salience (`sal`)** | Continuous `[0,1]` "does this matter" score; comparable only within one target's pack. |
| **Materiality (`mat`)** | Discrete bucket; the **cross-target-comparable** routing label. |
| **Confidence (`conf`)** | Computed `[0,1]` "are we sure it's real & correctly typed" (§6.6); **orthogonal to `sal`**. |
| **Rebase anchor** | A full snapshot stored every K observations (and on every event) to bound reconstruction (Phase 2). |
| **Corpus A / B** | A = raw-HTML upper bound (every version differs); B = post-normalization reality (no-op polls store zero). §9.0. |

### B. Frozen identifiers (one schema, one CLI)

- **Schemas:** event `changefeed/v1` (wire field `v:"1"`); canonical `changefeed.canonical/1`.
- **Hashes:** content addressing is `blake3:<hex>` (block-store form `b3:<32 hex>`, 128-bit truncated; `b3-256:` for `--hash-bits 256` stores). Segment fingerprint `seg.fp` is the `slot_key` prefix (`blake3:<12hex>`). Diff-internal block hashing uses XXH3 (`norm_hash`, `event_key`).
- **Event keys:** `v, id, src{url,tid,title?}, obs, base{obs,snap,rev}, seg[]{anchor,fp,label_path,role}, ct, delta, why{sal,mat,cat,summary}, followup{act,tgt?,params?,q?}, conf, prov{m,hash,etag?,status,ms?,pack?}`.
- **`ct` (closed, frozen v1):** `added | removed | modified | reordered | restyled`.
- **`delta.enc`:** `val | idiff | block | move | struct`.
- **`followup.act` (closed AGENT-FACING 8, frozen v1):** `ignore, notify, refetch_linked, reembed_kb, re_run_downstream, open_ticket, escalate_human, page_oncall`. **`verify_llm` is an internal scoring state, NOT in this enum, and NEVER emitted** (§6.5, §8.4).
- **`why.cat` (open vocab, fallback `content_edit`):** `price_increase, price_decrease, plan_added, plan_removed, api_breaking, api_deprecation, api_addition, incident_open, incident_update, incident_resolved, version_bump, availability_out, availability_in, legal_filing, job_posted, job_removed, content_edit, cosmetic`.
- **`seg.role` (open vocab):** `price | date | version | status | link | prose | code | table-cell | nav | meta`.
- **Default materiality cutoffs (pack-overridable; `mat` is the cross-target signal, `sal` is not — §6.4):** `critical ≥ 0.90 · high 0.70–0.89 · medium 0.40–0.69 · low 0.15–0.39 · none < 0.15`.
- **Exit codes (stable within a major; §4.5):** `0 no-change · 10 change · 11 first-obs · 12 sub-threshold · 1 usage · 2 not-found · 3 soft-fetch · 4 robots · 5 auth · 6 rate-limit · 7 render-needed`.
- **Canonical event size:** **792 bytes ≈ 305 tokens** (§6.1, Example 1, measured) — the single figure used doc-wide.

### C. Default tunables

| Parameter | Default | Section |
|---|---|---|
| Poll schedule | `15m` | §4.9 |
| HTTP timeout / render ceiling | 10 s / 15 s | §5.1 |
| Body cap | 5 MB | §5.1 |
| `needs_render` emptiness ratio | `<0.05` and `<200` chars | §5.1 |
| `select_overlap_min` (redesign guard) | 0.3 | §4.9 |
| `align_rate` target / `render_unstable` gate | 0.98 / <0.95 over 8 renders | §5, §5.1 |
| Diff match threshold `τ_match` | 0.62 (0.75 for `table_row`/`kv`) | §7.1 |
| Sim weights (jaccard/lev/slot) | 0.55 / 0.30 / 0.15 | §7.1 |
| Position window `W` / `move_min` | 32 / 3 | §7.1 |
| Context elision | ±6 tokens | §7.2 |
| `max_children` per clustered event | 32 (then `enc:struct`) | §6.1 |
| Flap EWMA α / auto-volatile gate (Phase 2) | 0.30 / `flap_ewma>0.6 & obs≥8` | §7.4 |
| Debounce window (Phase 2, daemon) | 2 fetches / 10 m | §7.4 |
| Salience top-k / signals (MVP) | 5 / 7 (8 in Phase 2) | §8.1, §8.2 |
| LLM verify band | `[0.45, 0.75]` | §8.6 |
| Content hash | BLAKE3-128 (`b3:`); 256 opt-in | §9.1 |
| Compression | zstd L19 cold / L12 hot | §9.3 |
| Rebase window `K` (Phase 2) | 24 + forced on event | §9.3 |
| MVP retention | keep last 8 CanonicalDocs/target (ring, no GC) | §9.8 |
| Phase-2 retention | `keep_last_n=200, keep_window=90d, keep_on_event=true, keep_bases=true` | §9.4 |
| `cf feed` page limit | 1000 + cursor | §4.7 |

### D. References

- [MCP 2026-07-28 release candidate — JSON Schema 2020-12 for inputSchema/outputSchema](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/)
- [MCP specification & docs (modelcontextprotocol)](https://github.com/modelcontextprotocol/modelcontextprotocol)
- [Dictionary compression performance — zstd/brotli (HTTP Toolkit)](https://httptoolkit.com/blog/dictionary-compression-performance-zstd-brotli/)
- [The Ultimate Guide to Shared Compression Dictionaries (DebugBear)](https://www.debugbear.com/blog/shared-compression-dictionaries)
- Mozilla Readability (Firefox Reader View) — DOM-scoring extraction.
- `zstd` `--train` (COVER) and `--patch-from` (VCDIFF-like delta) — facebook/zstd.
- Git histogram/patience diff — anchor-based sequence alignment.
