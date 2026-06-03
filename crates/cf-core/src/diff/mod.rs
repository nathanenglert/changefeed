//! §7 diff engine — mask → slot_key-anchor → LIS → similarity-fill → Myers intra-block → noise.
//! Pure: `fn(&old, &new, &Profile) -> Result<Changeset, CfError>`. Fixed-seed LSH and
//! `(preorder_idx, slot_key)` tie-breaks throughout (§4 determinism rule 5).
//!
//! Pipeline (DESIGN §7):
//! 0. **doc_hash short-circuit** — a `doc_hash`-equal pair returns an EMPTY changeset before any
//!    alignment runs (the cheap no-op path is structurally zero-work).
//! 1. **mask** (§7.0) — drop/redact/attr-strip per the profile's ignore rules, before hashing.
//! 2. **align** (§7.1) — phase 1 stable-key anchors (a text-edited block STILL anchors here);
//!    phase 2 LIS over anchor new-positions → stable spine + reorder candidates; phase 3 similarity
//!    fill for the residual (same type, ±W=32 window, LSH band; sim ≥ τ_match → modified else
//!    removed+added).
//! 3. **intra** (§7.2) — Myers token diff for each modified pair → idiff/val deltas; attribute
//!    deltas compared on POST-canonicalization values (a utm-only change is suppressed).
//! 4. **noise** (§7.3) — a soft `noise_score` per unit.
//!
//! Worst case stays `O(B log B)`: hashing is `O(B)`, LIS `O(A log A)`, similarity `O(B·b)` with `b`
//! the bounded LSH-band+window candidate count — never `O(B²)`.

pub mod align;
pub mod intra;
pub mod lis;
pub mod lsh;
pub mod mask;
pub mod noise;

use crate::model::{
    Block, BlockType, CanonicalDoc, ChangeType, Delta, EventKey, IgnoreRule, NormHash, Profile,
    SlotKey, TypedValue,
};
use crate::CfError;

use noise::NoiseInput;

/// τ_match default — accept a similarity pair as `modified` at/above this (DESIGN §7.1, App C).
pub const TAU_MATCH: f32 = 0.62;
/// τ_match for short `table_row`/`kv` types (DESIGN §7.1, App C).
pub const TAU_MATCH_SHORT: f32 = 0.75;
/// Position window `±W` for similarity candidates (DESIGN §7.1, App C).
pub const WINDOW: i64 = 32;
/// Minimum contiguous run length to emit a standalone reorder (DESIGN §7.1, App C).
pub const MOVE_MIN: usize = 3;
/// `slot_affinity` below this tags a matched pair `moved` (DESIGN §7.1).
pub const MOVED_AFFINITY: f32 = 0.5;

/// The §7.5 handoff features for one cluster/change unit. `segment_stability`/`novelty` default to
/// 1.0 in MVP (no learned volatility / no novelty window — those are Phase 2, §7.4).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Features {
    /// Token-level magnitude (§8.1 `mag`): text `1 − exp(−3·edit_ratio)`; numeric
    /// `clamp(|after−before|/max(|before|,ε), 0, 1)`.
    pub magnitude: f32,
    /// §7.3 noise score (mirrors [`ChangeUnit::noise_score`] for the §7.5 features block).
    pub noise_score: f32,
    /// 1 − flap_ewma. Phase 2; defaults to 1.0 in MVP.
    pub segment_stability: f32,
    /// §7.4 novelty window. Defaults to 1.0 in MVP.
    pub novelty: f32,
    /// Tagged when a matched pair's `slot_affinity < 0.5` (a reorder candidate).
    pub moved: bool,
}

impl Default for Features {
    fn default() -> Self {
        Features {
            magnitude: 0.0,
            noise_score: 0.0,
            segment_stability: 1.0,
            novelty: 1.0,
            moved: false,
        }
    }
}

/// A typed numeric change (§7.5 `numeric_change`): present iff both endpoints carry a comparable
/// numeric value (price minor-units or a bare number). `pct = |to − from| / max(|from|, ε)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NumericChange {
    pub from: f64,
    pub to: f64,
    pub pct: f64,
}

