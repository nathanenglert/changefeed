//! §8 salience scorer — 7 MVP signals → noisy-OR + bands → materiality + action + confidence.
//! Pure: `fn(&Changeset, &Pack, …) -> Result<Vec<ScoredEvent>, CfError>`. NO clock, NO RNG, NO
//! network (§4 determinism rules). Every signal, band, action, and confidence factor is replayable
//! offline from the stored snapshot pair + the rule pack.

pub mod action;
pub mod combine;
pub mod confidence;
pub mod signals;

use crate::diff::{ChangeUnit, Changeset};
use crate::model::{
    Action, AnchorScheme, Block, BlockType, ChangeType, Delta, FetchTier, Materiality, SlotKey,
    TypedValue,
};
use crate::packs::Pack;
use crate::CfError;

use action::{ActionDecision, DecidedBy};
use combine::{block_score, default_affinities, noisy_or, page_score};
use confidence::conf_for;
use rustc_hash::{FxHashMap, FxHashSet};
use signals::{signals_for, PosInput, SignalContext, Signals};

/// A scored change ready for wire assembly at the cli boundary (§8 output).
#[derive(Clone, Debug)]
pub struct ScoredEvent {
    pub slot_key: SlotKey,
    pub ct: ChangeType,
    pub delta: Delta,
    pub sal: f32,
    pub mat: Materiality,
    pub cat: String,
    pub act: Action,
    pub conf: f32,
    /// §8.5 per-event explanation breakdown (for `cf explain`).
    pub explanation: Explanation,
}

/// Source of the position signal (§8.1 Tier-1 dom-depth+order proxy), passed as data so scoring
/// stays clock/IO-free.
#[derive(Clone, Copy, Debug)]
pub enum PosSource {
    /// Tier-1 proxy from `dom_depth` + `preorder_idx`.
    DomProxy,
}

/// §8.5 explanation breakdown for one event (the `cf explain` table). Deterministic: lists the
/// top signals (descending contribution), the matched rules, the volatility damping, and how the
/// action was decided. `non_reproducible` is always `false` in MVP (the `date_proximity` clock
/// enrichment is default-off / OUT).
#[derive(Clone, Debug)]
pub struct Explanation {
    pub top_signals: Vec<SignalContribution>,
    pub matched_rules: Vec<String>,
    pub damped_by_volatility: f32,
    /// §7.3 — the fraction of the block score removed by the noise soft-negative feature
    /// (equal to the unit's `noise_score`; `0.0` for a genuine change).
    pub damped_by_noise: f32,
    pub decided_by: &'static str,
    pub non_reproducible: bool,
    /// The five §6.6 confidence factors (so `cf explain` can show the conf breakdown too).
    pub conf_factors: confidence::ConfFactors,
}

/// One signal's contribution row in the §8.5 explanation.
#[derive(Clone, Debug)]
pub struct SignalContribution {
    /// Signal id (`type`/`mag`/`num`/`date`/`neg`/`pos`/`kw`).
    pub signal: &'static str,
    /// The raw signal value `s_i` (0..1).
    pub value: f32,
    /// The post-affinity contribution `a_i · s_i` (the noisy-OR factor weight).
    pub contribution: f32,
    /// Optional human detail (`49→59 (+20%)`, `proxy:dom_depth (Tier-1)`).
    pub detail: Option<String>,
}

/// Per-observation scoring inputs that are NOT in the `Changeset` (doc-level metadata + provenance),
/// threaded as DATA so scoring stays pure (no clock / IO).
#[derive(Clone, Copy, Debug)]
pub struct ScoreInputs {
    /// Fetch tier (drives `c_fetch`; MVP emits `http`).
    pub tier: FetchTier,
    /// Doc-level alignment rate (1.0 for a clean observation; `<0.9` ⇒ suspected redesign).
    pub align_rate: f32,
    /// Position-signal source (Tier-1 dom proxy in MVP).
    pub pos_source: PosSource,
}

impl Default for ScoreInputs {
    fn default() -> Self {
        ScoreInputs {
            tier: FetchTier::Http,
            align_rate: 1.0,
            pos_source: PosSource::DomProxy,
        }
    }
}

