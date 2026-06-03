//! §8.3 rule-pack model + last-wins layering + content hash (ARCHITECTURE.md §1).
//!
//! The three shipped packs (`pricing`/`api-docs`/`status-page`) plus `default` are embedded via
//! `include_str!` and parsed from TOML. Pack content hash is `blake3:` (App B). Rules evaluate
//! first-match in declared order (§4 determinism rule 5).
//!
//! A pack is pure data — no code — resolved last-wins: **built-in `default` → archetype →
//! per-target** (`extends` chains the parent). Layering is structural:
//! * `block_type_weight` / `bands` / `band_action` keys merge with the child winning,
//! * regex `rule`s concatenate (parent first, then child) so the parent's stickies still fire,
//! * `category` rows merge by `cat` with the child winning.

use crate::model::{Action, BlockType, Materiality};
use crate::CfError;
use serde::Deserialize;
use std::collections::BTreeMap;

/// The four shipped packs, embedded at compile time.
pub const PRICING_TOML: &str = include_str!("../packs/pricing.toml");
pub const API_DOCS_TOML: &str = include_str!("../packs/api-docs.toml");
pub const STATUS_PAGE_TOML: &str = include_str!("../packs/status-page.toml");
pub const DEFAULT_TOML: &str = include_str!("../packs/default.toml");

/// §8.5 scorer version — stamped (with the pack content hash) into `prov.pack` so any score is
/// reproducible later.
pub const SCORER_VERSION: &str = "1";

/// §6.4 default materiality cutoffs (App B; pack-overridable). Inclusive lower bounds.
pub const DEFAULT_BAND_CRITICAL: f32 = 0.90;
pub const DEFAULT_BAND_HIGH: f32 = 0.70;
pub const DEFAULT_BAND_MEDIUM: f32 = 0.40;
pub const DEFAULT_BAND_LOW: f32 = 0.15;

/// §6.4 materiality band cutoffs (inclusive lower bounds), pack-overridable.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bands {
    pub critical: f32,
    pub high: f32,
    pub medium: f32,
    pub low: f32,
}

impl Default for Bands {
    fn default() -> Self {
        Bands {
            critical: DEFAULT_BAND_CRITICAL,
            high: DEFAULT_BAND_HIGH,
            medium: DEFAULT_BAND_MEDIUM,
            low: DEFAULT_BAND_LOW,
        }
    }
}

impl Bands {
    /// §6.4 — bucket a salience score into a materiality band (this pack's effective cutoffs).
    pub fn band(&self, sal: f32) -> Materiality {
        if sal >= self.critical {
            Materiality::Critical
        } else if sal >= self.high {
            Materiality::High
        } else if sal >= self.medium {
            Materiality::Medium
        } else if sal >= self.low {
            Materiality::Low
        } else {
            Materiality::None
        }
    }
}

/// §8.4 band → default action map (overridable per pack). Defaults per §8.4 step 3.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandAction {
    pub none: Action,
    pub low: Action,
    pub medium: Action,
    pub high: Action,
    pub critical: Action,
}

impl Default for BandAction {
    fn default() -> Self {
        BandAction {
            none: Action::Ignore,
            low: Action::Notify,
            medium: Action::Notify,
            high: Action::ReRunDownstream,
            critical: Action::EscalateHuman,
        }
    }
}

impl BandAction {
    /// The band-default action for a materiality (§8.4 step 3).
    pub fn for_band(&self, mat: Materiality) -> Action {
        match mat {
            Materiality::None => self.none,
            Materiality::Low => self.low,
            Materiality::Medium => self.medium,
            Materiality::High => self.high,
            Materiality::Critical => self.critical,
        }
    }
}

/// A §8.3 regex `[[rule]]`: the `kw` signal source + sticky/action override + optional category.
///
/// `regex` is a compiled `regex::Regex` (clock-free, pure). A match attaches `id` for the §8.5
/// explanation, contributes `kw=1.0`, may force a category, and (with `action`/`sticky`) feeds the
/// §8.4 action mapping.
#[derive(Clone, Debug)]
pub struct KwRule {
    pub id: String,
    pub regex: regex::Regex,
    /// Weight applied to the `kw` signal when this rule matches (defaults to 1.0).
    pub weight: f32,
    /// Optional action override (§8.4 steps 1–2).
    pub action: Option<Action>,
    /// `true` ⇒ the rule's action bypasses banding (§8.4 step 1).
    pub sticky: bool,
    /// Optional category this rule assigns (controlled-vocab; §6.4).
    pub cat: Option<String>,
}

