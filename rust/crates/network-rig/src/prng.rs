//! The schedule's random numbers: SplitMix64, written out here.
//!
//! A replay must produce the same faults from the same seed on every machine
//! and with every version of this repository's dependencies, so the generator
//! is a fixed algorithm in this crate rather than a library's default, which
//! may change between releases.

/// A SplitMix64 generator (Steele, Lea and Flood, 2014).
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// A generator that starts from `seed`.
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `lo..=hi`. A reversed range yields `lo`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        let span = hi - lo;
        if span == u64::MAX {
            return self.next_u64();
        }
        lo + self.next_u64() % (span + 1)
    }

    /// An index chosen with probability proportional to its weight, or `None`
    /// when every weight is zero.
    pub fn weighted(&mut self, weights: &[u64]) -> Option<usize> {
        let total: u64 = weights.iter().sum();
        if total == 0 {
            return None;
        }
        let mut pick = self.range(0, total - 1);
        weights.iter().position(|&weight| {
            if pick < weight {
                true
            } else {
                pick -= weight;
                false
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_reference_sequence() {
        // The reference outputs of SplitMix64 for seed 1234567.
        let mut rng = SplitMix64::new(1_234_567);
        let got: Vec<u64> = (0..5).map(|_| rng.next_u64()).collect();
        assert_eq!(
            got,
            vec![
                6_457_827_717_110_365_317,
                3_203_168_211_198_807_973,
                9_817_491_932_198_370_423,
                4_593_380_528_125_082_431,
                16_408_922_859_458_223_821,
            ]
        );
    }

    #[test]
    fn range_stays_inside_its_bounds() {
        let mut rng = SplitMix64::new(7);
        for _ in 0..10_000 {
            let value = rng.range(300, 1000);
            assert!((300..=1000).contains(&value));
        }
        assert_eq!(rng.range(5, 5), 5);
        assert_eq!(rng.range(9, 2), 9);
    }

    #[test]
    fn weighted_never_picks_a_zero_weight() {
        let mut rng = SplitMix64::new(11);
        for _ in 0..10_000 {
            assert_ne!(rng.weighted(&[3, 0, 1]), Some(1));
        }
        assert_eq!(rng.weighted(&[0, 0]), None);
    }
}
