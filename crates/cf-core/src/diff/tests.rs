//! §7 diff engine unit tests. Cover the DESIGN §10/§11 behaviours: the price-cell `val` change with
//! `numeric_change.pct ≈ 0.204`; phase-1 anchor match of an edited block; similarity-fill of a
//! renamed section; added/removed; a contiguous reorder run; the three ignore rule kinds; a
//! viewing-counter high noise; the doc_hash short-circuit; idempotency `event_key` stability; and a
//! 5000-block sanity-size run.

use super::*;
use crate::extract::ExtractNode;
use crate::model::{
    AnchorScheme, Block, BlockId, BlockType, CanonicalDoc, ChangeType, Delta, DiffOp, DocHash,
    DocStats, ExtractStrategy, IgnoreRule, NormHash, Profile, RenderMode, SlotKey, SourceMode,
    TypedValue,
};
use crate::normalize::normalize;
use crate::segment::segment;

// ===========================================================================================
// Test helpers.
// ===========================================================================================

fn profile() -> Profile {
    Profile {
        profile_id: "test".to_string(),
        render: RenderMode::Auto,
        strategy: ExtractStrategy::Full,
        root_selector: None,
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

fn elem(name: &str, attrs: &[(&str, &str)], children: Vec<ExtractNode>) -> ExtractNode {
    ExtractNode::Element {
        name: name.to_string(),
        attrs: attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        children,
    }
}
fn text(t: &str) -> ExtractNode {
    ExtractNode::Text(t.to_string())
}

/// Segment a hand-built DOM into a CanonicalDoc, going through normalize (real slot_keys + hash).
fn doc_raw(roots: Vec<ExtractNode>) -> CanonicalDoc {
    let p = profile();
    let dom = normalize(
        crate::extract::DomSubtree {
            roots,
            html: String::new(),
            stripped_attrs: 0,
            bytes_raw: 0,
        },
        &p,
    )
    .unwrap();
    segment(dom, &p).unwrap()
}

/// doc_hash-equal helper alias used by the short-circuit tests (a real segmented doc).
fn doc(roots: Vec<ExtractNode>) -> CanonicalDoc {
    doc_raw(roots)
}

fn find_unit<'a>(cs: &'a Changeset, text_needle: &str) -> Option<&'a ChangeUnit> {
    cs.units.iter().find(|u| match &u.delta {
        Delta::Val { a, b } => a.contains(text_needle) || b.contains(text_needle),
        Delta::Idiff { ops } => ops.iter().any(|o| o.text.contains(text_needle)),
        Delta::Block { a, b, .. } => {
            a.contains(text_needle) || b.as_deref().is_some_and(|s| s.contains(text_needle))
        }
        _ => false,
    })
}

// ===========================================================================================
// §6.2 restyled — content identical, only a presentation marker (strike/ins) changed.
// ===========================================================================================

#[test]
fn strikethrough_with_identical_text_is_a_restyled_op() {
    // The §6.2 motivating case: a stable block (anchored by id) gains a `<del>` wrapper — its
    // normalized text is byte-identical, so `norm_hash`/value are equal, yet the presentation
    // changed. That must surface as exactly one `restyled` op (not a no-op, not a modified).
    let before = doc(vec![elem(
        "section",
        &[],
        vec![elem("p", &[("id", "deprecation")], vec![text("Legacy API v1")])],
    )]);
    let after = doc(vec![elem(
        "section",
        &[],
        vec![elem(
            "p",
            &[("id", "deprecation")],
            vec![elem("del", &[], vec![text("Legacy API v1")])],
        )],
    )]);
    // The strike folds into doc_hash (only because it is nonzero), so the no-op short-circuit does
    // NOT fire on a pure restyle.
    assert_ne!(before.doc_hash, after.doc_hash, "a restyle changes doc_hash (presentation folded in)");
    let cs = diff(&before, &after, &profile()).unwrap();
    assert_eq!(cs.units.len(), 1, "exactly one op, got {:?}", cs.units);
    assert_eq!(cs.units[0].ct, ChangeType::Restyled, "a strike-only change is restyled");
}

#[test]
fn identical_markup_is_not_restyled() {
    // Control: the SAME page twice (strike present in both) is a clean no-op — restyle_sig matches,
    // so no event and the doc_hash short-circuit holds.
    let page = || {
        doc(vec![elem(
            "section",
            &[],
            vec![elem(
                "p",
                &[("id", "x")],
                vec![elem("del", &[], vec![text("Old plan")])],
            )],
        )])
    };
    let a = page();
    let b = page();
    assert_eq!(a.doc_hash, b.doc_hash, "identical restyled markup hashes equal");
    assert!(diff(&a, &b, &profile()).unwrap().units.is_empty(), "no change → no op");
}

// ===========================================================================================
// (0) doc_hash short-circuit.
// ===========================================================================================

