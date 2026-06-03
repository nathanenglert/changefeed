//! §8 salience scorer unit tests. Cover the required behaviours:
//! * §10 price change → `sal≈0.86` (0.80–0.92), `mat:high`, `cat:price_increase`,
//!   `act:re_run_downstream`, `conf≈0.97` (clean http) under the pricing pack;
//! * a polarity flip ("Supported" → "Deprecated") → `neg=1.0` → high/critical regardless of byte
//!   size;
//! * §6.7 Example-2 `api_breaking` per_page 1000→100 under the api-docs pack → `mat:critical` +
//!   an `open_ticket`-eligible action;
//! * a status-page "investigating" line → the sticky `incident.open` rule → `act:page_oncall`;
//! * determinism: an identical changeset in → identical `sal`/`mat`/`cat`/`act`/`conf` twice;
//! * the §6.6 conf formula directly (headless lowers `c_fetch` to 0.85);
//! * the §8.5 explanation lists `top_signals` with num/type/pos contributions.

use super::*;
use crate::diff::diff;
use crate::extract::{DomSubtree, ExtractNode};
use crate::model::{
    Action, AnchorScheme, BlockType, CanonicalDoc, ChangeType, Delta, FetchTier, Materiality,
};
use crate::normalize::normalize;
use crate::packs;
use crate::segment::segment;
use crate::salience::confidence::{confidence, ConfFactors};

// ===========================================================================================
// Test helpers — build canonical docs through the real normalize + segment stages.
// ===========================================================================================

