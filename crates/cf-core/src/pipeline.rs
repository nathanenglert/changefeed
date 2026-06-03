//! Pipeline orchestration (ARCHITECTURE.md §1, §4): the linear sequence of pure transforms
//! `extract → normalize → segment → diff → classify(salience) → event`, with the two short-circuits
//! (HTTP 304 in fetch, `doc_hash`-equal after segment) modeled as early-return `ObservationResult`
//! variants so the cheap no-op path is structurally zero-write.
//!
//! DETERMINISM: nothing here reads the clock or uses randomness. The `obs` timestamp and the
//! `cfe_`/`cfb_` ids are INJECTED as data (`PipelineCtx`), so the same two snapshots produce
//! byte-identical events modulo the injected `id`/`obs` (ARCHITECTURE §4 headline).

use crate::diff::{self, ChangeUnit, Changeset};
use crate::event;
use crate::extract;
use crate::model::{
    Base, Block, BlockType, CanonicalDoc, ChangeEvent, Delta, EventId, FetchTier, Followup,
    IgnoreRule, Materiality, ObservationResult, Profile, Prov, Seg, Src, TypedValue,
};
use crate::normalize;
use crate::packs::Pack;
use crate::salience::{self, PosSource, ScoreInputs, ScoredEvent};
use crate::segment;
use crate::CfError;
use rustc_hash::FxHashMap;

/// The boundary-injected data needed to assemble wire events for one observation. Built by the cli;
/// nothing here reads a clock or mints ids — those values are passed in.
pub struct PipelineCtx<'a> {
    /// Target id (`src.tid`), folded into the idempotency `event_key`.
    pub tid: &'a str,
    /// The page title, if known.
    pub title: Option<&'a str>,
    /// RFC3339 observation timestamp (injected).
    pub obs: &'a str,
    /// `cfb_<ULID>` batch id (injected).
    pub batch_id: &'a str,
    /// A source of `cfe_<ULID>` event ids (injected; tests pass a stepped sequence).
    pub event_ids: &'a mut dyn FnMut() -> String,
    /// Prior stored snapshot for this target, if any.
    pub prior: Option<&'a CanonicalDoc>,
    /// Prior revision (the baseline `from_rev`), if any.
    pub prev_rev: Option<u64>,
    /// The revision the new snapshot would take on a real change (= `prev_rev`+1 or 0).
    pub to_rev: u64,
    /// The rule pack (`why.cat`/`mat`/`act`/sal weights, `prov.pack`).
    pub pack: &'a Pack,
    /// The target's `--min-salience` threshold (gating).
    pub min_salience: Materiality,
    /// Emit a sub-threshold envelope (exit 12) instead of collapsing to no-change.
    pub emit_subthreshold: bool,
    /// The typed ignore-rule list (selector/attr/regex masking).
    pub ignore: &'a [IgnoreRule],
    /// Wire fetch metadata for `crawl`/`prov`.
    pub status: u16,
    pub etag: Option<String>,
    pub ms: Option<u32>,
}

/// Parse + canonicalize a fetched HTML body into a `CanonicalDoc` (extract → normalize → segment).
/// The cli fills `url`/`final_url`/`fetched_at`/`fetch` afterwards (they are EXCLUDED from
/// `doc_hash`, so this pure step does not need them).
pub fn canonicalize(html: &str, profile: &Profile) -> Result<CanonicalDoc, CfError> {
    let subtree = extract::extract(html, profile)?;
    let normalized = normalize::normalize(subtree, profile)?;
    segment::segment(normalized, profile)
}

/// Diff a prior canonical doc against the new one and score the resulting changeset against the
/// pack. Returns the scored events in deterministic order plus the doc-level alignment rate.
pub fn diff_and_score(
    prior: &CanonicalDoc,
    new: &CanonicalDoc,
    ignore: &[IgnoreRule],
    pack: &Pack,
    tier: FetchTier,
) -> Result<(Changeset, Vec<ScoredEvent>), CfError> {
    let changeset = diff::diff_with_ignores(prior, new, ignore)?;
    let align_rate = alignment_rate(prior, new, &changeset);
    let inputs = ScoreInputs {
        tier,
        align_rate,
        pos_source: PosSource::DomProxy,
    };
    let scored = salience::score_doc(
        &changeset,
        &new.blocks,
        new.stats.block_count,
        pack,
        &inputs,
    )?;
    Ok((changeset, scored.events))
}