/// Per-block metadata recovered from the new `CanonicalDoc` (the diff drops it from `ChangeUnit`).
#[derive(Clone, Copy, Debug)]
struct BlockMeta {
    dom_depth: u16,
    preorder_idx: u32,
    anchor_scheme: AnchorScheme,
}

/// The full result of scoring one changeset: the per-event scores + the page-level aggregate.
#[derive(Clone, Debug)]
pub struct ScoreResult {
    pub events: Vec<ScoredEvent>,
    /// §8.2 page-level noisy-OR over the top-k block scores (cross-event routing signal).
    pub page_score: f32,
    /// The page-level materiality (band of `page_score` under the pack's effective cutoffs).
    pub page_mat: Materiality,
}

/// §8 — score a `Changeset` against a rule pack using the NEW canonical doc for per-block position
/// + alignment metadata. This is the full-fidelity entry point used by the pipeline.
pub fn score_doc(
    changeset: &Changeset,
    new_doc_blocks: &[Block],
    block_count: u32,
    pack: &Pack,
    inputs: &ScoreInputs,
) -> Result<ScoreResult, CfError> {
    // Recover per-block position metadata from the new doc in ONE pre-order pass. We only need
    // metadata for the slot_keys that actually changed (a handful), plus the doc-wide `max_depth`
    // for the `pos` proxy — so we capture just the needed slots instead of building a full
    // B-entry map (the changeset is tiny; the doc is large, §8.2 "bounded by cluster count").
    let mut wanted: FxHashSet<SlotKey> = FxHashSet::default();
    for unit in &changeset.units {
        wanted.insert(unit.slot_key);
    }
    let mut meta: FxHashMap<SlotKey, BlockMeta> = FxHashMap::default();
    meta.reserve(wanted.len());
    let mut max_depth: u16 = 0;
    collect_meta(new_doc_blocks, &wanted, &mut meta, &mut max_depth);

    let affinities = default_affinities();
    let mut events: Vec<ScoredEvent> = Vec::with_capacity(changeset.units.len());
    let mut block_scores: Vec<f32> = Vec::with_capacity(changeset.units.len());

    for unit in &changeset.units {
        let bm = meta.get(&unit.slot_key).copied().unwrap_or(BlockMeta {
            dom_depth: 0,
            preorder_idx: 0,
            anchor_scheme: AnchorScheme::Struct,
        });
        let pos = PosInput {
            dom_depth: bm.dom_depth,
            max_depth,
            preorder_idx: bm.preorder_idx,
            block_count,
        };

        let (sig, sctx) = signals_for(unit, pack, pos, inputs.pos_source);
        let raw = noisy_or(&sig, &affinities);
        let w_vol = combine::W_VOL_MVP;
        // §7.3 noise is a soft negative feature: damp the block score by `noise_score` so a
        // volatile counter / whitespace flap / pure reorder does not fire as material (§7.4).
        let bs = combine::noise_damp(block_score(raw, w_vol), unit.noise_score);

        let cat = classify(unit, pack, &sctx);
        let mat = pack.bands.band(bs);

        // §8.4 action mapping — sticky/matched-rule first, then the cat-row action, then band.
        let after_text = present_text(unit);
        let decision = action::action_for_cat(&after_text, &cat, mat, pack);

        let factors = confidence::factors_for(unit, inputs.tier, bm.anchor_scheme, inputs.align_rate);
        let conf = confidence::confidence(&factors);

        let explanation =
            build_explanation(&sig, &affinities, &sctx, &decision, w_vol, unit.noise_score, factors);

        block_scores.push(bs);
        events.push(ScoredEvent {
            slot_key: unit.slot_key,
            ct: unit.ct,
            delta: unit.delta.clone(),
            sal: round2(bs),
            mat,
            cat,
            act: decision.action,
            conf: round2(conf),
            explanation,
        });
    }

    let ps = page_score(&block_scores);
    let page_mat = pack.bands.band(ps);
    Ok(ScoreResult {
        events,
        page_score: round2(ps),
        page_mat,
    })
}

