//! §8.1/§8.2 the 7 MVP signals: `type`, `mag`, `num`, `date`, `neg`, `pos`, `kw`. The eighth
//! signal `vol` is Phase-2 (`w_vol=1.0` in MVP). The `date` signal is magnitude-only (civil-date
//! arithmetic, NO `now()` — §4 determinism rule 3).

use crate::diff::ChangeUnit;
use crate::model::{Delta, DiffOp};
use crate::packs::Pack;
use crate::salience::PosSource;

/// The 7 MVP signal values for one change unit, each 0..1 (§8.2).
#[derive(Clone, Copy, Debug, Default)]
pub struct Signals {
    pub ty: f32,
    pub mag: f32,
    pub num: f32,
    pub date: f32,
    pub neg: f32,
    pub pos: f32,
    pub kw: f32,
}

/// §8.1 — the position-signal proxy inputs. Passed as DATA (no layout/clock) so scoring stays pure.
/// On the Tier-1 HTTP path there is no layout; `pos` falls back to a structural proxy from
/// `dom_depth` + `preorder_idx` (§8.1). The `max_depth`/`block_count` come from the doc the unit
/// belongs to (the caller threads them in; see [`PosInput`]).
#[derive(Clone, Copy, Debug)]
pub struct PosInput {
    pub dom_depth: u16,
    pub max_depth: u16,
    pub preorder_idx: u32,
    pub block_count: u32,
}

impl PosInput {
    /// §8.1 Tier-1 structural prominence proxy:
    /// `0.5·(1 − dom_depth/max_depth) + 0.5·(1 − preorder_index/block_count)`.
    /// Shallow + early-in-document blocks score higher. A *weak* proxy (low affinity `a_pos=0.5`).
    pub fn dom_proxy(&self) -> f32 {
        let depth_term = if self.max_depth == 0 {
            1.0
        } else {
            1.0 - (self.dom_depth as f32 / self.max_depth as f32).min(1.0)
        };
        let order_term = if self.block_count == 0 {
            1.0
        } else {
            1.0 - (self.preorder_idx as f32 / self.block_count as f32).min(1.0)
        };
        (0.5 * depth_term + 0.5 * order_term).clamp(0.0, 1.0)
    }
}

/// §8.1 polarity-flip dictionary: a meaning-inverting token *introduced* on the new side sets
/// `neg=1.0` regardless of byte size (a flip is decisive). Lower-cased substring match.
pub const NEG_TOKENS: &[&str] = &[
    "no longer",
    "deprecated",
    "removed",
    "unsupported",
    "sold out",
    "end-of-life",
    "breaking",
    "required",
];

/// The matched `kw` regex rule id (for §8.5 explanation), if any.
#[derive(Clone, Debug, Default)]
pub struct SignalContext {
    /// Matched §8.3 rule id (attaches for explainability, §8.1 `kw`).
    pub matched_rule_id: Option<String>,
    /// Human-readable `num` detail (`49→59 (+20%)`) for the §8.5 explanation, if numeric.
    pub num_detail: Option<String>,
    /// Whether `pos` used the Tier-1 dom proxy (always true in MVP).
    pub pos_proxy: bool,
}

/// §8.1/§8.2 — compute the 7 MVP signals for one change unit (clock-free; `date` is
/// magnitude-only). Returns the signals plus the §8.5 explanation context (matched rule id, etc.).
pub fn signals_for(
    unit: &ChangeUnit,
    pack: &Pack,
    pos: PosInput,
    _pos_source: PosSource,
) -> (Signals, SignalContext) {
    let mut ctx = SignalContext {
        pos_proxy: true,
        ..SignalContext::default()
    };

    // type — block-type weight table (§8.1), pack-overridable.
    let ty = pack.block_type_weight(unit.block_type) * pack.signal_scale("type");

    // mag — token edit ratio / numeric clamp, already computed by §7.2.
    let mag = unit.features.magnitude * pack.signal_scale("mag");

    // num — typed numeric/currency/percent delta magnitude (direction kept for cat).
    let (after_text, before_text) = delta_texts(&unit.delta);
    let num = match unit.numeric_change {
        Some(nc) => {
            let m = (nc.pct.abs().min(1.0)) as f32;
            let sign = if nc.to >= nc.from { "+" } else { "-" };
            let pct_disp = (nc.pct.abs() * 100.0).round() as i64;
            ctx.num_detail = Some(format!(
                "{}→{} ({sign}{pct_disp}%)",
                trim_num(nc.from),
                trim_num(nc.to)
            ));
            m
        }
        None => 0.0,
    } * pack.signal_scale("num");

    // date — scored by the SIZE of the date shift (no clock). Magnitude only.
    let date = date_shift_signal(&before_text, &after_text) * pack.signal_scale("date");

    // neg — polarity flip: a meaning-inverting token introduced on the NEW side (§8.1).
    let neg = if polarity_flipped(&before_text, &after_text, &unit.delta) {
        1.0
    } else {
        0.0
    } * pack.signal_scale("neg");

    // pos — Tier-1 structural prominence proxy (secondary, low affinity).
    let pos_val = pos.dom_proxy() * pack.signal_scale("pos");

    // kw — active rule-pack regex hit (§8.1: "active rule-pack regex hit, attach the matched rule
    // id"). First-match-wins over the pack's declared `[[rule]]` regexes; the matched rule's
    // `weight` is the signal value. The `salience_hints.keywords` set is an EXTRACTION/profile
    // concept (§5.7), NOT the scoring `kw` signal, so it deliberately does not fire here.
    let kw_haystack = if after_text.is_empty() {
        &before_text
    } else {
        &after_text
    };
    let kw = match pack.first_kw_match(kw_haystack) {
        Some(rule) => {
            ctx.matched_rule_id = Some(rule.id.clone());
            rule.weight * pack.signal_scale("kw")
        }
        None => 0.0,
    };

    let signals = Signals {
        ty: ty.clamp(0.0, 1.0),
        mag: mag.clamp(0.0, 1.0),
        num: num.clamp(0.0, 1.0),
        date: date.clamp(0.0, 1.0),
        neg: neg.clamp(0.0, 1.0),
        pos: pos_val.clamp(0.0, 1.0),
        kw: kw.clamp(0.0, 1.0),
    };
    (signals, ctx)
}

