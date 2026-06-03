//! §7.3 noise_score — how likely a change is cosmetic/volatile rather than meaningful. The score is
//! a SOFT feature handed to salience (a negative weight), NOT a hard gate (DESIGN §7.3).
//!
//! An op scores high noise when (1) it is whitespace/case-only after stripping reported whitespace
//! tokens; (2) the only changed tokens are volatile numbers next to `viewing`/`online`/`ago`/`now`
//! AND the block is not a salient-numeric type (a `price`/`number`/`kv` keyed on price/qty/version is
//! NEVER noise; a bare counter next to "viewing"/"online now"/"ago" is); (3) reorder-only with no
//! content change; (4) volatile media/link add/remove. All inputs are passed as data — pure, no
//! clock/RNG/IO.

use crate::model::{BlockType, ChangeType, Delta, DiffOp};

/// Context for scoring one change unit's noise (§7.3), passed as data so the function stays pure.
pub struct NoiseInput<'a> {
    pub ct: ChangeType,
    pub block_type: BlockType,
    pub before_text: &'a str,
    pub after_text: &'a str,
    pub delta: &'a Delta,
}

/// §7.3 — compute the noise score 0..1 for one change unit (1.0 = almost certainly cosmetic).
pub fn noise_score(input: &NoiseInput<'_>) -> f32 {
    match input.ct {
        // (3) A pure reorder with no content change is high noise.
        ChangeType::Reordered => 0.9,
        // (4) Add/remove of a link/media block is volatile-ish; a moderate score (still soft).
        ChangeType::Added | ChangeType::Removed => {
            if is_media_or_link(input.block_type) {
                0.6
            } else {
                0.0
            }
        }
        ChangeType::Restyled => 0.95,
        ChangeType::Modified => modified_noise(input),
    }
}

/// Noise scoring for a `modified` op: inspect the actual changed tokens (§7.3 rules 1 & 2).
fn modified_noise(input: &NoiseInput<'_>) -> f32 {
    // A salient-numeric type is NEVER noise on a value change (rule 2 exception).
    if is_salient_numeric(input.block_type) {
        return 0.0;
    }

    // (1) Whitespace/case-only after collapsing whitespace → near-pure noise.
    if differs_only_by_whitespace_or_case(input.before_text, input.after_text) {
        return 1.0;
    }

    // (2) Volatile numeric counter: the changed tokens are all numbers.
    let numeric_only = match input.delta {
        Delta::Idiff { ops } => changed_tokens_are_all_numeric(ops),
        Delta::Val { a, b } => values_differ_only_in_numbers(b, a),
        _ => false,
    };
    if numeric_only {
        // A volatility cue word (viewing/online/ago/now/watching/…) is strong evidence of a live
        // counter / relative timestamp → high noise (near-total suppression to mat=none).
        if has_volatility_cue(input.after_text) {
            return 0.85;
        }
        // Absent a cue, a numeric-only edit in a SHORT block is still very likely an un-typed live
        // counter or per-poll churn (e.g. "127 in cart", "132 shoppers browsing") — the closed cue
        // list can't enumerate every phrasing. Give it MODERATE noise so it lands at low-`mat` per
        // §7.4's quietness expectation (filtered by `--min-salience medium`), rather than firing
        // material. We gate on block length so a number embedded in real prose is left alone, and a
        // salient-numeric type (price/number/date) already returned 0.0 above, so a real value
        // change is never touched.
        if is_short_block(input.after_text) {
            return 0.6;
        }
    }

    // A genuine content edit: low noise.
    0.05
}

/// Word-token count at/below which a numeric-only edit is treated as a likely bare live counter
/// (§7.4). "127 in cart" / "132 shoppers browsing" are ≤ 4 tokens; a genuine numeric mention usually
/// sits in longer prose, so the cap is deliberately conservative to limit false positives.
const SHORT_COUNTER_TOKENS: usize = 5;

/// True if a block is short enough that a numeric-only change is more likely volatile churn than a
/// material edit (see [`SHORT_COUNTER_TOKENS`]).
fn is_short_block(text: &str) -> bool {
    text.split_whitespace().count() <= SHORT_COUNTER_TOKENS
}

/// Salient-numeric block types whose value changes are never noise (§7.3 rule 2 exception).
fn is_salient_numeric(ty: BlockType) -> bool {
    matches!(ty, BlockType::Price | BlockType::Number | BlockType::Date)
}

/// Link/media block types whose add/remove is treated as volatile (§7.3 rule 4).
fn is_media_or_link(ty: BlockType) -> bool {
    matches!(ty, BlockType::Link)
}

/// True if the two texts differ only by whitespace and/or ASCII case.
fn differs_only_by_whitespace_or_case(a: &str, b: &str) -> bool {
    let na = collapse(a);
    let nb = collapse(b);
    na == nb && a != b
}