/// The full pipeline over a fetched body (the `cf check` 200-class path). Runs
/// canonicalize → diff → score → event-build and returns the §4.5 [`ObservationResult`]:
/// * no prior → [`ObservationResult::Baseline`] (exit 11);
/// * `doc_hash`-equal or zero scored units → [`ObservationResult::NoChange`] (exit 0);
/// * all units below `--min-salience` (without `--emit-subthreshold`) → no-change (exit 0);
///   with the flag → sub-threshold no-change reason (exit 12);
/// * ≥1 material unit → [`ObservationResult::Changed`] (exit 10).
pub fn observe_body(new: &CanonicalDoc, ctx: &mut PipelineCtx<'_>) -> ObservationResult {
    // First observation: no prior to diff -> store a baseline (§6.8, exit 11).
    let Some(prior) = ctx.prior else {
        let env = event::baseline_envelope(
            ctx.tid,
            ctx.batch_id,
            ctx.obs,
            ctx.to_rev,
            Some(new.url.clone()),
            FetchTier::Http,
            ctx.status,
            ctx.ms,
            Some(new.doc_hash.to_wire()),
        );
        return ObservationResult::Baseline(env);
    };

    let (changeset, scored) =
        match diff_and_score(prior, new, ctx.ignore, ctx.pack, FetchTier::Http) {
            Ok(v) => v,
            Err(e) => return ObservationResult::FetchError(e),
        };

    // doc_hash-equal (or no aligned change) -> the cheap no-op (§5.6, exit 0). Zero writes.
    if scored.is_empty() {
        return ObservationResult::NoChange {
            reason: crate::model::NoChangeReason::DocHashEqual,
        };
    }

    // §4.9 / §11 redesign guard. A change exists; if the new observation shares almost none of the
    // prior's stable `slot_key`s (a whole-page redesign, or a selector now matching a DIFFERENT
    // subtree), the alignment is unreliable and the normal path would emit a FLOOD of confident-but-
    // wrong added/removed ops. Emit ONE low-conf, high-`mat` `content_edit` flagged for operator
    // review and suppress the garbage diff (§4.9 line 406).
    if let Some(guard) = redesign_guard_event(prior, new, ctx) {
        let env = event::changed_envelope(
            ctx.tid,
            ctx.batch_id,
            ctx.obs,
            ctx.prev_rev.unwrap_or(0),
            ctx.to_rev,
            Some(new.url.clone()),
            FetchTier::Http,
            ctx.status,
            ctx.ms,
            Some(new.doc_hash.to_wire()),
            vec![guard.clone()],
        );
        return ObservationResult::Changed {
            envelope: env,
            events: vec![guard],
        };
    }

    // §4.4 / §6.4 salience gating: keep only units at/above --min-salience.
    let material: Vec<&ScoredEvent> = scored
        .iter()
        .filter(|e| e.mat >= ctx.min_salience)
        .collect();

    if material.is_empty() {
        // Every change is below the threshold.
        if ctx.emit_subthreshold {
            return ObservationResult::NoChange {
                reason: crate::model::NoChangeReason::SubThreshold,
            };
        }
        return ObservationResult::NoChange {
            reason: crate::model::NoChangeReason::DocHashEqual,
        };
    }

    let from_rev = ctx.prev_rev.unwrap_or(0);
    let segs = build_seg_index(new);
    let events: Vec<ChangeEvent> = material
        .iter()
        .map(|se| {
            build_event(
                se,
                &changeset,
                new,
                &segs,
                ctx,
                from_rev,
                prior.doc_hash.to_wire(),
            )
        })
        .collect();

    let env = event::changed_envelope(
        ctx.tid,
        ctx.batch_id,
        ctx.obs,
        from_rev,
        ctx.to_rev,
        Some(new.url.clone()),
        FetchTier::Http,
        ctx.status,
        ctx.ms,
        Some(new.doc_hash.to_wire()),
        events.clone(),
    );
    ObservationResult::Changed {
        envelope: env,
        events,
    }
}

/// Doc-level alignment rate: fraction of prior blocks that survived (were not removed) into the new
/// doc. `1.0` for a clean observation; `<0.9` flags a suspected redesign (drives `c_stability`).
fn alignment_rate(prior: &CanonicalDoc, _new: &CanonicalDoc, changeset: &Changeset) -> f32 {
    let prior_count = count_blocks(&prior.blocks);
    if prior_count == 0 {
        return 1.0;
    }
    let removed = changeset
        .units
        .iter()
        .filter(|u| u.ct == crate::model::ChangeType::Removed)
        .count();
    let surviving = prior_count.saturating_sub(removed);
    (surviving as f32 / prior_count as f32).clamp(0.0, 1.0)
}

