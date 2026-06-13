//! §6.3 cascade clustering — collapse a container's mass child set-change into ONE bounded
//! `enc:"struct"` event instead of emitting (or, at the top-k cap, silently dropping) one event per
//! changed child.
//!
//! The §6.1 rule-3 contract: a single event is bounded, and a cascade of changed children is capped
//! at `max_children` (default 32). When a container block (in this segmenter, a [`Table`] whose
//! `<tr>` descendants are [`TableRow`] children, §5.4) has MORE than `max_children` changed direct
//! children, the per-child units are replaced by one [`Delta::Struct`] on the container's `slot_key`,
//! carrying `{added, removed, modified}` counts, ≤[`STRUCT_SAMPLE`] representative child deltas, and
//! a `truncated` remainder. This bounds both the event count and the wire size for a 200-row
//! inventory table that churns wholesale.
//!
//! Scope: clustering keys on the block tree's container→child edges, so it covers exactly the
//! containers the segmenter materializes (tables). List items segment as flat siblings with no
//! container block (§5.4), so they are not clustered — a faithful consequence of the tree shape, not
//! a special case. Pure: `fn(units, &old, &new) -> units`; no clock/RNG/IO; deterministic output
//! order (sorted by `(slot_key, ct)`), independent of map iteration order.
//!
//! [`Table`]: crate::model::BlockType::Table
//! [`TableRow`]: crate::model::BlockType::TableRow

use crate::diff::{ChangeUnit, Features};
use crate::model::{Block, BlockType, CanonicalDoc, ChangeType, Delta, NormHash, SlotKey};
use rustc_hash::FxHashMap;

/// §6.1 / App C — changed children at/below this count emit as individual events; beyond it the
/// cluster degrades to one `enc:"struct"` summary delta.
pub const MAX_CHILDREN: usize = 32;
/// §6.3 — a `struct` summary carries at most this many representative child deltas (`sample`).
pub const STRUCT_SAMPLE: usize = 8;

/// Per-container metadata recovered from a tree (the container's own type + `norm_hash`, used to
/// type the cluster unit and to derive its idempotency `event_key`).
#[derive(Clone, Copy)]
struct ContainerInfo {
    ty: BlockType,
    norm_hash: NormHash,
}

/// Walk a block tree, recording each container's children → container `slot_key` edge and the
/// container's metadata. Only blocks that HAVE children are containers (in this segmenter, tables),
/// so a flat block contributes no edge and is never clustered.
fn index_tree(
    blocks: &[Block],
    parent: Option<SlotKey>,
    parent_of: &mut FxHashMap<SlotKey, SlotKey>,
    containers: &mut FxHashMap<SlotKey, ContainerInfo>,
) {
    for b in blocks {
        if let Some(p) = parent {
            parent_of.insert(b.slot_key, p);
        }
        if !b.children.is_empty() {
            containers.insert(b.slot_key, ContainerInfo { ty: b.ty, norm_hash: b.norm_hash });
            index_tree(&b.children, Some(b.slot_key), parent_of, containers);
        }
    }
}

