//! §5.2 extract — strategy dispatch (selector first; readability best-effort for prose) + the
//! hard-strip set. Pure: `fn(&str, &Profile) -> Result<DomSubtree, CfError>`.
//!
//! Input is raw HTML + a [`Profile`]; output is a *pruned element tree* (an owned [`ExtractNode`]
//! forest — NOT a string, per DESIGN §5.2). Three strategies, chosen by [`ExtractStrategy`]:
//!
//! - [`ExtractStrategy::Selector`] — keep verbatim the subtree(s) matched by `root_selector`
//!   (DESIGN §5.7) or the `select` list. Recommended for structured pages (pricing tables, status
//!   grids); drops everything not under a match.
//! - [`ExtractStrategy::Readability`] (default) — a Mozilla-Readability-style text-density /
//!   link-density DOM scoring with tag bonuses for `<article>`/`<main>`/`<section>` and penalties
//!   for `<nav>`/`<aside>`/`<footer>`; keeps the highest-scoring content container.
//! - [`ExtractStrategy::Full`] — keep `<body>` minus a hardcoded boilerplate blocklist.
//!
//! REGARDLESS of strategy we ALWAYS hard-strip `<script>`/`<style>`/`<noscript>`/`<template>`/
//! `<svg>`/`<iframe>`, HTML comments, and boilerplate-class matches (`ad`, `promo`, `cookie`,
//! `banner`, `newsletter`, `social-share`, `breadcrumb`).
//!
//! Determinism (ARCHITECTURE §4): no clock, no RNG, no I/O. Tree walks are pre-order; selector
//! iteration follows `scraper`'s deterministic document order.

use scraper::node::Node;
use scraper::{ElementRef, Html, Selector};

use crate::model::{ExtractStrategy, Profile};
use crate::CfError;

/// Elements hard-stripped regardless of strategy (DESIGN §5.2). Tag names are matched
/// case-insensitively (HTML parsing already lowercases element names).
const HARD_STRIP_TAGS: &[&str] = &["script", "style", "noscript", "template", "svg", "iframe"];

/// Boilerplate class substrings: an element whose `class` attribute contains any of these (as a
/// case-insensitive substring of a whitespace-delimited token) is dropped regardless of strategy
/// (DESIGN §5.2). Substring match is intentional so e.g. `cookie-banner`, `gdpr-cookie`, and
/// `social-share-buttons` all match.
const BOILERPLATE_CLASS_NEEDLES: &[&str] = &[
    "ad",
    "promo",
    "cookie",
    "banner",
    "newsletter",
    "social-share",
    "breadcrumb",
];

/// Tags that carry a positive readability bonus (main-content containers).
const READABILITY_BONUS_TAGS: &[&str] = &["article", "main", "section"];

/// Tags that carry a readability penalty (chrome / boilerplate containers).
const READABILITY_PENALTY_TAGS: &[&str] = &["nav", "aside", "footer", "header"];

/// DESIGN §4.9 Appendix C: the redesign guard threshold. If a `selector`-strategy match overlaps
/// the prior observation's slot keys below this fraction (or matches zero nodes), the diff engine
/// should surface ONE low-confidence `content_edit` for operator review rather than a silently
/// wrong confident diff. Exposed for the cli/diff wiring (§4.9).
pub const SELECT_OVERLAP_MIN: f64 = 0.3;

/// One node of the pruned, owned content tree handed to normalize (§5.2 → §5.3).
///
/// This is the real deliverable: a structural element/text tree, not a serialized string. It is
/// owned (no borrow of the parsed `scraper` arena) so it can move by value through the pure
/// pipeline. `scraper`/`html5ever` parse + select + prune; the kept subtree is then *copied* into
/// this representation so downstream stages tree-walk it directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtractNode {
    /// An element with a lowercased tag name, retained attributes, and child nodes.
    Element {
        /// Lowercased tag name (e.g. `"div"`, `"h2"`).
        name: String,
        /// Retained attributes as `(name, value)` pairs, sorted by name for determinism.
        attrs: Vec<(String, String)>,
        /// Child nodes in document order.
        children: Vec<ExtractNode>,
    },
    /// A text node (verbatim; normalization happens in §5.3, not here).
    Text(String),
}

impl ExtractNode {
    /// The lowercased tag name if this is an element.
    pub fn tag(&self) -> Option<&str> {
        match self {
            ExtractNode::Element { name, .. } => Some(name.as_str()),
            ExtractNode::Text(_) => None,
        }
    }