/// One first-match-wins category rule: a category → materiality band + action (§8.3 / §8.4).
#[derive(Clone, Debug)]
pub struct CatRule {
    pub cat: String,
    pub mat: Materiality,
    pub act: Action,
}

/// A parsed, fully-resolved §8.3 rule pack: signal weights, materiality bands, the band→action
/// map, ordered regex rules, and category routing. `Default` is an empty pack with the §6.4 default
/// bands + §8.4 default band→action map (via the field types' own `Default`).
#[derive(Clone, Debug, Default)]
pub struct Pack {
    pub id: String,
    /// The `extends` parent archetype id of the most-derived layer (`cf rules` display), if any.
    pub parent: Option<String>,
    /// `blake3:<hex>` content hash of the pack source (feeds `prov.pack`, e.g. `pricing@b3:2f1a`).
    pub content_hash: String,
    /// §6.4 effective band cutoffs.
    pub bands: Bands,
    /// §8.4 effective band → default action map.
    pub band_action: BandAction,
    /// §8.1 block-type weight table override (`w_type`); falls back to the built-in default.
    pub block_type_weight: BTreeMap<String, f32>,
    /// Per-signal scale (`s_i` multiplier, default 1.0) keyed by signal id (`type`/`mag`/…).
    pub signal_scale: BTreeMap<String, f32>,
    /// §8.3 ordered regex rules (the `kw` set + sticky/action overrides).
    pub kw_rules: Vec<KwRule>,
    /// §8.3 category routing rows.
    pub cat_rules: Vec<CatRule>,
    /// `salience_hints.keywords` — extra keyword hints (the `kw` set when no regex rule matches).
    pub keywords: Vec<String>,
}

impl Pack {
    /// §8.1 — the block-type weight (`w_type`) for a type: pack override else the built-in default.
    pub fn block_type_weight(&self, ty: BlockType) -> f32 {
        let key = block_type_key(ty);
        self.block_type_weight
            .get(key)
            .copied()
            .unwrap_or_else(|| default_block_type_weight(ty))
    }

    /// Per-signal scale multiplier (default 1.0).
    pub fn signal_scale(&self, signal_id: &str) -> f32 {
        self.signal_scale.get(signal_id).copied().unwrap_or(1.0)
    }

    /// `prov.pack` stamp: `"<id>@b3:<short>"` where `<short>` is the first 4 hex of the hash.
    pub fn prov_stamp(&self) -> String {
        let short = self
            .content_hash
            .strip_prefix("blake3:")
            .unwrap_or(&self.content_hash)
            .get(..4)
            .unwrap_or("");
        format!("{}@b3:{short}", self.id)
    }

    /// The first matching regex rule for a text (first-match-wins, declared order).
    pub fn first_kw_match(&self, text: &str) -> Option<&KwRule> {
        self.kw_rules.iter().find(|r| r.regex.is_match(text))
    }

    /// All regex rules that match a text, in declared order.
    pub fn kw_matches<'a>(&'a self, text: &'a str) -> impl Iterator<Item = &'a KwRule> + 'a {
        self.kw_rules.iter().filter(move |r| r.regex.is_match(text))
    }

    /// The category routing row for a cat, if present.
    pub fn cat_rule(&self, cat: &str) -> Option<&CatRule> {
        self.cat_rules.iter().find(|r| r.cat == cat)
    }
}

/// §8.1 block-type weight defaults (the built-in table; rule-pack overridable).
pub fn default_block_type_weight(ty: BlockType) -> f32 {
    match ty {
        BlockType::Price => 1.00,
        BlockType::Date => 0.85,
        BlockType::Number => 0.85,
        BlockType::Heading => 0.80,
        BlockType::TableRow => 0.75,
        BlockType::Table => 0.75,
        BlockType::Code => 0.70,
        BlockType::ListItem => 0.55,
        BlockType::Link => 0.50,
        BlockType::Paragraph => 0.45,
        BlockType::Text => 0.20,
    }
}

/// The TOML key for a block type (matches the §8.1 table column names).
fn block_type_key(ty: BlockType) -> &'static str {
    match ty {
        BlockType::Price => "price",
        BlockType::Date => "date",
        BlockType::Number => "number",
        BlockType::Heading => "heading",
        BlockType::TableRow => "table_row",
        BlockType::Table => "table",
        BlockType::Code => "code",
        BlockType::ListItem => "list_item",
        BlockType::Link => "link",
        BlockType::Paragraph => "paragraph",
        BlockType::Text => "text",
    }
}