/// One aligned, classified change before salience scoring (§7 output, §8 input).
#[derive(Clone, Debug)]
pub struct ChangeUnit {
    pub slot_key: SlotKey,
    pub ct: ChangeType,
    pub delta: Delta,
    /// §7.3 noise score 0..1.
    pub noise_score: f32,
    /// Alignment similarity 0..1 (1.0 for a clean slot_key anchor match).
    pub sim: f32,
    /// §7.5 handoff features (magnitude / stability / novelty / moved).
    pub features: Features,
    /// Typed numeric delta when both sides are numeric (§7.5 `numeric_change`).
    pub numeric_change: Option<NumericChange>,
    /// The block type of the changed block (drives salience `type` weight + noise classification).
    pub block_type: BlockType,
    /// Prior-side normalized-text hash (`from`) — feeds the §7.4 idempotency `event_key`.
    pub from_norm_hash: NormHash,
    /// New-side normalized-text hash (`to`) — feeds the §7.4 idempotency `event_key`.
    pub to_norm_hash: NormHash,
}

impl ChangeUnit {
    /// §7.4 idempotency key for this unit under a target id: `xxh3(tid ‖ slot ‖ from ‖ to)`.
    /// Deterministic — re-running the diff on the same pair yields the same key (the seen-set join).
    pub fn event_key(&self, target_id: &str) -> EventKey {
        EventKey::derive(target_id, &self.slot_key, self.from_norm_hash, self.to_norm_hash)
    }
}

/// The full §7 result: aligned change units in deterministic order.
#[derive(Clone, Debug, Default)]
pub struct Changeset {
    pub units: Vec<ChangeUnit>,
}

/// §7 — diff a prior `CanonicalDoc` against a new one under a profile's ignore masking.
///
/// Uses the profile's threaded ignore rules (see [`mask::apply`]). For callers holding the typed
/// `TargetCfg.ignore` list, use [`diff_with_ignores`].
pub fn diff(old: &CanonicalDoc, new: &CanonicalDoc, profile: &Profile) -> Result<Changeset, CfError> {
    // (0) doc_hash short-circuit — nothing semantic changed → empty changeset (DESIGN §7, §5.6).
    if old.doc_hash == new.doc_hash {
        return Ok(Changeset::default());
    }
    let masked_old = mask::apply(old, profile);
    let masked_new = mask::apply(new, profile);
    Ok(run(&masked_old, &masked_new))
}

/// §7 diff with an explicit typed ignore list (the cli path; it holds `TargetCfg.ignore`).
pub fn diff_with_ignores(
    old: &CanonicalDoc,
    new: &CanonicalDoc,
    ignores: &[IgnoreRule],
) -> Result<Changeset, CfError> {
    if old.doc_hash == new.doc_hash {
        return Ok(Changeset::default());
    }
    let masked_old = mask::apply_rules(old, ignores);
    let masked_new = mask::apply_rules(new, ignores);
    Ok(run(&masked_old, &masked_new))
}

// ===========================================================================================
// Core alignment + classification over masked docs.
// ===========================================================================================

fn flatten(blocks: &[Block]) -> Vec<&Block> {
    let mut out = Vec::new();
    fn rec<'a>(bs: &'a [Block], out: &mut Vec<&'a Block>) {
        for b in bs {
            out.push(b);
            rec(&b.children, out);
        }
    }
    rec(blocks, &mut out);
    out
}