    /// Returns the value of an attribute, if present.
    pub fn attr(&self, key: &str) -> Option<&str> {
        match self {
            ExtractNode::Element { attrs, .. } => attrs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str()),
            ExtractNode::Text(_) => None,
        }
    }

    /// Concatenated visible text of this node and its descendants (pre-order, no separators added).
    /// Used by tests/diagnostics; normalization (whitespace collapse, NFC) is §5.3's job.
    pub fn text(&self) -> String {
        let mut out = String::new();
        self.collect_text(&mut out);
        out
    }

    fn collect_text(&self, out: &mut String) {
        match self {
            ExtractNode::Text(t) => out.push_str(t),
            ExtractNode::Element { children, .. } => {
                for c in children {
                    c.collect_text(out);
                }
            }
        }
    }

    /// Pre-order iterator over `(tag_name)` for every element in this subtree (self first).
    pub fn element_tags(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_tags(&mut out);
        out
    }

    fn collect_tags<'a>(&'a self, out: &mut Vec<&'a str>) {
        if let ExtractNode::Element { name, children, .. } = self {
            out.push(name.as_str());
            for c in children {
                c.collect_tags(out);
            }
        }
    }

    /// Serialize this subtree back to an HTML-ish string (used to fill [`DomSubtree::html`] so the
    /// downstream `normalize` skeleton has a representation, and for test diagnostics). This is a
    /// minimal, deterministic serializer — not a spec-perfect HTML serializer.
    fn write_html(&self, out: &mut String) {
        match self {
            ExtractNode::Text(t) => out.push_str(t),
            ExtractNode::Element {
                name,
                attrs,
                children,
            } => {
                out.push('<');
                out.push_str(name);
                for (k, v) in attrs {
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    out.push_str(v);
                    out.push('"');
                }
                if children.is_empty() {
                    out.push_str("></");
                    out.push_str(name);
                    out.push('>');
                } else {
                    out.push('>');
                    for c in children {
                        c.write_html(out);
                    }
                    out.push_str("</");
                    out.push_str(name);
                    out.push('>');
                }
            }
        }
    }
}

/// The extracted, root-scoped DOM subtree handed to normalize (§5.2 → §5.3).
///
/// The authoritative content is [`DomSubtree::roots`] — the pruned element forest. `html` is a
/// deterministic serialization of that forest, retained so the (not-yet-implemented) `normalize`
/// stage and storage/diagnostics have a string representation; downstream stages should prefer
/// walking `roots`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DomSubtree {
    /// The pruned element forest (the real output — a tree, not a string).
    pub roots: Vec<ExtractNode>,
    /// Serialized HTML of the kept subtree (derived from `roots`).
    pub html: String,
    /// Count of elements hard-stripped during extraction (feeds `DocStats`).
    pub stripped_attrs: u32,
    /// Raw byte length of the input body (feeds `DocStats`).
    pub bytes_raw: u64,
}

impl DomSubtree {
    fn from_roots(roots: Vec<ExtractNode>, stripped: u32, bytes_raw: u64) -> Self {
        let mut html = String::new();
        for r in &roots {
            r.write_html(&mut html);
        }
        DomSubtree {
            roots,
            html,
            stripped_attrs: stripped,
            bytes_raw,
        }
    }
}

/// §5.2 — keep the profile-selected subtree (selector/readability/full), apply the hard-strip set.
///
/// Returns the pruned [`DomSubtree`]. The hard-strip set (scripts/styles/svg/iframe/comments +
/// boilerplate classes) is applied on EVERY path, before strategy-specific selection scores the
/// remaining nodes, so e.g. a `<nav class="ad">` is gone before readability ever ranks it.
pub fn extract(html: &str, profile: &Profile) -> Result<DomSubtree, CfError> {
    let bytes_raw = html.len() as u64;
    let document = Html::parse_document(html);

    // Counter for elements hard-stripped (boilerplate + hard-strip tags + comments).
    let mut stripped = 0u32;

    let roots: Vec<ExtractNode> = match profile.strategy {
        ExtractStrategy::Selector => extract_selector(&document, profile, &mut stripped)?,
        ExtractStrategy::Readability => extract_readability(&document, &mut stripped),
        ExtractStrategy::Full => extract_full(&document, &mut stripped),
    };

    Ok(DomSubtree::from_roots(roots, stripped, bytes_raw))
}

// ===========================================================================================
// Strategy: selector — keep verbatim the subtree(s) matched by root_selector / select list.
// ===========================================================================================

fn extract_selector(
    document: &Html,
    profile: &Profile,
    stripped: &mut u32,
) -> Result<Vec<ExtractNode>, CfError> {
    let selectors = collect_selector_sources(profile);
    if selectors.is_empty() {
        return Err(CfError::Usage(
            "extract strategy=selector requires a root_selector (or non-empty select list)"
                .to_string(),
        ));
    }

    let mut roots = Vec::new();
    for src in &selectors {
        let sel = Selector::parse(src)
            .map_err(|e| CfError::Usage(format!("invalid CSS selector {src:?}: {e:?}")))?;
        // `scraper`'s `select` yields matches in deterministic document (pre-order) order.
        for el in document.select(&sel) {
            if let Some(node) = prune_element(el, stripped) {
                roots.push(node);
            } else {
                // The matched element was itself boilerplate/hard-strip — count it.
                *stripped += 1;
            }
        }
    }

    Ok(roots)
}