#[test]
fn doc_hash_equal_pair_yields_empty_changeset() {
    let a = doc(vec![elem(
        "section",
        &[],
        vec![elem("h2", &[], vec![text("Pro Plan")]), elem("p", &[], vec![text("$49/mo")])],
    )]);
    let b = a.clone();
    assert_eq!(a.doc_hash, b.doc_hash, "precondition: identical docs hash equal");
    let cs = diff(&a, &b, &profile()).unwrap();
    assert!(cs.units.is_empty(), "doc_hash-equal short-circuits to an EMPTY changeset");
}

#[test]
fn diff_of_a_with_itself_is_empty() {
    let a = doc(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Heading")]),
            elem("p", &[], vec![text("Some prose here.")]),
        ],
    )]);
    assert!(diff(&a, &a, &profile()).unwrap().units.is_empty());
}

// ===========================================================================================
// §10 price cell: exactly ONE modified op, val encoding, numeric_change pct ≈ 0.204, low noise.
// ===========================================================================================

fn pricing_doc(pro_price: &str) -> CanonicalDoc {
    doc_raw(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Starter Plan")]),
            elem("p", &[("class", "amount")], vec![text("$19/mo")]),
            elem("h2", &[], vec![text("Pro Plan")]),
            elem("p", &[("class", "amount")], vec![text(pro_price)]),
            elem("h2", &[], vec![text("Enterprise Plan")]),
            elem("p", &[("class", "amount")], vec![text("$199/mo")]),
        ],
    )])
}

#[test]
fn pricing_pro_price_change_is_one_modified_val_with_numeric_change() {
    let before = pricing_doc("$49/mo");
    let after = pricing_doc("$59/mo");

    let cs = diff(&before, &after, &profile()).unwrap();
    assert_eq!(cs.units.len(), 1, "exactly one modified op, got {:?}", cs.units);
    let u = &cs.units[0];
    assert_eq!(u.ct, ChangeType::Modified);
    assert_eq!(u.block_type, BlockType::Price);

    match &u.delta {
        Delta::Val { a, b } => {
            assert_eq!(a, "$59/mo", "a = after");
            assert_eq!(b, "$49/mo", "b = before");
        }
        other => panic!("expected a val delta, got {other:?}"),
    }

    let nc = u.numeric_change.expect("numeric_change present for a price change");
    assert!((nc.from - 49.0).abs() < 1e-9, "from = 49");
    assert!((nc.to - 59.0).abs() < 1e-9, "to = 59");
    assert!((nc.pct - 0.204).abs() < 0.001, "pct ≈ 0.204, got {}", nc.pct);

    assert!(u.noise_score < 0.1, "a real price change is low noise, got {}", u.noise_score);
    assert!((u.sim - 1.0).abs() < 1e-6, "anchored price cell has sim 1.0");
    assert_eq!(u.features.segment_stability, 1.0, "MVP default");
    assert_eq!(u.features.novelty, 1.0, "MVP default");
}

// ===========================================================================================
// Phase 1: an edited paragraph (slot_key stable) matches as MODIFIED via anchor, NOT similarity.
// ===========================================================================================

#[test]
fn edited_paragraph_matches_in_phase_one_not_similarity() {
    let build = |p1: &str| {
        doc_raw(vec![elem(
            "section",
            &[],
            vec![
                elem("h2", &[], vec![text("Pro Plan")]),
                elem("p", &[], vec![text(p1)]),
                elem("p", &[], vec![text("Cancel anytime.")]),
            ],
        )])
    };
    let before = build("Best for growing teams.");
    let after = build("Best for scaling teams.");

    let cs = diff(&before, &after, &profile()).unwrap();
    assert_eq!(cs.units.len(), 1, "one modified op, got {:?}", cs.units);
    let u = &cs.units[0];
    assert_eq!(u.ct, ChangeType::Modified);
    // The PROOF it matched in phase 1 (anchor), not similarity: sim is exactly 1.0 (certain anchor).
    // A similarity match would carry a fractional sim < 1.0.
    assert_eq!(u.sim, 1.0, "an anchored edit has sim 1.0 (phase-1 certain anchor)");

    match &u.delta {
        Delta::Idiff { ops } => {
            assert!(ops.iter().any(|o| o.op == DiffOp::Del && o.text == "growing"));
            assert!(ops.iter().any(|o| o.op == DiffOp::Ins && o.text == "scaling"));
        }
        other => panic!("expected idiff, got {other:?}"),
    }
}

// ===========================================================================================
// Phase 3: a renamed section (slot_key changed) falls to similarity fill.
// ===========================================================================================

/// Build a CanonicalDoc from raw blocks with explicit slot_keys (for rename/reorder control).
fn raw_doc(blocks: Vec<Block>) -> CanonicalDoc {
    let doc_hash = synthetic_doc_hash(&blocks);
    CanonicalDoc {
        schema: "changefeed.canonical/1",
        url: String::new(),
        final_url: String::new(),
        fetched_at: String::new(),
        fetch: Default::default(),
        profile_id: "test".to_string(),
        doc_hash,
        blocks,
        stats: DocStats::default(),
    }
}