fn run(old: &CanonicalDoc, new: &CanonicalDoc) -> Changeset {
    // Flatten to pre-order REFERENCE sequences — no per-block clone (the docs are borrowed for the
    // whole diff, so the aligner/LIS/similarity passes work over `&Block` indices). This is the
    // single largest allocation the diff used to pay (two full deep clones of the block forest).
    let old_flat = flatten(&old.blocks);
    let new_flat = flatten(&new.blocks);

    // Phase 1: slot_key anchors (a text-edited block anchors here; its key is text-free).
    let alignment = align::align(&old_flat, &new_flat);

    let mut units: Vec<UnitDraft> = Vec::new();

    // Phase 2: LIS over the anchors' new-positions → stable spine; off-spine anchors are reorder
    // candidates. We process EVERY anchor pair: equal norm_hash = clean (no event); differing
    // norm_hash = modified (§7.1 note: the edited-but-anchored block is a phase-1 modified).
    let anchor_new_positions: Vec<u32> = alignment.paired.iter().map(|&(_, n)| n as u32).collect();
    let spine_idx: std::collections::HashSet<usize> =
        lis::longest_increasing(&anchor_new_positions).into_iter().collect();

    // A moved anchor that is part of a contiguous off-spine run ≥ MOVE_MIN is a reorder (§7.1).
    let moved_flags = compute_moved_runs(&alignment.paired, &spine_idx);

    for (ai, &(oi, ni)) in alignment.paired.iter().enumerate() {
        let ob = old_flat[oi];
        let nb = new_flat[ni];
        let moved = moved_flags[ai];

        // A block is "content-unchanged" only when BOTH its normalized text AND its typed value are
        // equal. The text hash alone misses an attribute-only change (a Link whose anchor text is
        // identical but whose canonical href swapped, §7.2) — that is still a real modified op.
        //
        // Fast path: `norm_hash` (a u64 compare) settles the overwhelming majority of the 5k anchors
        // cheaply, and `value` is computed lazily ONLY when the text matched — so a real edit never
        // pays for the (potentially long) `TypedValue` string comparison, and the deep value compare
        // is skipped entirely whenever the cheap hash already proved a difference.
        let text_equal = ob.norm_hash == nb.norm_hash;
        if text_equal && values_equal(&ob.value, &nb.value) {
            // Unchanged content (text + value). A changed presentation marker (strike/ins, §6.2) is
            // a `restyled` op; otherwise a qualifying contiguous off-spine run is a standalone reorder.
            if ob.restyle_sig != nb.restyle_sig {
                units.push(UnitDraft::restyled(ob, nb));
            } else if moved {
                units.push(UnitDraft::reorder(ob, nb, oi, ni));
            }
            // else: a clean spine anchor — no event.
            continue;
        }
        // Anchored + content (text or value) changed → modified (sim = 1.0, the certain anchor).
        units.push(UnitDraft::modified(ob, nb, 1.0, moved));
    }

    // Phase 3: similarity fill over the residual (slot_key changed / genuinely new / removed).
    similarity_fill(&old_flat, &new_flat, &alignment, &mut units);

    finalize(units)
}

/// Compare two block `value`s for the anchor fast path, given that the caller has ALREADY proven the
/// blocks' `norm_hash` (normalized text) is equal. For the text-mirror value types (`Text`,
/// `Heading`, `Code`) the stored string is a copy of that same normalized text, so equal text ⇒
/// equal value — we settle those by discriminant alone and avoid re-comparing a possibly-long
/// string. All other variants (notably `Link`, whose `href_canonical` is attribute-derived and can
/// differ while the text is identical — §7.2) fall back to the full structural compare, preserving
/// behavior exactly.
#[inline]
fn values_equal(a: &Option<TypedValue>, b: &Option<TypedValue>) -> bool {
    match (a, b) {
        (Some(TypedValue::Text(_)), Some(TypedValue::Text(_)))
        | (Some(TypedValue::Heading(_)), Some(TypedValue::Heading(_)))
        | (Some(TypedValue::Code(_)), Some(TypedValue::Code(_))) => true,
        _ => a == b,
    }
}

/// Decide which paired anchors are "moved" reorder candidates: an anchor off the LIS spine whose
/// off-spine neighbours form a contiguous run of ≥ MOVE_MIN in prior order (DESIGN §7.1 `move_min`).
fn compute_moved_runs(
    paired: &[(usize, usize)],
    spine_idx: &std::collections::HashSet<usize>,
) -> Vec<bool> {
    let n = paired.len();
    let mut moved = vec![false; n];
    let mut i = 0;
    while i < n {
        if spine_idx.contains(&i) {
            i += 1;
            continue;
        }
        // Extend a contiguous run of off-spine anchors.
        let start = i;
        while i < n && !spine_idx.contains(&i) {
            i += 1;
        }
        let run_len = i - start;
        if run_len >= MOVE_MIN {
            for m in moved.iter_mut().take(i).skip(start) {
                *m = true;
            }
        }
    }
    moved
}

