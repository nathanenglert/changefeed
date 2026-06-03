//! The FROZEN core type contract (ARCHITECTURE.md §2).
//!
//! Every stage depends on these. Identity newtypes have **private inners** and constructor-only
//! creation, so joining on the wrong identity is a *type error* (§2.1). Closed enums are plain
//! `enum`s (NO `#[non_exhaustive]`) so integrator matches are exhaustive and default-less forever
//! within v1.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::Date as NaiveDate;

use crate::fetch::FetchMeta;
use crate::CfError;

// ===========================================================================================
// §2.1 Identities — private inners, constructor-only creation.
// ===========================================================================================

/// §5.4 CROSS-OBSERVATION join key. Text-free; the ONLY key the aligner/dedup/seg.fp join on.
/// `blake3(anchor)` or `blake3(breadcrumb‖type‖ordinal_within_section_of_type)[:12]`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct SlotKey([u8; 12]); // private inner — Copy, no heap

impl SlotKey {
    /// Explicit-anchor scheme (priority).
    pub fn anchor(id: &str) -> Self {
        let h = blake3::hash(id.as_bytes());
        Self(truncate12(h.as_bytes()))
    }

    /// Structural scheme.
    pub fn structural(breadcrumb: &str, ty: BlockType, ordinal: u32) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(breadcrumb.as_bytes());
        hasher.update(&[0x1f]); // unit separator framing so a‖b cannot collide with aa‖''
        hasher.update(&(ty as u32).to_le_bytes());
        hasher.update(&[0x1f]);
        hasher.update(&ordinal.to_le_bytes());
        Self(truncate12(hasher.finalize().as_bytes()))
    }

    /// 12-hex prefix for `seg.fp` (display only).
    pub fn fp_hex(&self) -> String {
        to_hex(&self.0)
    }

    /// Reconstruct from the raw 12-byte identity. Storage-only: persisting and restoring a
    /// `CanonicalDoc` byte-identically requires re-materializing the exact key bytes, which the
    /// hashing constructors (`anchor`/`structural`) cannot reach. NOT a join-key forge — the bytes
    /// come straight back out of the blob this same store wrote.
    pub fn from_bytes(bytes: [u8; 12]) -> Self {
        Self(bytes)
    }

    /// The raw 12 bytes (storage round-trip).
    pub fn as_bytes(&self) -> [u8; 12] {
        self.0
    }
}

/// §5.4 WITHIN-OBSERVATION content handle = `blake3(slot_key‖':'‖norm_text)[:12]`.
/// CHANGES on any text edit BY CONSTRUCTION. Never a join key (does not impl any join trait).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BlockId([u8; 12]);

impl BlockId {
    pub fn derive(slot: &SlotKey, norm_text: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&slot.0);
        hasher.update(b":");
        hasher.update(norm_text.as_bytes());
        Self(truncate12(hasher.finalize().as_bytes()))
    }

    /// 12-hex prefix (display only).
    pub fn hex(&self) -> String {
        to_hex(&self.0)
    }

    /// Reconstruct from the raw 12-byte handle (storage round-trip only — see
    /// [`SlotKey::from_bytes`]).
    pub fn from_bytes(bytes: [u8; 12]) -> Self {
        Self(bytes)
    }

    /// The raw 12 bytes (storage round-trip).
    pub fn as_bytes(&self) -> [u8; 12] {
        self.0
    }
}

/// §5.3 XXH3 of normalized text (App B: diff-internal hashing uses XXH3). Detects WHETHER changed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NormHash(u64);

impl NormHash {
    pub fn of(norm_text: &str) -> Self {
        Self(xxhash_rust::xxh3::xxh3_64(norm_text.as_bytes()))
    }

    /// Raw 64-bit value (diff-internal use).
    pub fn raw(&self) -> u64 {
        self.0
    }

    /// Reconstruct from a raw 64-bit value (storage round-trip — the stored value comes from a
    /// prior [`NormHash::raw`] this same store wrote).
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// §5.6 blake3-128 over the canonical tree EXCLUDING `fetched_at`/`fetch`/`block_id`.
/// Equal ⇒ no store. Wire form `blake3:<hex>`. Hash-scheme prefix versioned for forward-compat.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DocHash([u8; 16]);

impl DocHash {
    /// Construct from a 16-byte blake3-128 digest.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Wire form `blake3:<hex>`.
    pub fn to_wire(&self) -> String {
        format!("blake3:{}", to_hex(&self.0))
    }