/// The selector sources for the `selector` strategy: `root_selector` first (DESIGN §5.7), then any
/// profile-level `select` entries are merged in via the profile's `types`-independent path — in the
/// MVP the `select` list is threaded through the TargetCfg into `root_selector`; we also accept a
/// comma-joined `root_selector`. We keep this in one place so the cli can wire the `select` list.
fn collect_selector_sources(profile: &Profile) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(rs) = &profile.root_selector {
        let rs = rs.trim();
        if !rs.is_empty() {
            out.push(rs.to_string());
        }
    }
    out
}

// ===========================================================================================
// Strategy: readability — text-density/link-density DOM scoring.
// ===========================================================================================

fn extract_readability(document: &Html, stripped: &mut u32) -> Vec<ExtractNode> {
    let body = match body_element(document) {
        Some(b) => b,
        None => return Vec::new(),
    };

    // Score every candidate container (elements that hold prose), pick the best, prune it.
    let mut best: Option<(f64, ElementRef)> = None;
    for el in body.descendant_elements_including_self() {
        if is_hard_strip(&el) || is_boilerplate(&el) {
            continue;
        }
        let score = readability_score(&el);
        // `>` (not `>=`) keeps the FIRST (shallowest / earliest in document order) candidate on a
        // tie, which is deterministic since descendant iteration is pre-order.
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, el));
        }
    }

    let chosen = best.map(|(_, el)| el).unwrap_or(body);
    match prune_element(chosen, stripped) {
        Some(node) => vec![node],
        None => Vec::new(),
    }
}

/// Mozilla-Readability-style score: text density rewarded, link density penalized, tag bonuses for
/// content containers and penalties for chrome. Deterministic (integer/`f64` arithmetic over the
/// node's own subtree; no clock, no RNG).
fn readability_score(el: &ElementRef) -> f64 {
    let tag = el.value().name();

    // Text-density signals over the subtree.
    let text_len = visible_text_len(el) as f64;
    if text_len == 0.0 {
        return f64::MIN; // empty containers are never the main content
    }
    let link_text_len = link_text_len(el) as f64;
    let link_density = link_text_len / text_len; // 0..1
    let comma_bonus = (count_commas(el) as f64) * 3.0; // prose proxy (Readability heuristic)
    let para_bonus = (count_descendant_tag(el, "p") as f64) * 3.0;

    // Base content score from text length, discounted by link density.
    let mut score = text_len * (1.0 - link_density) + comma_bonus + para_bonus;

    // Tag bonuses / penalties.
    if READABILITY_BONUS_TAGS.contains(&tag) {
        score += 60.0;
    }
    if READABILITY_PENALTY_TAGS.contains(&tag) {
        score -= 60.0;
    }
    // A container that is mostly links (nav/menus) is chrome regardless of tag.
    if link_density > 0.5 {
        score -= text_len * 0.5;
    }

    score
}

// ===========================================================================================
// Strategy: full — body minus the hardcoded boilerplate blocklist.
// ===========================================================================================

fn extract_full(document: &Html, stripped: &mut u32) -> Vec<ExtractNode> {
    match body_element(document) {
        Some(body) => match prune_element(body, stripped) {
            Some(node) => vec![node],
            None => Vec::new(),
        },
        None => Vec::new(),
    }
}

// ===========================================================================================
// Pruning: the always-on hard-strip set + boilerplate-class removal, applied while copying the
// borrowed `scraper` subtree into the owned `ExtractNode` forest.
// ===========================================================================================

/// Copy an `ElementRef` subtree into an owned [`ExtractNode`], dropping hard-strip tags, comments,
/// and boilerplate-class elements anywhere in the subtree. Returns `None` if `el` itself is dropped.
/// Increments `stripped` for each dropped element (the element itself, not its attributes — the
/// attribute-strip count is normalize's §5.3 concern; this is the element-level prune count).
fn prune_element(el: ElementRef, stripped: &mut u32) -> Option<ExtractNode> {
    if is_hard_strip(&el) || is_boilerplate(&el) {
        return None;
    }
    let name = el.value().name().to_string();

    // Retain attributes, sorted by name for deterministic output. (Normalize §5.3 strips the
    // volatile/high-entropy subset later; extract keeps them so normalize can see them.)
    let mut attrs: Vec<(String, String)> = el
        .value()
        .attrs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    attrs.sort_by(|a, b| a.0.cmp(&b.0));

    let mut children = Vec::new();
    for child in el.children() {
        match child.value() {
            Node::Text(t) => children.push(ExtractNode::Text(t.to_string())),
            Node::Comment(_) => {
                // Comments are always hard-stripped (§5.2).
                *stripped += 1;
            }
            Node::Element(_) => {
                if let Some(child_el) = ElementRef::wrap(child) {
                    if is_hard_strip(&child_el) || is_boilerplate(&child_el) {
                        *stripped += 1; // dropped element (and its whole subtree)
                    } else if let Some(node) = prune_element(child_el, stripped) {
                        children.push(node);
                    }
                }
            }
            // Doctype / ProcessingInstruction / Document / Fragment carry no content.
            _ => {}
        }
    }

    Some(ExtractNode::Element {
        name,
        attrs,
        children,
    })
}