/// Phase 3 (§7.1): pair residual blocks. First pair equal `norm_hash` blocks (same content, moved
/// slot); then run windowed + LSH-banded similarity for the rest. Unpaired residuals become clean
/// removed/added.
fn similarity_fill(
    old: &[&Block],
    new: &[&Block],
    alignment: &align::Alignment,
    units: &mut Vec<UnitDraft>,
) {
    let mut old_res: Vec<usize> = alignment.old_residual.clone();
    let mut new_res: Vec<usize> = alignment.new_residual.clone();
    let mut old_done = vec![false; old.len()];
    let mut new_done = vec![false; new.len()];

    // 3a. Pair equal norm_hash residuals (identical content whose slot_key changed → a move, not a
    // remove+add). Greedy in prior order; first unused new match wins (deterministic).
    {
        use rustc_hash::FxHashMap;
        let mut by_hash: FxHashMap<u64, Vec<usize>> = FxHashMap::default();
        for &ni in &new_res {
            by_hash.entry(new[ni].norm_hash.raw()).or_default().push(ni);
        }
        for &oi in &old_res {
            let h = old[oi].norm_hash.raw();
            if let Some(cands) = by_hash.get_mut(&h) {
                if let Some(p) = cands.iter().position(|&ni| !new_done[ni]) {
                    let ni = cands[p];
                    if old[oi].ty == new[ni].ty {
                        old_done[oi] = true;
                        new_done[ni] = true;
                        // Same content, different slot → a moved block (modified-by-position).
                        units.push(UnitDraft::moved_same_content(old[oi], new[ni]));
                    }
                }
            }
        }
    }
    old_res.retain(|&i| !old_done[i]);
    new_res.retain(|&i| !new_done[i]);

    // 3b. Windowed + LSH-banded similarity over the remaining residual.
    // Build the candidate index once over the combined residual token sets (transient, discarded).
    similarity_match(old, new, &old_res, &new_res, &mut old_done, &mut new_done, units);

    // 3c. Anything still unmatched is a clean removed / added (deterministic order).
    for &oi in &old_res {
        if !old_done[oi] {
            units.push(UnitDraft::removed(old[oi]));
        }
    }
    for &ni in &new_res {
        if !new_done[ni] {
            units.push(UnitDraft::added(new[ni]));
        }
    }
}

/// 3b: similarity matching restricted to same type, ±W window, and the LSH band (DESIGN §7.1).
/// The band bounds candidates per block to a small constant → `O(B·b)`, never `O(B²)`.
fn similarity_match(
    old: &[&Block],
    new: &[&Block],
    old_res: &[usize],
    new_res: &[usize],
    old_done: &mut [bool],
    new_done: &mut [bool],
    units: &mut Vec<UnitDraft>,
) {
    if old_res.is_empty() || new_res.is_empty() {
        return;
    }

    // Build a transient LSH band over the *combined* residual token sets so old/new collide.
    // Map: combined-index -> (side, original index). Old residuals first, then new.
    let mut combined_tokens: Vec<Vec<u64>> = Vec::with_capacity(old_res.len() + new_res.len());
    let mut combined_ref: Vec<(Side, usize)> = Vec::with_capacity(old_res.len() + new_res.len());
    for &oi in old_res {
        combined_tokens.push(intra::token_hashes(&old[oi].text));
        combined_ref.push((Side::Old, oi));
    }
    for &ni in new_res {
        combined_tokens.push(intra::token_hashes(&new[ni].text));
        combined_ref.push((Side::New, ni));
    }
    let band = lsh::build_band(&combined_tokens);

    // Collect candidate (old_idx, new_idx) pairs from shared buckets, then add window neighbours of
    // the same type (so a short block whose tokens didn't collide still gets its in-window peers).
    use rustc_hash::FxHashSet;
    let mut candidates: FxHashSet<(usize, usize)> = FxHashSet::default();
    for bucket in &band.buckets {
        for &ca in bucket {
            for &cb in bucket {
                let (sa, ia) = combined_ref[ca];
                let (sb, ib) = combined_ref[cb];
                if sa == Side::Old && sb == Side::New {
                    candidates.insert((ia, ib));
                }
            }
        }
    }
    // Window neighbours (bounded by W) of the same type — caps per-block candidates regardless of
    // how few anchors survive (the honest-worst-case guard, DESIGN §7.1). `new_res` is in ascending
    // block-index order (built from `(0..n).filter()`), so for each `oi` we binary-search the
    // `[oi-W, oi+W]` slice and scan ONLY those ≤ 2W+1 neighbours — making this `O(|old_res|·W)`, i.e.
    // `O(B·b)` with a constant band, NOT the `O(B²)` of a full residual×residual nested scan (the
    // §7.1 "never O(B²)" guarantee, which a naive double-loop would have violated on a low-anchor
    // rename-everything redesign where nearly every block is a residual).
    for &oi in old_res {
        let oi_i = oi as i64;
        let lo_idx = oi.saturating_sub(WINDOW as usize);
        let hi_idx = oi.saturating_add(WINDOW as usize);
        let lo = new_res.partition_point(|&ni| ni < lo_idx);
        for &ni in &new_res[lo..] {
            if ni > hi_idx {
                break; // past the +W edge — `new_res` is ascending, so we are done for this oi.
            }
            debug_assert!((oi_i - ni as i64).abs() <= WINDOW);
            if old[oi].ty == new[ni].ty {
                candidates.insert((oi, ni));
            }
        }
    }

    // Score every candidate; keep only same-type pairs at/above τ. Sort by (descending sim, then
    // (old_pos, new_pos)) so the best, deterministic pairs are taken first (greedy 1:1).
    let mut scored: Vec<(f32, usize, usize)> = Vec::new();
    for &(oi, ni) in &candidates {
        if old[oi].ty != new[ni].ty {
            continue;
        }
        let s = sim(old[oi], new[ni]);
        let tau = tau_for(old[oi].ty);
        if s >= tau {
            scored.push((s, oi, ni));
        }
    }
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
    });

    for (s, oi, ni) in scored {
        if old_done[oi] || new_done[ni] {
            continue;
        }
        old_done[oi] = true;
        new_done[ni] = true;
        let moved = slot_affinity(old[oi], new[ni]) < MOVED_AFFINITY;
        units.push(UnitDraft::modified(old[oi], new[ni], s, moved));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
}