/// A content-sensitive synthetic doc_hash for raw test docs (equal content → equal hash).
fn synthetic_doc_hash(blocks: &[Block]) -> DocHash {
    let mut h = blake3::Hasher::new();
    fn fold(h: &mut blake3::Hasher, bs: &[Block]) {
        for b in bs {
            h.update(b.slot_key.fp_hex().as_bytes());
            h.update(&[b.ty as u8]);
            h.update(b.text.as_bytes());
            h.update(&[0xff]);
            fold(h, &b.children);
        }
    }
    fold(&mut h, blocks);
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    DocHash::from_bytes(out)
}

fn para(slot: SlotKey, body: &str) -> Block {
    Block {
        slot_key: slot,
        block_id: BlockId::derive(&slot, body),
        ty: BlockType::Paragraph,
        level: None,
        text: body.to_string(),
        value: Some(TypedValue::Text(body.to_string())),
        anchored_by: AnchorScheme::Struct,
        norm_hash: NormHash::of(body),
        preorder_idx: 0,
        dom_depth: 1,
        restyle_sig: 0,
        children: Vec::new(),
    }
}

#[test]
fn renamed_section_block_falls_to_similarity_and_pairs_as_modified() {
    // The slot_key CHANGES (the section heading ordinal/breadcrumb shifted), but the text is a
    // one-sentence rewrite with high overlap -> sim >= 0.62 -> modified via similarity fill.
    let s_old = SlotKey::structural("Old Heading", BlockType::Paragraph, 0);
    let s_new = SlotKey::structural("New Heading", BlockType::Paragraph, 0);
    let old = raw_doc(vec![para(
        s_old,
        "The quick brown fox jumps over the lazy dog every morning.",
    )]);
    let new = raw_doc(vec![para(
        s_new,
        "The quick brown fox leaps over the lazy dog every morning.",
    )]);

    let cs = diff(&old, &new, &profile()).unwrap();
    assert_eq!(cs.units.len(), 1, "one modified op via similarity, got {:?}", cs.units);
    let u = &cs.units[0];
    assert_eq!(u.ct, ChangeType::Modified);
    assert!(u.sim >= TAU_MATCH, "sim >= 0.62 accepted as modified, got {}", u.sim);
    assert!(u.sim < 1.0, "similarity match has fractional sim (not a phase-1 anchor)");
    assert_eq!(u.slot_key, s_new, "the modified unit carries the NEW slot_key");
}

#[test]
fn low_overlap_rename_emits_clean_removed_plus_added() {
    // The slot_key changed AND the text is a wholesale replacement (<50% overlap) -> sim < 0.62 ->
    // a clean removed + added, NOT a modified.
    let s_old = SlotKey::structural("Old", BlockType::Paragraph, 0);
    let s_new = SlotKey::structural("New", BlockType::Paragraph, 0);
    let old = raw_doc(vec![para(s_old, "alpha beta gamma delta epsilon zeta")]);
    let new = raw_doc(vec![para(s_new, "completely different words here entirely now")]);

    let cs = diff(&old, &new, &profile()).unwrap();
    let kinds: Vec<ChangeType> = cs.units.iter().map(|u| u.ct).collect();
    assert!(kinds.contains(&ChangeType::Removed), "removed present: {kinds:?}");
    assert!(kinds.contains(&ChangeType::Added), "added present: {kinds:?}");
    assert!(!kinds.contains(&ChangeType::Modified), "no modified for low-overlap: {kinds:?}");
}

// ===========================================================================================
// Added / removed.
// ===========================================================================================

#[test]
fn an_added_block_is_classified_added() {
    let before = doc_raw(vec![elem(
        "section",
        &[],
        vec![elem("h2", &[], vec![text("Plans")]), elem("p", &[], vec![text("Starter tier.")])],
    )]);
    let after = doc_raw(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Plans")]),
            elem("p", &[], vec![text("Starter tier.")]),
            elem("p", &[], vec![text("Brand new enterprise tier added today.")]),
        ],
    )]);
    let cs = diff(&before, &after, &profile()).unwrap();
    let added = cs.units.iter().filter(|u| u.ct == ChangeType::Added).count();
    assert_eq!(added, 1, "exactly one added op, units: {:?}", cs.units);
    let u = find_unit(&cs, "enterprise").expect("the added block delta");
    assert_eq!(u.ct, ChangeType::Added);
    match &u.delta {
        Delta::Block { a, b, .. } => {
            assert!(a.contains("enterprise"));
            assert!(b.is_none(), "added block has no before-side");
        }
        other => panic!("expected a block delta, got {other:?}"),
    }
}

