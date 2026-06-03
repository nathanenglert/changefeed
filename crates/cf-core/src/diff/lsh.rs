//! §7.1 transient in-RAM MinHash band — fixed seed constants (deterministic permutations), built
//! transient and discarded, NEVER persisted (§4 determinism rule 5). Bounds the similarity-fill
//! candidate set for low-anchor (rename-everything) diffs.
//!
//! Each residual block is reduced to a small token-shingle set (token hashes, see
//! [`crate::diff::intra::token_hashes`]). For each MinHash seed we keep the minimum permuted token
//! hash; blocks that collide on a seed share a bucket and become mutual candidates. Because the
//! caller restricts candidates further by block type and a `±W` position window, the band caps the
//! per-block candidate set to a small constant — i.e. `O(B·b)` similarity, never `O(B²)`.

/// Fixed seed constants for the MinHash permutations — deterministic across arch and run.
pub const MINHASH_SEEDS: [u64; 8] = [
    0x9e37_79b9_7f4a_7c15,
    0xc2b2_ae3d_27d4_eb4f,
    0x1656_67b1_9e37_79f9,
    0xa0761d6478bd642f_u64,
    0xe7037ed1a0b428db_u64,
    0x8ebc6af09c88c6e3_u64,
    0x589965cc75374cc3_u64,
    0x1d8e4e27c47d124f_u64,
];

/// A transient band of LSH buckets over residual blocks (smallvec for small bucket counts).
#[derive(Clone, Debug, Default)]
pub struct LshBand {
    /// One bucket per (seed, min-value) group; each holds the residual indices that collided.
    pub buckets: Vec<smallvec::SmallVec<[usize; 4]>>,
}

/// A per-block MinHash signature: one minimum permuted hash per [`MINHASH_SEEDS`] entry.
type Signature = [u64; MINHASH_SEEDS.len()];

/// §7.1 candidate-set bound. A bucket holding more than `MAX_BUCKET` members means that many
/// residual blocks share a MinHash signature — i.e. near-identical boilerplate drawn from one small
/// recurring vocabulary (re-sorted lists, permuted tags/breadcrumbs, faceted/glossary pages).
/// Expanding such a bucket all-pairs is both `O(n²)` — the very `O(B²)` blowup §7.1 forbids — AND
/// pointless: indistinguishable blocks are best paired in document order, which the caller's bounded
/// `±W` window path already does. So an oversized bucket is dropped. Keeping the cap a small constant
/// (a few dozen) is precisely what makes the band's contribution `O(B·b)` rather than `O(B²)`.
pub const MAX_BUCKET: usize = 32;

/// §7.1 — build the transient MinHash band over residual token-sets and group collision candidates.
///
/// `token_sets[i]` is the shingle (token-hash) multiset of residual block `i` (duplicates are
/// harmless — MinHash uses the min). Returns a band whose buckets group residual indices that share
/// at least one MinHash value on the same seed band.
pub fn build_band(token_sets: &[Vec<u64>]) -> LshBand {
    use rustc_hash::FxHashMap;

    let sigs: Vec<Signature> = token_sets.iter().map(|s| signature(s)).collect();

    // Bucket key = (band_position, min_hash_value). Group residual indices that collide on any band.
    let mut by_key: FxHashMap<(usize, u64), smallvec::SmallVec<[usize; 4]>> = FxHashMap::default();
    for (i, sig) in sigs.iter().enumerate() {
        for (band_pos, &v) in sig.iter().enumerate() {
            // An empty token set yields u64::MAX everywhere; do not bucket empties together (they
            // are not similar to one another, just both empty).
            if v == u64::MAX {
                continue;
            }
            by_key.entry((band_pos, v)).or_default().push(i);
        }
    }

    // Deterministic bucket order: sort the keys so the resulting `buckets` vec is arch-stable. We
    // emit only buckets with ≥2 members (a singleton yields no candidate pair) and ≤ MAX_BUCKET
    // members (an oversized bucket is non-distinctive boilerplate — dropped to hold the §7.1 O(B·b)
    // bound; the caller's ±W window still pairs those blocks in document order).
    let mut keys: Vec<(usize, u64)> = by_key.keys().copied().collect();
    keys.sort_unstable();
    let mut buckets = Vec::new();
    for k in keys {
        let mut members = by_key.remove(&k).unwrap();
        if (2..=MAX_BUCKET).contains(&members.len()) {
            members.sort_unstable();
            buckets.push(members);
        }
    }

    LshBand { buckets }
}

