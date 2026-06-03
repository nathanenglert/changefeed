//! §7.1 slot_key-anchor pairing — `O(B)` `FxHashMap<SlotKey, _>` join (Copy keys, fixed seed,
//! deterministic). Phase 1 of the diff: pair blocks whose text-free `slot_key` matches.
//!
//! Because `slot_key` is text-free (§5.4), **a block whose text was edited still anchors here** (its
//! key is unchanged) — so a `modified` block is matched in phase 1, NOT pushed to similarity
//! fallback. Phase 3 (similarity) only sees blocks whose `slot_key` itself changed, or genuinely
//! new/removed blocks.
//!
//! A `slot_key` is a *certain* anchor only when it appears **exactly once on each side** (DESIGN
//! §7.1). A key that repeats on either side is ambiguous and its blocks are pushed to the residual
//! for windowed similarity matching (the structural ordinal scheme normally makes same-section
//! same-type blocks distinct, so repeats are rare, but we handle them safely).

use rustc_hash::FxHashMap;

use crate::model::{Block, SlotKey};

/// A 1:1 pairing of prior→new block indices that share a `slot_key`, plus the unmatched residuals
/// on each side (handed to LIS / similarity-fill). Indices are into the *flattened pre-order*
/// sequences the caller passes in.
#[derive(Clone, Debug, Default)]
pub struct Alignment {
    /// `(old_idx, new_idx)` certain anchors, in ascending `old_idx` order.
    pub paired: Vec<(usize, usize)>,
    /// Prior-side indices with no certain anchor (slot_key changed / removed / ambiguous).
    pub old_residual: Vec<usize>,
    /// New-side indices with no certain anchor (slot_key changed / added / ambiguous).
    pub new_residual: Vec<usize>,
}

/// §7.1 phase 1 — pair on `slot_key` via a fixed-seed `FxHashMap`. Two passes, `O(B)`.
///
/// Only keys that occur exactly once on each side become certain anchors; all other blocks land in
/// the residuals. The `paired` list is sorted by `old_idx` (which, since the caller flattens in
/// pre-order, is the deterministic `(preorder_idx, slot_key)` order, §4 rule 5).
pub fn align(old: &[&Block], new: &[&Block]) -> Alignment {
    // Count occurrences and remember the (last) index per key on each side. We need both the count
    // (to detect uniqueness) and the index (to pair). A key seen once stores its index. Reserving up
    // front avoids the incremental rehash/regrow churn on a 5k-block page.
    let mut old_idx: FxHashMap<SlotKey, OccIndex> = FxHashMap::default();
    old_idx.reserve(old.len());
    for (i, b) in old.iter().enumerate() {
        old_idx.entry(b.slot_key).or_default().observe(i);
    }
    let mut new_idx: FxHashMap<SlotKey, OccIndex> = FxHashMap::default();
    new_idx.reserve(new.len());
    for (i, b) in new.iter().enumerate() {
        new_idx.entry(b.slot_key).or_default().observe(i);
    }

    let mut paired: Vec<(usize, usize)> = Vec::with_capacity(old.len().min(new.len()));
    let mut old_paired = vec![false; old.len()];
    let mut new_paired = vec![false; new.len()];

    for (i, b) in old.iter().enumerate() {
        if let Some(o) = old_idx.get(&b.slot_key) {
            if o.count == 1 {
                if let Some(n) = new_idx.get(&b.slot_key) {
                    if n.count == 1 {
                        let ni = n.index;
                        paired.push((i, ni));
                        old_paired[i] = true;
                        new_paired[ni] = true;
                    }
                }
            }
        }
    }

    // `paired` is already in ascending `old_idx` order — we pushed it while walking `old` in order —
    // so no sort is needed (this is the deterministic `(preorder_idx, slot_key)` order, §4 rule 5).
    debug_assert!(paired.windows(2).all(|w| w[0].0 <= w[1].0));

    let old_residual = (0..old.len()).filter(|&i| !old_paired[i]).collect();
    let new_residual = (0..new.len()).filter(|&i| !new_paired[i]).collect();

    Alignment {
        paired,
        old_residual,
        new_residual,
    }
}

/// Occurrence tracker for a `slot_key` on one side: count + the index of the (single) occurrence.
#[derive(Default, Clone, Copy)]
struct OccIndex {
    count: u32,
    index: usize,
}

impl OccIndex {
    fn observe(&mut self, i: usize) {
        if self.count == 0 {
            self.index = i;
        }
        self.count += 1;
    }
}