// ===========================================================================================
// TOML deserialization model + parse.
// ===========================================================================================

#[derive(Deserialize)]
struct RawPack {
    id: String,
    #[serde(default)]
    extends: Option<String>,
    #[serde(default)]
    bands: Option<RawBands>,
    #[serde(default)]
    band_action: Option<RawBandAction>,
    #[serde(default)]
    block_type_weight: BTreeMap<String, f32>,
    /// `[signals]` per-signal scale (default 1.0); also accepts the legacy enable-flag form.
    #[serde(default)]
    signals: BTreeMap<String, f32>,
    #[serde(default)]
    rule: Vec<RawRule>,
    #[serde(default)]
    salience_hints: Option<RawHints>,
}

#[derive(Deserialize)]
struct RawBands {
    critical: Option<f32>,
    high: Option<f32>,
    medium: Option<f32>,
    low: Option<f32>,
}

#[derive(Deserialize)]
struct RawBandAction {
    none: Option<String>,
    low: Option<String>,
    medium: Option<String>,
    high: Option<String>,
    critical: Option<String>,
}

#[derive(Deserialize)]
struct RawHints {
    #[serde(default)]
    keywords: Vec<String>,
}

/// A `[[rule]]` row. Carries BOTH flavors — a regex `match` rule (kw/sticky/action) and a category
/// routing row (`cat`+`mat`+`act`) — discriminated by which fields are present.
#[derive(Deserialize)]
struct RawRule {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "match", default)]
    match_re: Option<String>,
    #[serde(default)]
    weight: Option<f32>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default, rename = "act")]
    act: Option<String>,
    #[serde(default)]
    sticky: Option<bool>,
    #[serde(default)]
    cat: Option<String>,
    #[serde(default)]
    mat: Option<String>,
}

fn parse_action(s: &str) -> Result<Action, CfError> {
    Ok(match s {
        "ignore" => Action::Ignore,
        "notify" => Action::Notify,
        "refetch_linked" => Action::RefetchLinked,
        "reembed_kb" => Action::ReembedKb,
        "re_run_downstream" => Action::ReRunDownstream,
        "open_ticket" => Action::OpenTicket,
        "escalate_human" => Action::EscalateHuman,
        "page_oncall" => Action::PageOncall,
        other => {
            return Err(CfError::Usage(format!(
                "pack rule: unknown action {other:?} (not one of the 8 agent-facing actions)"
            )))
        }
    })
}

fn parse_materiality(s: &str) -> Result<Materiality, CfError> {
    Ok(match s {
        "none" => Materiality::None,
        "low" => Materiality::Low,
        "medium" => Materiality::Medium,
        "high" => Materiality::High,
        "critical" => Materiality::Critical,
        other => {
            return Err(CfError::Usage(format!("pack rule: unknown materiality {other:?}")))
        }
    })
}

/// Compute the `blake3:<hex>` content hash of a pack's raw TOML source (App B).
fn content_hash(src: &str) -> String {
    let h = blake3::hash(src.as_bytes());
    format!("blake3:{}", h.to_hex())
}

/// Parse a single pack from its TOML source into the structural layers needed for layering.
/// The `content_hash` is over THIS source layer (the per-target / archetype source).
fn parse_one(src: &str) -> Result<(RawPack, String), CfError> {
    let raw: RawPack = toml::from_str(src)
        .map_err(|e| CfError::Usage(format!("rule pack parse: {e}")))?;
    Ok((raw, content_hash(src)))
}

