//! §7.1 patience-sort LIS over anchor positions — detects reorders within the slot_key-paired set.
//! Tie-breaks resolve deterministically (the caller supplies positions already ordered by the
//! prior-side `(preorder_idx, slot_key)`, §4 determinism rule 5), so equal-position ambiguity
//! cannot arise: positions are distinct new-side indices.
//!
//! Given the anchors sorted by their *prior-side* order, their *new-side* positions form a sequence;
//! the longest increasing subsequence of that sequence is the stable spine (the maximal set of
//! anchors that kept their relative order). Anchors NOT on the spine are matched-but-moved → reorder
//! candidates. This is exactly `git`'s patience/histogram anchoring. `O(A log A)`.

/// Compute the indices (into `positions`) of a longest **strictly increasing** subsequence
/// (patience sort with predecessor links). Returns the indices in ascending order. For ties in
/// length the algorithm is deterministic: it always extends/replaces with the standard
/// lower-bound rule over distinct positions, so the same input yields the same spine on any arch.
///
/// Indices NOT in the returned set are the moved anchors (§7.1).
pub fn longest_increasing(positions: &[u32]) -> Vec<usize> {
    let n = positions.len();
    if n == 0 {
        return Vec::new();
    }

    // `tails[k]` = index into `positions` of the smallest tail of an increasing subsequence of
    // length k+1 seen so far. `prev[i]` links each element to its predecessor in the LIS ending at i.
    let mut tails: Vec<usize> = Vec::with_capacity(n);
    let mut prev: Vec<Option<usize>> = vec![None; n];

    for i in 0..n {
        let x = positions[i];
        // Lower-bound: first pile whose tail value is >= x (strictly increasing => replace ties).
        let mut lo = 0usize;
        let mut hi = tails.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if positions[tails[mid]] < x {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo > 0 {
            prev[i] = Some(tails[lo - 1]);
        }
        if lo == tails.len() {
            tails.push(i);
        } else {
            tails[lo] = i;
        }
    }

    // Reconstruct from the last pile's tail back through `prev`.
    let mut lis = Vec::with_capacity(tails.len());
    let mut cur = tails.last().copied();
    while let Some(i) = cur {
        lis.push(i);
        cur = prev[i];
    }
    lis.reverse();
    lis
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_empty() {
        assert!(longest_increasing(&[]).is_empty());
    }

    #[test]
    fn already_sorted_keeps_everything() {
        let idx = longest_increasing(&[0, 1, 2, 3, 4]);
        assert_eq!(idx, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn classic_lis_length_and_indices() {
        // positions = [2,1,4,3,5] -> an LIS is [1,4,5] (values) at indices [1,2,4] or [2,3,4]/etc.
        // The strictly-increasing patience rule yields a deterministic length-3 spine.
        let pos = [2u32, 1, 4, 3, 5];
        let idx = longest_increasing(&pos);
        assert_eq!(idx.len(), 3, "LIS length");
        // The reconstructed values are strictly increasing.
        let vals: Vec<u32> = idx.iter().map(|&i| pos[i]).collect();
        assert!(vals.windows(2).all(|w| w[0] < w[1]), "values strictly increasing: {vals:?}");
    }

    #[test]
    fn single_moved_block_falls_off_spine() {
        // A block moved to the front: positions [4,0,1,2,3]. Spine is [0,1,2,3] (indices 1..=4);
        // index 0 (the moved block) is off-spine.
        let pos = [4u32, 0, 1, 2, 3];
        let idx = longest_increasing(&pos);
        assert_eq!(idx, vec![1, 2, 3, 4]);
        assert!(!idx.contains(&0));
    }

    #[test]
    fn reversed_keeps_only_one() {
        let idx = longest_increasing(&[4, 3, 2, 1, 0]);
        assert_eq!(idx.len(), 1);
    }
}