/// Lowercase + collapse all whitespace runs to a single space + trim.
fn collapse(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// True if every edited (Del/Ins) token in the idiff is a pure number (digits/commas/dot), and at
/// least one such token exists.
fn changed_tokens_are_all_numeric(ops: &[crate::model::IdiffOp]) -> bool {
    let mut saw_edit = false;
    for o in ops {
        if o.op == DiffOp::Keep {
            continue;
        }
        let t = o.text.trim();
        if t.is_empty() {
            continue;
        }
        saw_edit = true;
        if !is_numeric_token(t) {
            return false;
        }
    }
    saw_edit
}

/// True if `s` is a numeric token (digits with optional `,`/`.`), e.g. "127", "1,024", "3.5".
fn is_numeric_token(s: &str) -> bool {
    let cleaned: String = s.chars().filter(|c| *c != ',' && *c != '.').collect();
    !cleaned.is_empty() && cleaned.chars().all(|c| c.is_ascii_digit())
}

/// True if the two `val` endpoints differ only in their numeric tokens (same non-numeric skeleton).
fn values_differ_only_in_numbers(a: &str, b: &str) -> bool {
    let skel = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_digit() { '#' } else { c })
            .collect()
    };
    skel(a) == skel(b) && a != b
}

/// Volatility cue words that mark a bare number as a live counter / relative timestamp (§7.3).
fn has_volatility_cue(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const CUES: [&str; 9] = [
        "viewing", "online", "ago", "watching", "viewers", "now", "live", "people", "active",
    ];
    CUES.iter().any(|cue| lower.contains(cue))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IdiffOp;

    fn idiff(ops: Vec<(DiffOp, &str)>) -> Delta {
        Delta::Idiff {
            ops: ops
                .into_iter()
                .map(|(op, t)| IdiffOp { op, text: t.to_string() })
                .collect(),
        }
    }

    #[test]
    fn price_value_change_is_not_noise() {
        let d = Delta::Val { a: "$59/mo".into(), b: "$49/mo".into() };
        let n = noise_score(&NoiseInput {
            ct: ChangeType::Modified,
            block_type: BlockType::Price,
            before_text: "$49/mo",
            after_text: "$59/mo",
            delta: &d,
        });
        assert!(n < 0.1, "a real price change is low noise, got {n}");
    }

    #[test]
    fn viewing_counter_is_high_noise() {
        let d = idiff(vec![
            (DiffOp::Del, "127"),
            (DiffOp::Ins, "132"),
            (DiffOp::Keep, " viewing right now"),
        ]);
        let n = noise_score(&NoiseInput {
            ct: ChangeType::Modified,
            block_type: BlockType::Paragraph,
            before_text: "127 viewing right now",
            after_text: "132 viewing right now",
            delta: &d,
        });
        assert!(n >= 0.8, "a live viewer counter is high noise, got {n}");
    }

    #[test]
    fn uncued_short_counter_is_moderate_noise() {
        // §7.4: an un-typed live counter whose phrasing is outside the cue-word list ("127 in cart")
        // must still be damped enough to land low-mat. Moderate noise (≈0.6), not the 0.85 a cue earns.
        let d = idiff(vec![
            (DiffOp::Del, "127"),
            (DiffOp::Ins, "132"),
            (DiffOp::Keep, " in cart"),
        ]);
        let n = noise_score(&NoiseInput {
            ct: ChangeType::Modified,
            block_type: BlockType::Paragraph,
            before_text: "127 in cart",
            after_text: "132 in cart",
            delta: &d,
        });
        assert!((0.5..0.8).contains(&n), "an un-cued short counter is moderate noise, got {n}");
    }

    #[test]
    fn numeric_change_in_long_prose_is_not_damped() {
        // A number embedded in genuine prose (not a bare counter) is left as a low-noise content edit
        // — the short-block gate prevents over-damping real numeric mentions.
        let d = idiff(vec![
            (DiffOp::Keep, "Annual revenue rose to "),
            (DiffOp::Del, "9"),
            (DiffOp::Ins, "90"),
            (DiffOp::Keep, " million dollars across many regions"),
        ]);
        let n = noise_score(&NoiseInput {
            ct: ChangeType::Modified,
            block_type: BlockType::Paragraph,
            before_text: "Annual revenue rose to 9 million dollars across many regions",
            after_text: "Annual revenue rose to 90 million dollars across many regions",
            delta: &d,
        });
        assert!(n < 0.2, "a numeric change in long prose is a low-noise content edit, got {n}");
    }

    #[test]
    fn whitespace_only_is_max_noise() {
        let d = idiff(vec![(DiffOp::Keep, "Hello world")]);
        let n = noise_score(&NoiseInput {
            ct: ChangeType::Modified,
            block_type: BlockType::Paragraph,
            before_text: "Hello   world",
            after_text: "Hello world",
            delta: &d,
        });
        assert_eq!(n, 1.0);
    }

    #[test]
    fn genuine_prose_edit_is_low_noise() {
        let d = idiff(vec![
            (DiffOp::Keep, "Best for "),
            (DiffOp::Del, "growing"),
            (DiffOp::Ins, "scaling"),
            (DiffOp::Keep, " teams."),
        ]);
        let n = noise_score(&NoiseInput {
            ct: ChangeType::Modified,
            block_type: BlockType::Paragraph,
            before_text: "Best for growing teams.",
            after_text: "Best for scaling teams.",
            delta: &d,
        });
        assert!(n < 0.2, "a real word edit is low noise, got {n}");
    }

    #[test]
    fn reorder_is_high_noise() {
        let d = Delta::Move { from: 3, to: 0, key: "k".into() };
        let n = noise_score(&NoiseInput {
            ct: ChangeType::Reordered,
            block_type: BlockType::Paragraph,
            before_text: "x",
            after_text: "x",
            delta: &d,
        });
        assert!(n >= 0.8);
    }
}