#[test]
fn a_removed_block_is_classified_removed() {
    let before = doc_raw(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Plans")]),
            elem("p", &[], vec![text("Starter tier.")]),
            elem("p", &[], vec![text("Deprecated legacy tier removed soon.")]),
        ],
    )]);
    let after = doc_raw(vec![elem(
        "section",
        &[],
        vec![elem("h2", &[], vec![text("Plans")]), elem("p", &[], vec![text("Starter tier.")])],
    )]);
    let cs = diff(&before, &after, &profile()).unwrap();
    let removed = cs.units.iter().filter(|u| u.ct == ChangeType::Removed).count();
    assert_eq!(removed, 1, "exactly one removed op, units: {:?}", cs.units);
    assert_eq!(find_unit(&cs, "legacy").unwrap().ct, ChangeType::Removed);
}

// ===========================================================================================
// Reorder: a contiguous run move of >= move_min=3 -> reorder.
// ===========================================================================================

fn anchored_para(id: &str, idx: u32) -> Block {
    let slot = SlotKey::anchor(id);
    let body = format!("paragraph {id}");
    Block {
        slot_key: slot,
        block_id: BlockId::derive(&slot, &body),
        ty: BlockType::Paragraph,
        level: None,
        text: body.clone(),
        value: Some(TypedValue::Text(body.clone())),
        anchored_by: AnchorScheme::Anchor,
        norm_hash: NormHash::of(&body),
        preorder_idx: idx,
        dom_depth: 1,
        restyle_sig: 0,
        children: Vec::new(),
    }
}

#[test]
fn contiguous_run_move_of_three_is_reordered() {
    // Eight anchored blocks; move a contiguous run of 3 (D,E,F) to the front. They share slot_keys
    // across both docs (anchored by id), so they anchor in phase 1; the LIS spine keeps A,B,C,G,H
    // increasing and the moved run D,E,F falls off the spine as a contiguous run of 3 -> reorder.
    let ids = ["A", "B", "C", "D", "E", "F", "G", "H"];
    let old: Vec<Block> = ids.iter().enumerate().map(|(i, id)| anchored_para(id, i as u32)).collect();
    let new_order = ["D", "E", "F", "A", "B", "C", "G", "H"];
    let new: Vec<Block> = new_order.iter().enumerate().map(|(i, id)| anchored_para(id, i as u32)).collect();

    let cs = diff(&raw_doc(old), &raw_doc(new), &profile()).unwrap();
    let reordered: Vec<&ChangeUnit> = cs.units.iter().filter(|u| u.ct == ChangeType::Reordered).collect();
    assert!(reordered.len() >= MOVE_MIN, "a contiguous run of >=3 emits reorders, got {:?}", cs.units);
    // No spurious modified/added/removed (content is identical, only positions changed).
    assert!(
        cs.units.iter().all(|u| u.ct == ChangeType::Reordered),
        "only reorders for a pure move, got {:?}",
        cs.units
    );
    assert!(reordered.iter().all(|u| u.features.moved), "reorder units are tagged moved");
}

#[test]
fn single_block_move_below_move_min_is_not_reordered() {
    // Moving ONE block (run length 1 < move_min=3) does not emit a standalone reorder.
    let ids = ["A", "B", "C", "D", "E"];
    let old: Vec<Block> = ids.iter().enumerate().map(|(i, id)| anchored_para(id, i as u32)).collect();
    let new_order = ["E", "A", "B", "C", "D"]; // one block (E) to front.
    let new: Vec<Block> = new_order.iter().enumerate().map(|(i, id)| anchored_para(id, i as u32)).collect();

    let cs = diff(&raw_doc(old), &raw_doc(new), &profile()).unwrap();
    assert!(
        cs.units.is_empty(),
        "a single-block move (< move_min) emits no standalone reorder, got {:?}",
        cs.units
    );
}

// ===========================================================================================
// §7.0 ignore masking: selector drop, regex redact, attr strip.
// ===========================================================================================

#[test]
fn ignore_selector_drops_a_block() {
    let before = doc_raw(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[("id", "main")], vec![text("Title")]),
            elem("li", &[], vec![text("counter value 100")]),
        ],
    )]);
    let after = doc_raw(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[("id", "main")], vec![text("Title")]),
            elem("li", &[], vec![text("counter value 200")]),
        ],
    )]);
    // Without the ignore: one modified op on the <li>.
    let plain = diff(&before, &after, &profile()).unwrap();
    assert_eq!(plain.units.len(), 1, "baseline: the li change is one op");

    // With a `selector: li` ignore: the list item is dropped on both sides -> empty changeset.
    let masked = diff_with_ignores(&before, &after, &[IgnoreRule::Selector("li".into())]).unwrap();
    assert!(masked.units.is_empty(), "selector-dropped block produces no unit, got {:?}", masked.units);
}

