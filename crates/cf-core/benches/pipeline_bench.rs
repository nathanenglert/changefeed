//! Criterion benchmarks for the changefeed MVP hot paths (DESIGN §7.1 / §8.2 / §5.6).
//!
//! Targets (engineering, not hard fails):
//! - `diff` on a ~5000-block page in low-tens of ms on the high-anchor path (DESIGN §7.1);
//! - `classify` (salience) well under 1ms/event (DESIGN §8.2);
//! - the no-op `doc_hash`-equal short-circuit roughly constant time (one hash compare, §5.6);
//! - `normalize` / `segment` on the same synthetic large page;
//! - event serialization to the canonical wire form.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use std::hint::black_box;

use cf_core::diff::{self, Changeset};
use cf_core::model::{
    Action, Base, ChangeEvent, ChangeType, Delta, EventId, ExtractStrategy, FetchTier, Followup,
    Materiality, Profile, Prov, RenderMode, Seg, SourceMode, Src, Why,
};
use cf_core::packs;
use cf_core::pipeline;
use cf_core::salience::{self, PosSource, ScoreInputs};
use cf_core::{extract, normalize, segment};

/// Number of pricing-tier "cards" in the synthetic page. Each card emits ~10 blocks (heading,
/// price, several list items, paragraphs), so ~500 cards ≈ 5000 blocks (the §7.1 target size).
const CARDS: usize = 500;

/// Build a synthetic large pricing page (~5000 blocks). Each card is a self-contained section with
/// a distinct heading (so `slot_key`s are distinct and the page is *high-anchor*: every block
/// anchors cleanly in phase 1, DESIGN §7.1 low-anchor-archetype note).
fn synthetic_page(price_of: impl Fn(usize) -> u32) -> String {
    let mut s = String::with_capacity(CARDS * 400);
    s.push_str("<section class=\"PricingTable\">");
    for i in 0..CARDS {
        let price = price_of(i);
        s.push_str(&format!(
            "<div class=\"card\" id=\"plan-{i}\">\
               <h2>Plan {i}</h2>\
               <p class=\"amount\">${price}/mo</p>\
               <p>Best for teams of size {i} who want predictable billing and support.</p>\
               <ul>\
                 <li>Unlimited projects for plan {i}</li>\
                 <li>Priority support tier {i}</li>\
                 <li>SSO and audit logs included</li>\
                 <li>Up to {seats} seats</li>\
                 <li>API access with {i}00 requests per minute</li>\
               </ul>\
               <p>Cancel anytime, no questions asked for plan {i}.</p>\
             </div>",
            i = i,
            price = price,
            seats = i + 5,
        ));
    }
    s.push_str("</section>");
    s
}

/// The pricing profile used end-to-end (selector strategy scoped to the table; `.amount` → price).
fn pricing_profile() -> Profile {
    Profile {
        profile_id: "bench-pricing".into(),
        render: RenderMode::Auto,
        strategy: ExtractStrategy::Selector,
        root_selector: Some("section.PricingTable".into()),
        strip_attrs: Vec::new(),
        strip_text: Vec::new(),
        unordered: Vec::new(),
        mode: SourceMode::Page,
        max_pages: 1,
        types: vec![(".amount".into(), cf_core::model::BlockType::Price)],
        archetype: Some("pricing".into()),
        salience_hints: Vec::new(),
    }
}

fn canonicalize(html: &str, profile: &Profile) -> cf_core::model::CanonicalDoc {
    pipeline::canonicalize(html, profile).expect("canonicalize")
}