fn profile() -> crate::model::Profile {
    crate::model::Profile {
        profile_id: "test".to_string(),
        render: crate::model::RenderMode::Auto,
        strategy: crate::model::ExtractStrategy::Full,
        root_selector: None,
        strip_attrs: Vec::new(),
        strip_text: Vec::new(),
        unordered: Vec::new(),
        mode: crate::model::SourceMode::Page,
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

fn doc(roots: Vec<ExtractNode>) -> CanonicalDoc {
    let p = profile();
    let dom = normalize(
        DomSubtree {
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

fn pricing_pack() -> packs::Pack {
    packs::layer(&[packs::DEFAULT_TOML, packs::PRICING_TOML]).unwrap()
}
fn api_docs_pack() -> packs::Pack {
    packs::layer(&[packs::DEFAULT_TOML, packs::API_DOCS_TOML]).unwrap()
}
fn status_pack() -> packs::Pack {
    packs::layer(&[packs::DEFAULT_TOML, packs::STATUS_PAGE_TOML]).unwrap()
}

/// Score a before/after doc pair under a pack, returning the per-event scores.
fn score_pair(before: &CanonicalDoc, after: &CanonicalDoc, pack: &packs::Pack) -> Vec<ScoredEvent> {
    let cs = diff(before, after, &profile()).unwrap();
    score_doc(&cs, &after.blocks, after.stats.block_count, pack, &ScoreInputs::default())
        .unwrap()
        .events
}

/// Find the scored event whose delta touches `needle` (in either side).
fn find_event<'a>(events: &'a [ScoredEvent], needle: &str) -> Option<&'a ScoredEvent> {
    events.iter().find(|e| match &e.delta {
        Delta::Val { a, b } => a.contains(needle) || b.contains(needle),
        Delta::Idiff { ops } => ops.iter().any(|o| o.text.contains(needle)),
        Delta::Block { a, b, .. } => {
            a.contains(needle) || b.as_deref().is_some_and(|s| s.contains(needle))
        }
        _ => false,
    })
}

// A realistic pricing page: the pricing table sits near the TOP (shallow + early in pre-order) and
// the page carries a deeply-nested footer that raises `max_depth` and the total `block_count`, so
// the Pro price cell's Tier-1 `pos` proxy is high (≈0.85, as in the §10 worked example) rather than
// the artificially-low value a flat 6-block document would yield. The Pro price cell carries an
// explicit `id` anchor so it aligns by anchor scheme (c_align = 1.0) and `conf` reproduces the
// §6.7/§10 documented 0.97.
fn pricing_doc(pro_price: &str) -> CanonicalDoc {
    // Deep footer (nested sections) — increases max_depth + block_count, pushing pos for the
    // early/shallow price cell toward the §10 figure. Pure structure, no value-typed blocks.
    let deep_footer = elem(
        "footer",
        &[],
        vec![elem(
            "section",
            &[],
            vec![elem(
                "div",
                &[],
                vec![elem(
                    "div",
                    &[],
                    vec![elem(
                        "ul",
                        &[],
                        vec![
                            elem("li", &[], vec![text("Company information and about us page.")]),
                            elem("li", &[], vec![text("Careers and open positions at the company.")]),
                            elem("li", &[], vec![text("Terms of service and the privacy policy.")]),
                            elem("li", &[], vec![text("Contact support and the help center.")]),
                            elem("li", &[], vec![text("Developer documentation and API guides.")]),
                            elem("li", &[], vec![text("Blog posts and company announcements.")]),
                        ],
                    )],
                )],
            )],
        )],
    );
    doc(vec![elem(
        "main",
        &[],
        vec![
            elem(
                "section",
                &[("class", "pricing")],
                vec![
                    elem("h2", &[], vec![text("Pro Plan")]),
                    elem("p", &[("id", "pro-price"), ("class", "amount")], vec![text(pro_price)]),
                    elem("h2", &[], vec![text("Starter Plan")]),
                    elem("p", &[("class", "amount")], vec![text("$19/mo")]),
                    elem("h2", &[], vec![text("Enterprise Plan")]),
                    elem("p", &[("class", "amount")], vec![text("$199/mo")]),
                ],
            ),
            deep_footer,
        ],
    )])
}

// ===========================================================================================
// §10 price change.
// ===========================================================================================

#[test]
fn price_change_scores_high_under_pricing_pack() {
    let before = pricing_doc("$49/mo");
    let after = pricing_doc("$59/mo");
    let events = score_pair(&before, &after, &pricing_pack());

    assert_eq!(events.len(), 1, "exactly one scored event, got {events:?}");
    let e = &events[0];

    assert_eq!(e.ct, ChangeType::Modified);
    assert!(
        (0.80..=0.92).contains(&e.sal),
        "§10 price change sal ≈ 0.86, got {}",
        e.sal
    );
    assert_eq!(e.mat, Materiality::High, "sal {} bands to high", e.sal);
    assert_eq!(e.cat, "price_increase");
    assert_eq!(e.act, Action::ReRunDownstream, "pricing pack maps price_increase → re_run_downstream");
    assert!(
        (e.conf - 0.97).abs() < 0.005,
        "clean http explicit-anchor price change conf ≈ 0.97, got {}",
        e.conf
    );
}

#[test]
fn price_decrease_carries_direction_in_cat() {
    let before = pricing_doc("$59/mo");
    let after = pricing_doc("$49/mo");
    let events = score_pair(&before, &after, &pricing_pack());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cat, "price_decrease", "direction flows to the category");
    assert_eq!(events[0].act, Action::ReRunDownstream);
}

// ===========================================================================================
// §7.3/§7.4 — noise is a SOFT NEGATIVE feature to salience. An un-typed "N viewing" live counter
// must be damped to a low/none band (NOT emitted as material), while the co-located real price
// change stays material. This guards the salience↔noise_score wiring end-to-end: the isolated
// diff::noise unit test asserts noise_score≈0.85 but does NOT prove the SCORE consumes it.
// ===========================================================================================

fn pricing_doc_with_counter(pro_price: &str, viewers: &str) -> CanonicalDoc {
    doc(vec![elem(
        "main",
        &[],
        vec![elem(
            "section",
            &[("class", "pricing")],
            vec![
                elem("h2", &[], vec![text("Pro Plan")]),
                elem("p", &[("id", "pro-price"), ("class", "amount")], vec![text(pro_price)]),
                elem("p", &[("class", "live")], vec![text(&format!("{viewers} viewing right now"))]),
                elem("h2", &[], vec![text("Starter Plan")]),
                elem("p", &[("class", "amount")], vec![text("$19/mo")]),
            ],
        )],
    )])
}

#[test]
fn viewing_counter_is_damped_while_price_stays_material() {
    let before = pricing_doc_with_counter("$49/mo", "123");
    let after = pricing_doc_with_counter("$59/mo", "127");
    let events = score_pair(&before, &after, &pricing_pack());

    // The real price change MUST NOT be damped (a salient-numeric value change has noise_score 0.0).
    let price = find_event(&events, "$59/mo").expect("the price change is scored");
    assert!(
        matches!(price.mat, Materiality::High | Materiality::Critical),
        "a real price change stays material, got {:?} (sal {})",
        price.mat,
        price.sal,
    );
    assert_eq!(price.explanation.damped_by_noise, 0.0, "a real price change is never noise-damped");

    // The "N viewing right now" counter is the §7.3 volatile-counter case (noise_score≈0.85): the
    // soft negative feature must pull it BELOW medium (§7.4: a low-mat event an agent filters with
    // --min-salience medium) — NEVER high. Before the fix this scored High (noise_score discarded).
    let counter = find_event(&events, "viewing right now").expect("the counter change is scored");
    assert!(
        matches!(counter.mat, Materiality::None | Materiality::Low),
        "a 'viewing' counter is damped to low/none, got {:?} (sal {})",
        counter.mat,
        counter.sal,
    );
    assert!(
        counter.explanation.damped_by_noise >= 0.8,
        "the counter records a high noise damping in cf explain, got {}",
        counter.explanation.damped_by_noise,
    );
}

// ===========================================================================================
// Polarity flip — neg=1.0 drives high/critical regardless of byte size.
// ===========================================================================================

#[test]
fn polarity_flip_supported_to_deprecated_is_decisive() {
    // A tiny one-word edit ("Supported" → "Deprecated"): byte-size is small, but the polarity flip
    // must drive materiality to at least High via neg=1.0 (a_neg=1.0).
    let before = doc(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Webhooks v1")]),
            elem("p", &[], vec![text("Status: Supported")]),
        ],
    )]);
    let after = doc(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Webhooks v1")]),
            elem("p", &[], vec![text("Status: Deprecated")]),
        ],
    )]);

    // Under the plain default pack (no archetype rules), neg alone must carry it.
    let default_pack = packs::parse(packs::DEFAULT_TOML).unwrap();
    let events = score_pair(&before, &after, &default_pack);
    let e = find_event(&events, "Deprecated").expect("the deprecation edit is scored");

    // neg=1.0 with a_neg=1.0 means raw_block reaches 1.0 → critical (regardless of small byte size).
    assert!(
        matches!(e.mat, Materiality::High | Materiality::Critical),
        "a polarity flip is at least High, got {:?} (sal {})",
        e.mat,
        e.sal
    );
    assert!(e.sal >= 0.90, "neg=1.0 drives sal to the top band, got {}", e.sal);
    assert_eq!(e.cat, "api_deprecation", "a 'Deprecated' flip categorizes as api_deprecation");
}

