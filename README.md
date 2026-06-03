# changefeed

**Turn a web page's changes into compact, structured change events for AI agents — instead of re-stuffing whole pages into an LLM.**

`changefeed` (binary: `cf`) re-observes a URL and emits, for each meaningful change, a tiny JSON event carrying three things:

1. **the delta** — what actually changed, computed by a deterministic, LLM-free pipeline;
2. **why it matters** — a salience score + materiality band + category;
3. **a suggested follow-up** — one action from a fixed agent-facing taxonomy.

The dominant case — *nothing changed* — costs the agent a single integer compare (exit `0`, empty stdout). Only a real, material change requires reading stdout. A `$49 → $59` price move becomes a ~300-token event instead of two full page copies plus reasoning.

> **Status:** MVP (v0.1). Tier-1 HTTP only; the optional LLM tier and headless render are off by default. 264 tests pass; `clippy -D warnings` clean. See [MVP scope](#mvp-scope) for what's deferred to Phase 2.

---

## Why

An agent that watches pages by re-feeding the whole DOM to a model pays for the entire page every poll, drowns in boilerplate/nonce/timestamp churn, and gets a non-deterministic "did anything change?" answer. `changefeed` inverts that: a deterministic core strips volatile noise, aligns the page against the last observation, scores only what changed, and hands the agent a bounded event (or just an exit code). **Noise resistance is the product** — a rotating "127 viewing now" counter or a `?v=12345` cache-buster is a zero-token no-op, while a real price or status change fires.

## How it works

A pure, deterministic pipeline (no clock, RNG, network, or disk in the core):

```
fetch → extract → normalize → segment → diff → salience → event
        (Readability/   (§5.3 volatile   (slot_key /   (slot-anchor → LIS    (7-signal      (frozen wire
         selector)       strip, NFC,       block_id /    → LSH fill → Myers;   noisy-OR +      schema +
                         URL canon.)       norm_hash)    noise + dedup)        confidence)     JSON Schema)
```

Three distinct identities keep alignment honest (§2.1): `slot_key` (the **only** cross-observation join key, text-free), `block_id` (within-observation content handle), and `norm_hash` (did this block change?).

## Install

Requires **Rust 1.80+**. Build from source:

```bash
cargo build --release          # binary at target/release/cf
# or install onto your PATH:
cargo install --path crates/cf
```

## Quick start

```bash
cf init                                   # scaffold ./changefeed.toml + ./.changefeed/
cf watch https://acme.com/pricing         # register a target
cf check acme-pricing --min-salience medium   # observe once → exit code (+ event on stdout if changed)
```

`cf check <url>` with no config is also legal — it fetches, compares to the last stored snapshot (or seeds a baseline), prints the event, and sets the exit code. **This is the call an agent makes in a loop:**

```bash
while sleep 900; do
  out=$(cf check acme-pricing --min-salience medium --format jsonl)   # one fetch, one diff
  case $? in
    0)  : ;;                                              # no change — zero tokens
    10) printf '%s\n' "$out" | your-agent ingest-change ;;# reuse the captured event
    3)  sleep 60 ;;                                       # transient fetch error, back off
    6)  sleep "$(printf '%s' "$out" | jq -r '.crawl.retry_after // 60')" ;;
  esac
done
```

## Example change event

`cf check` (or `cf score --dry-run before.html after.html --archetype pricing`) emits one compact JSON object per material change:

```json
{
  "v": "1",
  "id": "cfe_01J9Z…",
  "src": { "url": "https://acme.com/pricing", "tid": "acme-pricing" },
  "obs": "2026-06-02T14:30:11Z",
  "base": { "obs": "2026-06-01T14:30:09Z", "snap": "blake3:7ec358f4…", "rev": 4 },
  "seg": [
    { "anchor": "Pro Plan", "fp": "blake3:b33bc20655ac",
      "label_path": "Pricing › Pro Plan › price", "role": "price" }
  ],
  "ct": "modified",
  "delta": { "enc": "val", "a": "$59/mo", "b": "$49/mo" },
  "why": { "sal": 0.77, "mat": "high", "cat": "price_increase",
           "summary": "Pro Plan rose 20.4% ($49/mo→$59/mo)." },
  "followup": { "act": "re_run_downstream" },
  "conf": 0.87,
  "prov": { "m": "http", "hash": "blake3:a68240e8…", "status": 200, "pack": "pricing@b3:39a6" }
}
```

The wire schema is frozen and self-describing — run `cf schema` for the published JSON Schema (`changefeed/v1`).

## Exit codes — the cheap branch

An agent polls and branches on `$?` **without parsing stdout at all**. No-change vs failure is always distinguishable by exit code alone.

| Code | Meaning | Agent reaction |
|-----:|---------|----------------|
| `0`  | No change (nothing material per ignore rules + `--min-salience`) | Do nothing — empty stdout. |
| `10` | **Change** at/above `--min-salience` | Read stdout → act on the delta. |
| `11` | First observation (baseline stored, nothing to diff) | Note baseline; emits a minimal envelope. |
| `12` | Change **below** `--min-salience` (only with `--emit-subthreshold`) | Optional logging. |
| `1`  | Usage / config error | Bug in invocation; don't retry. |
| `2`  | Target not found | Fix config. |
| `3`  | Fetch failed (DNS/TLS/timeout/5xx) — *soft* | Transient; retry with backoff. |
| `4`  | Blocked by `robots.txt` | Don't retry without a policy change. |
| `5`  | Auth failure (401/403) | Refresh credentials. |
| `6`  | Rate-limited / crawl-delay not elapsed | Back off; read `crawl.retry_after` (stdout JSON + stderr). |
| `7`  | Render required but no browser | Install Chromium or set `render="never"`. |

Codes `0`/`10`/`11` are frozen across all major versions.

## Commands

| Command | Purpose |
|---------|---------|
| `cf init` | Scaffold `changefeed.toml` + `.changefeed/`. |
| `cf watch <url>` | Register a target (appends to config). |
| `cf check [targets…]` | Observe once: fetch → diff → emit event(s) + exit code. The agent primitive. |
| `cf snapshot <target>` | Capture/seed a baseline without diffing. |
| `cf diff <a.html> <b.html>` | Diff two local HTML files (no store). |
| `cf feed [targets…]` | Paginated catch-up stream of recent change events (`--limit`, `--after-cursor`, `--since`, `--max-salience-first`). |
| `cf ls` / `cf show <target>` | List targets / show one target's details. |
| `cf rules` | Show/validate the resolved rule pack (warns on zero-match or selector drift). |
| `cf schema` | Print the `changefeed/v1` JSON Schema. |
| `cf explain <event_id>` | Replay the salience scoring for an event as a table. |
| `cf score --dry-run <a> <b>` | Score an ad-hoc HTML pair without touching the store (the tuning loop). |

Useful flags: `--format jsonl|json|pretty`, `--min-salience none|low|medium|high|critical`, `--no-store`/`--peek` (read-only probe that re-emits), `--archetype <pack>`.

## Configuration

`changefeed.toml` is config-as-code (reviewable in PRs). Secrets use `${ENV}` expansion and are never written to the store or logs.

```toml
[defaults]
schedule       = "15m"
render         = "auto"          # auto | never | chromium
timeout        = "30s"
respect_robots = true
min_salience   = "low"

[[target]]
id        = "acme-pricing"       # stable handle → src.tid in events
url       = "https://acme.com/pricing"
archetype = "pricing"            # selects the salience rule pack + extract profile
select    = [".PricingTable"]    # CSS selector(s) to scope extraction
ignore    = [".cookie-banner", { attr = "data-csrf-nonce" }, { regex = '\d+ viewing right now' }]

# [target.auth]                  # optional; ${ENV}-expanded, redacted from logs
# header = { Authorization = "Bearer ${API_TOKEN}" }
```

## Rule packs

A rule pack is pure declarative TOML (no code), resolved last-wins: **built-in default → archetype → per-target**. It tunes signal weights, materiality bands, regex/category rules, and the band→action map. MVP ships:

- **`pricing`** — price/plan changes (twitchy bands; `plan removed → escalate`).
- **`api-docs`** — breaking-change detection (e.g. a rate-limit drop → `open_ticket`).
- **`status-page`** — incident detection (`investigating/outage → page_oncall`, sticky).

## Architecture

A two-crate workspace with a **compile-enforced purity wall**:

- **`cf-core`** — the entire deterministic pipeline. Depends on no network/clock/disk crate, so impurity in a core stage is a *compile error*. Same input → byte-identical `sal`/`mat`/`act`/`conf` on any machine, offline.
- **`cf`** — the impure boundary: `reqwest`+`rustls` Tier-1 fetch, `tokio`, `rusqlite`+`zstd` storage (one prior `CanonicalDoc` blob + raw HTML, keep-last-N ring), `clap` CLI, ULID/clock.

See [`DESIGN.md`](DESIGN.md) for the full specification and [`ARCHITECTURE.md`](ARCHITECTURE.md) for the frozen type contract.

## MVP scope

**In:** Tier-1 HTTP (ETag/304, robots, `render=chromium` → exit 7), full §5.3 normalization, the three identities, the full diff (slot-anchor → LIS → similarity-fill → Myers, static ignore masking, idempotency dedup), 7 salience signals + noisy-OR + confidence, the three rule packs above, zstd-19 keep-last-N storage with `doc_hash`/304 zero-write short-circuits, the full CLI, and the §4.5 exit-code contract.

**Phase 2 (not yet):** headless/Tier-2 render, the daemon + `cf feed --tail`, an MCP server, CAS/delta-chain storage, the `move`/`struct` cascade delta encodings, learned per-`slot_key` volatility, and the optional LLM adjudication tier.

## Development

```bash
cargo build --release
cargo test --workspace          # 264 tests
cargo clippy --workspace --all-targets -- -D warnings
cargo bench -p cf-core          # diff/salience criterion benches
```

The core is clock/RNG/network-free, so every stage is unit-testable with plain data; fetch tests use a local `wiremock` origin (never the real internet).