/// Trim an f64 numeric for display (`49.0` → `49`, `0.204` → `0.204`).
fn trim_num(v: f64) -> String {
    if (v.fract()).abs() < 1e-9 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.4}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Reconstruct `(after, before)` text from a delta. For an `idiff`, `after = keep+ins`,
/// `before = keep+del`. For a `val`, `a` is after and `b` is before. For a `block` add/remove,
/// the non-empty side is `a`.
fn delta_texts(delta: &Delta) -> (String, String) {
    match delta {
        Delta::Val { a, b } => (a.clone(), b.clone()),
        Delta::Idiff { ops } => {
            let mut after = String::new();
            let mut before = String::new();
            for op in ops {
                match op.op {
                    DiffOp::Keep => {
                        after.push_str(&op.text);
                        before.push_str(&op.text);
                    }
                    DiffOp::Ins => after.push_str(&op.text),
                    DiffOp::Del => before.push_str(&op.text),
                }
            }
            (after, before)
        }
        // A `block` add carries the new text in `a`; a `block` remove carries the old text in `a`.
        // We cannot tell direction from the delta alone here, so treat `a` as the present side and
        // `b` as the prior side (the caller's `ct` disambiguates add vs remove for cat).
        Delta::Block { a, b, .. } => (a.clone(), b.clone().unwrap_or_default()),
        Delta::Move { .. } | Delta::Struct { .. } => (String::new(), String::new()),
    }
}

/// §8.1 `neg` — a polarity flip is a meaning-inverting token that appears on the NEW side but not
/// the prior side (an introduced "deprecated"/"removed"/…). For an add (`before` empty) the token
/// merely being present on the new side is a flip. Substring, case-insensitive.
fn polarity_flipped(before: &str, after: &str, delta: &Delta) -> bool {
    let after_l = after.to_ascii_lowercase();
    let before_l = before.to_ascii_lowercase();
    // For an idiff, the cleanest signal is a neg token inside an INSERTED span.
    if let Delta::Idiff { ops } = delta {
        let inserted: String = ops
            .iter()
            .filter(|o| o.op == DiffOp::Ins)
            .map(|o| o.text.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(" ");
        if NEG_TOKENS.iter().any(|t| inserted.contains(t)) {
            return true;
        }
    }
    NEG_TOKENS
        .iter()
        .any(|t| after_l.contains(t) && !before_l.contains(t))
}

// ===========================================================================================
// `date` signal — civil-date shift magnitude (NO clock).
// ===========================================================================================

/// §8.1 `date` — score by the SIZE of the date shift between the before/after civil dates. NO
/// reference to the current clock (the proximity enrichment is opt-in, default-off, OUT in MVP).
/// A larger shift scores higher, saturating: `1 − exp(−|Δdays| / 30)` (a ~1-month move ≈ 0.63, a
/// 60-day move ≈ 0.86, a 1-day move ≈ 0.03).
fn date_shift_signal(before: &str, after: &str) -> f32 {
    match (first_iso_date(before), first_iso_date(after)) {
        (Some(a), Some(b)) if a != b => {
            let days = (a - b).whole_days().unsigned_abs() as f32;
            1.0 - (-days / 30.0).exp()
        }
        _ => 0.0,
    }
}

/// Parse the first `YYYY-MM-DD` / `YYYY/MM/DD` token in `text` to a civil date (clock-free).
fn first_iso_date(text: &str) -> Option<time::Date> {
    let token = text.split_whitespace().find(|w| looks_like_iso_date(w))?;
    let sep = if token.contains('-') { '-' } else { '/' };
    let mut parts = token.split(sep);
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u8 = parts.next()?.parse().ok()?;
    let d: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let month = time::Month::try_from(m).ok()?;
    time::Date::from_calendar_date(y, month, d).ok()
}

/// True if `w` is a `YYYY-MM-DD` or `YYYY/MM/DD` shape (4-2-2 digits, single separator kind).
fn looks_like_iso_date(w: &str) -> bool {
    let sep = if w.contains('-') {
        '-'
    } else if w.contains('/') {
        '/'
    } else {
        return false;
    };
    let parts: Vec<&str> = w.split(sep).collect();
    parts.len() == 3
        && parts[0].len() == 4
        && (1..=2).contains(&parts[1].len())
        && (1..=2).contains(&parts[2].len())
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
}