#[test]
fn ignore_regex_redacts_only_the_matched_span_surrounding_text_still_diffs() {
    let before = doc_raw(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Status")]),
            elem("p", &[], vec![text("Last updated: 2026-06-01. Pro tier is available.")]),
        ],
    )]);
    let after = doc_raw(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Status")]),
            elem("p", &[], vec![text("Last updated: 2026-06-02. Pro tier is unavailable.")]),
        ],
    )]);

    let re = IgnoreRule::Regex(r"Last updated: \d{4}-\d{2}-\d{2}".into());

    // The date-only flap, with the surrounding sentence unchanged, is fully suppressed.
    let date_before = doc_raw(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Status")]),
            elem("p", &[], vec![text("Last updated: 2026-06-01. Pro tier is available.")]),
        ],
    )]);
    let date_after = doc_raw(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Status")]),
            elem("p", &[], vec![text("Last updated: 2026-06-09. Pro tier is available.")]),
        ],
    )]);
    let suppressed = diff_with_ignores(&date_before, &date_after, std::slice::from_ref(&re)).unwrap();
    assert!(
        suppressed.units.is_empty(),
        "redacting the flapping date span suppresses a date-only change, got {:?}",
        suppressed.units
    );

    // The real availability change still surfaces (surrounding text diffs) even with the redaction.
    let cs = diff_with_ignores(&before, &after, std::slice::from_ref(&re)).unwrap();
    assert_eq!(cs.units.len(), 1, "the availability edit still diffs, got {:?}", cs.units);
    let u = &cs.units[0];
    assert_eq!(u.ct, ChangeType::Modified);
    match &u.delta {
        Delta::Idiff { ops } => {
            let joined: String = ops.iter().map(|o| o.text.as_str()).collect();
            assert!(
                joined.contains("available") || joined.contains("unavailable"),
                "the surrounding sentence change is reported: {joined}"
            );
            assert!(
                !ops.iter().any(|o| o.text.contains("2026")),
                "the redacted date is NOT part of the reported diff: {ops:?}"
            );
        }
        other => panic!("expected idiff, got {other:?}"),
    }
}

#[test]
fn ignore_attr_strips_link_href_before_hashing() {
    // A Link whose ONLY change is its href (with identical anchor text). An `attr: href` ignore
    // strips the href before hashing, so a utm/host swap on the link is invisible.
    let link = |href: &str| {
        doc_raw(vec![elem(
            "section",
            &[],
            vec![
                elem("h2", &[], vec![text("Docs")]),
                elem("a", &[("href", href)], vec![text("Read the guide")]),
            ],
        )])
    };
    let before = link("https://cdn-a.example.com/guide");
    let after = link("https://cdn-b.example.com/guide");

    // Baseline: with href as the value, the two docs differ (href change) -> one unit.
    let plain = diff(&before, &after, &profile()).unwrap();
    assert_eq!(plain.units.len(), 1, "baseline: the href swap is one op, got {:?}", plain.units);

    // With `attr: href` ignore the href is stripped on both sides -> identical -> no unit.
    let masked = diff_with_ignores(&before, &after, &[IgnoreRule::Attr("href".into())]).unwrap();
    assert!(
        masked.units.is_empty(),
        "stripping href before hashing suppresses an href-only swap, got {:?}",
        masked.units
    );
}

// ===========================================================================================
// §7.2 attribute deltas compared on POST-canonicalization values.
// ===========================================================================================

/// raw_doc whose synthetic hash also folds the typed value (so a value-only change is detected).
fn raw_doc_value(blocks: Vec<Block>) -> CanonicalDoc {
    let mut h = blake3::Hasher::new();
    fn fold(h: &mut blake3::Hasher, bs: &[Block]) {
        for b in bs {
            h.update(b.slot_key.fp_hex().as_bytes());
            h.update(&[b.ty as u8]);
            h.update(b.text.as_bytes());
            if let Some(TypedValue::Link { href_canonical }) = &b.value {
                h.update(href_canonical.as_bytes());
            }
            h.update(&[0xff]);
            fold(h, &b.children);
        }
    }
    fold(&mut h, &blocks);
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    CanonicalDoc {
        schema: "changefeed.canonical/1",
        url: String::new(),
        final_url: String::new(),
        fetched_at: String::new(),
        fetch: Default::default(),
        profile_id: "test".to_string(),
        doc_hash: DocHash::from_bytes(out),
        blocks,
        stats: DocStats::default(),
    }
}