/// Apply a raw layer onto an accumulating pack (child wins; regex rules concatenate).
fn apply_layer(pack: &mut Pack, raw: RawPack) -> Result<(), CfError> {
    pack.id = raw.id;
    if raw.extends.is_some() {
        pack.parent = raw.extends;
    }

    if let Some(b) = raw.bands {
        if let Some(v) = b.critical {
            pack.bands.critical = v;
        }
        if let Some(v) = b.high {
            pack.bands.high = v;
        }
        if let Some(v) = b.medium {
            pack.bands.medium = v;
        }
        if let Some(v) = b.low {
            pack.bands.low = v;
        }
    }

    if let Some(ba) = raw.band_action {
        if let Some(s) = ba.none {
            pack.band_action.none = parse_action(&s)?;
        }
        if let Some(s) = ba.low {
            pack.band_action.low = parse_action(&s)?;
        }
        if let Some(s) = ba.medium {
            pack.band_action.medium = parse_action(&s)?;
        }
        if let Some(s) = ba.high {
            pack.band_action.high = parse_action(&s)?;
        }
        if let Some(s) = ba.critical {
            pack.band_action.critical = parse_action(&s)?;
        }
    }

    for (k, v) in raw.block_type_weight {
        pack.block_type_weight.insert(k, v);
    }
    for (k, v) in raw.signals {
        pack.signal_scale.insert(k, v);
    }

    for r in raw.rule {
        if let Some(pat) = r.match_re {
            // A regex rule. Append (parent rules already present, so parent stickies still fire).
            let regex = regex::Regex::new(&pat).map_err(|e| {
                CfError::Usage(format!("rule pack regex {:?}: {e}", r.id.as_deref().unwrap_or("?")))
            })?;
            let action = match r.action.as_deref().or(r.act.as_deref()) {
                Some(a) => Some(parse_action(a)?),
                None => None,
            };
            pack.kw_rules.push(KwRule {
                id: r.id.unwrap_or_else(|| "rule".to_string()),
                regex,
                weight: r.weight.unwrap_or(1.0),
                action,
                sticky: r.sticky.unwrap_or(false),
                cat: r.cat,
            });
        } else if let (Some(cat), Some(mat)) = (r.cat.clone(), r.mat.as_deref()) {
            // A category routing row. Child wins on cat.
            let mat = parse_materiality(mat)?;
            let act = match r.action.as_deref().or(r.act.as_deref()) {
                Some(a) => parse_action(a)?,
                None => pack.band_action.for_band(mat),
            };
            if let Some(existing) = pack.cat_rules.iter_mut().find(|c| c.cat == cat) {
                existing.mat = mat;
                existing.act = act;
            } else {
                pack.cat_rules.push(CatRule { cat, mat, act });
            }
        }
        // A row with neither `match` nor (`cat`+`mat`) is ignored (forward-compat tolerance).
    }

    if let Some(h) = raw.salience_hints {
        if !h.keywords.is_empty() {
            pack.keywords = h.keywords;
        }
    }
    Ok(())
}

/// Parse a pack from its embedded TOML source (computes the `blake3:` content hash). The pack is
/// resolved standalone (no `extends` chaining); use [`layer`] to stack archetype → per-target.
pub fn parse(toml_src: &str) -> Result<Pack, CfError> {
    let (raw, hash) = parse_one(toml_src)?;
    let mut pack = Pack::default();
    apply_layer(&mut pack, raw)?;
    pack.content_hash = hash;
    Ok(pack)
}

/// §8.3 last-wins layering: resolve `built-in default → archetype → per-target`. Each layer is a
/// TOML source string (the parent first, child last); the child wins on conflicting scalars and its
/// regex rules append after the parent's so parent stickies still fire. The final `content_hash`
/// is over the concatenated layer sources (so the stamp changes if ANY layer changes).
pub fn layer(layers: &[&str]) -> Result<Pack, CfError> {
    let mut pack = Pack::default();
    let mut combined = String::new();
    for src in layers {
        let (raw, _) = parse_one(src)?;
        apply_layer(&mut pack, raw)?;
        combined.push_str(src);
        combined.push('\n');
    }
    pack.content_hash = content_hash(&combined);
    Ok(pack)
}

/// Resolve a shipped archetype name to its TOML source (MVP ships three + `default`).
pub fn shipped_toml(archetype: &str) -> Option<&'static str> {
    match archetype {
        "pricing" => Some(PRICING_TOML),
        "api-docs" => Some(API_DOCS_TOML),
        "status-page" => Some(STATUS_PAGE_TOML),
        "default" => Some(DEFAULT_TOML),
        _ => None,
    }
}

/// Resolve a shipped archetype into a layered pack (`default` → archetype). Unknown archetypes
/// fall back to the `default` pack alone.
pub fn resolve(archetype: Option<&str>) -> Result<Pack, CfError> {
    match archetype {
        Some(name) => match shipped_toml(name) {
            Some(arch) => layer(&[DEFAULT_TOML, arch]),
            None => parse(DEFAULT_TOML),
        },
        None => parse(DEFAULT_TOML),
    }
}