/// τ_match for a block type: 0.75 for short table_row types, else 0.62 (DESIGN §7.1).
fn tau_for(ty: BlockType) -> f32 {
    match ty {
        BlockType::TableRow => TAU_MATCH_SHORT,
        _ => TAU_MATCH,
    }
}

/// §7.1 similarity score: `0.55·jaccard + 0.30·(1 − norm_lev) + 0.15·slot_affinity`.
fn sim(a: &Block, b: &Block) -> f32 {
    let jac = intra::token_jaccard(&a.text, &b.text);
    let lev = intra::norm_levenshtein(&a.text, &b.text);
    let aff = slot_affinity(a, b);
    0.55 * jac + 0.30 * (1.0 - lev) + 0.15 * aff
}

/// §7.1 `slot_affinity`: how "in place" the candidate pair is. 1.0 if the slot_keys are equal
/// (cannot happen in the residual, but defined), else a soft proxy from the heading level + the
/// type match. Here, post-segment, we use the anchored_by scheme + type identity as the affinity:
/// identical type + same anchor scheme = high affinity; a type/scheme mismatch lowers it.
fn slot_affinity(a: &Block, b: &Block) -> f32 {
    if a.slot_key == b.slot_key {
        return 1.0;
    }
    let mut aff = 0.0f32;
    if a.ty == b.ty {
        aff += 0.6;
    }
    if a.anchored_by == b.anchored_by {
        aff += 0.2;
    }
    if a.level == b.level {
        aff += 0.2;
    }
    aff
}

// ===========================================================================================
// Unit drafting: build deltas + features for each classified change, then finalize noise.
// ===========================================================================================

/// An in-progress change unit before noise scoring (which needs the block texts/types).
struct UnitDraft {
    slot_key: SlotKey,
    ct: ChangeType,
    delta: Delta,
    sim: f32,
    moved: bool,
    block_type: BlockType,
    before_text: String,
    after_text: String,
    numeric_change: Option<NumericChange>,
    magnitude: f32,
    from_norm_hash: NormHash,
    to_norm_hash: NormHash,
}

impl UnitDraft {
    fn modified(ob: &Block, nb: &Block, sim: f32, moved: bool) -> Self {
        let (delta, numeric_change, magnitude) = build_modified_delta(ob, nb);
        UnitDraft {
            slot_key: nb.slot_key,
            ct: ChangeType::Modified,
            delta,
            sim,
            moved,
            block_type: nb.ty,
            before_text: ob.text.clone(),
            after_text: nb.text.clone(),
            numeric_change,
            magnitude,
            from_norm_hash: ob.norm_hash,
            to_norm_hash: nb.norm_hash,
        }
    }