/// §8 — score a `Changeset` against a rule pack (the skeleton signature). Without the new doc it
/// uses neutral position metadata (a mid proxy) and clean-http defaults; prefer [`score_doc`] when
/// the new `CanonicalDoc` is available so `pos`/`c_align` are exact.
pub fn score(
    changeset: &Changeset,
    pack: &Pack,
    pos_source: PosSource,
) -> Result<Vec<ScoredEvent>, CfError> {
    let inputs = ScoreInputs {
        pos_source,
        ..ScoreInputs::default()
    };
    // No doc → no per-block metadata; pass an empty block set (neutral pos proxy = 1.0).
    Ok(score_doc(changeset, &[], 0, pack, &inputs)?.events)
}

/// Recursively collect per-block metadata for the `wanted` slot_keys (the changed units) plus the
/// doc-wide max dom depth. Only the wanted slots are inserted, so the map stays O(changes), not O(B)
/// — the `max_depth` still needs the full pre-order pass (it is doc-level), which is cheap.
fn collect_meta(
    blocks: &[Block],
    wanted: &FxHashSet<SlotKey>,
    out: &mut FxHashMap<SlotKey, BlockMeta>,
    max_depth: &mut u16,
) {
    for b in blocks {
        *max_depth = (*max_depth).max(b.dom_depth);
        if wanted.contains(&b.slot_key) {
            out.insert(
                b.slot_key,
                BlockMeta {
                    dom_depth: b.dom_depth,
                    preorder_idx: b.preorder_idx,
                    anchor_scheme: b.anchored_by,
                },
            );
        }
        collect_meta(&b.children, wanted, out, max_depth);
    }
}

/// The present (new-side) text of a unit, used to evaluate regex action rules.
fn present_text(unit: &ChangeUnit) -> String {
    match &unit.delta {
        Delta::Val { a, .. } => a.clone(),
        Delta::Idiff { ops } => ops
            .iter()
            .filter(|o| o.op != crate::model::DiffOp::Del)
            .map(|o| o.text.as_str())
            .collect::<String>(),
        Delta::Block { a, .. } => a.clone(),
        Delta::Move { .. } | Delta::Struct { .. } => String::new(),
    }
}

// ===========================================================================================
// §6.4 category assignment (controlled vocab; fallback `content_edit`).
// ===========================================================================================

/// Assign the controlled-vocab `why.cat` for a unit (§6.4). Deterministic, first-match-wins:
/// 1. a matched regex rule that names a `cat`,
/// 2. a numeric price delta → `price_increase`/`price_decrease` by direction,
/// 3. a polarity-flip / deprecation phrase → `api_deprecation`,
/// 4. a `cosmetic`/`content_edit` fallback by change shape.
fn classify(unit: &ChangeUnit, pack: &Pack, sctx: &SignalContext) -> String {
    let present = present_text(unit);
    let prior = prior_text(unit);

    // (1) A matched regex rule that carries a category wins.
    if let Some(rule) = pack
        .kw_rules
        .iter()
        .find(|r| r.cat.is_some() && (r.regex.is_match(&present) || r.regex.is_match(&prior)))
    {
        if let Some(cat) = &rule.cat {
            return cat.clone();
        }
    }

    // (2) A typed numeric price delta → direction-aware category.
    if let Some(nc) = unit.numeric_change {
        if unit.block_type == BlockType::Price {
            return if nc.to > nc.from {
                "price_increase".to_string()
            } else if nc.to < nc.from {
                "price_decrease".to_string()
            } else {
                "content_edit".to_string()
            };
        }
    }

    // (3) A polarity flip that reads as a deprecation/removal.
    let present_l = present.to_ascii_lowercase();
    if signals::NEG_TOKENS.iter().any(|t| present_l.contains(t)) {
        if present_l.contains("deprecated")
            || present_l.contains("end-of-life")
            || present_l.contains("sunset")
        {
            return "api_deprecation".to_string();
        }
        if present_l.contains("removed")
            || present_l.contains("no longer")
            || present_l.contains("unsupported")
            || present_l.contains("breaking")
        {
            return "api_breaking".to_string();
        }
        // A generic polarity flip is still a real content edit.
        return "content_edit".to_string();
    }
    let _ = sctx; // matched-rule id already consumed above.

    // (4) Shape-based fallback.
    match unit.ct {
        ChangeType::Reordered | ChangeType::Restyled => "cosmetic".to_string(),
        _ => "content_edit".to_string(),
    }
}