#[test]
fn link_host_swap_is_reported_via_canonical_href() {
    // Two Links with the SAME anchor text but different canonical hosts. Because the text is equal,
    // they anchor in phase 1; the canonical href differs -> a modified val on the href is reported.
    let s = SlotKey::anchor("guide-link");
    let link_block = |href: &str| Block {
        slot_key: s,
        block_id: BlockId::derive(&s, "Read the guide"),
        ty: BlockType::Link,
        level: None,
        text: "Read the guide".to_string(),
        value: Some(TypedValue::Link { href_canonical: href.to_string() }),
        anchored_by: AnchorScheme::Anchor,
        norm_hash: NormHash::of("Read the guide"),
        preorder_idx: 0,
        dom_depth: 1,
        restyle_sig: 0,
        children: Vec::new(),
    };
    let old = raw_doc_value(vec![link_block("https://host-a.example.com/guide")]);
    let new = raw_doc_value(vec![link_block("https://host-b.example.com/guide")]);
    assert_ne!(old.doc_hash, new.doc_hash, "a host swap changes the value-aware doc_hash");

    let cs = diff(&old, &new, &profile()).unwrap();
    assert_eq!(cs.units.len(), 1, "the host swap is one modified op, got {:?}", cs.units);
    match &cs.units[0].delta {
        Delta::Val { a, b } => {
            assert!(a.contains("host-b"), "after-host reported");
            assert!(b.contains("host-a"), "before-host reported");
        }
        other => panic!("expected a val delta for the href change, got {other:?}"),
    }
}

// ===========================================================================================
// §7.3 noise: a "127 viewing" counter change gets a high noise_score.
// ===========================================================================================

#[test]
fn viewing_counter_change_gets_high_noise_score() {
    let before = doc_raw(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Live")]),
            elem("p", &[], vec![text("127 viewing right now")]),
        ],
    )]);
    let after = doc_raw(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Live")]),
            elem("p", &[], vec![text("132 viewing right now")]),
        ],
    )]);
    let cs = diff(&before, &after, &profile()).unwrap();
    assert_eq!(cs.units.len(), 1, "the counter change is one op, got {:?}", cs.units);
    let u = &cs.units[0];
    assert!(u.noise_score >= 0.8, "a live viewer counter is high noise, got {}", u.noise_score);
    assert_eq!(u.features.noise_score, u.noise_score, "features mirror the unit's noise_score");
}

// ===========================================================================================
// Idempotency: running diff twice yields the SAME event_key + byte-stable changeset.
// ===========================================================================================

#[test]
fn idempotency_event_key_is_stable_across_two_runs() {
    let before = pricing_doc("$49/mo");
    let after = pricing_doc("$59/mo");
    let tid = "comp-pricing";

    let a = diff(&before, &after, &profile()).unwrap();
    let b = diff(&before, &after, &profile()).unwrap();
    assert_eq!(a.units.len(), 1);
    assert_eq!(b.units.len(), 1);
    let ka = a.units[0].event_key(tid);
    let kb = b.units[0].event_key(tid);
    assert_eq!(ka, kb, "the same snapshot pair yields the same event_key (idempotency)");

    // And the key is the §7.4 derivation: xxh3(tid ‖ slot ‖ from_norm_hash ‖ to_norm_hash).
    let expected = crate::model::EventKey::derive(
        tid,
        &a.units[0].slot_key,
        a.units[0].from_norm_hash,
        a.units[0].to_norm_hash,
    );
    assert_eq!(ka, expected, "event_key matches the §7.4 formula");
}

#[test]
fn diff_is_byte_stable_across_runs() {
    let before = pricing_doc("$49/mo");
    let after = pricing_doc("$59/mo");
    let a = diff(&before, &after, &profile()).unwrap();
    let b = diff(&before, &after, &profile()).unwrap();
    assert_eq!(a.units.len(), b.units.len());
    for (x, y) in a.units.iter().zip(b.units.iter()) {
        assert_eq!(x.slot_key, y.slot_key);
        assert_eq!(x.ct, y.ct);
        assert_eq!(format!("{:?}", x.delta), format!("{:?}", y.delta));
        assert_eq!(x.noise_score, y.noise_score);
        assert_eq!(x.sim, y.sim);
    }
}

// ===========================================================================================
// Performance sanity: a 5000-block input completes quickly (NOT a hard benchmark).
// ===========================================================================================

#[test]
fn five_thousand_block_diff_completes_quickly() {
    use std::time::Instant;

    let mk = |edit_at: usize| -> CanonicalDoc {
        let blocks: Vec<Block> = (0..5000usize)
            .map(|i| {
                let id = format!("block-{i}");
                let slot = SlotKey::anchor(&id);
                let body = if i == edit_at {
                    format!("paragraph number {i} EDITED")
                } else {
                    format!("paragraph number {i}")
                };
                Block {
                    slot_key: slot,
                    block_id: BlockId::derive(&slot, &body),
                    ty: BlockType::Paragraph,
                    level: None,
                    text: body.clone(),
                    value: Some(TypedValue::Text(body.clone())),
                    anchored_by: AnchorScheme::Anchor,
                    norm_hash: NormHash::of(&body),
                    preorder_idx: i as u32,
                    dom_depth: 1,
                    restyle_sig: 0,
                    children: Vec::new(),
                }
            })
            .collect();
        raw_doc(blocks)
    };

    let before = mk(usize::MAX); // no edit
    let after = mk(2500); // one block edited

    let t = Instant::now();
    let cs = diff(&before, &after, &profile()).unwrap();
    let elapsed = t.elapsed();

    assert_eq!(cs.units.len(), 1, "one edited block -> one modified op");
    assert_eq!(cs.units[0].ct, ChangeType::Modified);
    assert!(elapsed.as_secs() < 5, "5k-block diff should be fast, took {elapsed:?}");
}