#[test]
fn neg_signal_is_one_for_an_introduced_negation() {
    // Directly exercise the neg signal: an inserted "no longer" flips polarity.
    let before = doc(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Feature")]),
            elem("p", &[], vec![text("This endpoint is available to all customers today.")]),
        ],
    )]);
    let after = doc(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Feature")]),
            elem("p", &[], vec![text("This endpoint is no longer available to customers.")]),
        ],
    )]);
    let cs = diff(&before, &after, &profile()).unwrap();
    let unit = cs
        .units
        .iter()
        .find(|u| matches!(&u.delta, Delta::Idiff { .. }))
        .expect("a prose idiff for the negation edit");
    let pack = packs::parse(packs::DEFAULT_TOML).unwrap();
    let pos = PosInput { dom_depth: 1, max_depth: 2, preorder_idx: 1, block_count: 3 };
    let (sig, _) = signals::signals_for(unit, &pack, pos, PosSource::DomProxy);
    assert_eq!(sig.neg, 1.0, "an introduced 'no longer' sets neg=1.0");
}

// ===========================================================================================
// §6.7 Example-2 — api_breaking per_page 1000→100 under the api-docs pack.
// ===========================================================================================

#[test]
fn api_breaking_per_page_cut_is_critical_and_open_ticket_eligible() {
    let before = doc(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Parameters")]),
            elem("p", &[], vec![text("Default 100, max 1000 per page.")]),
        ],
    )]);
    let after = doc(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("Parameters")]),
            // The phrase "max 100 per page" trips the api.breaking sticky regex (max … per_page).
            elem("p", &[], vec![text("Default 25, max 100 per page. Larger values now error.")]),
        ],
    )]);

    let events = score_pair(&before, &after, &api_docs_pack());
    let e = find_event(&events, "per page").expect("the per_page edit is scored");
    assert_eq!(e.cat, "api_breaking", "the api.breaking rule forces api_breaking");
    assert_eq!(e.mat, Materiality::Critical, "api_breaking bands to critical under api-docs");
    assert_eq!(e.act, Action::OpenTicket, "an api_breaking change is open_ticket-eligible (§6.7)");
}