fn count_blocks(blocks: &[Block]) -> usize {
    let mut n = 0;
    for b in blocks {
        n += 1;
        n += count_blocks(&b.children);
    }
    n
}

// ===========================================================================================
// Event assembly: turn a ScoredEvent + the new doc's seg index into a wire ChangeEvent (§6.2).
// ===========================================================================================

/// The display metadata for one block, recovered from the new doc's heading ancestry.
#[derive(Clone)]
struct SegInfo {
    anchor: String,
    label_path: String,
    role: String,
}

/// Build a `SlotKey -> SegInfo` index by walking the new doc's block tree, tracking the active
/// heading path so each block gets a nearest-heading `anchor` and a `›`-joined `label_path`.
fn build_seg_index(doc: &CanonicalDoc) -> FxHashMap<crate::model::SlotKey, SegInfo> {
    let mut out = FxHashMap::default();
    let mut path: Vec<String> = Vec::new();
    walk_seg(&doc.blocks, &mut path, &mut out);
    out
}

fn walk_seg(
    blocks: &[Block],
    path: &mut Vec<String>,
    out: &mut FxHashMap<crate::model::SlotKey, SegInfo>,
) {
    for b in blocks {
        let anchor = path.last().cloned().unwrap_or_else(|| b.text.clone());
        // Bound the nearest-heading anchor — a real heading is short, but a pathological page (a
        // multi-KB heading) would otherwise blow the §6.1 8 KB event ceiling through this display
        // field, which `enforce_size_ceiling` does not reach into the delta for (see B2). The join
        // key is `fp` (the slot_key), not the anchor text, so capping is display-only and safe.
        let anchor = cap_chars(&anchor, ANCHOR_CAP);
        let role = role_for(b.ty);
        let mut label_segments = path.clone();
        // Append the block's own short label (heading text or role) for display.
        let own = if b.ty == BlockType::Heading {
            b.text.clone()
        } else {
            role.to_string()
        };
        label_segments.push(own);
        let label_path = cap_chars(&label_segments.join(" \u{203a} "), LABEL_PATH_CAP);
        out.insert(
            b.slot_key,
            SegInfo {
                anchor,
                label_path,
                role: role.to_string(),
            },
        );

        if b.ty == BlockType::Heading {
            // Headings introduce a breadcrumb scope for the blocks that follow them as siblings.
            // Track only the most-recent heading at each conceptual level via a simple stack: pop to
            // this heading's level by using `level` when present.
            if let Some(level) = b.level {
                path.retain(|_| true);
                truncate_to_level(path, level);
            }
            path.push(b.text.clone());
        }
        walk_seg(&b.children, path, out);
    }
}

/// Keep the heading path no deeper than `level-1` entries before pushing a level-`level` heading,
/// so same-level headings are breadcrumb-siblings (mirrors segment's heading-stack discipline).
fn truncate_to_level(path: &mut Vec<String>, level: u8) {
    let keep = level.saturating_sub(1) as usize;
    while path.len() > keep {
        path.pop();
    }
}

/// Map a block type to an open-vocab `seg.role` (§6.2).
fn role_for(ty: BlockType) -> &'static str {
    match ty {
        BlockType::Price => "price",
        BlockType::Date => "date",
        BlockType::Number => "meta",
        BlockType::Heading => "prose",
        BlockType::Link => "link",
        BlockType::Code => "code",
        BlockType::TableRow | BlockType::Table => "table-cell",
        BlockType::Paragraph | BlockType::ListItem | BlockType::Text => "prose",
    }
}

/// `seg.fp` wire form: `blake3:<12hex>` — the slot_key prefix (DESIGN §6.2 / App B).
fn fp_wire(slot: &crate::model::SlotKey) -> String {
    let hex = slot.fp_hex();
    let short: String = hex.chars().take(12).collect();
    format!("blake3:{short}")
}

/// Find the change unit that produced a scored event (matched by slot_key) — needed for the
/// idempotency `event_key` and the §6.6 confidence factors. Deterministic: slot keys are unique
/// within a changeset's units in MVP.
fn unit_for<'a>(changeset: &'a Changeset, se: &ScoredEvent) -> Option<&'a ChangeUnit> {
    changeset.units.iter().find(|u| u.slot_key == se.slot_key)
}