/// The prior (old-side) text of a unit.
fn prior_text(unit: &ChangeUnit) -> String {
    match &unit.delta {
        Delta::Val { b, .. } => b.clone(),
        Delta::Idiff { ops } => ops
            .iter()
            .filter(|o| o.op != crate::model::DiffOp::Ins)
            .map(|o| o.text.as_str())
            .collect::<String>(),
        Delta::Block { b, .. } => b.clone().unwrap_or_default(),
        Delta::Move { .. } | Delta::Struct { .. } => String::new(),
    }
}

// ===========================================================================================
// §8.5 explanation breakdown.
// ===========================================================================================

fn build_explanation(
    sig: &Signals,
    affinities: &[f32; 7],
    sctx: &SignalContext,
    decision: &ActionDecision,
    w_vol: f32,
    noise_score: f32,
    factors: confidence::ConfFactors,
) -> Explanation {
    let rows = [
        ("type", sig.ty, affinities[0], None),
        ("mag", sig.mag, affinities[1], None),
        ("num", sig.num, affinities[2], sctx.num_detail.clone()),
        ("date", sig.date, affinities[3], None),
        ("neg", sig.neg, affinities[4], None),
        (
            "pos",
            sig.pos,
            affinities[5],
            Some("proxy:dom_depth (Tier-1)".to_string()),
        ),
        (
            "kw",
            sig.kw,
            affinities[6],
            sctx.matched_rule_id.clone().map(|id| format!("rule:{id}")),
        ),
    ];

    let mut top_signals: Vec<SignalContribution> = rows
        .iter()
        .filter(|(_, v, _, _)| *v > 0.0)
        .map(|(name, v, a, detail)| SignalContribution {
            signal: name,
            value: round2(*v),
            contribution: round2((a * v).clamp(0.0, 1.0)),
            detail: detail.clone(),
        })
        .collect();
    // Descending by contribution, then by name for a deterministic, arch-free tie-break.
    top_signals.sort_by(|x, y| {
        y.contribution
            .partial_cmp(&x.contribution)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(x.signal.cmp(y.signal))
    });

    let mut matched_rules = Vec::new();
    if let Some(id) = &sctx.matched_rule_id {
        matched_rules.push(id.clone());
    }
    if let Some(id) = &decision.rule_id {
        if !matched_rules.contains(id) {
            matched_rules.push(id.clone());
        }
    }

    Explanation {
        top_signals,
        matched_rules,
        // `w_vol` damps `raw` by `(1 - w_vol)` of it; in MVP `w_vol=1.0` ⇒ 0.0 damping.
        damped_by_volatility: round2(1.0 - w_vol),
        // §7.3 soft negative feature: `block_score · (1 − noise_score)`.
        damped_by_noise: round2(noise_score),
        decided_by: match decision.decided_by {
            DecidedBy::Rule => "rule",
            DecidedBy::Band => "band",
        },
        non_reproducible: false,
        conf_factors: factors,
    }
}

/// Round to 2 decimals (the wire emits `sal`/`conf` to 2 decimals — §6.4). Deterministic, arch-free.
fn round2(v: f32) -> f32 {
    (v * 100.0).round() / 100.0
}

/// §6.6 — the conf for one change unit (re-exported convenience over [`confidence::conf_for`]).
pub fn conf_of(unit: &ChangeUnit, tier: FetchTier, anchor_scheme: AnchorScheme, align_rate: f32) -> f32 {
    conf_for(unit, tier, anchor_scheme, align_rate)
}

/// Map a unit's `TypedValue` to a comparable numeric (price minor units / number) — re-exported so
/// the explanation/test code can introspect a unit's parsed value when present.
pub fn typed_numeric(v: &TypedValue) -> Option<f64> {
    match v {
        TypedValue::Price { amount_minor, .. } => Some(*amount_minor as f64 / 100.0),
        TypedValue::Number(n) => n.to_string().parse::<f64>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