// ===========================================================================================
// Status-page sticky incident.open rule → page_oncall.
// ===========================================================================================

#[test]
fn status_investigating_hits_sticky_incident_open_rule() {
    let before = doc(vec![elem(
        "section",
        &[],
        vec![elem("h2", &[], vec![text("System Status")]), elem("p", &[], vec![text("All systems operational.")])],
    )]);
    let after = doc(vec![elem(
        "section",
        &[],
        vec![
            elem("h2", &[], vec![text("System Status")]),
            elem("p", &[], vec![text("All systems operational.")]),
            elem(
                "p",
                &[],
                vec![text("Investigating — elevated API error rates in us-east-1.")],
            ),
        ],
    )]);

    let events = score_pair(&before, &after, &status_pack());
    let e = find_event(&events, "Investigating").expect("the new incident line is scored");
    assert_eq!(e.act, Action::PageOncall, "the sticky incident.open rule pages on-call");
    assert_eq!(e.cat, "incident_open");
    // A sticky rule decides the action regardless of banding.
    assert_eq!(e.explanation.decided_by, "rule", "a sticky rule decides the action");
    assert!(
        e.explanation.matched_rules.iter().any(|r| r == "incident.open"),
        "the explanation records the incident.open rule, got {:?}",
        e.explanation.matched_rules
    );
}

#[test]
fn sticky_action_bypasses_low_band() {
    // Even if the score banded low, a sticky rule still forces page_oncall (§8.4 step 1).
    let pack = status_pack();
    let decision = action::action_for(
        "we are investigating reports of 5xx errors",
        Materiality::Low,
        &pack,
    );
    assert_eq!(decision.action, Action::PageOncall);
    assert_eq!(decision.decided_by, action::DecidedBy::Rule);
}

#[test]
fn none_band_unit_ignores_cat_row_action() {
    // §8.4 / §6.4: a `none`-band unit is below the noise floor — a noise-damped live counter (§7.3)
    // or a sub-threshold cosmetic edit. The band map says `none→ignore`, and a category routing row
    // must NOT rescue it to an actionable verb (otherwise a zero-token no-op would carry `notify`/
    // `open_ticket`). A *material* unit in the same category still takes the cat-row action.
    let mut pack = packs::Pack::default();
    pack.cat_rules.push(packs::CatRule {
        cat: "live_counter".into(),
        mat: Materiality::Low,
        act: Action::OpenTicket,
    });

    // A none-band (noise-floored) unit → `ignore`, decided by the band map (not the cat row).
    let damped =
        action::action_for_cat("123 viewing right now", "live_counter", Materiality::None, &pack);
    assert_eq!(
        damped.action,
        Action::Ignore,
        "a none-band unit defaults to ignore, not the cat-row action"
    );
    assert_eq!(damped.decided_by, action::DecidedBy::Band);

    // A material unit in the SAME category → the cat-row action still applies (regression guard:
    // the fix must not break the documented §8.3 cat-row override for real changes).
    let material =
        action::action_for_cat("legacy plan removed", "live_counter", Materiality::Medium, &pack);
    assert_eq!(
        material.action,
        Action::OpenTicket,
        "a material unit still takes the cat-row action"
    );
    assert_eq!(material.decided_by, action::DecidedBy::Rule);
}

// ===========================================================================================
// Determinism — identical changeset in → identical scores out across two runs.
// ===========================================================================================

#[test]
fn scoring_is_deterministic_across_two_runs() {
    let before = pricing_doc("$49/mo");
    let after = pricing_doc("$59/mo");
    let pack = pricing_pack();

    let run1 = score_pair(&before, &after, &pack);
    let run2 = score_pair(&before, &after, &pack);

    assert_eq!(run1.len(), run2.len());
    for (a, b) in run1.iter().zip(run2.iter()) {
        assert_eq!(a.sal, b.sal, "sal identical across runs");
        assert_eq!(a.mat, b.mat, "mat identical across runs");
        assert_eq!(a.cat, b.cat, "cat identical across runs");
        assert_eq!(a.act, b.act, "act identical across runs");
        assert_eq!(a.conf, b.conf, "conf identical across runs");
    }
}

