//! §7.2 Myers token diff for `idiff` (shared tokenizer), via `similar`'s Myers algorithm with a
//! deterministic span-coalescing + ±6-token context-elision pass (App C).
//!
//! The tokenizer is shared with the similarity layer (§7.1): words + punctuation + whitespace runs.
//! **Whitespace tokens are diffed but NEVER reported** — they participate in alignment (so a real
//! word edit is not masked by surrounding whitespace shifts) but are dropped from the emitted
//! `idiff` ops. Adjacent non-equal edits are coalesced into spans; equal runs longer than `2·CTX`
//! tokens are elided to ±`CTX` (6) tokens of context with a `…` marker.

use similar::{capture_diff_slices, Algorithm, DiffTag};

use crate::model::{DiffOp, IdiffOp};

/// Context window (tokens) kept on each side of an edit before `…` elision (App C: ±6 tokens).
pub const CTX: usize = 6;

/// The ellipsis marker emitted for elided equal runs.
pub const ELLIPSIS: &str = "…";

/// A single lexical token with its kind. The kind decides whether a token is *reportable*.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    pub kind: TokenKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// A run of word characters (alphanumerics + `_`).
    Word,
    /// A run of whitespace — diffed for alignment but NEVER reported (§7.2).
    Whitespace,
    /// A single punctuation/symbol character.
    Punct,
}

/// §7.2 — token-level Myers diff of two normalized strings into `idiff` ops with ±6-token context.
///
/// Whitespace-only edits are suppressed entirely (they produce no reportable op). The returned ops
/// are coalesced spans with elided equal context, ready for the `delta.idiff` encoding.
pub fn token_diff(before: &str, after: &str) -> Vec<IdiffOp> {
    let a = tokenize(before);
    let b = tokenize(after);
    let a_texts: Vec<&str> = a.iter().map(|t| t.text.as_str()).collect();
    let b_texts: Vec<&str> = b.iter().map(|t| t.text.as_str()).collect();

    let ops = capture_diff_slices(Algorithm::Myers, &a_texts, &b_texts);

    // First pass: build a flat sequence of (op, token, reportable) honoring whitespace suppression.
    // We emit Keep/Del/Ins per token (Replace is expanded into Del-then-Ins so coalescing is uniform).
    let mut flat: Vec<RawOp> = Vec::new();
    for op in ops {
        match op.tag() {
            DiffTag::Equal => {
                let r = op.old_range();
                for tok in &a[r.clone()] {
                    flat.push(RawOp::new(DiffOp::Keep, tok));
                }
            }
            DiffTag::Delete => {
                for tok in &a[op.old_range()] {
                    flat.push(RawOp::new(DiffOp::Del, tok));
                }
            }
            DiffTag::Insert => {
                for tok in &b[op.new_range()] {
                    flat.push(RawOp::new(DiffOp::Ins, tok));
                }
            }
            DiffTag::Replace => {
                for tok in &a[op.old_range()] {
                    flat.push(RawOp::new(DiffOp::Del, tok));
                }
                for tok in &b[op.new_range()] {
                    flat.push(RawOp::new(DiffOp::Ins, tok));
                }
            }
        }
    }

    suppress_whitespace_only_edits(&mut flat);
    let coalesced = coalesce(&flat);
    elide_context(coalesced)
}

/// One token-level op before coalescing.
#[derive(Clone, Debug)]
struct RawOp {
    op: DiffOp,
    text: String,
    kind: TokenKind,
}

impl RawOp {
    fn new(op: DiffOp, tok: &Token) -> Self {
        RawOp {
            op,
            text: tok.text.clone(),
            kind: tok.kind,
        }
    }
    fn is_edit(&self) -> bool {
        matches!(self.op, DiffOp::Del | DiffOp::Ins)
    }
}

/// Demote whitespace tokens that are part of a *whitespace-only* edit back to Keep so they vanish:
/// a Del/Ins whose token is whitespace is suppressed (whitespace is never reported). We rewrite such
/// ops to Keep — they then merge into the surrounding equal run during coalescing and elide away if
/// far from a real edit. (A whitespace token that genuinely flanks a word edit is still kept as
/// context but reported only if its op is Keep, which is what this produces.)
fn suppress_whitespace_only_edits(flat: &mut [RawOp]) {
    for r in flat.iter_mut() {
        if r.is_edit() && r.kind == TokenKind::Whitespace {
            r.op = DiffOp::Keep;
        }
    }
}

