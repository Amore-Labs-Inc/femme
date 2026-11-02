//! A tiny deterministic PRNG for synthetic bench data (fake transcripts, fire, shell scroll).
//! Not for crypto, not for anything that must be unpredictable.

/// xorshift64*, deterministic, not for crypto.
///
/// The same seed always yields the same sequence, on every platform.
#[derive(Clone, Debug)]
pub struct Rng(u64);

/// splitmix64 finaliser: spreads any seed (including 0 and small consecutive integers) over
/// the whole state space, so nearby seeds give unrelated sequences.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Rng {
    /// Creates a generator from any seed, 0 included (the seed is mixed through splitmix64, and
    /// the one seed that would mix to the forbidden all-zero state is remapped).
    pub fn new(seed: u64) -> Self {
        match splitmix64(seed) {
            0 => Rng(0x9E37_79B9_7F4A_7C15),
            s => Rng(s),
        }
    }

    /// Next 64 uniformly distributed bits.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..n` (multiply-shift, bias below 2^-32); `n == 0` returns 0.
    pub fn below(&mut self, n: u32) -> u32 {
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u32
    }

    /// Uniform in `lo..=hi`; returns `lo` when `hi <= lo`.
    pub fn range(&mut self, lo: u32, hi: u32) -> u32 {
        if hi <= lo {
            return lo;
        }
        let span = u128::from(hi - lo) + 1;
        lo + ((u128::from(self.next_u64()) * span) >> 64) as u32
    }

    /// True with probability `num / den` (always false when `den == 0`, always true when `num >= den > 0`).
    pub fn chance(&mut self, num: u32, den: u32) -> bool {
        den != 0 && self.below(den) < num
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_per_seed() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        let mut c = Rng::new(43);
        let xs: Vec<u64> = (0..16).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..16).map(|_| b.next_u64()).collect();
        let zs: Vec<u64> = (0..16).map(|_| c.next_u64()).collect();
        assert_eq!(xs, ys);
        assert_ne!(xs, zs);
    }

    #[test]
    fn seed_zero_works() {
        let mut r = Rng::new(0);
        let xs: Vec<u64> = (0..8).map(|_| r.next_u64()).collect();
        assert!(xs.iter().all(|&x| x != 0));
        assert_ne!(xs[0], xs[1]);
    }

    #[test]
    fn below_bounds_and_coverage() {
        let mut r = Rng::new(7);
        assert_eq!(r.below(0), 0);
        assert_eq!(r.below(1), 0);
        let mut seen = [0u32; 10];
        for _ in 0..10_000 {
            let v = r.below(10);
            assert!(v < 10);
            seen[v as usize] += 1;
        }
        // Each bucket expects 1000; a fair generator stays well inside 800..1200.
        assert!(seen.iter().all(|&n| (800..1200).contains(&n)), "{seen:?}");
    }

    #[test]
    fn range_is_inclusive() {
        let mut r = Rng::new(1);
        let (mut lo_seen, mut hi_seen) = (false, false);
        for _ in 0..1_000 {
            let v = r.range(3, 5);
            assert!((3..=5).contains(&v));
            lo_seen |= v == 3;
            hi_seen |= v == 5;
        }
        assert!(lo_seen && hi_seen);
        assert_eq!(r.range(9, 9), 9);
        assert_eq!(r.range(9, 2), 9);
        // Full span must not overflow.
        let _ = r.range(0, u32::MAX);
    }

    #[test]
    fn chance_edges_and_rate() {
        let mut r = Rng::new(3);
        assert!(!r.chance(1, 0));
        assert!(!r.chance(0, 5));
        assert!(r.chance(5, 5));
        assert!(r.chance(9, 5));
        let hits = (0..10_000).filter(|_| r.chance(3, 10)).count();
        assert!((2_700..3_300).contains(&hits), "{hits}");
    }
}