#[test]
fn pack_content_hash_is_deterministic() {
    let a = pricing_pack();
    let b = pricing_pack();
    assert_eq!(a.content_hash, b.content_hash);
    assert!(a.content_hash.starts_with("blake3:"));
    assert!(a.prov_stamp().starts_with("pricing@b3:"), "prov stamp {}", a.prov_stamp());
}

// ===========================================================================================
// §6.6 confidence formula — direct unit tests.
// ===========================================================================================

#[test]
fn conf_formula_is_the_product_of_five_factors() {
    let f = ConfFactors {
        c_fetch: 1.0,
        c_align: 1.0,
        c_match: 1.0,
        c_parse: 0.97,
        c_stability: 1.0,
    };
    assert!((confidence(&f) - 0.97).abs() < 1e-6, "1·1·1·0.97·1 = 0.97");

    let g = ConfFactors {
        c_fetch: 0.85, // headless
        c_align: 0.9,  // struct slot
        c_match: 1.0,
        c_parse: 1.0,
        c_stability: 0.6, // suspected redesign
    };
    let expected = 0.85 * 0.9 * 1.0 * 1.0 * 0.6;
    assert!((confidence(&g) - expected).abs() < 1e-6, "product of five factors");
}

#[test]
fn headless_tier_lowers_c_fetch_to_0_85() {
    assert_eq!(confidence::c_fetch(FetchTier::Http), 1.0);
    assert_eq!(confidence::c_fetch(FetchTier::Api), 1.0);
    assert_eq!(confidence::c_fetch(FetchTier::Rss), 0.95);
    assert_eq!(confidence::c_fetch(FetchTier::Headless), 0.85, "a render is inherently less certain");
}

#[test]
fn c_stability_drops_to_0_6_on_low_align_rate() {
    assert_eq!(confidence::c_stability(1.0), 1.0);
    assert_eq!(confidence::c_stability(0.95), 1.0);
    assert_eq!(confidence::c_stability(0.85), 0.6, "align_rate < 0.9 ⇒ suspected redesign");
}

#[test]
fn c_align_distinguishes_anchor_struct_and_similarity() {
    // Build a real anchored price unit (sim == 1.0) and check both anchor schemes + a similarity
    // pair via the c_align helper on the unit's sim.
    let before = pricing_doc("$49/mo");
    let after = pricing_doc("$59/mo");
    let cs = diff(&before, &after, &profile()).unwrap();
    let unit = &cs.units[0];
    assert!((unit.sim - 1.0).abs() < 1e-6, "an anchored price cell has sim 1.0");
    assert_eq!(confidence::c_align(unit, AnchorScheme::Anchor), 1.0);
    assert_eq!(confidence::c_align(unit, AnchorScheme::Struct), 0.9);
}

#[test]
fn added_and_removed_blocks_get_clean_c_align_not_zero_conf() {
    // §6.6 / §6.7 Ex.3: an added/removed block is a CLEAN structural op (its `sim` is 0.0 by
    // construction — there is no counterpart to score similarity against, NOT a poor match). So
    // c_align must be the clean anchor/struct value, never the 0.0 sim. Regression: c_align used to
    // return sim, zeroing `conf` for EVERY added/removed event (a new incident, a removed plan, an
    // API addition) — exactly the high-value changes an agent gating on `conf > 0.6` would discard.
    let mut blocks = vec![elem("h2", &[], vec![text("Updates")])];
    for i in 0..10 {
        blocks.push(elem("p", &[], vec![text(&format!("Stable paragraph number {i} with several words of content."))]));
    }
    let before = doc(vec![elem("main", &[], blocks.clone())]);
    let mut after_blocks = blocks;
    after_blocks.push(elem("p", &[], vec![text("A brand new final paragraph just appeared today with real content.")]));
    let after = doc(vec![elem("main", &[], after_blocks)]);

    let cs = diff(&before, &after, &profile()).unwrap();
    let added = cs.units.iter().find(|u| u.ct == ChangeType::Added).expect("an added unit");
    assert!((added.sim - 0.0).abs() < 1e-6, "an added block carries sim 0.0 by construction");
    assert_eq!(confidence::c_align(added, AnchorScheme::Struct), 0.9, "added → clean struct align, not 0.0");
    assert_eq!(confidence::c_align(added, AnchorScheme::Anchor), 1.0, "added under an explicit anchor → 1.0");

    // End-to-end: the scored added event's conf is high (the bug made it exactly 0.0).
    let events = score_pair(&before, &after, &packs::Pack::default());
    let ev = events.iter().find(|e| e.ct == ChangeType::Added).expect("a scored added event");
    assert!(ev.conf > 0.6, "an added block's conf must be high, got {} (regression: was 0.0)", ev.conf);
}

