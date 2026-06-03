//! §7.0 ignore masking — drop/redact/attr-strip pre-hash, driven by the profile's `IgnoreRule`s.
//!
//! Runs first and is cheap. Three rule kinds (DESIGN §7.0), each applied *before* the masked doc's
//! per-block content is rehashed so the diff never sees the ignored content:
//!
//! - `selector` matches a block → **drops** the block entirely (it does not participate in
//!   alignment). MVP selector matching mirrors segment's [`TypeOverrides`] reach: a `.class` token,
//!   an `#id`, or a bare tag name matched against the block's own type/anchor — full CSS-descendant
//!   matching is Phase 2. Because segment discards the originating element's attrs, the selector is
//!   matched against the recorded `block_id` *anchor scheme* and the block's type/slot — see
//!   [`selector_matches`] for the supported forms.
//! - `regex` matching only *part* of a block's text → **redacts** the matched span with the
//!   object-replacement sentinel `\u{fffc}` before hashing (so "Last updated: 2026-06-02" stops
//!   flapping but the surrounding sentence still diffs).
//! - `attr` → strips the named attribute before the block hash is computed. After segment, the only
//!   attribute-derived value still carried on a block is a [`TypedValue::Link`]'s `href_canonical`;
//!   an `attr` rule naming `href`/`src` therefore clears that value (and the link is re-keyed on its
//!   text only) so an attribute-only swap is invisible to the diff.
//!
//! Determinism: masking is a pure, allocation-only transform — no clock/RNG/IO. After dropping /
//! redacting / attr-stripping, every touched block's `norm_hash`/`block_id` (and the doc's
//! `doc_hash`) are recomputed from the masked text so the §5.6 short-circuit and §7.1 anchoring see
//! the post-mask content.

use regex::Regex;

use crate::model::{
    Block, BlockId, CanonicalDoc, IgnoreRule, NormHash, Profile, SlotKey, TypedValue,
};

/// The Unicode OBJECT REPLACEMENT CHARACTER (`U+FFFC`) used as the redaction sentinel (DESIGN §7.0).
pub const REDACT_SENTINEL: char = '\u{fffc}';

/// Apply the profile's ignore rules to a doc before alignment, returning a masked copy whose
/// dropped/redacted blocks no longer participate in the diff (§7.0).
///
/// The returned doc's `blocks` are rebuilt: selector-dropped blocks are removed, regex-redacted
/// blocks carry the sentinel in place of the matched span (with `norm_hash`/`block_id` recomputed),
/// and attr-stripped blocks have the named value cleared. `doc_hash` is *not* refolded here (the
/// caller compares per-block `norm_hash`/the short-circuit upstream); masking only needs the block
/// content to be post-mask correct, which it is.
pub fn apply(doc: &CanonicalDoc, profile: &Profile) -> CanonicalDoc {
    if profile_has_no_ignore_rules(profile) {
        return doc.clone();
    }
    let compiled = CompiledIgnores::new(profile);
    let blocks = mask_blocks(&doc.blocks, &compiled);
    CanonicalDoc {
        blocks,
        ..doc.clone()
    }
}

/// Fast path: a profile with no ignore rules masks to an unchanged clone.
fn profile_has_no_ignore_rules(profile: &Profile) -> bool {
    ignore_rules(profile).next().is_none()
}

/// The ignore rules in effect for this diff. In MVP they come from the resolved profile; the
/// profile does not itself carry the target's `ignore` list (that lives on [`TargetCfg`]), so the
/// cli threads them through `profile.strip_text` as regex rules and through dedicated fields. To
/// keep the pure stage self-contained and testable we read the three rule kinds from the profile's
/// `strip_text` (regex, already the §5.3 contract) plus the explicit `ignore`-style selectors and
/// attrs the cli copies into the profile. Since the frozen `Profile` only exposes `strip_text`,
/// `strip_attrs`, and `types`, we map: `strip_text` → regex redactions, `strip_attrs` → attr strips.
/// Selector drops are expressed via the same `types` selector grammar but with a sentinel block type
/// — to avoid widening the frozen contract we instead accept selector drops via a parsed prefix on
/// `strip_text` entries of the form `selector:<sel>` and `attr:<name>` is likewise honored. The
/// canonical, fully-typed source remains [`build_rules`] for callers that have the real
/// [`IgnoreRule`] list.
fn ignore_rules(profile: &Profile) -> impl Iterator<Item = IgnoreRule> + '_ {
    profile.strip_text.iter().filter_map(|s| {
        // Only the explicit `selector:`/`attr:`/`regex:` ignore prefixes are §7.0 masking rules.
        // Bare `strip_text` entries are §5.3 normalize regexes, already applied upstream; they are
        // NOT re-applied here (that would double-strip).
        if let Some(sel) = s.strip_prefix("selector:") {
            return Some(IgnoreRule::Selector(sel.to_string()));
        }
        if let Some(attr) = s.strip_prefix("attr:") {
            return Some(IgnoreRule::Attr(attr.to_string()));
        }
        s.strip_prefix("regex:")
            .map(|rx| IgnoreRule::Regex(rx.to_string()))
    })
}

/// Build the masking transform directly from a typed [`IgnoreRule`] list. This is the contract the
/// cli uses (it has the real `TargetCfg.ignore`); [`apply`] is the profile-threaded convenience used
/// in pure tests. Both funnel into [`mask_blocks`].
pub fn apply_rules(doc: &CanonicalDoc, rules: &[IgnoreRule]) -> CanonicalDoc {
    if rules.is_empty() {
        return doc.clone();
    }
    let compiled = CompiledIgnores::from_rules(rules);
    let blocks = mask_blocks(&doc.blocks, &compiled);
    CanonicalDoc {
        blocks,
        ..doc.clone()
    }
}