/// True if the element is in the always-hard-strip tag set (§5.2).
fn is_hard_strip(el: &ElementRef) -> bool {
    HARD_STRIP_TAGS.contains(&el.value().name())
}

/// True if any of the element's class tokens contains a boilerplate needle as a case-insensitive
/// substring (§5.2). Substring (not exact) so `cookie-banner`, `gdpr-cookie`, `social-share-bar`
/// all match; matched per whitespace-delimited class token so an unrelated word in another token
/// does not pull in `ad` by accident across the full attribute string.
fn is_boilerplate(el: &ElementRef) -> bool {
    let Some(class_attr) = el.value().attr("class") else {
        return false;
    };
    let class_lower = class_attr.to_ascii_lowercase();
    class_lower
        .split_whitespace()
        .any(|token| BOILERPLATE_CLASS_NEEDLES.iter().any(|n| token.contains(n)))
}

// ===========================================================================================
// Readability text-density helpers (borrowed-tree walks; no allocation of the whole subtree text).
// ===========================================================================================

/// Length (in `char`s) of the visible, non-whitespace-only text under `el`, EXCLUDING the subtrees
/// of hard-strip / boilerplate elements (so a nav buried in an article does not inflate its score).
fn visible_text_len(el: &ElementRef) -> usize {
    text_len_filtered(el, false)
}

/// Length (in `char`s) of text that sits inside `<a>` descendants (link density numerator).
fn link_text_len(el: &ElementRef) -> usize {
    text_len_filtered(el, true)
}

/// Shared text-length walk. When `only_links` is true, count only text inside an `<a>` ancestor.
fn text_len_filtered(el: &ElementRef, only_links: bool) -> usize {
    fn walk(node: scraper::ElementRef, in_link: bool, only_links: bool, acc: &mut usize) {
        for child in node.children() {
            match child.value() {
                Node::Text(t) if !only_links || in_link => {
                    *acc += t.trim().chars().count();
                }
                Node::Element(_) => {
                    if let Some(child_el) = ElementRef::wrap(child) {
                        if is_hard_strip(&child_el) || is_boilerplate(&child_el) {
                            continue;
                        }
                        let now_in_link = in_link || child_el.value().name() == "a";
                        walk(child_el, now_in_link, only_links, acc);
                    }
                }
                _ => {}
            }
        }
    }
    let mut acc = 0;
    let start_in_link = el.value().name() == "a";
    walk(*el, start_in_link, only_links, &mut acc);
    acc
}

fn count_commas(el: &ElementRef) -> usize {
    // Cheap prose proxy: count commas in the full subtree text.
    let mut n = 0;
    fn walk(node: scraper::ElementRef, n: &mut usize) {
        for child in node.children() {
            match child.value() {
                Node::Text(t) => *n += t.matches(',').count(),
                Node::Element(_) => {
                    if let Some(child_el) = ElementRef::wrap(child) {
                        if is_hard_strip(&child_el) || is_boilerplate(&child_el) {
                            continue;
                        }
                        walk(child_el, n);
                    }
                }
                _ => {}
            }
        }
    }
    walk(*el, &mut n);
    n
}

fn count_descendant_tag(el: &ElementRef, tag: &str) -> usize {
    // `descendent_elements()` is strict descendants (excludes self), which is what we want.
    el.descendent_elements()
        .filter(|e| e.value().name() == tag)
        .count()
}

// ===========================================================================================
// Small extension: descendant-elements-including-self over a borrowed ElementRef.
// ===========================================================================================

trait ElementRefExt<'a> {
    /// Pre-order iterator over this element and all descendant elements.
    fn descendant_elements_including_self(self) -> Vec<ElementRef<'a>>;
}

impl<'a> ElementRefExt<'a> for ElementRef<'a> {
    fn descendant_elements_including_self(self) -> Vec<ElementRef<'a>> {
        let mut out = vec![self];
        out.extend(self.descendent_elements());
        out
    }
}

/// The `<body>` element of a parsed document, if present.
fn body_element(document: &Html) -> Option<ElementRef<'_>> {
    let sel = Selector::parse("body").ok()?;
    document.select(&sel).next()
}