    /// The raw 16-byte digest (storage round-trip — pairs with [`DocHash::from_bytes`]).
    pub fn as_bytes(&self) -> [u8; 16] {
        self.0
    }
}

/// §7.4 idempotency key = `xxh3_128(target_id‖slot_key‖from_norm_hash‖to_norm_hash)`. Seen-set key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EventKey(u128);

impl EventKey {
    pub fn derive(
        target_id: &str,
        slot: &SlotKey,
        from: NormHash,
        to: NormHash,
    ) -> Self {
        let mut buf = Vec::with_capacity(target_id.len() + 12 + 16);
        buf.extend_from_slice(target_id.as_bytes());
        buf.push(0x1f);
        buf.extend_from_slice(&slot.0);
        buf.push(0x1f);
        buf.extend_from_slice(&from.0.to_le_bytes());
        buf.extend_from_slice(&to.0.to_le_bytes());
        Self(xxhash_rust::xxh3::xxh3_128(&buf))
    }

    /// Raw 128-bit value.
    pub fn raw(&self) -> u128 {
        self.0
    }
}

/// Records which `slot_key` scheme produced the key (§5.4) — informs `c_align`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AnchorScheme {
    Anchor,
    Struct,
}

#[inline]
fn truncate12(bytes: &[u8]) -> [u8; 12] {
    let mut out = [0u8; 12];
    out.copy_from_slice(&bytes[..12]);
    out
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ===========================================================================================
// §2.2 CanonicalDoc / Block / TypedValue (§5.4–§5.6)
// ===========================================================================================

#[derive(Clone, Debug)]
pub struct CanonicalDoc {
    pub schema: &'static str, // "changefeed.canonical/1"
    pub url: String,
    pub final_url: String,
    pub fetched_at: String, // RFC3339 — DATA ONLY; EXCLUDED from doc_hash
    pub fetch: FetchMeta,    // EXCLUDED from doc_hash
    pub profile_id: String,
    pub doc_hash: DocHash, // over slot_key+type+norm_text/value (NOT block_id)
    pub blocks: Vec<Block>, // pre-order tree
    pub stats: DocStats,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub slot_key: SlotKey,
    pub block_id: BlockId,
    pub ty: BlockType,
    pub level: Option<u8>, // heading level
    pub text: String,      // normalized visible text (NFC, collapsed)
    pub value: Option<TypedValue>, // parsed value; diff compares this when present
    pub anchored_by: AnchorScheme,
    pub norm_hash: NormHash,
    pub preorder_idx: u32, // deterministic tie-break key
    pub dom_depth: u16,    // pos-signal proxy (Tier-1)
    /// §6.2 presentation signature: bitflags for content-preserving markup that nonetheless carries
    /// meaning (strikethrough/`<del>`/`<s>` = deprecation; `<ins>` = insertion). Normalized text is
    /// presentation-agnostic, so a pure restyle leaves `norm_hash` unchanged — this is the only
    /// signal that distinguishes it, letting the diff emit a low-salience `restyled` op (§6.2). `0`
    /// for the overwhelming majority of blocks; it only folds into `doc_hash` when nonzero so normal
    /// pages keep byte-identical hashes.
    pub restyle_sig: u8,
    pub children: Vec<Block>,
}

/// Presentation-signature bitflags for [`Block::restyle_sig`] (§6.2).
pub mod restyle {
    /// The block's text (or part of it) is struck through / marked deleted (`<del>`/`<s>`/`<strike>`).
    pub const STRIKE: u8 = 0b0000_0001;
    /// The block's text (or part of it) is marked inserted (`<ins>`).
    pub const INSERT: u8 = 0b0000_0010;
}

/// §5.5 — closed for typing logic; drives the `type` salience weight.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockType {
    Heading,
    Paragraph,
    ListItem,
    TableRow,
    Table,
    Code,
    Link,
    Price,
    Date,
    Number,
    Text,
}