    /// Same content, slot_key changed (a moved block matched by equal norm_hash in 3a). It is
    /// reported as a reorder (content identical, position changed).
    fn moved_same_content(ob: &Block, nb: &Block) -> Self {
        UnitDraft {
            slot_key: nb.slot_key,
            ct: ChangeType::Reordered,
            delta: Delta::Move {
                from: ob.preorder_idx,
                to: nb.preorder_idx,
                key: nb.slot_key.fp_hex(),
            },
            sim: 1.0,
            moved: true,
            block_type: nb.ty,
            before_text: ob.text.clone(),
            after_text: nb.text.clone(),
            numeric_change: None,
            magnitude: 0.0,
            from_norm_hash: ob.norm_hash,
            to_norm_hash: nb.norm_hash,
        }
    }

    /// §6.2 — content (text + value) identical, only a presentation marker (`<del>`/`<s>` strike or
    /// `<ins>`) changed: a `restyled` op. `from`/`to` `norm_hash` are equal (text unchanged); the
    /// `ct` is the whole signal. noise.rs scores `restyled` at 0.95 → low salience, so an agent
    /// filters it with `--min-salience low` (it surfaces a deprecation strike when it matters).
    fn restyled(ob: &Block, nb: &Block) -> Self {
        UnitDraft {
            slot_key: nb.slot_key,
            ct: ChangeType::Restyled,
            delta: Delta::Val {
                a: nb.text.clone(),
                b: ob.text.clone(),
            },
            sim: 1.0,
            moved: false,
            block_type: nb.ty,
            before_text: ob.text.clone(),
            after_text: nb.text.clone(),
            numeric_change: None,
            magnitude: 0.0,
            from_norm_hash: ob.norm_hash,
            to_norm_hash: nb.norm_hash,
        }
    }

    fn reorder(ob: &Block, nb: &Block, _oi: usize, _ni: usize) -> Self {
        UnitDraft {
            slot_key: nb.slot_key,
            ct: ChangeType::Reordered,
            delta: Delta::Move {
                from: ob.preorder_idx,
                to: nb.preorder_idx,
                key: nb.slot_key.fp_hex(),
            },
            sim: 1.0,
            moved: true,
            block_type: nb.ty,
            before_text: ob.text.clone(),
            after_text: nb.text.clone(),
            numeric_change: None,
            magnitude: 0.0,
            from_norm_hash: ob.norm_hash,
            to_norm_hash: nb.norm_hash,
        }
    }

    fn added(nb: &Block) -> Self {
        UnitDraft {
            slot_key: nb.slot_key,
            ct: ChangeType::Added,
            delta: Delta::Block {
                a: truncate_block(&nb.text),
                b: None,
                atrunc: nb.text.chars().count() > BLOCK_TRUNC,
            },
            sim: 0.0,
            moved: false,
            block_type: nb.ty,
            before_text: String::new(),
            after_text: nb.text.clone(),
            numeric_change: None,
            magnitude: 1.0,
            from_norm_hash: NormHash::of(""),
            to_norm_hash: nb.norm_hash,
        }
    }

    fn removed(ob: &Block) -> Self {
        UnitDraft {
            slot_key: ob.slot_key,
            ct: ChangeType::Removed,
            delta: Delta::Block {
                a: truncate_block(&ob.text),
                b: None,
                atrunc: ob.text.chars().count() > BLOCK_TRUNC,
            },
            sim: 0.0,
            moved: false,
            block_type: ob.ty,
            before_text: ob.text.clone(),
            after_text: String::new(),
            numeric_change: None,
            magnitude: 1.0,
            from_norm_hash: ob.norm_hash,
            to_norm_hash: NormHash::of(""),
        }
    }
}

/// Max block text (chars) carried in a `Block` delta before truncation (DESIGN §6.3 ≤600c).
const BLOCK_TRUNC: usize = 600;

fn truncate_block(s: &str) -> String {
    if s.chars().count() <= BLOCK_TRUNC {
        s.to_string()
    } else {
        s.chars().take(BLOCK_TRUNC).collect()
    }
}