/// Coalesce adjacent same-op tokens into spans, concatenating their text. Whitespace Keep tokens are
/// concatenated with their neighbors so spans read naturally ("Rate limit is " not "Rate"+" "+...).
fn coalesce(flat: &[RawOp]) -> Vec<IdiffOp> {
    let mut out: Vec<IdiffOp> = Vec::new();
    for r in flat {
        if let Some(last) = out.last_mut() {
            if last.op == r.op {
                last.text.push_str(&r.text);
                continue;
            }
        }
        out.push(IdiffOp {
            op: r.op,
            text: r.text.clone(),
        });
    }
    // Drop zero-length spans that can arise if everything was suppressed.
    out.retain(|o| !(o.op == DiffOp::Keep && o.text.is_empty()));
    out
}

/// Elide long equal (`Keep`) spans to ±`CTX` tokens of context with a `…` marker. An equal span at
/// the very start/end keeps only the trailing/leading context respectively (it never needs both).
fn elide_context(ops: Vec<IdiffOp>) -> Vec<IdiffOp> {
    // If there is no edit at all, the blocks are equal under the tokenizer → emit nothing.
    let has_edit = ops.iter().any(|o| o.op != DiffOp::Keep);
    if !has_edit {
        return Vec::new();
    }

    let n = ops.len();
    let mut out = Vec::with_capacity(n);
    for (i, o) in ops.iter().enumerate() {
        if o.op != DiffOp::Keep {
            out.push(o.clone());
            continue;
        }
        let at_start = i == 0;
        let at_end = i == n - 1;
        let toks = split_tokens(&o.text);
        if toks.len() <= keep_budget(at_start, at_end) {
            out.push(o.clone());
            continue;
        }
        out.push(elide_one(&toks, at_start, at_end));
    }
    out
}

/// How many tokens an equal span may keep before eliding: a middle span keeps 2·CTX (CTX each side),
/// a boundary span keeps CTX (only the inner side matters).
fn keep_budget(at_start: bool, at_end: bool) -> usize {
    if at_start || at_end {
        CTX
    } else {
        2 * CTX
    }
}

/// Build the elided text for one long equal span.
fn elide_one(toks: &[String], at_start: bool, at_end: bool) -> IdiffOp {
    let text = if at_start {
        // Leading span: only the trailing CTX tokens matter (they precede the first edit).
        format!("{}{}", ELLIPSIS, toks[toks.len() - CTX..].join(""))
    } else if at_end {
        // Trailing span: only the leading CTX tokens matter (they follow the last edit).
        format!("{}{}", toks[..CTX].join(""), ELLIPSIS)
    } else {
        // Middle span: keep CTX on each side around the `…`.
        format!(
            "{}{}{}",
            toks[..CTX].join(""),
            ELLIPSIS,
            toks[toks.len() - CTX..].join("")
        )
    };
    IdiffOp {
        op: DiffOp::Keep,
        text,
    }
}

/// Re-tokenize a coalesced Keep span's concatenated text into individual lexical tokens so the
/// ±CTX context budget counts *tokens*, not characters (App C).
fn split_tokens(s: &str) -> Vec<String> {
    tokenize(s).into_iter().map(|t| t.text).collect()
}

// ===========================================================================================
// Shared tokenizer (§7.2) — words + punctuation + whitespace runs.
// ===========================================================================================

/// Tokenize into words (alphanumeric + `_` runs), single punctuation chars, and whitespace runs.
/// This is the shared tokenizer used by both the intra-block diff and the §7.1 similarity layer.
pub fn tokenize(s: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            let mut run = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    run.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(Token {
                text: run,
                kind: TokenKind::Whitespace,
            });
        } else if is_word_char(c) {
            let mut run = String::new();
            while let Some(&c) = chars.peek() {
                if is_word_char(c) {
                    run.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(Token {
                text: run,
                kind: TokenKind::Word,
            });
        } else {
            // Single punctuation/symbol token (so "$49/mo" splits into $ 49 / mo for fine diffs).
            out.push(Token {
                text: c.to_string(),
                kind: TokenKind::Punct,
            });
            chars.next();
        }
    }
    out
}

#[inline]
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ===========================================================================================
// Similarity primitives (§7.1) — shared with the alignment layer.
// ===========================================================================================

/// The reportable (non-whitespace) token texts of a string — the unit of the similarity metrics.
fn content_tokens(s: &str) -> Vec<String> {
    tokenize(s)
        .into_iter()
        .filter(|t| t.kind != TokenKind::Whitespace)
        .map(|t| t.text)
        .collect()
}

/// Stable per-token hashes (xxh3) of a block's content tokens — the shingle set fed to the LSH band
/// (§7.1). Whitespace is excluded (it never anchors similarity).
pub fn token_hashes(s: &str) -> Vec<u64> {
    content_tokens(s)
        .iter()
        .map(|t| xxhash_rust::xxh3::xxh3_64(t.as_bytes()))
        .collect()
}

