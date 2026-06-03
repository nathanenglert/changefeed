//! End-to-end linchpin integration test (DESIGN §5.3 → §5.6): the cross-module guarantee that
//! "volatile-only inputs produce a byte-identical `doc_hash` and trigger the no-store short-circuit."
//!
//! These tests drive the REAL public pipeline `extract -> normalize -> segment` (no mocks, no
//! `todo!()` stages) over the committed HTML fixtures, asserting the downstream `doc_hash` /
//! `norm_hash` outcomes — exactly the cross-stage check the normalize-module unit tests could not
//! make in isolation (a unit test can only show the `NormalizedDom` is equal, which is necessary but
//! not sufficient; equal `doc_hash` is the sufficient, store-deciding signal).
//!
//! The fixtures are designed for this:
//! - `pricing_before` vs `pricing_noop` differ ONLY by a rotating CSRF nonce + a live viewer count
//!   (both volatile) -> MUST hash identically (no false-positive: §5.3 promise).
//! - `pricing_before` vs `pricing_after` differ by a genuine `$49 -> $59` price change
//!   -> MUST hash differently (no false-negative: real changes are never silently suppressed).

use cf_core::extract::extract;
use cf_core::model::{CanonicalDoc, ExtractStrategy, Profile, RenderMode, SourceMode};
use cf_core::normalize::normalize;
use cf_core::segment::segment;

const BEFORE: &str = include_str!("../../../tests/fixtures/pricing_before.html");
const NOOP: &str = include_str!("../../../tests/fixtures/pricing_noop.html");
const AFTER: &str = include_str!("../../../tests/fixtures/pricing_after.html");

/// A pricing profile: keep the pricing table, treat the live-viewer line as a per-target volatile
/// (the profile-extensible `strip_text` seam, §5.3), and type the `.price` blocks as prices.
fn pricing_profile() -> Profile {
    Profile {
        profile_id: "competitor-pricing".to_string(),
        render: RenderMode::Auto,
        strategy: ExtractStrategy::Selector,
        root_selector: Some("section.PricingTable".to_string()),
        strip_attrs: Vec::new(),
        // The live counter is site-specific volatile chrome -> promote it into the §5.3 pass.
        strip_text: vec![r"\d+\s+viewing right now".to_string()],
        unordered: Vec::new(),
        mode: SourceMode::Page,
        max_pages: 1,
        types: vec![(".price".to_string(), cf_core::model::BlockType::Price)],
        archetype: Some("pricing".to_string()),
        salience_hints: Vec::new(),
    }
}

/// Drive the full public pipeline on raw HTML.
fn observe(html: &str) -> CanonicalDoc {
    let p = pricing_profile();
    let sub = extract(html, &p).expect("extract");
    let dom = normalize(sub, &p).expect("normalize");
    segment(dom, &p).expect("segment")
}

/// Model the §5.6 no-store decision: a new observation stores ONLY if its `doc_hash` differs from
/// the prior. This is the exact branch `run_observation` takes after segment — a pure `doc_hash`
/// compare, independent of the (separately-tested) diff engine.
fn would_store(prior: &CanonicalDoc, new: &CanonicalDoc) -> bool {
    prior.doc_hash != new.doc_hash
}

#[test]
fn volatile_only_delta_yields_identical_doc_hash_and_no_store() {
    let prior = observe(BEFORE);
    let new = observe(NOOP);

    // The downstream sufficiency check the verifier flagged as missing: equal doc_hash.
    assert_eq!(
        prior.doc_hash, new.doc_hash,
        "CSRF-nonce + viewer-count only delta MUST produce an identical doc_hash"
    );
    assert_eq!(prior.doc_hash.to_wire(), new.doc_hash.to_wire());

    // ... which drives the §5.6 no-store short-circuit (zero bytes written).
    assert!(
        !would_store(&prior, &new),
        "a volatile-only observation must hit the no-store short-circuit (§5.6)"
    );
}

#[test]
fn genuine_price_change_yields_different_doc_hash_and_stores() {
    let prior = observe(BEFORE);
    let new = observe(AFTER);

    assert_ne!(
        prior.doc_hash, new.doc_hash,
        "a real $49->$59 price change MUST change the doc_hash (no silent false-negative)"
    );
    assert!(
        would_store(&prior, &new),
        "a real change must NOT be short-circuited — it stores a new snapshot"
    );
}

#[test]
fn the_changed_block_is_localized_to_the_pro_price() {
    // Beyond the doc-level hash: the change is localized. Only the Pro-plan price block's norm_hash
    // moved; every other block (and its slot_key) is identical across the before/after pair. This
    // is what lets the (separately-tested) diff engine emit exactly one `modified` op.
    let prior = observe(BEFORE);
    let after = observe(AFTER);

    let prior_blocks = flatten(&prior);
    let after_blocks = flatten(&after);
    assert_eq!(prior_blocks.len(), after_blocks.len(), "same block count");

    let mut differing = 0usize;
    for (a, b) in prior_blocks.iter().zip(after_blocks.iter()) {
        assert_eq!(a.slot_key, b.slot_key, "slot_key stable for {:?}", a.text);
        if a.norm_hash != b.norm_hash {
            differing += 1;
            assert!(
                a.text.contains("$49/mo") && b.text.contains("$59/mo"),
                "the only differing block must be the Pro price, got {:?} -> {:?}",
                a.text,
                b.text
            );
        }
    }
    assert_eq!(differing, 1, "exactly one block changed");
}

#[test]
fn parsed_price_value_is_exact_and_typed() {
    // The Pro price block is typed `Price` with exact minor units (no f64), so the diff/salience
    // stages can compute a percentage on the structured value, not the string.
    use cf_core::model::{BlockType, TypedValue};
    let before = observe(BEFORE);
    let pro = flatten(&before)
        .into_iter()
        .find(|b| b.text.contains("$49/mo"))
        .expect("Pro price block");
    assert_eq!(pro.ty, BlockType::Price);
    match &pro.value {
        Some(TypedValue::Price { amount_minor, currency, period }) => {
            assert_eq!(*amount_minor, 4900);
            assert_eq!(currency, "USD");
            assert_eq!(period.as_deref(), Some("mo"));
        }
        other => panic!("expected an exact Price value, got {other:?}"),
    }
}

#[test]
fn pipeline_is_deterministic_end_to_end() {
    // The headline determinism contract across all three stages: same input -> same doc_hash, twice
    // in one process (catches HashMap-order / nondeterminism leaks).
    let a = observe(BEFORE);
    let b = observe(BEFORE);
    assert_eq!(a.doc_hash, b.doc_hash);

    let c = observe(AFTER);
    let d = observe(AFTER);
    assert_eq!(c.doc_hash, d.doc_hash);
}

/// Flatten a [`CanonicalDoc`]'s block tree to a pre-order Vec of references.
fn flatten(doc: &CanonicalDoc) -> Vec<&cf_core::model::Block> {
    fn rec<'a>(bs: &'a [cf_core::model::Block], out: &mut Vec<&'a cf_core::model::Block>) {
        for b in bs {
            out.push(b);
            rec(&b.children, out);
        }
    }
    let mut out = Vec::new();
    rec(&doc.blocks, &mut out);
    out
}