/// §5.5 — carries the PARSED value so diff compares structured values, not strings.
/// Money/quantity use exact decimals/integers (`rust_decimal` / minor units) — NO f64 nondeterminism.
#[derive(Clone, Debug, PartialEq)]
pub enum TypedValue {
    Price {
        amount_minor: i64,
        currency: String,
        period: Option<String>,
    },
    Date(NaiveDate),  // civil date, NO instant / NO clock
    Number(Decimal),  // rust_decimal — exact
    Code(String),
    TableRow(Vec<String>), // cell texts in order
    Link { href_canonical: String }, // post-§5.3-canonicalization
    Heading(String),
    Text(String),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DocStats {
    pub block_count: u32,
    pub stripped_attrs: u32,
    pub bytes_raw: u64,
}

// ===========================================================================================
// §2.3 ChangeEvent and all sub-objects (§6.2, App B)
// ===========================================================================================

/// The frozen schema-version string for `changefeed/v1`. Kept as a private constant so the public
/// `v: &'static str` fields stay `'static` (the frozen §2.3 contract) while still round-tripping.
const SCHEMA_VERSION: &str = "1";

/// Validate an incoming `v` major version. A `&'static str` field cannot be *borrowed* from a
/// runtime deserializer, so the mirror structs below carry an owned `v: String`; this maps a
/// recognized major back to the static `"1"`. Unknown majors are rejected (§6.9: `v` is the major).
fn checked_version(v: &str) -> Result<&'static str, String> {
    match v {
        "1" => Ok(SCHEMA_VERSION),
        other => Err(format!(
            "unsupported schema version {other:?} (this build speaks changefeed/v1)"
        )),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ChangeEvent {
    pub v: &'static str, // "1"
    pub id: EventId,     // ULID, "cfe_" prefix — minted ONLY at the cli boundary
    pub src: Src,
    pub obs: String, // RFC3339 — injected at boundary, never read in core
    pub base: Base,
    pub seg: Vec<Seg>, // usually len 1
    pub ct: ChangeType,
    pub delta: Delta,
    pub why: Why,
    pub followup: Followup,
    pub conf: f32, // §6.6 product of five factors
    pub prov: Prov,
}

/// Owned deserialization mirror of [`ChangeEvent`]: identical wire shape but with an owned `v` so
/// the public type can keep its frozen `v: &'static str` field. Unknown fields are ignored (§6.9 —
/// serde's default, no `deny_unknown_fields`).
#[derive(Deserialize)]
struct ChangeEventWire {
    v: String,
    id: EventId,
    src: Src,
    obs: String,
    base: Base,
    seg: Vec<Seg>,
    ct: ChangeType,
    delta: Delta,
    why: Why,
    followup: Followup,
    conf: f32,
    prov: Prov,
}

impl std::convert::TryFrom<ChangeEventWire> for ChangeEvent {
    type Error = String;
    fn try_from(w: ChangeEventWire) -> Result<Self, Self::Error> {
        Ok(ChangeEvent {
            v: checked_version(&w.v)?,
            id: w.id,
            src: w.src,
            obs: w.obs,
            base: w.base,
            seg: w.seg,
            ct: w.ct,
            delta: w.delta,
            why: w.why,
            followup: w.followup,
            conf: w.conf,
            prov: w.prov,
        })
    }
}

// Hand-written so the `'de` lifetime is fully decoupled from the public `v: &'static str` field
// (serde's derive would otherwise add a `'de: 'static` bound from that field type, making the
// event un-deserializable from a runtime buffer). Delegates to the owned `ChangeEventWire`.
impl<'de> Deserialize<'de> for ChangeEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ChangeEventWire::deserialize(deserializer)?;
        ChangeEvent::try_from(wire).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Src {
    pub url: String,
    pub tid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Base {
    pub obs: String,
    pub snap: String, // "blake3:<hex>"
    pub rev: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Seg {
    pub anchor: String,     // AUTHORITATIVE join anchor (nearest stable heading/label)
    pub fp: String,         // AUTHORITATIVE: "blake3:<12hex>" slot_key prefix
    pub label_path: String, // DISPLAY ONLY — never a join key
    pub role: String,       // open vocab: price|date|version|status|link|prose|code|table-cell|nav|meta
}

/// §6.3 tagged by `enc`. Modeled so illegal combinations are unrepresentable.
/// `a` = after, `b` = before (`a` first). `move`/`struct` are Phase-2-emitted variants
/// (present, unused in MVP).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "enc", rename_all = "lowercase")]
pub enum Delta {
    Val {
        a: String,
        b: String,
    },
    Idiff {
        ops: Vec<IdiffOp>, // [op,text] tuples, ±6-token context
    },
    Block {
        a: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        b: Option<String>, // ≤600c each side
        atrunc: bool,
    },
    Move {
        from: u32,
        to: u32,
        key: String,
    }, // Phase 2
    Struct {
        added: u32,
        removed: u32,
        modified: u32,
        sample: Vec<Delta>,
        truncated: u32,
    }, // Phase 2
}

/// §6.3 op-codes: `=` keep / `-` delete / `+` insert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffOp {
    Keep,
    Del,
    Ins,
}

/// One `idiff` operation, serialized as the diff-match-patch `[op, text]` tuple form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdiffOp {
    pub op: DiffOp,
    pub text: String,
}

impl Serialize for IdiffOp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeTuple;
        let op = match self.op {
            DiffOp::Keep => "=",
            DiffOp::Del => "-",
            DiffOp::Ins => "+",
        };
        let mut tup = serializer.serialize_tuple(2)?;
        tup.serialize_element(op)?;
        tup.serialize_element(&self.text)?;
        tup.end()
    }
}