#[allow(clippy::too_many_arguments)]
fn build_event(
    se: &ScoredEvent,
    changeset: &Changeset,
    new: &CanonicalDoc,
    segs: &FxHashMap<crate::model::SlotKey, SegInfo>,
    ctx: &mut PipelineCtx<'_>,
    from_rev: u64,
    prior_snap: String,
) -> ChangeEvent {
    let seg_info = segs.get(&se.slot_key).cloned().unwrap_or(SegInfo {
        anchor: String::new(),
        label_path: se.cat.clone(),
        role: "meta".to_string(),
    });

    let summary = build_summary(se, &seg_info, changeset);

    let id = (ctx.event_ids)();
    let mut event = ChangeEvent {
        v: "1",
        id: EventId::new(id),
        src: Src {
            url: new.url.clone(),
            tid: ctx.tid.to_string(),
            title: ctx.title.map(|s| s.to_string()),
        },
        obs: ctx.obs.to_string(),
        base: Base {
            obs: ctx
                .prior
                .map(|p| p.fetched_at.clone())
                .unwrap_or_default(),
            snap: prior_snap,
            rev: from_rev,
        },
        seg: vec![Seg {
            anchor: seg_info.anchor.clone(),
            fp: fp_wire(&se.slot_key),
            label_path: seg_info.label_path.clone(),
            role: seg_info.role.clone(),
        }],
        ct: se.ct,
        delta: se.delta.clone(),
        why: crate::model::Why {
            sal: se.sal,
            mat: se.mat,
            cat: se.cat.clone(),
            summary,
        },
        followup: Followup {
            act: se.act,
            tgt: None,
            params: None,
            q: None,
        },
        conf: se.conf,
        prov: Prov {
            m: FetchTier::Http,
            hash: new.doc_hash.to_wire(),
            etag: ctx.etag.clone(),
            status: ctx.status,
            ms: ctx.ms,
            pack: Some(ctx.pack.prov_stamp()),
        },
    };
    enforce_size_ceiling(&mut event);
    event
}

/// Collect every block's `slot_key` fingerprint hex (recursively) — the comparable key set the §4.9
/// redesign guard measures overlap over.
fn collect_slot_hexes(blocks: &[Block], out: &mut Vec<String>) {
    for b in blocks {
        out.push(b.slot_key.fp_hex());
        collect_slot_hexes(&b.children, out);
    }
}

/// §4.9 / §11 redesign guard. Returns one synthetic low-conf, high-`mat` `content_edit` event when
/// the new observation's stable `slot_key` set overlaps the prior's by less than `select_overlap_min`
/// (0.3) — i.e. a redesign / selector-drift the diff cannot align reliably. `None` when overlap is
/// healthy (the normal diff path proceeds). The single event flags the target for operator review
/// (low `conf` via `c_stability=0.6`) rather than emitting a flood of confident-but-wrong ops.
fn redesign_guard_event(
    prior: &CanonicalDoc,
    new: &CanonicalDoc,
    ctx: &mut PipelineCtx<'_>,
) -> Option<ChangeEvent> {
    let mut cur = Vec::new();
    collect_slot_hexes(&new.blocks, &mut cur);
    let mut old = Vec::new();
    collect_slot_hexes(&prior.blocks, &mut old);

    let detail = match extract::select_overlap_warning(&cur, &old)? {
        extract::SelectOverlapWarning::ZeroMatch => {
            "the selector/extraction matched zero blocks".to_string()
        }
        extract::SelectOverlapWarning::LowOverlap { overlap } => {
            format!("only {:.0}% slot-key overlap with the prior observation", overlap * 100.0)
        }
    };
    let summary = format!(
        "page structure changed substantially ({detail}); diff suppressed and target flagged for operator review"
    );

    // §6.6 conf with `c_stability` lowered to 0.6 (the redesign signal); other factors nominal.
    let factors = crate::salience::confidence::ConfFactors {
        c_fetch: 1.0,
        c_align: 1.0,
        c_match: 1.0,
        c_parse: 1.0,
        c_stability: 0.6,
    };
    let conf = crate::salience::confidence::confidence(&factors);

    let id = (ctx.event_ids)();
    Some(ChangeEvent {
        v: "1",
        id: EventId::new(id),
        src: Src {
            url: new.url.clone(),
            tid: ctx.tid.to_string(),
            title: ctx.title.map(|s| s.to_string()),
        },
        obs: ctx.obs.to_string(),
        base: Base {
            obs: prior.fetched_at.clone(),
            snap: prior.doc_hash.to_wire(),
            rev: ctx.prev_rev.unwrap_or(0),
        },
        seg: vec![Seg {
            anchor: String::new(),
            fp: String::new(),
            label_path: "page".to_string(),
            role: "meta".to_string(),
        }],
        ct: crate::model::ChangeType::Modified,
        delta: Delta::Block { a: summary.clone(), b: None, atrunc: false },
        why: crate::model::Why {
            sal: 0.75,
            mat: Materiality::High,
            cat: "content_edit".to_string(),
            summary,
        },
        // §8.4: `verify_llm` is inert in MVP (llm off), so the high band-default action stands.
        followup: Followup {
            act: crate::model::Action::ReRunDownstream,
            tgt: None,
            params: None,
            q: None,
        },
        conf,
        prov: Prov {
            m: FetchTier::Http,
            hash: new.doc_hash.to_wire(),
            etag: ctx.etag.clone(),
            status: ctx.status,
            ms: ctx.ms,
            pack: Some(ctx.pack.prov_stamp()),
        },
    })
}

