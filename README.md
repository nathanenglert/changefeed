# changefeed

**Turn a web page's changes into compact, structured events for AI agents — instead of re-stuffing whole pages into an LLM.**

`changefeed` (binary: `cf`) re-observes a URL and, for each *meaningful* change, emits one tiny JSON event with three things:

- **the delta** — what actually changed, from a deterministic, LLM-free pipeline;
- **why it matters** — a salience score, materiality band, and category;
- **a follow-up** — one suggested action from a fixed, agent-facing set.

The common case — *nothing changed* — costs one integer compare (exit `0`, empty output, zero tokens). Only a real change needs reading. A `$49 → $59` price move becomes a ~300-token event instead of two full page copies plus reasoning. **Noise resistance is the point:** a rotating "127 viewing now" counter or a `?v=12345` cache-buster is a free no-op, while a real price or status change fires.

> **Status: v1.0** — Tier-1 HTTP. Headless render, the daemon, and an LLM tier are deferred to Phase 2 (see the roadmap in [DESIGN.md](DESIGN.md)). 281 tests pass; `clippy -D warnings` clean.

## The mental model

An agent polls in a loop and branches on the **exit code** — usually without reading the output at all:

- **`0`** — no material change. Empty output, zero tokens. *(the dominant case)*
- **`10`** — a real change at/above your `--min-salience`. Read the JSON on stdout and act.

`0`, `10`, and `11` (first observation) are a frozen contract; other codes signal errors (bad config, fetch failure, rate-limited, …). Full exit-code table → [DESIGN.md](DESIGN.md).

## Quick start

```bash
cf init                                        # scaffold ./changefeed.toml + ./.changefeed/
cf watch https://acme.com/pricing              # register a target
cf check acme-pricing --min-salience medium    # observe once → exit code (+ event if changed)
```

`cf check <url>` with no config also works — it seeds a baseline on first run and diffs thereafter. To wire it into an agent, branch on the exit code; the polling loop is in **[HOW_IT_WORKS.md](HOW_IT_WORKS.md)**.

## Example change event

`cf check` emits one compact JSON object per material change:

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

The wire schema is frozen and self-describing — run `cf schema` for the published JSON Schema (`changefeed/v1`). A field-by-field gloss is in [HOW_IT_WORKS.md](HOW_IT_WORKS.md).

## How it works

A pure, deterministic pipeline — no clock, randomness, network, or disk in the core, so the same two page versions always produce the same verdict:

```
fetch → extract → normalize → segment → diff → salience → event
```

Get the page cheaply → keep the real content → strip per-load noise → cut into labeled blocks → compare against last time → score how much it matters → write the tiny event. The plain-English walkthrough (with this exact pricing example, end to end) is in **[HOW_IT_WORKS.md](HOW_IT_WORKS.md)**; the full specification is in [DESIGN.md](DESIGN.md).

## Install

Requires **Rust 1.80+**.

```bash
cargo build --release            # binary at target/release/cf
cargo install --path crates/cf   # or install onto your PATH
```

## Configure

`changefeed.toml` is config-as-code (reviewable in PRs); secrets use `${ENV}` expansion and are never written to the store or logs.

```toml
[defaults]
render       = "auto"
min_salience = "low"

[[target]]
id        = "acme-pricing"        # stable handle → src.tid in events
url       = "https://acme.com/pricing"
archetype = "pricing"             # picks a tuned rule pack + extract profile
select    = [".PricingTable"]     # scope extraction with CSS selector(s)
ignore    = [".cookie-banner", { regex = '\d+ viewing right now' }]
```

Three archetype packs ship — **pricing** (twitchy price moves), **status-page** (incidents → `page_oncall`), and **api-docs** (breaking changes → `open_ticket`). The full config and rule-pack reference is in [DESIGN.md](DESIGN.md).

## Development

```bash
cargo build --release
cargo test --workspace                                  # 281 tests
cargo clippy --workspace --all-targets -- -D warnings
cargo bench -p cf-core                                  # diff/salience benches
```

The core is clock/RNG/network-free, so every stage is unit-testable with plain data; fetch tests use a local `wiremock` origin (never the real internet). The two-crate purity wall and the frozen type contract are documented in **[ARCHITECTURE.md](ARCHITECTURE.md)**.

## Learn more

- **[HOW_IT_WORKS.md](HOW_IT_WORKS.md)** — friendly, step-by-step walkthrough *(start here)*.
- **[DESIGN.md](DESIGN.md)** — the full spec: commands, exit codes, config, diff, salience, storage, roadmap.
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — the frozen Rust type & API contract and the purity wall.