impl<'de> Deserialize<'de> for IdiffOp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let (op, text): (String, String) = Deserialize::deserialize(deserializer)?;
        let op = match op.as_str() {
            "=" => DiffOp::Keep,
            "-" => DiffOp::Del,
            "+" => DiffOp::Ins,
            other => {
                return Err(serde::de::Error::custom(format!(
                    "unknown idiff op {other:?}"
                )))
            }
        };
        Ok(IdiffOp { op, text })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Why {
    pub sal: f32, // 0..1, emitted to 2 decimals (within-target comparable only)
    pub mat: Materiality, // cross-target-comparable routing label
    pub cat: String, // open vocab; unknown -> "content_edit"
    pub summary: String, // ≤160 chars
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Followup {
    pub act: Action, // CLOSED 8-value enum, exactly one
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tgt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Prov {
    pub m: FetchTier, // http|headless|api|rss (MVP emits http)
    pub hash: String, // "blake3:<hex>" this observation's snapshot
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pack: Option<String>, // "pricing@b3:2f1a" — pack id + content hash
}

/// §6.2 `ct` — CLOSED, FROZEN v1. No `#[non_exhaustive]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeType {
    Added,
    Removed,
    Modified,
    Reordered,
    Restyled,
}

/// §6.4 cross-target-comparable bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Materiality {
    None,
    Low,
    Medium,
    High,
    Critical,
}

/// §6.5 / App B — CLOSED AGENT-FACING 8, FROZEN v1, ordered by escalating cost.
/// `verify_llm` is DELIBERATELY ABSENT (it is an internal `ScoreState`, never emitted).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Ignore,
    Notify,
    RefetchLinked,
    ReembedKb,
    ReRunDownstream,
    OpenTicket,
    EscalateHuman,
    PageOncall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FetchTier {
    Http,
    Headless,
    Api,
    Rss,
}

/// `"cfe_<ULID>"` — built only in cli; tests inject a seeded source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventId(String);