/// §6.1 rule 3 — the hard per-event size ceiling. No single event may exceed ~8 KB on the wire so an
/// agent can rely on a bounded token cost per change. The per-block 600c truncation and ±6-token
/// idiff elision keep the common event far under (~700–900 B), but a pathological delta — a block
/// with hundreds of scattered idiff edits, a very wide table row — could still blow past. The MVP has
/// no `enc:struct` cascade fallback (Phase 2), so the graceful degradation here is: (1) collapse an
/// oversized delta to a capped `Block` summary (≤600c/side, `atrunc:true`); (2) if still over (a huge
/// `seg` array), keep only the primary segment. The result is a guaranteed-bounded event.
const EVENT_CEILING_BYTES: usize = 8 * 1024;

fn enforce_size_ceiling(ev: &mut ChangeEvent) {
    if wire_len(ev) <= EVENT_CEILING_BYTES {
        return;
    }
    ev.delta = collapse_delta(&ev.delta);
    if wire_len(ev) <= EVENT_CEILING_BYTES {
        return;
    }
    if ev.seg.len() > 1 {
        ev.seg.truncate(1);
    }
    if wire_len(ev) <= EVENT_CEILING_BYTES {
        return;
    }
    // Final net: the only remaining unbounded fields are the retained seg's display strings
    // (`anchor`/`label_path`). These are normally capped at build (`walk_seg`); cap again here so the
    // "guaranteed-bounded event" contract holds even if a future field arrives uncapped. The join
    // key (`fp`) is untouched, so addressing is unaffected.
    for s in &mut ev.seg {
        s.anchor = cap_chars(&s.anchor, ANCHOR_CAP);
        s.label_path = cap_chars(&s.label_path, LABEL_PATH_CAP);
    }
}

/// Per-field caps for the `seg` display strings (§6.2). An anchor is the nearest heading/label and a
/// label_path is a breadcrumb — both are short for real content; the caps only bite on pathological
/// pages (a multi-KB heading) and keep a single-segment event well under [`EVENT_CEILING_BYTES`].
const ANCHOR_CAP: usize = 160;
const LABEL_PATH_CAP: usize = 240;

/// Serialized wire length of an event in bytes (0 on the impossible serialize error, which only
/// matters as a conservative "don't over-truncate" fallback).
fn wire_len(ev: &ChangeEvent) -> usize {
    event::to_wire(ev).map(|s| s.len()).unwrap_or(0)
}

/// Collapse any delta to a bounded `Block` delta (≤600c/side, `atrunc:true`) — the §6.3 worst-case
/// encoding available in MVP. Reconstructs both sides from whatever encoding the delta carried.
fn collapse_delta(d: &Delta) -> Delta {
    use crate::model::DiffOp;
    let (before, after) = match d {
        Delta::Val { a, b } => (b.clone(), a.clone()),
        Delta::Idiff { ops } => {
            let before: String = ops
                .iter()
                .filter(|o| o.op != DiffOp::Ins)
                .map(|o| o.text.as_str())
                .collect();
            let after: String = ops
                .iter()
                .filter(|o| o.op != DiffOp::Del)
                .map(|o| o.text.as_str())
                .collect();
            (before, after)
        }
        Delta::Block { a, b, .. } => (b.clone().unwrap_or_default(), a.clone()),
        _ => (String::new(), String::new()),
    };
    Delta::Block {
        a: cap_chars(&after, EVENT_DELTA_SIDE_CAP),
        b: if before.is_empty() {
            None
        } else {
            Some(cap_chars(&before, EVENT_DELTA_SIDE_CAP))
        },
        atrunc: true,
    }
}

/// Per-side char cap when collapsing an oversized delta (matches the diff's BLOCK_TRUNC = 600).
const EVENT_DELTA_SIDE_CAP: usize = 600;

fn cap_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        s.chars().take(cap).collect()
    }
}