fn bench_normalize(c: &mut Criterion) {
    let profile = pricing_profile();
    let html = synthetic_page(|_| 49);
    // Extract once (extract is not the subject here); normalize each iteration over a fresh subtree.
    let mut group = c.benchmark_group("normalize");
    group.bench_function("normalize_5k_blocks", |b| {
        b.iter_batched(
            || extract::extract(&html, &profile).expect("extract"),
            |subtree| {
                let dom = normalize::normalize(subtree, &profile).expect("normalize");
                black_box(dom);
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_segment(c: &mut Criterion) {
    let profile = pricing_profile();
    let html = synthetic_page(|_| 49);
    let subtree = extract::extract(&html, &profile).expect("extract");
    let mut group = c.benchmark_group("segment");
    group.bench_function("segment_5k_blocks", |b| {
        b.iter_batched(
            || normalize::normalize(subtree.clone(), &profile).expect("normalize"),
            |dom| {
                let doc = segment::segment(dom, &profile).expect("segment");
                black_box(doc);
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_diff_high_anchor(c: &mut Criterion) {
    let profile = pricing_profile();
    // High-anchor: prior vs new differ only in ONE plan's price (a single modified op). Every block
    // anchors by text-free slot_key (§7.1 phase 1), so this exercises the fast spine path.
    let before = canonicalize(&synthetic_page(|_| 49), &profile);
    let after = canonicalize(
        &synthetic_page(|i| if i == CARDS / 2 { 59 } else { 49 }),
        &profile,
    );
    assert_ne!(before.doc_hash, after.doc_hash, "the price change must alter doc_hash");

    let mut group = c.benchmark_group("diff");
    group.bench_function("diff_5k_blocks_high_anchor", |b| {
        b.iter(|| {
            let cs = diff::diff_with_ignores(black_box(&before), black_box(&after), &[])
                .expect("diff");
            black_box(cs);
        });
    });
    group.finish();
}

fn bench_diff_low_anchor(c: &mut Criterion) {
    let profile = pricing_profile();
    // Low-anchor "rename-everything redesign" (DESIGN §7.1 honest worst case): every plan's heading
    // text changes, so a large fraction of slot_keys shift and most blocks fall to the residual /
    // similarity fill. This is the path that MUST stay O(B·b) (window+LSH bounded), NEVER O(B²).
    let before = canonicalize(&synthetic_page(|_| 49), &profile);
    let after_html = {
        // Rename every heading ("Plan i" → "Tier i") so the breadcrumb-derived slot_keys change for
        // the whole subtree, forcing the residual/similarity path for most blocks.
        synthetic_page(|_| 49).replace("<h2>Plan", "<h2>Tier")
    };
    let after = canonicalize(&after_html, &profile);
    assert_ne!(before.doc_hash, after.doc_hash);

    let mut group = c.benchmark_group("diff");
    group.sample_size(20);
    group.bench_function("diff_5k_blocks_low_anchor", |b| {
        b.iter(|| {
            let cs = diff::diff_with_ignores(black_box(&before), black_box(&after), &[])
                .expect("diff");
            black_box(cs);
        });
    });
    group.finish();
}

fn bench_diff_noop_short_circuit(c: &mut Criterion) {
    let profile = pricing_profile();
    // Two byte-identical canonical docs → doc_hash-equal → the §5.6 short-circuit (one hash compare,
    // empty changeset, zero alignment work). This should be ~constant time regardless of block count.
    let a = canonicalize(&synthetic_page(|_| 49), &profile);
    let b = canonicalize(&synthetic_page(|_| 49), &profile);
    assert_eq!(a.doc_hash, b.doc_hash, "identical pages must be doc_hash-equal");

    let mut group = c.benchmark_group("diff_noop");
    group.bench_function("doc_hash_equal_short_circuit", |bn| {
        bn.iter(|| {
            let cs = diff::diff_with_ignores(black_box(&a), black_box(&b), &[]).expect("diff");
            debug_assert!(cs.units.is_empty());
            black_box(cs);
        });
    });
    group.finish();
}

fn bench_classify(c: &mut Criterion) {
    let profile = pricing_profile();
    let pack = packs::resolve(Some("pricing")).expect("pack");
    // A realistic single-change changeset: one price moved (the §10 worked example shape).
    let before = canonicalize(&synthetic_page(|_| 49), &profile);
    let after = canonicalize(
        &synthetic_page(|i| if i == CARDS / 2 { 59 } else { 49 }),
        &profile,
    );
    let changeset: Changeset =
        diff::diff_with_ignores(&before, &after, &[]).expect("diff");
    assert!(!changeset.units.is_empty(), "expected ≥1 change unit to score");
    let inputs = ScoreInputs {
        tier: FetchTier::Http,
        align_rate: 1.0,
        pos_source: PosSource::DomProxy,
    };

    let mut group = c.benchmark_group("classify");
    // Per-event latency: divide the reported time by the unit count (here typically 1).
    group.bench_function("score_doc_single_change", |b| {
        b.iter(|| {
            let r = salience::score_doc(
                black_box(&changeset),
                black_box(&after.blocks),
                after.stats.block_count,
                black_box(&pack),
                &inputs,
            )
            .expect("score");
            black_box(r);
        });
    });
    group.finish();
}

/// A canonical price-change event (mirrors §6.7 Example 1) for the serialization benchmark.
fn example_event() -> ChangeEvent {
    let mut params = serde_json::Map::new();
    params.insert("urgency".into(), serde_json::Value::String("high".into()));
    ChangeEvent {
        v: "1",
        id: EventId::new("cfe_01J9Z4K7QH8M2N3P4R5S6T7V8W".into()),
        src: Src {
            url: "https://acme.com/pricing".into(),
            tid: "acme-pricing".into(),
            title: Some("Pricing — Acme".into()),
        },
        obs: "2026-06-02T14:03:11Z".into(),
        base: Base {
            obs: "2026-05-26T14:01:55Z".into(),
            snap: "blake3:9f2c5d…e1".into(),
            rev: 41,
        },
        seg: vec![Seg {
            anchor: "Pro plan".into(),
            fp: "blake3:4b9c1e".into(),
            label_path: "Pricing › Pro Plan › price".into(),
            role: "price".into(),
        }],
        ct: ChangeType::Modified,
        delta: Delta::Val {
            a: "$39/mo".into(),
            b: "$29/mo".into(),
        },
        why: Why {
            sal: 0.86,
            mat: Materiality::High,
            cat: "price_increase".into(),
            summary: "Pro monthly price rose 34% ($29→$39).".into(),
        },
        followup: Followup {
            act: Action::ReRunDownstream,
            tgt: Some("pricing-watchers".into()),
            params: Some(params),
            q: Some("Did the annual price or feature list change too?".into()),
        },
        conf: 0.97,
        prov: Prov {
            m: FetchTier::Http,
            hash: "blake3:1c8a72…d4".into(),
            etag: Some("W/\"3f1-9aXc\"".into()),
            status: 200,
            ms: None,
            pack: Some("pricing@b3:2f1a".into()),
        },
    }
}

fn bench_event_serialize(c: &mut Criterion) {
    let event = example_event();
    let mut group = c.benchmark_group("event_serialize");
    group.bench_function("to_wire_single_event", |b| {
        b.iter(|| {
            let s = cf_core::event::to_wire(black_box(&event)).expect("to_wire");
            black_box(s);
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_normalize,
    bench_segment,
    bench_diff_high_anchor,
    bench_diff_low_anchor,
    bench_diff_noop_short_circuit,
    bench_classify,
    bench_event_serialize,
);
criterion_main!(benches);