/// Build the delta + numeric_change + magnitude for a modified pair.
///
/// - If both sides carry a comparable typed value (price/number), emit a `val` delta on the
///   *canonicalized display* (so a utm-only Link change is suppressed: equal canonical → no unit;
///   handled before this is called) and a typed `numeric_change`.
/// - For an attribute (Link) change, compare on the POST-canonicalization `href_canonical`.
/// - Otherwise emit a Myers `idiff` over the normalized text (§7.2).
fn build_modified_delta(ob: &Block, nb: &Block) -> (Delta, Option<NumericChange>, f32) {
    // Numeric/price typed value → val + numeric_change.
    if let (Some(from), Some(to)) = (numeric_of(ob), numeric_of(nb)) {
        let pct = pct_change(from, to);
        let mag = (pct.abs().min(1.0)) as f32;
        let delta = Delta::Val {
            a: nb.text.clone(),
            b: ob.text.clone(),
        };
        return (delta, Some(NumericChange { from, to, pct }), mag);
    }

    // Link attribute change: compare canonicalized href (already §5.3-canonicalized by normalize).
    if let (Some(TypedValue::Link { href_canonical: ha }), Some(TypedValue::Link { href_canonical: hb })) =
        (&ob.value, &nb.value)
    {
        let delta = Delta::Val {
            a: hb.clone(),
            b: ha.clone(),
        };
        // Magnitude: a host/CDN swap is a full-token change; treat as high.
        return (delta, None, 1.0);
    }

    // General prose: Myers token idiff (§7.2).
    let ops = intra::token_diff(&ob.text, &nb.text);
    let mag = text_magnitude(&ops);
    (Delta::Idiff { ops }, None, mag)
}

/// Extract a comparable numeric value (as f64 for the ratio) from a block's typed value.
fn numeric_of(b: &Block) -> Option<f64> {
    match &b.value {
        Some(TypedValue::Price { amount_minor, .. }) => Some(*amount_minor as f64 / 100.0),
        Some(TypedValue::Number(n)) => n.to_string().parse::<f64>().ok(),
        _ => None,
    }
}

/// `pct = (to − from) / max(|from|, ε)` (signed; the §7.5 numeric_change carries direction).
fn pct_change(from: f64, to: f64) -> f64 {
    let denom = from.abs().max(1e-9);
    (to - from) / denom
}

/// Token-level text magnitude (§8.1 `mag`): `1 − exp(−3·r)` where `r` is the edited-token ratio.
fn text_magnitude(ops: &[crate::model::IdiffOp]) -> f32 {
    use crate::model::DiffOp;
    let mut edited = 0usize;
    let mut total = 0usize;
    for o in ops {
        let toks = o.text.split_whitespace().count().max(1);
        total += toks;
        if o.op != DiffOp::Keep {
            edited += toks;
        }
    }
    if total == 0 {
        return 0.0;
    }
    let r = edited as f32 / total as f32;
    1.0 - (-3.0 * r).exp()
}

/// Finalize: compute the noise score for each draft, build the §7.5 features, and emit the
/// `ChangeUnit`s in deterministic order (drafts are pushed in a deterministic order already; we
/// re-sort by `(slot_key, ct)` only to guarantee stability regardless of phase interleaving).
fn finalize(drafts: Vec<UnitDraft>) -> Changeset {
    let mut units: Vec<ChangeUnit> = drafts
        .into_iter()
        .map(|d| {
            let noise = noise::noise_score(&NoiseInput {
                ct: d.ct,
                block_type: d.block_type,
                before_text: &d.before_text,
                after_text: &d.after_text,
                delta: &d.delta,
            });
            ChangeUnit {
                slot_key: d.slot_key,
                ct: d.ct,
                delta: d.delta,
                noise_score: noise,
                sim: d.sim,
                features: Features {
                    magnitude: d.magnitude,
                    noise_score: noise,
                    segment_stability: 1.0,
                    novelty: 1.0,
                    moved: d.moved,
                },
                numeric_change: d.numeric_change,
                block_type: d.block_type,
                from_norm_hash: d.from_norm_hash,
                to_norm_hash: d.to_norm_hash,
            }
        })
        .collect();

    // Deterministic output order: by slot_key then change-type discriminant (stable, arch-free).
    units.sort_by(|a, b| {
        a.slot_key
            .cmp(&b.slot_key)
            .then((a.ct as u8).cmp(&(b.ct as u8)))
    });
    Changeset { units }
}

#[cfg(test)]
mod tests;