/// A concise (≤160 char) human summary for `why.summary` (§6.2). Numeric price deltas read as a
/// percentage move; other deltas describe the change shape.
fn build_summary(se: &ScoredEvent, seg: &SegInfo, changeset: &Changeset) -> String {
    let label = if seg.anchor.is_empty() {
        seg.role.as_str()
    } else {
        seg.anchor.as_str()
    };
    if let Some(unit) = unit_for(changeset, se) {
        if let Some(nc) = unit.numeric_change {
            let pct = nc.pct * 100.0;
            let (before, after) = numeric_strings(unit);
            let dir = if nc.to > nc.from { "rose" } else { "fell" };
            let s = format!("{label} {dir} {pct:.1}% ({before}\u{2192}{after}).");
            return truncate160(&s);
        }
    }
    match &se.delta {
        Delta::Val { a, b } => truncate160(&format!("{label} {b}\u{2192}{a}.")),
        Delta::Block { b: None, .. } => truncate160(&format!("{label} added.")),
        _ => truncate160(&format!("{label} {}.", se.cat)),
    }
}

/// Pull before/after display strings for a numeric unit (price minor units / bare number).
fn numeric_strings(unit: &ChangeUnit) -> (String, String) {
    match &unit.delta {
        Delta::Val { a, b } => (b.clone(), a.clone()),
        _ => (
            typed_display(&unit.from_norm_hash, None),
            typed_display(&unit.to_norm_hash, None),
        ),
    }
}