impl EventId {
    /// Construct from a pre-formatted `cfe_<ULID>` string (minted at the cli boundary).
    pub fn new(id: String) -> Self {
        Self(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ===========================================================================================
// §2.4 FeedEnvelope — change / no-change / baseline forms (§6.8)
// ===========================================================================================

/// One envelope per crawl of one target. crawl-level metadata sent once.
#[derive(Clone, Debug, Serialize)]
pub struct FeedEnvelope {
    pub v: &'static str, // "1"
    pub feed: String,    // tid
    pub batch: String,   // "cfb_<ULID>"
    pub obs: String,
    pub crawl: Crawl,
    pub events: Vec<ChangeEvent>, // empty for no-change / baseline
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>, // schedule hint (daemon)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>, // feed pagination
}

/// Owned deserialization mirror of [`FeedEnvelope`] (see [`ChangeEventWire`]). Unknown fields are
/// ignored (§6.9). `events` reuses the public `ChangeEvent` (already `try_from`-deserializable).
#[derive(Deserialize)]
struct FeedEnvelopeWire {
    v: String,
    feed: String,
    batch: String,
    obs: String,
    crawl: Crawl,
    #[serde(default)]
    events: Vec<ChangeEvent>,
    #[serde(default)]
    next: Option<String>,
    #[serde(default)]
    next_cursor: Option<String>,
}

impl std::convert::TryFrom<FeedEnvelopeWire> for FeedEnvelope {
    type Error = String;
    fn try_from(w: FeedEnvelopeWire) -> Result<Self, Self::Error> {
        Ok(FeedEnvelope {
            v: checked_version(&w.v)?,
            feed: w.feed,
            batch: w.batch,
            obs: w.obs,
            crawl: w.crawl,
            events: w.events,
            next: w.next,
            next_cursor: w.next_cursor,
        })
    }
}

// Hand-written for the same reason as [`ChangeEvent`]: decouple `'de` from `v: &'static str`.
impl<'de> Deserialize<'de> for FeedEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = FeedEnvelopeWire::deserialize(deserializer)?;
        FeedEnvelope::try_from(wire).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Crawl {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_rev: Option<u64>, // null on first observation
    pub to_rev: u64,           // == from_rev with n:0 ⇒ no-change
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub m: FetchTier,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag_hit: Option<bool>, // true ⇒ ~0-byte 304 short-circuit
    pub n: u32,                 // event count
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<bool>, // true + from_rev:null ⇒ first observation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u32>, // emitted on rate-limit; no second fetch needed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub err: Option<String>, // fetch failure (also distinguishable by exit code)
}

// ===========================================================================================
// §2.5 Profile / Config (§4.9, §5.7)
// ===========================================================================================

/// Per-source tuning seam threaded as `&Profile` into EVERY pure stage (testability backbone).
#[derive(Clone, Debug)]
pub struct Profile {
    pub profile_id: String,
    pub render: RenderMode, // auto|never|chromium (chromium -> exit 7 in MVP)
    pub strategy: ExtractStrategy, // readability|selector|full
    pub root_selector: Option<String>,
    pub strip_attrs: Vec<String>,
    pub strip_text: Vec<String>, // regex sources
    pub unordered: Vec<String>,  // selectors -> children sorted by slot_key
    pub mode: SourceMode,        // page (MVP); feed|paginated|infinite Phase 2
    pub max_pages: u32,          // MVP default 1
    pub types: Vec<(String, BlockType)>, // CSS selector -> forced type override
    pub archetype: Option<String>, // resolves the rule pack
    pub salience_hints: Vec<String>, // keywords
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderMode {
    Auto,
    Never,
    Chromium,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtractStrategy {
    Readability,
    Selector,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceMode {
    Page,
    Feed,
    Paginated,
    Infinite,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub defaults: Defaults,
    pub sinks: Vec<SinkCfg>, // daemon-only; parsed but inert in MVP
    pub targets: Vec<TargetCfg>,
}

#[derive(Clone, Debug)]
pub struct Defaults {
    pub schedule: Duration,  // humantime; daemon-only
    pub render: RenderMode,
    pub timeout: Duration,
    pub user_agent: String,
    pub respect_robots: bool,
    pub min_salience: Materiality,
    pub store_format: String, // MVP: "zstd"
}

#[derive(Clone, Debug)]
pub struct TargetCfg {
    pub id: String, // -> src.tid
    pub url: String,
    pub schedule: Option<Duration>,
    pub archetype: Option<String>,
    pub select: Vec<String>,
    pub ignore: Vec<IgnoreRule>, // selector | { attr } | { regex }
    pub salience_hints: Vec<String>,
    pub auth: Option<AuthCfg>,
    pub profile: Profile, // resolved: archetype preset + per-target overrides (last-wins)
}

#[derive(Clone, Debug)]
pub enum IgnoreRule {
    Selector(String),
    Attr(String),
    Regex(String),
}

#[derive(Clone)]
pub enum AuthCfg {
    // ${ENV}-expanded at the cli boundary, redacted from logs.
    Header { headers: Vec<(String, String)> },
    Cookie { cookies: String },
    Basic { username: String, password: String },
    Browser, // Phase 2 (render=chromium)
}

/// §12: secret-bearing values are NEVER rendered in logs. `AuthCfg` carries `${ENV}`-expanded
/// secrets, so its `Debug` is hand-written to redact every secret value (header values, the cookie
/// string, the basic password) rather than derived — otherwise a stray `{:?}` on a `TargetCfg` /
/// `Config` (which embed an `AuthCfg`) would leak a live token into a log line. Only non-secret
/// shape is shown: the variant, the header NAMES, and the basic username.
impl std::fmt::Debug for AuthCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthCfg::Header { headers } => {
                let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
                f.debug_struct("Header")
                    .field("header_names", &names)
                    .field("values", &"<redacted>")
                    .finish()
            }
            AuthCfg::Cookie { .. } => f.debug_struct("Cookie").field("cookies", &"<redacted>").finish(),
            AuthCfg::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
            AuthCfg::Browser => f.write_str("Browser"),
        }
    }
}

/// Sink configuration — daemon Phase 2. Parsed but inert in MVP.
#[derive(Clone, Debug)]
pub struct SinkCfg {
    pub kind: String,
    pub path_or_url: String,
    pub headers: Vec<(String, String)>,
    pub min_salience: Materiality,
}

/// A wall-clock-free duration (humantime in config I/O). Holds whole seconds; NO clock read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Duration {
    secs: u64,
}

impl Duration {
    pub fn from_secs(secs: u64) -> Self {
        Self { secs }
    }

    pub fn as_secs(&self) -> u64 {
        self.secs
    }
}

// ===========================================================================================
// §2.6 ExitCode (§4.5) and the crate error type
// ===========================================================================================

/// §4.5 frozen agent contract — single source of truth, mapped to `std::process::ExitCode`
/// in `cf::main`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    NoChange = 0,     // re-observed, nothing material (cheapest path; empty stdout)
    Change = 10,      // change at/above --min-salience (only code that requires reading stdout)
    FirstObs = 11,    // baseline stored, no prior to diff
    SubThreshold = 12, // change below --min-salience (only with --emit-subthreshold)
    Usage = 1,        // bad flag / malformed TOML
    NotFound = 2,     // unknown target id
    SoftFetch = 3,    // DNS/TLS/timeout/5xx (transient; --fail-on-fetch-error promotes to 1)
    Robots = 4,       // blocked by robots.txt
    Auth = 5,         // 401/403
    RateLimit = 6,    // 429 / crawl-delay not elapsed (read crawl.retry_after)
    RenderNeeded = 7, // render required but no browser (MVP: render=chromium hits this)
}

impl ExitCode {
    /// The raw process exit code byte.
    pub fn code(self) -> u8 {
        self as u8
    }
}

/// `run_observation` returns this; cli maps it (+ events) to (stdout bytes, ExitCode).
/// One tested place.
pub enum ObservationResult {
    NoChange { reason: NoChangeReason }, // Http304 | DocHashEqual | SubThreshold
    Baseline(FeedEnvelope),              // exit 11
    Changed {
        envelope: FeedEnvelope,
        events: Vec<ChangeEvent>,
    }, // exit 10
    FetchError(CfError),                 // exit 3/4/5/6/7 via CfError::exit_code()
}

pub enum NoChangeReason {
    Http304,
    DocHashEqual,
    SubThreshold,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §12 (B5) — `AuthCfg`'s `Debug` must redact every secret-bearing value so a stray `{:?}` (on
    /// the `AuthCfg` itself or a `TargetCfg`/`Config` that embeds it, whose `Debug` delegates here)
    /// can never leak a live credential into a log line. Header NAMES and the basic username are not
    /// secrets and may show.
    #[test]
    fn auth_cfg_debug_redacts_secrets() {
        let h = AuthCfg::Header {
            headers: vec![("Authorization".into(), "Bearer tok_LIVE_SECRET".into())],
        };
        let s = format!("{h:?}");
        assert!(!s.contains("tok_LIVE_SECRET"), "header value must be redacted, got {s}");
        assert!(s.contains("Authorization"), "the header name is not a secret: {s}");

        let c = AuthCfg::Cookie { cookies: "session=DEADBEEF; csrf=TOPSECRET".into() };
        let s = format!("{c:?}");
        assert!(!s.contains("DEADBEEF") && !s.contains("TOPSECRET"), "cookie must be redacted, got {s}");

        let b = AuthCfg::Basic { username: "alice".into(), password: "hunter2pw".into() };
        let s = format!("{b:?}");
        assert!(!s.contains("hunter2pw"), "password must be redacted, got {s}");
        assert!(s.contains("alice"), "username is not a secret: {s}");
    }
}