/// Compute a residual block's MinHash signature from its token-hash set.
fn signature(tokens: &[u64]) -> Signature {
    let mut sig = [u64::MAX; MINHASH_SEEDS.len()];
    for &t in tokens {
        for (s, seed) in MINHASH_SEEDS.iter().enumerate() {
            let permuted = mix(t ^ seed);
            if permuted < sig[s] {
                sig[s] = permuted;
            }
        }
    }
    sig
}

/// A fast deterministic 64-bit mixer (splitmix64 finalizer) used as the MinHash permutation. Pure
/// arithmetic — no RNG, no clock; identical on every arch.
#[inline]
fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_token_sets_collide() {
        let a = vec![1u64, 2, 3, 4, 5];
        let band = build_band(&[a.clone(), a.clone()]);
        // The two identical sets must land in at least one shared bucket containing both 0 and 1.
        assert!(band
            .buckets
            .iter()
            .any(|b| b.contains(&0) && b.contains(&1)));
    }

    #[test]
    fn disjoint_token_sets_do_not_collide() {
        let a = vec![1u64, 2, 3, 4, 5];
        let b = vec![100u64, 200, 300, 400, 500];
        let band = build_band(&[a, b]);
        // No bucket should pair the two disjoint blocks (overwhelmingly likely; mixer is fixed).
        assert!(!band
            .buckets
            .iter()
            .any(|bk| bk.contains(&0) && bk.contains(&1)));
    }

    #[test]
    fn empty_sets_are_not_bucketed_together() {
        let band = build_band(&[vec![], vec![]]);
        assert!(band.buckets.is_empty(), "two empty token sets are not mutual candidates");
    }

    #[test]
    fn oversized_buckets_are_dropped_but_small_ones_survive() {
        // §7.1 bound: many residual blocks sharing one token set all collide on every band. Without
        // the MAX_BUCKET cap, each band-bucket would hold them all and the caller's all-pairs
        // expansion would be O(n²) — the blowup that wedged a 4k-block diff for ~100s.
        let boiler = vec![1u64, 2, 3, 4, 5];
        let needle = vec![900u64, 901, 902, 903];
        let n0 = MAX_BUCKET * 2;
        let mut sets: Vec<Vec<u64>> = (0..n0).map(|_| boiler.clone()).collect();
        sets.push(needle.clone()); // index n0
        sets.push(needle.clone()); // index n0 + 1
        let band = build_band(&sets);
        // No emitted bucket exceeds the cap, so Σ bucket_size² is bounded → never O(B²).
        assert!(
            band.buckets.iter().all(|b| b.len() <= MAX_BUCKET),
            "largest bucket = {}",
            band.buckets.iter().map(|b| b.len()).max().unwrap_or(0)
        );
        // ...yet a genuine small similar-pair still collides and is retained.
        assert!(
            band.buckets.iter().any(|b| b.contains(&n0) && b.contains(&(n0 + 1))),
            "the distinct needle pair is still bucketed"
        );
    }

    #[test]
    fn band_is_deterministic() {
        let sets = vec![vec![1u64, 2, 3], vec![1u64, 2, 9], vec![7u64, 8]];
        let a = build_band(&sets);
        let b = build_band(&sets);
        assert_eq!(a.buckets, b.buckets, "LSH band is byte-stable across runs");
    }
}