fn typed_display(_h: &crate::model::NormHash, v: Option<&TypedValue>) -> String {
    match v {
        Some(TypedValue::Price {
            amount_minor,
            currency,
            ..
        }) => format!("{} {}", *amount_minor as f64 / 100.0, currency),
        Some(TypedValue::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn truncate160(s: &str) -> String {
    if s.chars().count() <= 160 {
        s.to_string()
    } else {
        s.chars().take(157).collect::<String>() + "..."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExtractStrategy, IgnoreRule, RenderMode, SourceMode};
    use crate::packs;

    const BEFORE: &str = include_str!("../../../tests/fixtures/pricing_before.html");
    const AFTER: &str = include_str!("../../../tests/fixtures/pricing_after.html");
    const NOOP: &str = include_str!("../../../tests/fixtures/pricing_noop.html");

    /// A pricing profile scoped to the PricingTable, stripping the volatile viewer counter so the
    /// no-op short-circuits at the doc_hash (the §5.3 → §5.6 linchpin).
    fn pricing_profile() -> Profile {
        Profile {
            profile_id: "comp-pricing".into(),
            render: RenderMode::Auto,
            strategy: ExtractStrategy::Selector,
            root_selector: Some("section.PricingTable".into()),
            strip_attrs: vec!["data-csrf-nonce".into()],
            strip_text: vec![r"\d+\s+viewing right now".into()],
            unordered: Vec::new(),
            mode: SourceMode::Page,
            max_pages: 1,
            types: Vec::new(),
            archetype: Some("pricing".into()),
            salience_hints: Vec::new(),
        }
    }

    fn doc(html: &str) -> CanonicalDoc {
        let mut d = canonicalize(html, &pricing_profile()).unwrap();
        d.url = "https://competitor.com/pricing".into();
        d
    }

    fn run(prior: Option<&CanonicalDoc>, new: &CanonicalDoc) -> ObservationResult {
        let pack = packs::resolve(Some("pricing")).unwrap();
        let ignore: Vec<IgnoreRule> = Vec::new();
        let mut n = 0u32;
        let mut minter = move || {
            let id = format!("cfe_test{n:06}");
            n += 1;
            id
        };
        let mut ctx = PipelineCtx {
            tid: "comp-pricing",
            title: None,
            obs: "2026-06-02T14:30:11Z",
            batch_id: "cfb_test",
            event_ids: &mut minter,
            prior,
            prev_rev: prior.map(|_| 0),
            to_rev: if prior.is_some() { 1 } else { 0 },
            pack: &pack,
            min_salience: Materiality::Low,
            emit_subthreshold: false,
            ignore: &ignore,
            status: 200,
            etag: None,
            ms: None,
        };
        observe_body(new, &mut ctx)
    }

    #[test]
    fn event_size_ceiling_collapses_a_pathological_delta() {
        use crate::model::{Action, ChangeType, DiffOp, IdiffOp, Why};
        // §6.1 rule 3: a pathological idiff (thousands of ops) far exceeds the 8 KB ceiling. The
        // guard must collapse it to a bounded `Block` delta (`atrunc`) so the event fits.
        let ops: Vec<IdiffOp> = (0..4000)
            .map(|i| IdiffOp {
                op: if i % 2 == 0 { DiffOp::Del } else { DiffOp::Ins },
                text: format!("token{i} "),
            })
            .collect();
        let mut ev = ChangeEvent {
            v: "1",
            id: EventId::new("cfe_test000000".into()),
            src: Src { url: "https://x.test/p".into(), tid: "t".into(), title: None },
            obs: "2026-06-02T00:00:00Z".into(),
            base: Base { obs: String::new(), snap: "blake3:0".into(), rev: 0 },
            seg: vec![Seg {
                anchor: "A".into(),
                fp: "blake3:0".into(),
                label_path: "p".into(),
                role: "prose".into(),
            }],
            ct: ChangeType::Modified,
            delta: Delta::Idiff { ops },
            why: Why {
                sal: 0.5,
                mat: Materiality::Medium,
                cat: "content_edit".into(),
                summary: "x".into(),
            },
            followup: Followup { act: Action::Notify, tgt: None, params: None, q: None },
            conf: 0.9,
            prov: Prov {
                m: FetchTier::Http,
                hash: "blake3:0".into(),
                etag: None,
                status: 200,
                ms: None,
                pack: None,
            },
        };
        assert!(wire_len(&ev) > EVENT_CEILING_BYTES, "precondition: raw event exceeds the ceiling");
        enforce_size_ceiling(&mut ev);
        assert!(
            wire_len(&ev) <= EVENT_CEILING_BYTES,
            "event must be bounded by the ceiling, got {} bytes",
            wire_len(&ev)
        );
        match &ev.delta {
            Delta::Block { atrunc, .. } => assert!(*atrunc, "collapsed delta is marked truncated"),
            other => panic!("an oversized delta must collapse to a Block, got {other:?}"),
        }
    }

    #[test]
    fn event_size_ceiling_caps_an_oversized_anchor_and_label_path() {
        use crate::model::{Action, ChangeType, Why};
        // B2 regression: a change anchored under a multi-KB heading made `seg.anchor`/`seg.label_path`
        // carry the full heading text, blowing the event past the 8 KB ceiling even though the delta
        // (a tiny `Val`) and summary were already bounded. The guard must cap the seg display strings.
        let huge = "MegaHeading ".repeat(4000); // ~48 KB
        let mut ev = ChangeEvent {
            v: "1",
            id: EventId::new("cfe_test000001".into()),
            src: Src { url: "https://x.test/p".into(), tid: "t".into(), title: None },
            obs: "2026-06-02T00:00:00Z".into(),
            base: Base { obs: String::new(), snap: "blake3:0".into(), rev: 0 },
            seg: vec![Seg {
                anchor: huge.clone(),
                fp: "blake3:abcd1234".into(),
                label_path: huge,
                role: "price".into(),
            }],
            ct: ChangeType::Modified,
            delta: Delta::Val { a: "$59/mo".into(), b: "$49/mo".into() },
            why: Why {
                sal: 0.8,
                mat: Materiality::High,
                cat: "price_increase".into(),
                summary: "x".into(),
            },
            followup: Followup { act: Action::Notify, tgt: None, params: None, q: None },
            conf: 0.9,
            prov: Prov {
                m: FetchTier::Http,
                hash: "blake3:0".into(),
                etag: None,
                status: 200,
                ms: None,
                pack: None,
            },
        };
        assert!(wire_len(&ev) > EVENT_CEILING_BYTES, "precondition: oversized anchor exceeds the ceiling");
        let fp_before = ev.seg[0].fp.clone();
        enforce_size_ceiling(&mut ev);
        assert!(
            wire_len(&ev) <= EVENT_CEILING_BYTES,
            "event with an oversized anchor must be bounded, got {} bytes",
            wire_len(&ev)
        );
        assert!(ev.seg[0].anchor.chars().count() <= ANCHOR_CAP, "anchor capped to ANCHOR_CAP");
        assert!(ev.seg[0].label_path.chars().count() <= LABEL_PATH_CAP, "label_path capped to LABEL_PATH_CAP");
        assert_eq!(ev.seg[0].fp, fp_before, "the join key (fp) is never truncated");
    }

    #[test]
    fn whole_page_redesign_emits_one_guard_event_not_a_flood() {
        fn full_profile() -> Profile {
            Profile {
                profile_id: "redesign".into(),
                render: RenderMode::Auto,
                strategy: ExtractStrategy::Full,
                root_selector: None,
                strip_attrs: Vec::new(),
                strip_text: Vec::new(),
                unordered: Vec::new(),
                mode: SourceMode::Page,
                max_pages: 1,
                types: Vec::new(),
                archetype: Some("pricing".into()),
                salience_hints: Vec::new(),
            }
        }
        fn d(html: &str) -> CanonicalDoc {
            let mut x = canonicalize(html, &full_profile()).unwrap();
            x.url = "https://x.test/p".into();
            x
        }
        // Two structurally-disjoint pages: breadcrumb-derived slot_keys barely overlap (<30%), the
        // §4.9 redesign condition.
        let prior = d(r#"<html><body><main>
            <h1>Apple Orchard</h1>
            <h2>Fuji</h2><p>Crisp and sweet apples from the north field.</p>
            <h2>Gala</h2><p>Mild apples picked in early autumn here.</p>
            <h2>Honeycrisp</h2><p>Large apples with a tart finish today.</p>
        </main></body></html>"#);
        let new = d(r#"<html><body><main>
            <h1>Zebra Sanctuary</h1>
            <h2>Plains</h2><p>Grazing herds roam the open savanna here.</p>
            <h2>Mountain</h2><p>Sure-footed stripes on the high rocky slopes.</p>
            <h2>Grevy</h2><p>The largest wild equids with narrow stripes.</p>
        </main></body></html>"#);

        match run(Some(&prior), &new) {
            ObservationResult::Changed { events, .. } => {
                assert_eq!(
                    events.len(),
                    1,
                    "a redesign emits exactly ONE guard event, not a flood; got {}",
                    events.len()
                );
                let e = &events[0];
                assert_eq!(e.why.mat, Materiality::High, "guard is high-mat");
                assert_eq!(e.why.cat, "content_edit", "guard is a content_edit");
                assert!(
                    (e.conf - 0.6).abs() < 0.01,
                    "guard is low-conf (c_stability=0.6), got {}",
                    e.conf
                );
                assert!(
                    e.why.summary.to_lowercase().contains("review"),
                    "guard summary flags operator review, got {:?}",
                    e.why.summary
                );
            }
            _ => panic!("a whole-page redesign must be Changed with one guard event"),
        }
    }

    #[test]
    fn first_observation_is_baseline() {
        let new = doc(BEFORE);
        match run(None, &new) {
            ObservationResult::Baseline(env) => {
                assert_eq!(env.crawl.from_rev, None);
                assert_eq!(env.crawl.baseline, Some(true));
                assert!(env.events.is_empty());
            }
            _ => panic!("first observation must be a baseline"),
        }
    }

    #[test]
    fn volatile_only_change_is_no_change() {
        let prior = doc(BEFORE);
        let new = doc(NOOP);
        // The viewer counter + nonce are stripped, so the docs are doc_hash-equal.
        assert_eq!(prior.doc_hash, new.doc_hash);
        assert!(matches!(run(Some(&prior), &new), ObservationResult::NoChange { .. }));
    }

    #[test]
    fn price_change_is_changed_with_a_val_delta() {
        let prior = doc(BEFORE);
        let new = doc(AFTER);
        match run(Some(&prior), &new) {
            ObservationResult::Changed { events, .. } => {
                let price = events
                    .iter()
                    .find(|e| matches!(&e.delta, Delta::Val { .. }))
                    .expect("a val price delta");
                match &price.delta {
                    Delta::Val { a, b } => {
                        assert_eq!(a, "$59/mo");
                        assert_eq!(b, "$49/mo");
                    }
                    _ => unreachable!(),
                }
                assert_eq!(price.why.cat, "price_increase");
                assert_eq!(price.seg[0].role, "price");
            }
            _ => panic!("a real price change must be Changed"),
        }
    }

    #[test]
    fn diff_score_is_deterministic_across_two_runs() {
        let prior = doc(BEFORE);
        let new = doc(AFTER);
        let pack = packs::resolve(Some("pricing")).unwrap();
        let (_, s1) =
            diff_and_score(&prior, &new, &[], &pack, FetchTier::Http).unwrap();
        let (_, s2) =
            diff_and_score(&prior, &new, &[], &pack, FetchTier::Http).unwrap();
        assert_eq!(s1.len(), s2.len());
        for (a, b) in s1.iter().zip(&s2) {
            assert_eq!(a.sal, b.sal);
            assert_eq!(a.mat, b.mat);
            assert_eq!(a.cat, b.cat);
            assert_eq!(a.act, b.act);
            assert_eq!(a.conf, b.conf);
            assert_eq!(a.slot_key, b.slot_key);
        }
    }
}
