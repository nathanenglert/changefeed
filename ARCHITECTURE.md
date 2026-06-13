# changefeed — MVP Implementation Architecture

**Status:** Authoritative for MVP · Scope: DESIGN.md §13 MVP, frozen by Appendix B, tuned by Appendix C.
**Binary:** `cf` (alias `changefeed`). Single static binary (musl/macOS/Windows).

This document synthesizes the strongest single architecture from three reviewed proposals. The
load-bearing decisions: (1) a **two-crate workspace** so the entire pipeline is library-testable
with no process and no clock; (2) a **compiler-enforced purity wall** — `cf-core` declares no
`reqwest`/`tokio`/`rusqlite`/`rand`/time dependency, so reading a clock, hitting the network, or
touching disk in a pure stage is a *build error*, not a review note; (3) **three distinct identity
newtypes** (`SlotKey`/`BlockId`/`NormHash`) with private inners so joining on the wrong identity is
a *type error* (§2.1 is the design's central correction); (4) **frozen closed enums without
`#[non_exhaustive]`** (App B) so integrators get exhaustive, default-less matches.

---

## 1. Crate / Module Layout

A Cargo **workspace of two crates**. `cf-core` is pure and holds one module per pipeline stage plus
shared types/config; `cf` is the only crate that builds the binary and owns *all* impurity
(clock, RNG, network I/O, disk I/O, env, stdio, process exit).

```
changefeed/
├── Cargo.toml                       # [workspace] members = ["crates/cf-core", "crates/cf"]
├── crates/
│   ├── cf-core/                     # PURE. NO reqwest/tokio/rusqlite/rand/time in Cargo.toml.
│   │   ├── src/
│   │   │   ├── lib.rs               # re-exports + Pipeline::run_observation(input,&Profile,&Ctx)
│   │   │   ├── model.rs             # SHARED TYPES: Block, CanonicalDoc, identities, TypedValue,
│   │   │   │                        #   ChangeEvent + sub-objects, enums, ExitCode, CfError
│   │   │   ├── config.rs            # Config/Profile model + parse (bytes -> Config; no file I/O)
│   │   │   ├── event.rs             # changefeed/v1 wire build + minified serde + JSON Schema
│   │   │   ├── extract.rs           # §5.2 pure fn(&Html,&Profile) -> DomSubtree
│   │   │   ├── normalize.rs         # §5.3 pure fn(subtree,&Profile) -> NormalizedDom
│   │   │   ├── segment.rs           # §5.4/§5.5 pure fn -> CanonicalDoc (slot_key/block_id/type)
│   │   │   ├── diff/                # §7 engine
│   │   │   │   ├── mod.rs           #   pure fn(&old,&new,&Profile) -> Changeset
│   │   │   │   ├── mask.rs          #   §7.0 ignore masking (drop/redact/attr-strip pre-hash)
│   │   │   │   ├── align.rs         #   §7.1 slot_key-anchor pairing (O(B) FxHashMap)
│   │   │   │   ├── lis.rs           #   §7.1 patience-sort LIS over anchor positions
│   │   │   │   ├── lsh.rs           #   §7.1 transient in-RAM MinHash band (fixed seeds)
│   │   │   │   ├── intra.rs         #   §7.2 Myers token diff (shared tokenizer)
│   │   │   │   └── noise.rs         #   §7.3 noise_score
│   │   │   ├── salience/            # §8 scorer
│   │   │   │   ├── mod.rs           #   pure fn(&Changeset,&Pack,pos_source) -> Vec<ScoredEvent>
│   │   │   │   ├── signals.rs       #   7 MVP signals (type/mag/num/date/neg/pos/kw; vol=1.0)
│   │   │   │   ├── combine.rs       #   noisy-OR + bands -> materiality
│   │   │   │   ├── action.rs        #   §8.4 delta -> one of 8 actions (first-match-wins)
│   │   │   │   └── confidence.rs    #   §6.6 conf = c_fetch·c_align·c_match·c_parse·c_stability
│   │   │   ├── packs.rs             # §8.3 TOML rule-pack model + last-wins layering + content hash
│   │   │   ├── fetch.rs             # FetchClient TRAIT + FetchOutcome/FetchMeta (NO http impl here)
│   │   │   ├── storage.rs           # Store TRAIT + types (NO rusqlite/zstd impl here)
│   │   │   └── schema/v1.json       # published JSON Schema 2020-12 (include_str!)
│   │   └── packs/                   # pricing.toml, api-docs.toml, status-page.toml, default.toml
│   │                                #   (include_str! into packs.rs)
│   └── cf/                          # bin `cf` (+ `changefeed`). IMPURE. The ONLY process::exit.
│       └── src/
│           ├── main.rs             # clap derive tree; maps ObservationResult -> ExitCode
│           ├── cli.rs              # subcommands: init/watch/check/snapshot/diff/feed/ls/show/rules/schema
│           ├── ctx.rs             # Ctx{ store, clock, ids, fetch } dependency-injection container
│           ├── fetch_http.rs      # impl FetchClient: reqwest+rustls Tier-1 (the only async surface)
│           ├── store_sqlite.rs    # impl Store: rusqlite(WAL)+zstd fixed ring (§9.8)
│           ├── ids_clock.rs       # impl Clock + IdGen (system time, monotonic ULID)
│           ├── config_io.rs       # read changefeed.toml + .changefeed/secrets.env, ${ENV} expand
│           └── render.rs          # jsonl/json/pretty output; stdout=events, stderr=logs
└── ARCHITECTURE.md
```

**Why the trait split.** `fetch` and `storage` live in `cf-core` as *traits + data types*; their
impure implementations live in `cf`. This keeps every stage callable in tests with mock fetch/store,
keeps the purity wall intact (core never links reqwest/rusqlite/tokio/zstd), and leaves clean
Phase-2 seams (a headless `FetchClient`, a CAS `Store`) that slot in without reshaping core.

**Pipeline orchestration** lives in `cf-core::lib::run_observation` as a linear sequence of pure
transforms with the two short-circuits (HTTP 304 in fetch, `doc_hash`-equal after segment) modeled
as early-return `ObservationResult` variants, so the cheap no-op path is structurally zero-write.

---

## 2. The FROZEN Core Type Contract

Every stage depends on these. Exact, minimal, in `cf-core::model`. Identity newtypes have **private
inners** and constructor-only creation. Closed enums are plain `enum`s (no `#[non_exhaustive]`) so
matches are exhaustive and complete forever within v1.

### 2.1 Identities (§2.1, §5.4, App B)

```rust
/// §5.4 CROSS-OBSERVATION join key. Text-free; the ONLY key the aligner/dedup/seg.fp join on.
/// blake3(anchor) or blake3(breadcrumb‖type‖ordinal_within_section_of_type)[:12].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SlotKey([u8; 12]);                  // private inner — Copy, no heap

impl SlotKey {
    pub fn anchor(id: &str) -> Self;           // explicit-anchor scheme (priority)
    pub fn structural(breadcrumb: &str, ty: BlockType, ordinal: u32) -> Self; // struct scheme
    pub fn fp_hex(&self) -> String;            // 12-hex prefix for seg.fp (display only)
}

/// §5.4 WITHIN-OBSERVATION content handle = blake3(slot_key‖':'‖norm_text)[:12] (base32).
/// CHANGES on any text edit BY CONSTRUCTION. Never a join key (does not impl any join trait).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId([u8; 12]);

impl BlockId { pub fn derive(slot: &SlotKey, norm_text: &str) -> Self; }

/// §5.3 XXH3 of normalized text (App B: diff-internal hashing uses XXH3). Detects WHETHER changed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NormHash(u64);
impl NormHash { pub fn of(norm_text: &str) -> Self; }

/// §5.6 blake3-128 over the canonical tree EXCLUDING fetched_at/fetch/block_id. Equal ⇒ no store.
/// Wire form `blake3:<hex>`. Hash-scheme prefix versioned for forward-compat (b3-256 Phase 2).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DocHash([u8; 16]);
impl DocHash { pub fn to_wire(&self) -> String; }   // "blake3:<hex>"

/// §7.4 idempotency key = xxh3_128(target_id‖slot_key‖from_norm_hash‖to_norm_hash). Seen-set key.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventKey(u128);

/// records which slot_key scheme produced the key (§5.4) — informs c_align.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AnchorScheme { Anchor, Struct }
```

### 2.2 CanonicalDoc / Block / TypedValue (§5.4–§5.6)

```rust
pub struct CanonicalDoc {
    pub schema: &'static str,        // "changefeed.canonical/1"
    pub url: String,
    pub final_url: String,
    pub fetched_at: String,          // RFC3339 — DATA ONLY; EXCLUDED from doc_hash
    pub fetch: FetchMeta,            // EXCLUDED from doc_hash
    pub profile_id: String,
    pub doc_hash: DocHash,           // computed over slot_key+type+norm_text/value (NOT block_id)
    pub blocks: Vec<Block>,          // pre-order tree
    pub stats: DocStats,
}

pub struct Block {
    pub slot_key: SlotKey,
    pub block_id: BlockId,
    pub ty: BlockType,
    pub level: Option<u8>,           // heading level
    pub text: String,                // normalized visible text (NFC, collapsed)
    pub value: Option<TypedValue>,   // parsed value; diff compares this when present
    pub anchored_by: AnchorScheme,
    pub norm_hash: NormHash,
    pub preorder_idx: u32,           // deterministic tie-break key
    pub dom_depth: u16,              // pos-signal proxy (Tier-1)
    pub children: Vec<Block>,
}

/// §5.5 — closed for typing logic; drives the `type` salience weight.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BlockType {
    Heading, Paragraph, ListItem, TableRow, Table, Code, Link,
    Price, Date, Number, Text,
}

/// §5.5 — carries the PARSED value so diff compares structured values, not strings.
/// Money/quantity use exact decimals/integers (rust_decimal / minor units) — NO f64 nondeterminism.
pub enum TypedValue {
    Price  { amount_minor: i64, currency: String, period: Option<String> },
    Date   (NaiveDate),                       // civil date, NO instant / NO clock
    Number (Decimal),                         // rust_decimal — exact
    Code   (String),
    TableRow(Vec<String>),                    // cell texts in order
    Link   { href_canonical: String },        // post-§5.3-canonicalization
    Heading(String),
    Text   (String),
}

pub struct DocStats { pub block_count: u32, pub stripped_attrs: u32, pub bytes_raw: u64 }
```

### 2.3 ChangeEvent and all sub-objects (§6.2, App B)

```rust
pub struct ChangeEvent {
    pub v: &'static str,             // "1"
    pub id: EventId,                 // ULID, "cfe_" prefix — minted ONLY at the cli boundary
    pub src: Src,
    pub obs: String,                 // RFC3339 — injected at boundary, never read in core
    pub base: Base,
    pub seg: Vec<Seg>,               // usually len 1
    pub ct: ChangeType,
    pub delta: Delta,
    pub why: Why,
    pub followup: Followup,
    pub conf: f32,                   // §6.6 product of five factors
    pub prov: Prov,
}

pub struct Src   { pub url: String, pub tid: String, pub title: Option<String> }
pub struct Base  { pub obs: String, pub snap: String, pub rev: u64 }   // snap = "blake3:<hex>"

pub struct Seg {
    pub anchor: String,              // AUTHORITATIVE join anchor (nearest stable heading/label)
    pub fp: String,                  // AUTHORITATIVE: "blake3:<12hex>" slot_key prefix
    pub label_path: String,          // DISPLAY ONLY — never a join key
    pub role: String,                // open vocab: price|date|version|status|link|prose|code|table-cell|nav|meta
}

/// §6.3 tagged by `enc`. Modeled so illegal combinations are unrepresentable.
/// `a` = after, `b` = before (a first). All five variants are emitted.
pub enum Delta {
    Val    { a: String, b: String },
    Idiff  { ops: Vec<IdiffOp> },                                  // [op,text] tuples, ±6-token context
    Block  { a: String, b: Option<String>, atrunc: bool },        // ≤600c each side
    Move   { from: u32, to: u32, key: String },                   // §7.1 reorder
    Struct { added: u32, removed: u32, modified: u32, sample: Vec<Delta>, truncated: u32 }, // §6.3 cascade cluster
}
pub enum DiffOp { Keep, Del, Ins }                 // "=" / "-" / "+"
pub struct IdiffOp { pub op: DiffOp, pub text: String }

pub struct Why {
    pub sal: f32,                    // 0..1, emitted to 2 decimals (within-target comparable only)
    pub mat: Materiality,            // cross-target-comparable routing label
    pub cat: String,                 // open vocab; unknown -> "content_edit"
    pub summary: String,             // ≤160 chars
}

pub struct Followup {
    pub act: Action,                 // CLOSED 8-value enum, exactly one
    pub tgt: Option<String>,
    pub params: Option<serde_json::Map<String, serde_json::Value>>,
    pub q: Option<String>,
}

pub struct Prov {
    pub m: FetchTier,                // http|headless|api|rss (MVP emits http)
    pub hash: String,                // "blake3:<hex>" this observation's snapshot
    pub etag: Option<String>,
    pub status: u16,
    pub ms: Option<u32>,
    pub pack: Option<String>,        // "pricing@b3:2f1a" — pack id + content hash
}

/// §6.2 ct — CLOSED, FROZEN v1. No #[non_exhaustive].
pub enum ChangeType { Added, Removed, Modified, Reordered, Restyled }

/// §6.4 cross-target-comparable bucket.
pub enum Materiality { None, Low, Medium, High, Critical }

/// §6.5 / App B — CLOSED AGENT-FACING 8, FROZEN v1, ordered by escalating cost.
/// `verify_llm` is DELIBERATELY ABSENT (it is an internal ScoreState, never emitted).
pub enum Action {
    Ignore, Notify, RefetchLinked, ReembedKb,
    ReRunDownstream, OpenTicket, EscalateHuman, PageOncall,
}

#[derive(Clone, Copy)]
pub enum FetchTier { Http, Headless, Api, Rss }

pub struct EventId(String);          // "cfe_<ULID>" — built only in cli; tests inject seeded source
```

### 2.4 FeedEnvelope — change / no-change / baseline forms (§6.8)

```rust
/// One envelope per crawl of one target. crawl-level metadata sent once.
pub struct FeedEnvelope {
    pub v: &'static str,             // "1"
    pub feed: String,                // tid
    pub batch: String,               // "cfb_<ULID>"
    pub obs: String,
    pub crawl: Crawl,
    pub events: Vec<ChangeEvent>,    // empty for no-change / baseline
    pub next: Option<String>,        // schedule hint (daemon)
    pub next_cursor: Option<String>, // feed pagination
}

pub struct Crawl {
    pub from_rev: Option<u64>,       // null on first observation
    pub to_rev: u64,                 // == from_rev with n:0 ⇒ no-change
    pub url: Option<String>,
    pub m: FetchTier,
    pub status: u16,
    pub ms: Option<u32>,
    pub hash: Option<String>,
    pub etag_hit: Option<bool>,      // true ⇒ ~0-byte 304 short-circuit
    pub n: u32,                      // event count
    pub baseline: Option<bool>,      // true + from_rev:null ⇒ first observation
    pub retry_after: Option<u32>,    // emitted on rate-limit; no second fetch needed
    pub err: Option<String>,         // fetch failure (also distinguishable by exit code)
}
```

### 2.5 Profile / Config (§4.9, §5.7)

```rust
/// Per-source tuning seam threaded as `&Profile` into EVERY pure stage (testability backbone).
pub struct Profile {
    pub profile_id: String,
    pub render: RenderMode,          // auto|never|chromium (chromium -> exit 7 in MVP)
    pub strategy: ExtractStrategy,   // readability|selector|full
    pub root_selector: Option<String>,
    pub strip_attrs: Vec<String>,
    pub strip_text: Vec<String>,     // regex sources
    pub unordered: Vec<String>,      // selectors -> children sorted by slot_key
    pub mode: SourceMode,            // page (MVP); feed|paginated|infinite Phase 2
    pub max_pages: u32,              // MVP default 1
    pub types: Vec<(String, BlockType)>,  // CSS selector -> forced type override
    pub archetype: Option<String>,        // resolves the rule pack
    pub salience_hints: Vec<String>,      // keywords
}
pub enum RenderMode { Auto, Never, Chromium }
pub enum ExtractStrategy { Readability, Selector, Full }
pub enum SourceMode { Page, Feed, Paginated, Infinite }

pub struct Config {
    pub defaults: Defaults,
    pub sinks: Vec<SinkCfg>,         // daemon-only; parsed but inert in MVP
    pub targets: Vec<TargetCfg>,
}
pub struct Defaults {
    pub schedule: Duration,          // humantime; daemon-only
    pub render: RenderMode,
    pub timeout: Duration,
    pub user_agent: String,
    pub respect_robots: bool,
    pub min_salience: Materiality,
    pub store_format: String,        // MVP: "zstd"
}
pub struct TargetCfg {
    pub id: String,                  // -> src.tid
    pub url: String,
    pub schedule: Option<Duration>,
    pub archetype: Option<String>,
    pub select: Vec<String>,
    pub ignore: Vec<IgnoreRule>,     // selector | { attr } | { regex }
    pub salience_hints: Vec<String>,
    pub auth: Option<AuthCfg>,
    pub profile: Profile,            // resolved: archetype preset + per-target overrides (last-wins)
}
pub enum IgnoreRule { Selector(String), Attr(String), Regex(String) }
pub enum AuthCfg {                   // ${ENV}-expanded at the cli boundary, redacted from logs
    Header { headers: Vec<(String, String)> },
    Cookie { cookies: String },
    Basic  { username: String, password: String },
    Browser,                          // Phase 2 (render=chromium)
}
pub struct SinkCfg { /* type, path/url, headers, min_salience — daemon Phase 2 */ }
```

### 2.6 ExitCode (§4.5) and the crate error type

```rust
/// §4.5 frozen agent contract — single source of truth, mapped to std::process::ExitCode in cf::main.
#[repr(u8)]
pub enum ExitCode {
    NoChange     = 0,    // re-observed, nothing material (cheapest path; empty stdout)
    Change       = 10,   // change at/above --min-salience (the only code that requires reading stdout)
    FirstObs     = 11,   // baseline stored, no prior to diff
    SubThreshold = 12,   // change below --min-salience (only with --emit-subthreshold)
    Usage        = 1,    // bad flag / malformed TOML
    NotFound     = 2,    // unknown target id
    SoftFetch    = 3,    // DNS/TLS/timeout/5xx (transient; --fail-on-fetch-error promotes to 1)
    Robots       = 4,    // blocked by robots.txt
    Auth         = 5,    // 401/403
    RateLimit    = 6,    // 429 / crawl-delay not elapsed (read crawl.retry_after)
    RenderNeeded = 7,    // render required but no browser (MVP: render=chromium hits this)
}

/// run_observation returns this; cli maps it (+ events) to (stdout bytes, ExitCode). One tested place.
pub enum ObservationResult {
    NoChange   { reason: NoChangeReason },        // Http304 | DocHashEqual | SubThreshold
    Baseline   (FeedEnvelope),                    // exit 11
    Changed    { envelope: FeedEnvelope, events: Vec<ChangeEvent> }, // exit 10
    FetchError (CfError),                         // exit 3/4/5/6/7 via CfError::exit_code()
}
pub enum NoChangeReason { Http304, DocHashEqual, SubThreshold }

/// Typed errors in core (thiserror); cli adds context (anyhow) and maps to exit codes.
#[derive(thiserror::Error, Debug)]
pub enum CfError {
    #[error("usage/config: {0}")]      Usage(String),        // -> 1
    #[error("target not found: {0}")]  NotFound(String),     // -> 2
    #[error("fetch failed: {0}")]      SoftFetch(String),    // -> 3
    #[error("blocked by robots.txt")]  Robots,               // -> 4
    #[error("auth failure: {0}")]      Auth(u16),            // -> 5
    #[error("rate limited")]           RateLimit { retry_after: Option<u32> }, // -> 6
    #[error("render required, no browser")] RenderNeeded,    // -> 7
}
impl CfError { pub fn exit_code(&self) -> ExitCode; }
```

---

## 3. Pinned Dependencies

`cf-core` (pure — note the absence of reqwest/tokio/rusqlite/rand/time, which is the purity wall):

| Crate | Version | Use |
|---|---|---|
| `serde` (derive) + `serde_json` | `1` | event/CanonicalDoc/pack; **canonical JSON via explicit sorted-key serializer** for `doc_hash` (never HashMap order) |
| `toml` | `0.8` | `changefeed.toml` + rule packs (TOML over YAML, §4.9) |
| `scraper` | `0.20` | CSS selector extraction (wraps `html5ever` + `selectors`) |
| `html5ever` | `0.27` | DOM parse + tree-walk in extract/segment |
| `blake3` | `1.5` | `block_id`, `doc_hash`, `seg.fp`, pack content hash (App B `blake3:`) |
| `xxhash-rust` (xxh3) | `0.8` | `norm_hash` (u64) + `event_key` (xxh3_128) (App B XXH3) |
| `unicode-normalization` | `0.1` | NFC pass (§5.3) |
| `regex` | `1.10` | volatile strip, ignore rules, pack `RegexSet` (compile-once) |
| `rust_decimal` | `1` | exact price/number values — no f64 nondeterminism |
| `url` | `2.5` | URL canonicalization (§5.3) |
| `rustc-hash` | `2.0` | `FxHashMap<SlotKey,_>` aligner maps — Copy keys, fixed-seed, deterministic |
| `time` | `0.3` (formatting, parsing, macros) | civil-date parse in typing (NO clock use in core) |
| `similar` | `2` | Myers token diff for `idiff`, behind a thin adapter (hand-rolled fallback) |
| `smallvec` | `1.13` | `seg[]` (usually len 1), cluster ops, LSH band buckets |
| `thiserror` | `2` | typed `CfError` |

`cf` (bin — the only crate with impure deps):

| Crate | Version | Use |
|---|---|---|
| `clap` (derive, env) | `4.5` | command tree, completions, clean arg errors |
| `reqwest` (default-features=false; rustls-tls, gzip, brotli, zstd) | `0.12` | Tier-1 HTTP; pure-Rust TLS (no OpenSSL) |
| `tokio` (rt, macros) | `1` | async only at the fetch boundary; current-thread rt for one-shot `cf check` |
| `webpki-roots` | `0.26` | embedded TLS roots (no system trust store) — static-binary requirement |
| `rusqlite` (bundled) | `0.32` | `store.db`: version log, seen-set ring, per-host politeness; bundled SQLite, WAL |
| `zstd` | `0.13` | level-19 CanonicalDoc + raw-HTML blobs (§9.8) |
| `ulid` | `1` | `cfe_`/`cfb_` ids — the ONE place clock+RNG meet, isolated at emit time |
| `humantime` (+`humantime-serde`) | `2.1` | schedule/timeout durations |
| `anyhow` | `1` | boundary error context (core uses thiserror) |
| `tracing` (+ `tracing-subscriber` env-filter) | `0.1` | structured stderr logs (never stdout) |

Dev-deps: `assert_cmd 2.2` + `predicates` (CLI integration), `insta 1` (golden snapshots),
`proptest 1` (normalize/segment/diff properties), `tempfile 3` (throwaway stores),
`wiremock 0.6` (HTTP error-code matrix), `jsonschema 0` (schema conformance), `criterion 0.5` (benches).

---

## 4. Determinism Rules (enforced structurally, not by discipline)

The headline contract is §1.4 goal 2: **same two snapshots → byte-identical event** (modulo the ULID
`id`/`obs`, which are injected at the boundary and pinned in tests).

1. **Clock/RNG/network/disk-free modules — compiler-enforced.** All of `extract`, `normalize`,
   `segment`, `diff`, `salience`, `event`, `packs`, `model`, `config` live in `cf-core`, whose
   `Cargo.toml` lists **no** `reqwest`/`tokio`/`rusqlite`/`zstd`/`rand`/`std::time`-via-`time`-clock
   surface. Reading a clock, opening a socket, or touching disk in a pure stage **fails to compile**.
   Every pure stage signature is `fn(input, &Profile) -> Result<Out, CfError>`.
2. **Impurity confined to `cf`.** Clock (`obs`), RNG+clock (ULID `id`), network (`FetchClient` impl),
   disk (`Store` impl), env (`${ENV}` secret expansion), stdio, and `process::exit` exist *only* in
   the bin crate, injected via `Ctx { store, clock, ids, fetch }`. Tests inject a frozen `Clock`, a
   seeded/stepped `IdGen`, recorded HTML, and a tmp-dir store, so a golden test asserts exact event
   bytes including `id`/`obs`.
3. **No clock in scoring.** `salience::confidence` is a pure function of the snapshot pair + fetch
   tier (passed as data). The `date` signal is **magnitude-only** (civil-date arithmetic, no `now()`).
   The clock-using `date_proximity` enrichment (§8.5) is behind a `date-proximity` cargo feature that
   is **OFF for MVP**, so the non-reproducible path is not even compiled into the shipped binary.
4. **Canonical hashing order.** `doc_hash` uses an explicit sorted-key, no-insignificant-whitespace
   serializer (never `serde_json` struct order, never HashMap iteration), framed with explicit field
   order and length-prefixed concatenation so `a‖b` cannot collide with `aa‖''`. `block_id`/`doc_hash`
   use blake3; `norm_hash`/`event_key` use XXH3 (App B).
5. **Stable ordering everywhere.** `FxHashMap` (fixed seed) for aligner joins; any output-affecting
   iteration is sorted by `(preorder_idx, slot_key)`. Segment ordinals assigned in deterministic
   pre-order. Diff tie-breaks (equal `sim`, LIS ambiguity) resolve by `(preorder_idx, slot_key)`
   lexicographically. LSH MinHash uses **fixed seed constants** (deterministic permutations), built
   transient in-RAM and discarded — never persisted. Rule-pack rules evaluate first-match in declared
   order.
6. **Float discipline.** Prices/numbers use `rust_decimal`/integer minor units (exact). `sal` is f64
   internally but emitted to 2 decimals; band cutoffs are inclusive-lower/exclusive-upper constants,
   so the same delta yields byte-identical `sal`/`mat`/`act` on any arch.
7. **ULID excluded from identity.** The event `id` is the one deliberate non-determinism; it is
   excluded from `event_key`/dedup and every hash, so replays and dedup are stable.

---

## 5. Test Strategy

Pyramid keyed to the pure-function boundaries; golden-file-heavy because determinism is the headline.

1. **Per-module unit tests (fixtures in `crates/cf-core/tests/fixtures/<stage>/`).**
   - *normalize:* table cases for every §5.3 rule (nonce/csrf/high-entropy strip, cache-buster
     collapse, URL canonicalization incl. utm-swap-is-noise, NFC/whitespace, relative-time→`⟦TS⟧`)
     **plus** a proptest that a page differing *only* by volatile tokens normalizes to an identical
     `NormalizedDom` and yields an equal `doc_hash` with **zero** store writes (the §5.3→§5.6 linchpin).
   - *segment:* the §5.4 invariance matrix as explicit tests — edit P1 ⇒ `slot_key` unchanged,
     `block_id` changed, exactly one `modified`; wrapper-div insertion ⇒ keys unchanged; sibling
     reorder of different type ⇒ not delete+add; the two-paragraph worked example.
   - *typing:* table-driven per type (`$11.00/mo` ⇒ `{1100,USD,mo}`, ISO/RFC dates, locale numbers).
   - *diff:* golden `CanonicalDoc` fixture pairs covering the §11 matrix — `val` price change,
     `idiff` prose edit, slot_key-rename ⇒ similarity-fill, mass reorder of 8 lookalike pricing cards
     (struct ordinals disambiguate), redesign/low-overlap ⇒ one low-`conf` `content_edit`,
     first-observation; plus properties `diff(a,a)==empty` and noise-only deltas suppressed.
   - *salience:* `insta` snapshots per shipped pack asserting `sal`/`mat`/`cat`/`act` + the explanation
     breakdown; sticky-rule and band-default paths; Example-1 reproduces `sal 0.86 / mat high /
     act re_run_downstream`.
   - *enum-freeze:* a default-less `match` over `ChangeType`/`Action` in the test crate that fails to
     **compile** if a frozen enum gains a variant; assert `verify_llm` is not constructible as `Action`.
2. **Determinism golden (the headline).** With frozen `Clock` + seeded `IdGen`, run a stored snapshot
   pair end-to-end and assert the **exact minified event bytes**; run each fixture **twice in one
   process** and assert identical bytes (catches HashMap-order / RNG leaks); run across the CI matrix
   (linux-musl/macOS/Windows). Assert the canonical event stays ≤8 KB and Example-1 ≈792 B / ~305 tok.
3. **The §10 worked example as a single golden integration test.** `cf check --stdin --url` with the
   before/after pricing HTML drives the full pipeline to **exit 10** and asserts the exact §10 event
   shape (`$49→$59`, `sal 0.86`, `mat high`, `re_run_downstream`). No network.
4. **CLI integration via `assert_cmd`.** Drive `init/watch/check/snapshot/diff/feed/ls/show/rules/
   schema` against fixtures + `wiremock` origin, asserting the **full exit-code table**
   (0/10/11/12/1/2/3/4/5/6/7), `stdout=events-only` / `stderr=logs` separation, `--peek`/`--no-store`
   re-emit semantics, `--stdin --url`, `--min-salience` gating, feed `--limit`/`--after-cursor`.
5. **Storage tests (tmp `.changefeed/`).** keep-last-N=8 ring eviction; idempotency seen-set suppresses
   a re-run (exit 0); `doc_hash`-equal / 304 short-circuit writes **zero bytes**; per-host crawl-delay
   timestamp persisted; two concurrent `cf check`s do not corrupt the ring (single write lock + WAL).
6. **Schema conformance.** `cf schema --version 1` validates every emitted golden event (`jsonschema`
   dev-dep); assert no field serializes as `null` (presence-as-signal, §6.1 rule 4); assert closed
   enums never emit `verify_llm`.
7. **`criterion` benches.** Diff on a synthetic 5k-block page: high-anchor path single-digit ms,
   low-anchor (rename-everything, LSH-bounded) path low-tens of ms; assert sim()-calls-per-residual
   stays ≤ the `±W`+band cap (proves no O(B²) regression by *operation count*, not wall-clock).

---

## 6. Implementation Order

Build inward-out along the pure spine first (each stage is independently golden-testable), then the
impure boundary, then wiring. The order matches the dependency arrows: later stages consume earlier
types.

1. **`event` + `model`** — the frozen type contract, minified serde, JSON Schema, `cf schema`.
   Everything else depends on these types; lock them and the App-B freeze first (enum-freeze
   compile tests in place from day one).
2. **`extract`** — strategy dispatch (selector first — it is deterministic and the recommended path
   for the high-value archetypes; readability best-effort for prose), hard-strip set. Golden HTML
   fixtures per strategy.
3. **`normalize`** — the §5.3 volatile-strip set (the no-false-positive linchpin). Property test
   that volatile-only deltas normalize identically *before* anything downstream relies on it.
4. **`segment`** (incl. typing) — `slot_key`/`block_id`/`norm_hash`, `BlockType`+`TypedValue` parsing,
   incremental `doc_hash` fold. The §5.4 invariance matrix.
5. **`diff`** — mask → slot_key-anchor → LIS → similarity-fill → Myers intra-block → noise. The most
   golden/property-tested module; fixed-seed LSH and `(preorder_idx, slot_key)` tie-breaks throughout.
6. **`salience`** — 7 signals + noisy-OR + bands + action mapping + `conf`; the three shipped packs
   (`pricing`/`api-docs`/`status-page`) embedded via `include_str!`. Snapshot tests per pack.
7. **`storage`** (`Store` trait + `store_sqlite` impl) — the §9.8 fixed-ring zstd-19 blobs +
   `store.db` (version log, seen-set ring N=1000, per-host politeness). 304/`doc_hash` short-circuit
   = zero writes. Single write lock + WAL.
8. **`fetch`** (`FetchClient` trait + `fetch_http` impl) — reqwest+rustls Tier-1, conditional GET +
   304 short-circuit, 10 s/3-redirect/5 MB caps, robots.txt cache + crawl-delay gate. `render=chromium`
   → exit 7. `--stdin --url` bypasses fetch entirely (drives the deterministic golden tests).
9. **`cli`** — clap tree, `Ctx` injection (`Clock`/`IdGen`/`Store`/`FetchClient`), config + secrets
   I/O, exit-code mapping, stdout/stderr split, render formats. The agent contract assembled and
   integration-tested last with `assert_cmd` against the full exit-code table.

---

## Key Decisions (synthesis rationale)

- **Two-crate workspace + trait-isolated I/O** (from proposals 2 & 3) over a five-crate split
  (proposal 1): the pipeline is fully library-testable with no process, and the purity wall is still
  compiler-enforced because `cf-core` simply does not depend on reqwest/tokio/rusqlite/rand/time.
  `FetchClient`/`Store` as traits in core (impls in `cf`) preserve both purity and the Phase-2 seams.
- **Three identity newtypes with private inners** (all three proposals agree): `SlotKey`/`BlockId`/
  `NormHash` make the §2.1 correction a *type-system invariant* — joining on `block_id` is a compile
  error.
- **Frozen closed enums without `#[non_exhaustive]`** and `Action` with exactly 8 variants (no
  `verify_llm`): integrators get exhaustive, default-less matches valid forever within v1.
- **Determinism by construction**: explicit sorted-key canonical-JSON serializer for `doc_hash`,
  `rust_decimal`/minor-units for money, `FxHashMap` fixed seed, `(preorder_idx, slot_key)` tie-breaks,
  fixed-seed LSH, ULID excluded from all hashes, `date_proximity` feature-gated OFF.
- **Strict MVP scope**: Tier-1 HTTP only, §9.8 fixed-ring zstd store (no CAS/packfiles/dictionary/GC),
  7 salience signals (`w_vol=1.0`), 3 packs, LLM tier not built, the 10 listed CLI verbs.