// ===========================================================================================
// §8.5 explanation breakdown.
// ===========================================================================================

#[test]
fn explanation_lists_top_signals_with_num_type_pos() {
    let before = pricing_doc("$49/mo");
    let after = pricing_doc("$59/mo");
    let events = score_pair(&before, &after, &pricing_pack());
    let e = &events[0];

    let names: Vec<&str> = e.explanation.top_signals.iter().map(|s| s.signal).collect();
    assert!(names.contains(&"num"), "num is a top signal, got {names:?}");
    assert!(names.contains(&"type"), "type is a top signal, got {names:?}");
    assert!(names.contains(&"pos"), "pos is a top signal, got {names:?}");

    // num row carries the directional detail "49→59 (+20%)".
    let num_row = e.explanation.top_signals.iter().find(|s| s.signal == "num").unwrap();
    let detail = num_row.detail.as_deref().unwrap_or("");
    assert!(detail.contains("49") && detail.contains("59") && detail.contains('+'), "num detail {detail:?}");

    // type contribution = a_type · w_type = 0.6 · 1.00 = 0.60.
    let type_row = e.explanation.top_signals.iter().find(|s| s.signal == "type").unwrap();
    assert!((type_row.value - 1.0).abs() < 1e-2, "price block_type weight 1.00");
    assert!((type_row.contribution - 0.60).abs() < 1e-2, "a_type·w_type = 0.60");

    // pos used the Tier-1 dom proxy.
    let pos_row = e.explanation.top_signals.iter().find(|s| s.signal == "pos").unwrap();
    assert_eq!(pos_row.detail.as_deref(), Some("proxy:dom_depth (Tier-1)"));

    // Sorted descending by contribution.
    let contribs: Vec<f32> = e.explanation.top_signals.iter().map(|s| s.contribution).collect();
    let mut sorted = contribs.clone();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap());
    assert_eq!(contribs, sorted, "top_signals sorted by descending contribution");

    assert!(!e.explanation.non_reproducible, "MVP score uses no clock → reproducible");
    assert_eq!(e.explanation.damped_by_volatility, 0.0, "w_vol=1.0 in MVP ⇒ no damping");
    assert_eq!(e.explanation.decided_by, "rule", "the price_increase cat-row action decides it");
}

// ===========================================================================================
// noisy-OR + bands + pack parsing sanity.
// ===========================================================================================

#[test]
fn noisy_or_is_disjunctive_and_monotone() {
    let lo = Signals { ty: 0.2, ..Default::default() };
    let hi = Signals { ty: 0.2, neg: 1.0, ..Default::default() };
    let aff = combine::default_affinities();
    let s_lo = combine::noisy_or(&lo, &aff);
    let s_hi = combine::noisy_or(&hi, &aff);
    assert!(s_hi > s_lo, "adding a decisive signal raises the score");
    assert!((s_hi - 1.0).abs() < 1e-6, "neg=1.0 with a_neg=1.0 saturates the noisy-OR");
}

#[test]
fn bands_use_default_cutoffs() {
    assert_eq!(combine::band(0.95), Materiality::Critical);
    assert_eq!(combine::band(0.80), Materiality::High);
    assert_eq!(combine::band(0.50), Materiality::Medium);
    assert_eq!(combine::band(0.20), Materiality::Low);
    assert_eq!(combine::band(0.10), Materiality::None);
}

#[test]
fn page_score_aggregates_top_k() {
    // Five blocks at 0.3 each aggregate well above any single one (disjunctive page roll-up).
    let scores = vec![0.3, 0.3, 0.3, 0.3, 0.3, 0.3];
    let ps = combine::page_score(&scores);
    assert!(ps > 0.3, "page roll-up exceeds a single block, got {ps}");
    // Only the top-k=5 count: a 6th 0.3 cannot push it past the 5-block roll-up of 0.3s.
    let five = combine::page_score(&[0.3, 0.3, 0.3, 0.3, 0.3]);
    assert!((ps - five).abs() < 1e-6, "only top_k=5 blocks count");
}