/// Pure helper: the page `<title>` text (DESIGN §6.2 `src.title`), with HTML entities decoded and
/// surrounding whitespace trimmed. Returns `None` when there is no non-empty title.
///
/// The selector/readability strategies prune `<head>` away, so `src.title` cannot be recovered from
/// the extracted subtree; the cli boundary calls this over the FULL body to fill `ctx.title`. Pure
/// (no clock / IO), so it lives in `cf-core` next to the rest of the HTML parsing.
pub fn page_title(html: &str) -> Option<String> {
    let document = Html::parse_document(html);
    let sel = Selector::parse("title").ok()?;
    let el = document.select(&sel).next()?;
    let text = el.text().collect::<String>().trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

// ===========================================================================================
// §4.9 select_overlap warning hook (redesign guard).
// ===========================================================================================

/// Outcome of the §4.9 redesign guard for the `selector` strategy.
///
/// `Eq` is intentionally NOT derived: the `LowOverlap.overlap` field is an `f64`. Equality in tests
/// uses `PartialEq` (and asserts the overlap value exactly, which is fine for these exact ratios).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SelectOverlapWarning {
    /// The selector matched zero content nodes — likely a redesign or a broken selector. DESIGN
    /// §11: surfaced as an operator alert (exit-3-style) rather than a silent empty diff.
    ZeroMatch,
    /// The matched subtree's slot-key overlap with the prior observation fell below
    /// [`SELECT_OVERLAP_MIN`] — likely a redesign or the selector now matches a DIFFERENT subtree.
    /// DESIGN §11: emit ONE low-confidence (`c_stability=0.6`), high-materiality `content_edit` and
    /// flag for operator review.
    LowOverlap { overlap: f64 },
}

/// §4.9 redesign guard. Given the slot keys (as opaque comparable strings — typically
/// `SlotKey::fp_hex()` values) the `selector` strategy produced *this* observation and the prior
/// observation's slot keys, decide whether to warn.
///
/// - Zero current matches ⇒ [`SelectOverlapWarning::ZeroMatch`].
/// - Jaccard overlap of the two key sets `< SELECT_OVERLAP_MIN` ⇒
///   [`SelectOverlapWarning::LowOverlap`].
/// - Otherwise `None` (healthy match).
///
/// Pure and clock-free. This is the hook the cli/diff layer calls once it has both key sets; full
/// wiring (lowering `conf`, emitting the single `content_edit`) lands in the cli/diff stage (§4.9).
pub fn select_overlap_warning(
    current_slot_keys: &[String],
    prior_slot_keys: &[String],
) -> Option<SelectOverlapWarning> {
    if current_slot_keys.is_empty() {
        return Some(SelectOverlapWarning::ZeroMatch);
    }
    // With no prior we cannot judge overlap; treat as healthy (this is the first/baseline observation).
    if prior_slot_keys.is_empty() {
        return None;
    }

    let overlap = jaccard(current_slot_keys, prior_slot_keys);
    if overlap < SELECT_OVERLAP_MIN {
        Some(SelectOverlapWarning::LowOverlap { overlap })
    } else {
        None
    }
}