/// §7.1 token Jaccard over the *set* of content tokens: `|A∩B| / |A∪B|`. Two empty strings are
/// defined as identical (1.0) so empty-vs-empty never spuriously lowers similarity.
pub fn token_jaccard(a: &str, b: &str) -> f32 {
    use rustc_hash::FxHashSet;
    let sa: FxHashSet<String> = content_tokens(a).into_iter().collect();
    let sb: FxHashSet<String> = content_tokens(b).into_iter().collect();
    if sa.is_empty() && sb.is_empty() {
        return 1.0;
    }
    let inter = sa.intersection(&sb).count();
    let union = sa.union(&sb).count();
    if union == 0 {
        0.0
    } else {
        inter as f32 / union as f32
    }
}

/// §7.1 normalized Levenshtein over content tokens: `lev(A,B) / max(|A|,|B|)` in token units. The
/// sim formula uses `1 − this`. Bounded `O(|A|·|B|)` per pair, but the candidate set is window+LSH
/// bounded so total work stays `O(B·b)`.
pub fn norm_levenshtein(a: &str, b: &str) -> f32 {
    let ta = content_tokens(a);
    let tb = content_tokens(b);
    let la = ta.len();
    let lb = tb.len();
    if la == 0 && lb == 0 {
        return 0.0;
    }
    let dist = token_levenshtein(&ta, &tb);
    dist as f32 / la.max(lb) as f32
}

/// Classic two-row Levenshtein distance over a token slice.
fn token_levenshtein(a: &[String], b: &[String]) -> usize {
    let la = a.len();
    let lb = b.len();
    if la == 0 {
        return lb;
    }
    if lb == 0 {
        return la;
    }
    let mut prev: Vec<usize> = (0..=lb).collect();
    let mut cur = vec![0usize; lb + 1];
    for i in 1..=la {
        cur[0] = i;
        for j in 1..=lb {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[lb]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_splits_words_punct_whitespace() {
        let toks = tokenize("Rate limit is 100 req/s.");
        let kinds: Vec<TokenKind> = toks.iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&TokenKind::Word));
        assert!(kinds.contains(&TokenKind::Whitespace));
        assert!(kinds.contains(&TokenKind::Punct));
        // "req/s." -> req / s .
        let joined: String = toks.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(joined, "Rate limit is 100 req/s.");
    }

    #[test]
    fn single_number_edit_reports_minimal_span() {
        let ops = token_diff("Rate limit is 100 req/s.", "Rate limit is 60 req/s.");
        // Must contain a Del "100" and an Ins "60", with surrounding context Kept.
        assert!(ops.iter().any(|o| o.op == DiffOp::Del && o.text == "100"));
        assert!(ops.iter().any(|o| o.op == DiffOp::Ins && o.text == "60"));
        // And the context is preserved (reconstruct the before from Keep+Del).
        let before: String = ops
            .iter()
            .filter(|o| o.op != DiffOp::Ins)
            .map(|o| o.text.as_str())
            .collect();
        assert!(before.contains("Rate limit is "));
        assert!(before.contains(" req/s."));
    }

    #[test]
    fn whitespace_only_change_is_not_reported() {
        // Collapsing double spaces / adding a trailing space is whitespace-only → no reportable op.
        let ops = token_diff("Hello   world", "Hello world ");
        assert!(ops.is_empty(), "whitespace-only edit yields no idiff ops, got {ops:?}");
    }

    #[test]
    fn identical_text_yields_no_ops() {
        assert!(token_diff("same text here", "same text here").is_empty());
    }

    #[test]
    fn long_equal_runs_are_elided_with_ellipsis() {
        let before = "a b c d e f g h i j CHANGED k l m n o p q r s t";
        let after = "a b c d e f g h i j EDITED k l m n o p q r s t";
        let ops = token_diff(before, after);
        let joined: String = ops.iter().map(|o| o.text.as_str()).collect();
        assert!(joined.contains(ELLIPSIS), "long context must be elided: {joined}");
        assert!(ops.iter().any(|o| o.op == DiffOp::Del && o.text == "CHANGED"));
        assert!(ops.iter().any(|o| o.op == DiffOp::Ins && o.text == "EDITED"));
    }

    #[test]
    fn jaccard_and_levenshtein_bounds() {
        assert_eq!(token_jaccard("a b c", "a b c"), 1.0);
        assert_eq!(token_jaccard("", ""), 1.0);
        assert_eq!(token_jaccard("a b c", "x y z"), 0.0);
        assert_eq!(norm_levenshtein("a b c", "a b c"), 0.0);
        assert!((norm_levenshtein("a b c d", "a b x d") - 0.25).abs() < 1e-6);
    }

    #[test]
    fn token_hashes_are_stable_and_whitespace_free() {
        let h1 = token_hashes("the quick brown fox");
        let h2 = token_hashes("the quick brown fox");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 4, "four content tokens, whitespace excluded");
    }
}