#[test]
fn low_anchor_diff_is_bounded() {
    // The honest worst case: rename-everything (every slot_key changes) so the similarity path runs,
    // proving the window+LSH band keeps it tractable (not O(B^2)).
    use std::time::Instant;
    let n = 1500usize;
    let mk = |suffix: &str| -> CanonicalDoc {
        let blocks: Vec<Block> = (0..n)
            .map(|i| {
                let slot = SlotKey::structural(&format!("{suffix}-sec-{i}"), BlockType::Paragraph, 0);
                let body = format!("the quick brown fox number {i} jumps over the lazy dog");
                Block {
                    slot_key: slot,
                    block_id: BlockId::derive(&slot, &body),
                    ty: BlockType::Paragraph,
                    level: None,
                    text: body.clone(),
                    value: Some(TypedValue::Text(body.clone())),
                    anchored_by: AnchorScheme::Struct,
                    norm_hash: NormHash::of(&body),
                    preorder_idx: i as u32,
                    dom_depth: 1,
                    restyle_sig: 0,
                    children: Vec::new(),
                }
            })
            .collect();
        raw_doc(blocks)
    };
    let before = mk("old");
    let after = mk("new");
    let t = Instant::now();
    let _cs = diff(&before, &after, &profile()).unwrap();
    let elapsed = t.elapsed();
    assert!(elapsed.as_secs() < 10, "low-anchor diff stays bounded, took {elapsed:?}");
}

// ===========================================================================================
// §6.3 cascade clustering — a container's mass child set-change collapses to one enc:"struct".
// ===========================================================================================

/// Build a single-`<table>` doc whose `n` rows each carry `cell(i)` as their one cell's text.
fn table_doc(n: usize, cell: impl Fn(usize) -> String) -> CanonicalDoc {
    let rows: Vec<ExtractNode> = (0..n)
        .map(|i| elem("tr", &[], vec![elem("td", &[], vec![text(&cell(i))])]))
        .collect();
    doc(vec![elem("table", &[], rows)])
}

#[test]
fn mass_table_change_collapses_to_one_struct_unit() {
    // 40 > max_children(32) rows each modified → ONE enc:"struct" on the table's slot, not 40 row
    // events plus the oversized whole-table unit. Counts are exact; the sample is bounded to 8.
    let n = cluster::MAX_CHILDREN + 8; // 40
    let before = table_doc(n, |i| format!("Item {i}: $100"));
    let after = table_doc(n, |i| format!("Item {i}: $200"));
    let cs = diff(&before, &after, &profile()).unwrap();

    assert_eq!(
        cs.units.len(),
        1,
        "a mass table change is ONE struct event, got {:?}",
        cs.units.iter().map(|u| u.ct).collect::<Vec<_>>()
    );
    let u = &cs.units[0];
    assert_eq!(u.ct, ChangeType::Modified, "the cluster is a modified container");
    match &u.delta {
        Delta::Struct { added, removed, modified, sample, truncated } => {
            assert_eq!(*added, 0);
            assert_eq!(*removed, 0);
            assert_eq!(*modified, n as u32, "every row counts as modified");
            assert_eq!(sample.len(), cluster::STRUCT_SAMPLE, "sample is bounded to 8");
            assert_eq!(*truncated, (n - cluster::STRUCT_SAMPLE) as u32, "remainder past the sample");
        }
        other => panic!("expected enc:struct, got {other:?}"),
    }
}

#[test]
fn table_change_at_or_below_cap_stays_per_row() {
    // 20 <= max_children rows changed → individual events, NEVER a struct: the bounded fallback only
    // engages PAST the cap (§6.3).
    let n = 20;
    let before = table_doc(n, |i| format!("Item {i}: $100"));
    let after = table_doc(n, |i| format!("Item {i}: $200"));
    let cs = diff(&before, &after, &profile()).unwrap();
    assert!(
        cs.units.iter().all(|u| !matches!(u.delta, Delta::Struct { .. })),
        "below the cap there is no struct fallback, got {:?}",
        cs.units.iter().map(|u| u.ct).collect::<Vec<_>>()
    );
    let modified_rows = cs
        .units
        .iter()
        .filter(|u| u.block_type == BlockType::TableRow && u.ct == ChangeType::Modified)
        .count();
    assert_eq!(modified_rows, n, "all rows emit as individual modified events");
}