#[test]
fn pack_layering_is_last_wins() {
    // The pricing pack overrides the price block_type weight; default still supplies the rest.
    let p = pricing_pack();
    assert_eq!(p.id, "pricing");
    assert_eq!(p.parent.as_deref(), Some("default"));
    assert!((p.block_type_weight(BlockType::Price) - 1.0).abs() < 1e-6);
    assert!((p.block_type_weight(BlockType::TableRow) - 0.90).abs() < 1e-6, "pricing overrides table_row");
    // A type the pricing pack does NOT override falls back to the default table.
    assert!((p.block_type_weight(BlockType::Heading) - 0.80).abs() < 1e-6);
}

#[test]
fn all_shipped_packs_parse() {
    for src in [packs::DEFAULT_TOML, packs::PRICING_TOML, packs::API_DOCS_TOML, packs::STATUS_PAGE_TOML] {
        let p = packs::parse(src).expect("shipped pack parses");
        assert!(!p.id.is_empty());
    }
    // And the archetype resolver layers default → archetype.
    for arch in ["pricing", "api-docs", "status-page"] {
        let p = packs::resolve(Some(arch)).unwrap();
        assert_eq!(p.id, arch);
    }
    // An unknown archetype falls back to default.
    assert_eq!(packs::resolve(Some("nonexistent")).unwrap().id, "default");
}

#[test]
fn verify_llm_is_never_emitted() {
    // Exhaustively scoring every shipped pack over a battery of edits must never yield an action
    // outside the 8-value taxonomy (the type system guarantees it; this asserts no panic + the
    // inert LLM gate leaves a real action). The closed enum makes `verify_llm` unrepresentable.
    let before = pricing_doc("$49/mo");
    let after = pricing_doc("$59/mo");
    for pack in [pricing_pack(), api_docs_pack(), status_pack()] {
        for e in score_pair(&before, &after, &pack) {
            // Every action is one of the closed 8 (a default-less match would fail to compile if not).
            let _: &'static str = match e.act {
                Action::Ignore => "ignore",
                Action::Notify => "notify",
                Action::RefetchLinked => "refetch_linked",
                Action::ReembedKb => "reembed_kb",
                Action::ReRunDownstream => "re_run_downstream",
                Action::OpenTicket => "open_ticket",
                Action::EscalateHuman => "escalate_human",
                Action::PageOncall => "page_oncall",
            };
        }
    }
}

#[test]
fn date_shift_signal_scales_with_shift_size_no_clock() {
    // The date signal is magnitude-only: a 60-day shift scores higher than a 1-day shift, with NO
    // reference to the current clock (fully reproducible offline).
    let before = doc(vec![elem(
        "section",
        &[],
        vec![elem("h2", &[], vec![text("Deadline")]), elem("p", &[], vec![text("2026-01-01")])],
    )]);
    let after_small = doc(vec![elem(
        "section",
        &[],
        vec![elem("h2", &[], vec![text("Deadline")]), elem("p", &[], vec![text("2026-01-02")])],
    )]);
    let after_big = doc(vec![elem(
        "section",
        &[],
        vec![elem("h2", &[], vec![text("Deadline")]), elem("p", &[], vec![text("2026-03-02")])],
    )]);

    let pack = packs::parse(packs::DEFAULT_TOML).unwrap();
    let pos = PosInput { dom_depth: 1, max_depth: 2, preorder_idx: 1, block_count: 3 };

    let cs_small = diff(&before, &after_small, &profile()).unwrap();
    let cs_big = diff(&before, &after_big, &profile()).unwrap();
    let u_small = &cs_small.units[0];
    let u_big = &cs_big.units[0];
    let (sig_small, _) = signals::signals_for(u_small, &pack, pos, PosSource::DomProxy);
    let (sig_big, _) = signals::signals_for(u_big, &pack, pos, PosSource::DomProxy);

    assert!(sig_big.date > sig_small.date, "a 60-day shift scores higher than a 1-day shift");
    assert!(sig_small.date > 0.0, "any date shift registers");
}