/// Jaccard similarity of two string sets: `|A ∩ B| / |A ∪ B|`. Deterministic; duplicates collapse.
fn jaccard(a: &[String], b: &[String]) -> f64 {
    use std::collections::BTreeSet;
    let sa: BTreeSet<&String> = a.iter().collect();
    let sb: BTreeSet<&String> = b.iter().collect();
    if sa.is_empty() && sb.is_empty() {
        return 1.0;
    }
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExtractStrategy, Profile, RenderMode, SourceMode};

    /// A bare profile with a chosen strategy; all stage seams empty/default (MVP defaults).
    fn profile(strategy: ExtractStrategy, root_selector: Option<&str>) -> Profile {
        Profile {
            profile_id: "test".to_string(),
            render: RenderMode::Auto,
            strategy,
            root_selector: root_selector.map(str::to_string),
            strip_attrs: Vec::new(),
            strip_text: Vec::new(),
            unordered: Vec::new(),
            mode: SourceMode::Page,
            max_pages: 1,
            types: Vec::new(),
            archetype: None,
            salience_hints: Vec::new(),
        }
    }

    /// Collect every element tag in the whole subtree forest, pre-order.
    fn all_tags(sub: &DomSubtree) -> Vec<String> {
        sub.roots
            .iter()
            .flat_map(|r| r.element_tags())
            .map(str::to_string)
            .collect()
    }

    fn has_tag(sub: &DomSubtree, tag: &str) -> bool {
        all_tags(sub).iter().any(|t| t == tag)
    }

    // --- selector strategy --------------------------------------------------------------------

    #[test]
    fn selector_keeps_only_matched_subtree_and_drops_siblings() {
        let html = r#"
            <html><body>
              <nav id="nav"><a href="/x">Home</a></nav>
              <main id="content">
                <h1>Keep Me</h1>
                <p>Body prose.</p>
              </main>
              <footer id="foot">Copyright</footer>
            </body></html>
        "#;
        let sub = extract(html, &profile(ExtractStrategy::Selector, Some("#content"))).unwrap();

        // Exactly one root, and it is the matched <main>.
        assert_eq!(sub.roots.len(), 1);
        assert_eq!(sub.roots[0].tag(), Some("main"));
        assert_eq!(sub.roots[0].attr("id"), Some("content"));

        // The matched subtree's content is kept verbatim.
        assert!(has_tag(&sub, "h1"));
        assert!(has_tag(&sub, "p"));
        assert!(sub.roots[0].text().contains("Keep Me"));
        assert!(sub.roots[0].text().contains("Body prose."));

        // Siblings (nav, footer) are dropped entirely.
        assert!(!has_tag(&sub, "nav"));
        assert!(!has_tag(&sub, "footer"));
        assert!(!sub.roots[0].text().contains("Copyright"));
        assert!(!sub.roots[0].text().contains("Home"));
    }

    #[test]
    fn selector_strategy_requires_a_selector() {
        let err = extract("<body><p>x</p></body>", &profile(ExtractStrategy::Selector, None));
        assert!(matches!(err, Err(CfError::Usage(_))));
    }

    #[test]
    fn selector_strategy_rejects_invalid_css() {
        let err = extract(
            "<body><p>x</p></body>",
            &profile(ExtractStrategy::Selector, Some(">>>not-css")),
        );
        assert!(matches!(err, Err(CfError::Usage(_))));
    }

    #[test]
    fn selector_can_match_multiple_subtrees() {
        let html = r#"
            <body>
              <div class="plan">A</div>
              <div class="other">skip</div>
              <div class="plan">B</div>
            </body>
        "#;
        let sub = extract(html, &profile(ExtractStrategy::Selector, Some(".plan"))).unwrap();
        assert_eq!(sub.roots.len(), 2);
        assert_eq!(sub.roots[0].text(), "A");
        assert_eq!(sub.roots[1].text(), "B");
    }

    // --- readability strategy -----------------------------------------------------------------

    #[test]
    fn readability_picks_article_over_nav_and_footer() {
        let html = r#"
            <html><body>
              <nav><a href="/a">Alpha</a> <a href="/b">Beta</a> <a href="/c">Gamma</a>
                   <a href="/d">Delta</a> <a href="/e">Epsilon</a></nav>
              <article>
                <h1>The Real Article</h1>
                <p>This is a substantial paragraph of prose, with commas, clauses, and
                   sentences that a reader would actually read for content.</p>
                <p>A second paragraph continues the article body with more real text,
                   ensuring the text density here dominates the surrounding chrome.</p>
              </article>
              <footer><a href="/privacy">Privacy</a> <a href="/terms">Terms</a></footer>
            </body></html>
        "#;
        let sub = extract(html, &profile(ExtractStrategy::Readability, None)).unwrap();

        // The chosen content root is the <article> (a bonus tag with the densest prose).
        assert_eq!(sub.roots.len(), 1);
        assert_eq!(sub.roots[0].tag(), Some("article"));
        assert!(sub.roots[0].text().contains("The Real Article"));
        assert!(sub.roots[0].text().contains("substantial paragraph"));

        // Nav and footer link chrome are NOT part of the chosen subtree.
        assert!(!sub.roots[0].text().contains("Privacy"));
        assert!(!sub.roots[0].text().contains("Alpha"));
    }

    #[test]
    fn readability_prefers_main_with_prose_over_link_heavy_aside() {
        let html = r#"
            <body>
              <aside>
                <a href="/1">one</a><a href="/2">two</a><a href="/3">three</a>
                <a href="/4">four</a><a href="/5">five</a><a href="/6">six</a>
              </aside>
              <main>
                <p>Real readable content lives here, with several commas, and enough
                   words to clearly out-score a list of bare navigation links.</p>
              </main>
            </body>
        "#;
        let sub = extract(html, &profile(ExtractStrategy::Readability, None)).unwrap();
        assert_eq!(sub.roots[0].tag(), Some("main"));
        assert!(sub.roots[0].text().contains("Real readable content"));
    }

    // --- hard-strip (all strategies) ----------------------------------------------------------

    #[test]
    fn hard_strip_removes_script_style_svg_iframe_noscript_template_and_comments() {
        let html = r#"
            <body>
              <main id="c">
                <script>alert('x')</script>
                <style>.a{color:red}</style>
                <noscript>no js</noscript>
                <template><p>tmpl</p></template>
                <svg><circle/></svg>
                <iframe src="//evil"></iframe>
                <!-- a comment that must vanish -->
                <p>visible prose</p>
              </main>
            </body>
        "#;
        // Use selector so we deterministically keep <main> and assert its pruned contents.
        let sub = extract(html, &profile(ExtractStrategy::Selector, Some("#c"))).unwrap();

        for stripped in ["script", "style", "noscript", "template", "svg", "iframe", "circle"] {
            assert!(
                !has_tag(&sub, stripped),
                "tag {stripped} should have been hard-stripped, tags = {:?}",
                all_tags(&sub)
            );
        }
        assert!(has_tag(&sub, "p"));
        assert!(sub.roots[0].text().contains("visible prose"));
        // Script/style text content must be gone.
        assert!(!sub.roots[0].text().contains("alert"));
        assert!(!sub.roots[0].text().contains("color:red"));
        // The comment text is gone (no comment node is ever emitted).
        assert!(!sub.html.contains("a comment that must vanish"));
        // At least the script/style/noscript/template/svg/iframe + comment were counted.
        assert!(sub.stripped_attrs >= 7, "stripped = {}", sub.stripped_attrs);
    }

    #[test]
    fn cookie_banner_class_element_is_removed() {
        let html = r#"
            <body>
              <main id="c">
                <div class="cookie-banner">We use cookies. Accept?</div>
                <div class="gdpr-cookie-consent">More cookies</div>
                <div class="promo-strip">Sale!</div>
                <div class="social-share">share</div>
                <p>genuine content</p>
              </main>
            </body>
        "#;
        let sub = extract(html, &profile(ExtractStrategy::Selector, Some("#c"))).unwrap();
        let text = sub.roots[0].text();
        assert!(!text.contains("We use cookies"));
        assert!(!text.contains("More cookies"));
        assert!(!text.contains("Sale!"));
        assert!(!text.contains("share"));
        assert!(text.contains("genuine content"));
    }

    #[test]
    fn boilerplate_strip_does_not_overmatch_unrelated_tokens() {
        // "header" contains "ad"? no. "headline" contains no needle either. Ensure a real word
        // like "address" (contains "ad") in a NON-class attribute does not trigger removal, and a
        // class token "address" DOES (substring policy is documented). We assert the documented
        // behavior precisely.
        let html = r#"
            <body>
              <main id="c">
                <div data-note="address book">kept (ad only in data-*, not class)</div>
                <div class="readable">kept (token has 'ad' substring? 'readable' contains 'ad')</div>
              </main>
            </body>
        "#;
        let sub = extract(html, &profile(ExtractStrategy::Selector, Some("#c"))).unwrap();
        let text = sub.roots[0].text();
        // data-* attribute is never consulted for boilerplate.
        assert!(text.contains("kept (ad only in data-*, not class)"));
        // Documented substring policy: a class token containing "ad" ("readable") IS dropped.
        assert!(!text.contains("'readable' contains 'ad'"));
    }

    // --- full strategy ------------------------------------------------------------------------

    #[test]
    fn full_strategy_keeps_body_minus_boilerplate_and_hard_strip() {
        let html = r#"
            <body>
              <script>x()</script>
              <div class="ad">advert</div>
              <nav>menu</nav>
              <p>kept content</p>
            </body>
        "#;
        let sub = extract(html, &profile(ExtractStrategy::Full, None)).unwrap();
        assert_eq!(sub.roots.len(), 1);
        assert_eq!(sub.roots[0].tag(), Some("body"));
        let text = sub.roots[0].text();
        assert!(text.contains("kept content"));
        assert!(text.contains("menu")); // full keeps nav (only boilerplate-class + hard-strip dropped)
        assert!(!text.contains("advert")); // class="ad" dropped
        assert!(!text.contains("x()")); // script dropped
        assert!(!has_tag(&sub, "script"));
        assert!(!has_tag(&sub, "div") || !text.contains("advert"));
    }

    // --- pricing fixture ----------------------------------------------------------------------

    #[test]
    fn pricing_before_fixture_extracts_the_pricing_table() {
        let html = include_str!("../../../tests/fixtures/pricing_before.html");
        let sub = extract(
            html,
            &profile(ExtractStrategy::Selector, Some("section.PricingTable")),
        )
        .unwrap();

        assert_eq!(sub.roots.len(), 1);
        assert_eq!(sub.roots[0].tag(), Some("section"));

        let text = sub.roots[0].text();
        // The three plans and their prices are all present.
        assert!(text.contains("Starter Plan"));
        assert!(text.contains("$19/mo"));
        assert!(text.contains("Pro Plan"));
        assert!(text.contains("$49/mo"));
        assert!(text.contains("Enterprise Plan"));
        assert!(text.contains("Contact us"));
        assert!(text.contains("Unlimited projects"));

        // The fixture's volatile chrome lives inside the section (live counter) and IS still
        // present after extract — §5.3 normalize, not §5.2 extract, removes volatile text. We only
        // assert the structural table survived; we do NOT over-claim that extract strips it.
        assert!(has_tag(&sub, "h1"));
        assert!(has_tag(&sub, "h2"));
        assert!(has_tag(&sub, "ul"));
        assert!(has_tag(&sub, "li"));
    }

    #[test]
    fn pricing_before_fixture_readability_finds_the_table_section() {
        // Readability over the same fixture should also land on the content section (it is the only
        // substantial container), demonstrating the default strategy does not destroy the table.
        let html = include_str!("../../../tests/fixtures/pricing_before.html");
        let sub = extract(html, &profile(ExtractStrategy::Readability, None)).unwrap();
        let text: String = sub.roots.iter().map(|r| r.text()).collect();
        assert!(text.contains("Starter Plan"));
        assert!(text.contains("$49/mo"));
        assert!(text.contains("Enterprise Plan"));
    }

    // --- determinism --------------------------------------------------------------------------

    #[test]
    fn extract_is_deterministic_across_runs() {
        let html = include_str!("../../../tests/fixtures/pricing_before.html");
        let p = profile(ExtractStrategy::Readability, None);
        let a = extract(html, &p).unwrap();
        let b = extract(html, &p).unwrap();
        assert_eq!(a, b, "extract must be byte-for-byte deterministic");

        let ps = profile(ExtractStrategy::Selector, Some("section.PricingTable"));
        let c = extract(html, &ps).unwrap();
        let d = extract(html, &ps).unwrap();
        assert_eq!(c, d);
    }

    #[test]
    fn attrs_are_sorted_for_determinism() {
        let html = r#"<body><main id="c"><div zeta="1" alpha="2" mid="3">x</div></main></body>"#;
        let sub = extract(html, &profile(ExtractStrategy::Selector, Some("#c"))).unwrap();
        if let ExtractNode::Element { children, .. } = &sub.roots[0] {
            if let ExtractNode::Element { attrs, .. } = &children[0] {
                let keys: Vec<&str> = attrs.iter().map(|(k, _)| k.as_str()).collect();
                assert_eq!(keys, vec!["alpha", "mid", "zeta"]);
            } else {
                panic!("expected element child");
            }
        } else {
            panic!("expected element root");
        }
    }

    // --- §4.9 select_overlap warning hook -----------------------------------------------------

    #[test]
    fn select_overlap_zero_match_warns() {
        let prior = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            select_overlap_warning(&[], &prior),
            Some(SelectOverlapWarning::ZeroMatch)
        );
    }

    #[test]
    fn select_overlap_low_overlap_warns_below_threshold() {
        // current {a} vs prior {b,c,d} → intersection 0 / union 4 = 0.0 < 0.3.
        let current = vec!["a".to_string()];
        let prior = vec!["b".to_string(), "c".to_string(), "d".to_string()];
        match select_overlap_warning(&current, &prior) {
            Some(SelectOverlapWarning::LowOverlap { overlap }) => {
                assert!(overlap < SELECT_OVERLAP_MIN);
                assert_eq!(overlap, 0.0);
            }
            other => panic!("expected LowOverlap, got {other:?}"),
        }
    }

    #[test]
    fn select_overlap_healthy_match_does_not_warn() {
        // current {a,b,c} vs prior {a,b,c,d} → 3/4 = 0.75 ≥ 0.3.
        let current = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let prior = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        assert_eq!(select_overlap_warning(&current, &prior), None);
    }

    #[test]
    fn select_overlap_first_observation_no_prior_is_healthy() {
        let current = vec!["a".to_string()];
        assert_eq!(select_overlap_warning(&current, &[]), None);
    }

    // --- page_title (src.title source, §6.2) --------------------------------------------------

    #[test]
    fn page_title_reads_decodes_and_trims_the_title() {
        // Entity-decoded + whitespace-trimmed; the em-dash survives as a multibyte glyph.
        let html = "<html><head><title>  Pricing &amp; Plans — Acme  </title></head><body><p>x</p></body></html>";
        assert_eq!(page_title(html).as_deref(), Some("Pricing & Plans — Acme"));
    }

    #[test]
    fn page_title_is_none_when_absent_or_empty() {
        assert_eq!(page_title("<html><body><p>no head</p></body></html>"), None);
        assert_eq!(page_title("<html><head><title>   </title></head><body></body></html>"), None);
    }

    #[test]
    fn page_title_matches_the_pricing_fixture() {
        let html = include_str!("../../../tests/fixtures/pricing_before.html");
        assert_eq!(page_title(html).as_deref(), Some("Pricing — Competitor"));
    }

    #[test]
    fn select_overlap_boundary_is_inclusive_lower_exclusive() {
        // Construct overlap exactly 0.3-ish vs just-below. {a,b,c} vs {a,b,c,d,e,f,g} → 3/7 ≈ 0.4286
        // (healthy). {a} vs {a,b,c,d} → 1/4 = 0.25 < 0.3 (warn).
        let healthy_cur = vec!["a".into(), "b".into(), "c".into()];
        let healthy_prior = vec![
            "a".into(),
            "b".into(),
            "c".into(),
            "d".into(),
            "e".into(),
            "f".into(),
            "g".into(),
        ];
        assert_eq!(select_overlap_warning(&healthy_cur, &healthy_prior), None);

        let warn_cur = vec!["a".to_string()];
        let warn_prior = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        assert!(matches!(
            select_overlap_warning(&warn_cur, &warn_prior),
            Some(SelectOverlapWarning::LowOverlap { .. })
        ));
    }
}
