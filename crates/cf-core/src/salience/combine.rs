//! §8.2 noisy-OR signal combination + materiality bands. Band cutoffs are inclusive-lower /
//! exclusive-upper constants so the same delta yields byte-identical `sal`/`mat` on any arch
//! (§4 determinism rule 6).
//!
//! ```text
//! raw_block   = 1 − Π_i (1 − a_i · s_i)          # noisy-OR over the 7 MVP signals
//! block_score = raw_block · w_vol                # volatility damps, never boosts (1.0 in MVP)
//! page_score  = 1 − Π_{b ∈ top_k} (1 − block_score_b)   # k=5
//! ```

use crate::model::Materiality;
use crate::packs::Bands;
use crate::salience::signals::Signals;

/// Default materiality cutoffs (App B; pack-overridable). Inclusive lower bounds.
pub const BAND_CRITICAL: f32 = 0.90;
pub const BAND_HIGH: f32 = 0.70;
pub const BAND_MEDIUM: f32 = 0.40;
pub const BAND_LOW: f32 = 0.15;

/// §8.2 default signal affinities `a_i` (pack-overridable). Ordered `type,mag,num,date,neg,pos,kw`.
pub const A_TYPE: f32 = 0.6;
pub const A_MAG: f32 = 0.7;
pub const A_NUM: f32 = 0.9;
pub const A_DATE: f32 = 0.85;
pub const A_NEG: f32 = 1.0;
pub const A_POS: f32 = 0.5;
pub const A_KW: f32 = 0.95;

/// MVP volatility damping factor (`w_vol`). Phase-2 lowers it from the per-`slot_key` flap EWMA;
/// in MVP there is no learned volatility, so it is fixed at 1.0 (§8.1/§8.2).
pub const W_VOL_MVP: f32 = 1.0;

/// §8.2 `top_k` — the page-level noisy-OR aggregates the top-k most-salient blocks (App C: k=5).
pub const TOP_K: usize = 5;

/// The seven default affinities in signal order (`type,mag,num,date,neg,pos,kw`).
pub fn default_affinities() -> [f32; 7] {
    [A_TYPE, A_MAG, A_NUM, A_DATE, A_NEG, A_POS, A_KW]
}

/// §8.2 — noisy-OR over the 7 weighted signals: `raw = 1 − Π (1 − a_i·s_i)`. Each `a_i·s_i` is
/// clamped to `[0,1]` so a misconfigured pack can never push a factor negative or above 1.
pub fn noisy_or(signals: &Signals, affinities: &[f32; 7]) -> f32 {
    let s = [
        signals.ty,
        signals.mag,
        signals.num,
        signals.date,
        signals.neg,
        signals.pos,
        signals.kw,
    ];
    let mut prod = 1.0f32;
    for i in 0..7 {
        let contrib = (affinities[i] * s[i]).clamp(0.0, 1.0);
        prod *= 1.0 - contrib;
    }
    (1.0 - prod).clamp(0.0, 1.0)
}

/// `block_score = raw_block · w_vol` (volatility damps, never boosts; `w_vol=1.0` in MVP).
pub fn block_score(raw_block: f32, w_vol: f32) -> f32 {
    (raw_block * w_vol).clamp(0.0, 1.0)
}

/// §7.3 — noise is a **soft negative feature** to salience (not a hard gate). A high `noise_score`
/// (a volatile "N viewing" counter, whitespace/case-only churn, a pure reorder, a `restyled` op)
/// multiplicatively damps the block's score so an un-typed live counter lands in a low/none band
/// (§7.4 MVP quietness expectation) instead of firing as material — while a genuine change, whose
/// `noise_score ≈ 0` (a salient-numeric value change is exactly `0.0`), is left untouched. The op
/// is still *emitted* (soft, not a gate): `block_score · (1 − noise_score)`.
pub fn noise_damp(block_score: f32, noise_score: f32) -> f32 {
    (block_score * (1.0 - noise_score.clamp(0.0, 1.0))).clamp(0.0, 1.0)
}

/// §8.2 page-level aggregation: noisy-OR over the top-k block scores. Sorting takes the k highest
/// scores deterministically (descending, NaN-free). Empty input ⇒ 0.0.
pub fn page_score(block_scores: &[f32]) -> f32 {
    if block_scores.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f32> = block_scores.to_vec();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let mut prod = 1.0f32;
    for &bs in sorted.iter().take(TOP_K) {
        prod *= 1.0 - bs.clamp(0.0, 1.0);
    }
    (1.0 - prod).clamp(0.0, 1.0)
}

/// §6.4 — bucket a salience score into a materiality band using the default cutoffs (App B). For a
/// pack-overridable banding use [`Bands::band`].
pub fn band(sal: f32) -> Materiality {
    Bands::default().band(sal)
}

/// §6.4 — bucket using a pack's effective band cutoffs.
pub fn band_with(sal: f32, bands: &Bands) -> Materiality {
    bands.band(sal)
}