/// A compiled set of ignore rules: regexes are compiled once (DESIGN §3 `regex` compile-once).
struct CompiledIgnores {
    selectors: Vec<String>,
    attrs: Vec<String>,
    regexes: Vec<Regex>,
}

impl CompiledIgnores {
    fn new(profile: &Profile) -> Self {
        let rules: Vec<IgnoreRule> = ignore_rules(profile).collect();
        Self::from_rules(&rules)
    }

    fn from_rules(rules: &[IgnoreRule]) -> Self {
        let mut selectors = Vec::new();
        let mut attrs = Vec::new();
        let mut regexes = Vec::new();
        for r in rules {
            match r {
                IgnoreRule::Selector(s) => selectors.push(s.clone()),
                IgnoreRule::Attr(a) => attrs.push(a.clone()),
                IgnoreRule::Regex(rx) => {
                    // A rule that fails to compile is inert (skipped) rather than panicking — config
                    // is validated at the cli boundary; here we degrade safely and deterministically.
                    if let Ok(re) = Regex::new(rx) {
                        regexes.push(re);
                    }
                }
            }
        }
        Self {
            selectors,
            attrs,
            regexes,
        }
    }
}

/// Rebuild a block forest under the ignore rules: drop selector matches, redact regex matches,
/// strip named attribute values. Children are masked recursively; a dropped container drops its
/// whole subtree (it never reaches alignment).
fn mask_blocks(blocks: &[Block], rules: &CompiledIgnores) -> Vec<Block> {
    let mut out = Vec::with_capacity(blocks.len());
    for b in blocks {
        if selector_matches(b, &rules.selectors) {
            continue; // dropped — excluded from alignment entirely (§7.0).
        }
        let children = mask_blocks(&b.children, rules);
        out.push(mask_one(b, rules, children));
    }
    out
}

/// Apply regex redaction + attr stripping to a single block, recomputing its content hashes from
/// the post-mask text/value so the diff sees only the masked content.
fn mask_one(b: &Block, rules: &CompiledIgnores, children: Vec<Block>) -> Block {
    let mut text = b.text.clone();
    let mut redacted = false;
    for re in &rules.regexes {
        if re.is_match(&text) {
            text = re
                .replace_all(&text, REDACT_SENTINEL.to_string().as_str())
                .into_owned();
            redacted = true;
        }
    }

    // Attr strip: after segment, the only attribute-derived value on a block is a Link's href. An
    // `attr` rule naming a link attribute clears that value so an attribute-only swap is invisible.
    let mut value = b.value.clone();
    let mut attr_stripped = false;
    if !rules.attrs.is_empty() {
        if let Some(TypedValue::Link { href_canonical }) = &value {
            let strips_href = rules
                .attrs
                .iter()
                .any(|a| a.eq_ignore_ascii_case("href") || a.eq_ignore_ascii_case("src"));
            if strips_href && !href_canonical.is_empty() {
                value = Some(TypedValue::Link {
                    href_canonical: String::new(),
                });
                attr_stripped = true;
            }
        }
    }

    if !redacted && !attr_stripped {
        // Untouched block: clone with its (possibly masked) children, hashes unchanged.
        return Block {
            children,
            ..b.clone()
        };
    }

    // When text was redacted, sync any text-bearing typed value to the redacted text so the diff's
    // value comparison reflects the mask (a Text/Heading/Code value must not retain the un-redacted
    // span, or it would re-surface a redacted change via the value).
    if redacted {
        value = match value {
            Some(TypedValue::Text(_)) => Some(TypedValue::Text(text.clone())),
            Some(TypedValue::Heading(_)) => Some(TypedValue::Heading(text.clone())),
            Some(TypedValue::Code(_)) => Some(TypedValue::Code(text.clone())),
            other => other,
        };
    }

    // Recompute the content handles from the post-mask text (slot_key is text-free and unchanged).
    let slot_key: SlotKey = b.slot_key;
    let norm_hash = NormHash::of(&text);
    let block_id = BlockId::derive(&slot_key, &text);
    Block {
        slot_key,
        block_id,
        text,
        value,
        norm_hash,
        children,
        ..b.clone()
    }
}

/// Does an ignore `selector` match this block? MVP supports the same selector reach as segment's
/// type overrides, matched against post-segment block facts (the originating element's attrs are not
/// retained after segment):
///
/// - a bare tag name maps to the block's [`crate::model::BlockType`] (`p`→Paragraph, `a`→Link, …);
/// - an `#id` matches a block whose `slot_key` equals `SlotKey::anchor(id)` (the anchor scheme);
/// - a `.class` token cannot be matched post-segment (attrs are gone) and is treated as a no-op so a
///   class-based ignore is at worst inert, never a false drop.
fn selector_matches(b: &Block, selectors: &[String]) -> bool {
    selectors.iter().any(|sel| {
        let tail = sel.split_whitespace().last().unwrap_or(sel.as_str());
        if let Some(id) = tail.strip_prefix('#') {
            !id.is_empty() && b.slot_key == SlotKey::anchor(id)
        } else if let Some(_class) = tail.strip_prefix('.') {
            false // class attrs are not retained post-segment; inert by design.
        } else if let Some(ty) = tag_to_type(tail) {
            b.ty == ty
        } else {
            false
        }
    })
}

/// Map a bare tag selector to the [`crate::model::BlockType`] segment assigns it.
fn tag_to_type(tag: &str) -> Option<crate::model::BlockType> {
    use crate::model::BlockType::*;
    Some(match tag {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => Heading,
        "p" => Paragraph,
        "li" => ListItem,
        "tr" => TableRow,
        "table" => Table,
        "pre" | "code" => Code,
        "a" => Link,
        _ => return None,
    })
}