#[test]
fn cluster_counts_added_rows() {
    // before 36 rows, after 50: rows 0..35 edited (modified) + 14 appended rows (new ordinals →
    // added). 50 > cap → struct with modified=36, added=14.
    let before = table_doc(36, |i| format!("Item {i}: $100"));
    let after = table_doc(50, |i| format!("Item {i}: $200"));
    let cs = diff(&before, &after, &profile()).unwrap();
    assert_eq!(cs.units.len(), 1, "one struct, got {:?}", cs.units.iter().map(|u| u.ct).collect::<Vec<_>>());
    match &cs.units[0].delta {
        Delta::Struct { added, removed, modified, .. } => {
            assert_eq!(*modified, 36, "the 36 surviving rows are modified");
            assert_eq!(*added, 14, "the 14 appended rows are added");
            assert_eq!(*removed, 0);
        }
        other => panic!("expected enc:struct, got {other:?}"),
    }
}

#[test]
fn cluster_counts_removed_rows() {
    // before 50 rows, after 36: rows 0..35 edited (modified) + 14 dropped rows (removed).
    let before = table_doc(50, |i| format!("Item {i}: $100"));
    let after = table_doc(36, |i| format!("Item {i}: $200"));
    let cs = diff(&before, &after, &profile()).unwrap();
    assert_eq!(cs.units.len(), 1, "one struct, got {:?}", cs.units.iter().map(|u| u.ct).collect::<Vec<_>>());
    match &cs.units[0].delta {
        Delta::Struct { added, removed, modified, .. } => {
            assert_eq!(*modified, 36, "the 36 surviving rows are modified");
            assert_eq!(*removed, 14, "the 14 dropped rows are removed");
            assert_eq!(*added, 0);
        }
        other => panic!("expected enc:struct, got {other:?}"),
    }
}

#[test]
fn cluster_anchors_on_the_container_slot_and_is_deterministic() {
    // The struct unit's slot_key is the TABLE's slot (so seg.fp points at the container), and the
    // whole diff is byte-stable across runs (sorted output, no map-iteration leakage).
    // Salient `Price:` rows → each child is a genuine value move (noise 0.0), so the aggregated
    // cluster noise (the MIN over children) stays low and the change reads as material.
    let before = table_doc(40, |i| format!("Tier {i} Price: $100"));
    let after = table_doc(40, |i| format!("Tier {i} Price: $200"));
    let table_slot = before.blocks.iter().find(|b| b.ty == BlockType::Table).unwrap().slot_key;

    let cs1 = diff(&before, &after, &profile()).unwrap();
    let cs2 = diff(&before, &after, &profile()).unwrap();
    assert_eq!(cs1.units.len(), 1);
    assert_eq!(cs1.units[0].slot_key, table_slot, "the cluster anchors on the table container");
    assert_eq!(cs1.units[0].slot_key, cs2.units[0].slot_key, "deterministic slot");
    // A clustered real-value table change is low noise (= MIN child noise), not damped to none.
    assert!(cs1.units[0].noise_score < 0.5, "a real row-value cluster is low noise, got {}", cs1.units[0].noise_score);
}

#[test]
fn wholly_removed_table_clusters_as_a_removed_struct() {
    // A whole >max_children table disappears: every child is Removed, so the cluster's ct is
    // Removed (not Modified) — the container itself was removed (§6.2).
    let before = table_doc(40, |i| format!("Tier {i} Price: $100"));
    let after = doc(vec![elem("p", &[], vec![text("The pricing table has been retired.")])]);
    let cs = diff(&before, &after, &profile()).unwrap();
    let s = cs
        .units
        .iter()
        .find(|u| matches!(u.delta, Delta::Struct { .. }))
        .expect("a struct cluster for the removed table");
    assert_eq!(s.ct, ChangeType::Removed, "a wholly-removed table is a removed struct");
    match &s.delta {
        Delta::Struct { removed, added, modified, .. } => {
            assert_eq!(*removed, 40);
            assert_eq!(*added, 0);
            assert_eq!(*modified, 0);
        }
        _ => unreachable!(),
    }
}

#[test]
fn wholly_added_table_clusters_as_an_added_struct() {
    // The mirror: a >max_children table appears where there was none → an Added struct.
    let before = doc(vec![elem("p", &[], vec![text("Pricing coming soon.")])]);
    let after = table_doc(40, |i| format!("Tier {i} Price: $100"));
    let cs = diff(&before, &after, &profile()).unwrap();
    let s = cs
        .units
        .iter()
        .find(|u| matches!(u.delta, Delta::Struct { .. }))
        .expect("a struct cluster for the added table");
    assert_eq!(s.ct, ChangeType::Added, "a wholly-added table is an added struct");
    match &s.delta {
        Delta::Struct { removed, added, modified, .. } => {
            assert_eq!(*added, 40);
            assert_eq!(*removed, 0);
            assert_eq!(*modified, 0);
        }
        _ => unreachable!(),
    }
}
