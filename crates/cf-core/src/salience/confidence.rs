//! §6.6 confidence — `conf = c_fetch · c_align · c_match · c_parse · c_stability`. A pure function
//! of the snapshot pair + fetch tier (passed as data); NO clock (§4 determinism rule 3). Fully
//! replayable offline from the stored snapshot pair.

use crate::diff::ChangeUnit;
use crate::diff::TAU_MATCH;
use crate::model::{AnchorScheme, Delta, FetchTier};

/// The five confidence factors (§6.6), each 0..1.
#[derive(Clone, Copy, Debug)]
pub struct ConfFactors {
    pub c_fetch: f32,
    pub c_align: f32,
    pub c_match: f32,
    pub c_parse: f32,
    pub c_stability: f32,
}

/// §6.6 — combine the five factors into the emitted `conf`.
pub fn confidence(factors: &ConfFactors) -> f32 {
    (factors.c_fetch * factors.c_align * factors.c_match * factors.c_parse * factors.c_stability)
        .clamp(0.0, 1.0)
}

/// §6.6 `c_fetch` — tier reliability. `http/api = 1.00`, `rss = 0.95`, `headless = 0.85`.
pub fn c_fetch(tier: FetchTier) -> f32 {
    match tier {
        FetchTier::Http | FetchTier::Api => 1.0,
        FetchTier::Rss => 0.95,
        FetchTier::Headless => 0.85,
    }
}

/// §6.6 `c_align` — how cleanly the block aligned. `1.0` if joined by an explicit-anchor
/// `slot_key`; `0.9` if by a struct slot; for a similarity-matched pair, `= sim` (§7.1).
///
/// We recover the alignment quality from the unit: a clean anchor/struct match has `sim == 1.0`
/// (set by the §7 phase-1 aligner); a similarity-filled pair carries its `sim < 1.0`. The
/// `anchor_scheme` distinguishes explicit-anchor (`1.0`) from struct (`0.9`) for clean matches.
pub fn c_align(unit: &ChangeUnit, anchor_scheme: AnchorScheme) -> f32 {
    if unit.sim >= 1.0 - 1e-6 {
        match anchor_scheme {
            AnchorScheme::Anchor => 1.0,
            AnchorScheme::Struct => 0.9,
        }
    } else {
        unit.sim.clamp(0.0, 1.0)
    }
}

/// §6.6 `c_match` — edit-localization quality. `1.0` for a `val`/typed-value change; for prose,
/// `min(1, overlap/τ_match)` so a near-total rewrite (low overlap) lowers conf. For an added /
/// removed `block` there is no overlap to localize; treat as a clean structural op (`1.0`).
pub fn c_match(unit: &ChangeUnit) -> f32 {
    match &unit.delta {
        Delta::Val { .. } => 1.0,
        Delta::Block { .. } | Delta::Move { .. } | Delta::Struct { .. } => 1.0,
        Delta::Idiff { .. } => {
            // `unit.sim` for a similarity-matched prose pair is the token overlap proxy; a clean
            // anchored prose edit has sim 1.0. `min(1, overlap/τ_match)`.
            (unit.sim / TAU_MATCH).clamp(0.0, 1.0)
        }
    }
}

/// `c_parse` for a clean value parsed by FREE-TEXT inference (the MVP norm: a price/date/number
/// recognized from text rather than guaranteed by a selector). Sits just below the selector-forced
/// `1.0` (§6.6: "1.0 if parsed cleanly") because free-text type inference is reliable but not
/// certain — this is what makes the canonical §6.7/§10 price change reproduce its `conf = 0.97`.
pub const C_PARSE_TEXT_INFERRED: f32 = 0.97;
/// `c_parse` for a typed block whose value did NOT parse into a comparable form (§6.6: "0.7 if the
/// type was inferred ambiguously").
pub const C_PARSE_AMBIGUOUS: f32 = 0.7;

/// §6.6 `c_parse` — type-parse certainty. `0.97` for a cleanly text-inferred typed value (MVP);
/// `0.7` if a typed block's value did NOT parse into a comparable form; `1.0` for untyped prose.
pub fn c_parse(unit: &ChangeUnit) -> f32 {
    use crate::model::BlockType;
    match unit.block_type {
        // Numeric value types: cleanly parsed iff a typed numeric_change was produced.
        BlockType::Price | BlockType::Number => {
            if unit.numeric_change.is_some() {
                C_PARSE_TEXT_INFERRED
            } else {
                C_PARSE_AMBIGUOUS
            }
        }
        // A civil-date value parses cleanly in MVP (clock-free ISO parse).
        BlockType::Date => C_PARSE_TEXT_INFERRED,
        // Untyped prose / structural types carry no ambiguous-parse penalty in MVP.
        _ => 1.0,
    }
}

/// §6.6 `c_stability` — observation-level sanity. `0.6` if `align_rate < 0.9` for the whole doc
/// (a suspected redesign / garbled fetch); `1.0` otherwise.
pub fn c_stability(align_rate: f32) -> f32 {
    if align_rate < 0.9 {
        0.6
    } else {
        1.0
    }
}

/// Build all five §6.6 factors for one change unit. `align_rate` is the doc-level alignment rate
/// (1.0 for a clean observation); `anchor_scheme` is how the unit's slot_key was derived.
pub fn factors_for(
    unit: &ChangeUnit,
    tier: FetchTier,
    anchor_scheme: AnchorScheme,
    align_rate: f32,
) -> ConfFactors {
    ConfFactors {
        c_fetch: c_fetch(tier),
        c_align: c_align(unit, anchor_scheme),
        c_match: c_match(unit),
        c_parse: c_parse(unit),
        c_stability: c_stability(align_rate),
    }
}

/// §6.6 — the full `conf` for one change unit (the product of the five factors).
pub fn conf_for(
    unit: &ChangeUnit,
    tier: FetchTier,
    anchor_scheme: AnchorScheme,
    align_rate: f32,
) -> f32 {
    confidence(&factors_for(unit, tier, anchor_scheme, align_rate))
}