/// §6.3 — collapse over-`max_children` container child-sets into `enc:"struct"` units. Returns the
/// units unchanged (and allocation-free aside from the two tree indexes) when no cascade qualifies.
pub fn cluster_cascades(
    units: Vec<ChangeUnit>,
    old: &CanonicalDoc,
    new: &CanonicalDoc,
) -> Vec<ChangeUnit> {
    let mut old_parent = FxHashMap::default();
    let mut old_cont = FxHashMap::default();
    index_tree(&old.blocks, None, &mut old_parent, &mut old_cont);
    let mut new_parent = FxHashMap::default();
    let mut new_cont = FxHashMap::default();
    index_tree(&new.blocks, None, &mut new_parent, &mut new_cont);

    // Group changed-child unit indices by their container slot. A removed unit's slot is the OLD
    // block's (look up the old tree); every other unit carries the NEW block's slot (new tree, with
    // the old tree as a fallback for a similarity-repaired pair whose new slot is absent here).
    let mut groups: FxHashMap<SlotKey, Vec<usize>> = FxHashMap::default();
    for (i, u) in units.iter().enumerate() {
        let parent = if u.ct == ChangeType::Removed {
            old_parent.get(&u.slot_key)
        } else {
            new_parent.get(&u.slot_key).or_else(|| old_parent.get(&u.slot_key))
        };
        if let Some(&p) = parent {
            groups.entry(p).or_default().push(i);
        }
    }

    // Containers whose changed-child count exceeds the cap, in deterministic slot order.
    let mut clustered: Vec<(SlotKey, Vec<usize>)> =
        groups.into_iter().filter(|(_, idxs)| idxs.len() > MAX_CHILDREN).collect();
    if clustered.is_empty() {
        return units; // fast path: nothing cascades — the common case.
    }
    clustered.sort_by_key(|a| a.0);

    let mut drop = vec![false; units.len()];
    let mut cluster_units: Vec<ChangeUnit> = Vec::with_capacity(clustered.len());
    for (container_slot, mut members) in clustered {
        // Most-material first (lowest noise), then slot_key — so the `sample` surfaces the changes
        // an agent most wants to see, deterministically.
        members.sort_by(|&a, &b| {
            units[a]
                .noise_score
                .partial_cmp(&units[b].noise_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(units[a].slot_key.cmp(&units[b].slot_key))
        });

        let mut added = 0u32;
        let mut removed = 0u32;
        let mut modified = 0u32;
        let mut sample = Vec::with_capacity(STRUCT_SAMPLE);
        let mut sum_mag = 0.0f32;
        let mut sum_noise = 0.0f32;
        let mut type_tally: Vec<(BlockType, usize)> = Vec::new();
        for &i in &members {
            drop[i] = true;
            match units[i].ct {
                ChangeType::Added => added += 1,
                ChangeType::Removed => removed += 1,
                // Modified / Reordered / Restyled all fold into the one `modified` count `struct`
                // exposes (§6.3 has no per-shape child breakdown beyond add/remove/modified).
                _ => modified += 1,
            }
            sum_mag += units[i].features.magnitude;
            sum_noise += units[i].noise_score;
            if sample.len() < STRUCT_SAMPLE {
                sample.push(units[i].delta.clone());
            }
            let ty = units[i].block_type;
            match type_tally.iter_mut().find(|(t, _)| *t == ty) {
                Some(e) => e.1 += 1,
                None => type_tally.push((ty, 1)),
            }
        }
        let truncated = (members.len() - sample.len()) as u32;
        // Aggregate the children's magnitude/noise by their MEAN (members is non-empty: len > cap).
        // Mean — not min/max — so the cluster's salience reflects the whole set: a table of mostly
        // cosmetic churn stays quiet even with one real row, and a table of mostly real moves stays
        // material even with a few noisy ones. Clustering runs before salience gating (pipeline.rs),
        // so a sub-threshold cluster is suppressed like any other sub-threshold change (§6.4).
        let n = members.len() as f32;
        let mean_mag = sum_mag / n;
        let mean_noise = sum_noise / n;

        // §6.2 `ct` reflects the container's state: a wholesale add/remove of its children IS an
        // add/remove of the container; a mix (or in-place edits) is a modification.
        let ct = if removed > 0 && added == 0 && modified == 0 {
            ChangeType::Removed
        } else if added > 0 && removed == 0 && modified == 0 {
            ChangeType::Added
        } else {
            ChangeType::Modified
        };

        // The container block itself usually appears as its own (oversized) unit, because its
        // aggregate text changed when its rows did — replace it with the struct rather than emit both.
        if let Some(ci) = units.iter().position(|u| u.slot_key == container_slot) {
            drop[ci] = true;
        }

        // Type the cluster as its dominant child type (table rows → `table_row` weight) so salience
        // treats it like the row-level change it summarizes, not as the bare container.
        type_tally.sort_by(|a, b| b.1.cmp(&a.1).then((a.0 as u8).cmp(&(b.0 as u8))));
        let block_type = type_tally
            .first()
            .map(|(t, _)| *t)
            .or_else(|| new_cont.get(&container_slot).map(|c| c.ty))
            .unwrap_or(BlockType::TableRow);

        let from_norm_hash =
            old_cont.get(&container_slot).map(|c| c.norm_hash).unwrap_or_else(|| NormHash::of(""));
        let to_norm_hash =
            new_cont.get(&container_slot).map(|c| c.norm_hash).unwrap_or_else(|| NormHash::of(""));

        cluster_units.push(ChangeUnit {
            slot_key: container_slot,
            ct,
            delta: Delta::Struct { added, removed, modified, sample, truncated },
            noise_score: mean_noise,
            sim: 1.0,
            features: Features {
                magnitude: mean_mag,
                noise_score: mean_noise,
                segment_stability: 1.0,
                novelty: 1.0,
                moved: false,
            },
            numeric_change: None,
            block_type,
            from_norm_hash,
            to_norm_hash,
        });
    }

    let mut out: Vec<ChangeUnit> = units
        .into_iter()
        .zip(drop)
        .filter_map(|(u, dropped)| (!dropped).then_some(u))
        .collect();
    out.extend(cluster_units);
    // Same deterministic ordering finalize() emits (slot_key, then change-type discriminant).
    out.sort_by(|a, b| a.slot_key.cmp(&b.slot_key).then((a.ct as u8).cmp(&(b.ct as u8))));
    out
}
