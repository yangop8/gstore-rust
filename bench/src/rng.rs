//! A small, portable, seedable pseudo-random generator: SplitMix64.
//!
//! The generated data must be a pure function of the workload parameters on
//! every platform and compiler version, so the harness carries its own
//! generator instead of depending on an external crate whose stream could
//! change. SplitMix64 (Steele, Lea and Flood, "Fast splittable pseudorandom
//! number generators", OOPSLA 2014) is tiny, fast and passes BigCrush; bounded
//! integers use Lemire's nearly divisionless method ("Fast Random Integer
//! Generation in an Interval", ACM TOMACS 2019), which is unbiased.
//!
//! The streams are pinned by golden-value tests (`tests` below and
//! `bench/tests/golden.rs`): changing anything here changes the data, the
//! data hash in every baseline and therefore requires a new generator
//! version (see [`crate::lubm::VERSION`]).

/// The SplitMix64 increment (the 64-bit golden ratio).
pub const GOLDEN_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

/// The SplitMix64 output function: a strong 64-bit mixer (Stafford's
/// variant 13). Also used to derive independent stream seeds.
#[inline]
pub fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A SplitMix64 stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// A stream starting from `seed` (any value, including 0).
    pub fn new(seed: u64) -> SplitMix64 {
        SplitMix64 { state: seed }
    }

    /// The next 64 random bits.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GOLDEN_GAMMA);
        mix64(self.state)
    }

    /// A uniform integer in `0..n` (Lemire's method; unbiased). `n` must be
    /// non-zero.
    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "SplitMix64::below(0)");
        let mut m = u128::from(self.next_u64()) * u128::from(n);
        let mut low = m as u64;
        if low < n {
            // The threshold 2^64 mod n.
            let t = n.wrapping_neg() % n;
            while low < t {
                m = u128::from(self.next_u64()) * u128::from(n);
                low = m as u64;
            }
        }
        (m >> 64) as u64
    }

    /// A uniform index in `0..n` as `usize`.
    #[inline]
    pub fn index(&mut self, n: usize) -> usize {
        self.below(n as u64) as usize
    }

    /// A uniform integer in `lo..=hi` (Python's `randint`). Requires `lo <= hi`.
    #[inline]
    pub fn range_inclusive(&mut self, lo: u64, hi: u64) -> u64 {
        assert!(lo <= hi, "SplitMix64::range_inclusive({lo}, {hi})");
        match (hi - lo).checked_add(1) {
            Some(span) => lo + self.below(span),
            None => self.next_u64(),
        }
    }

    /// A uniform float in `[0, 1)` with 53 random bits.
    #[inline]
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// A uniform float in `[a, b)` (Python's `uniform`).
    #[inline]
    pub fn uniform(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.next_f64()
    }

    /// `k` distinct values of `0..n` in random order (Python's
    /// `sample(range(n), k)`). Requires `k <= n`.
    ///
    /// Small samples (`2k <= n`) draw and reject duplicates; larger ones run a
    /// partial Fisher–Yates shuffle over `0..n`. Both are deterministic
    /// functions of the stream.
    pub fn sample(&mut self, n: usize, k: usize) -> Vec<usize> {
        assert!(k <= n, "SplitMix64::sample: k={k} > n={n}");
        if k == 0 {
            return Vec::new();
        }
        if 2 * k <= n {
            let mut out: Vec<usize> = Vec::with_capacity(k);
            while out.len() < k {
                let x = self.index(n);
                if !out.contains(&x) {
                    out.push(x);
                }
            }
            out
        } else {
            let mut pool: Vec<usize> = (0..n).collect();
            for i in 0..k {
                let j = i + self.index(n - i);
                pool.swap(i, j);
            }
            pool.truncate(k);
            pool
        }
    }

    /// Shuffle `items` in place (Fisher–Yates, from the back).
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.index(i + 1);
            items.swap(i, j);
        }
    }
}

/// An independent stream seed for (`seed`, `tag`, `a`, `b`): every argument
/// passes through the mixer, so neighbouring universities or departments get
/// unrelated streams.
pub fn stream_seed(seed: u64, tag: u64, a: u64, b: u64) -> u64 {
    let h = mix64(seed ^ mix64(tag));
    let h = mix64(h ^ a.wrapping_mul(GOLDEN_GAMMA));
    mix64(h ^ b.wrapping_add(GOLDEN_GAMMA))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_reference_values() {
        // The reference SplitMix64 stream for seed 0 (as published with the
        // algorithm, e.g. in xoshiro's seeding code).
        let mut r = SplitMix64::new(0);
        assert_eq!(r.next_u64(), 0xe220_a839_7b1d_cdaf);
        assert_eq!(r.next_u64(), 0x6e78_9e6a_a1b9_65f4);
        assert_eq!(r.next_u64(), 0x06c4_5d18_8009_454f);
    }

    #[test]
    fn below_is_in_range_and_covers_it() {
        let mut r = SplitMix64::new(7);
        let mut seen = [false; 7];
        for _ in 0..1000 {
            let x = r.below(7);
            assert!(x < 7);
            seen[x as usize] = true;
        }
        assert!(seen.iter().all(|&s| s));
        assert_eq!(r.range_inclusive(5, 5), 5);
        let x = r.range_inclusive(0, u64::MAX);
        let _ = x;
    }

    #[test]
    fn sample_is_distinct_and_complete() {
        let mut r = SplitMix64::new(1);
        for (n, k) in [(10, 0), (10, 3), (10, 7), (10, 10), (1, 1), (1000, 5)] {
            let s = r.sample(n, k);
            assert_eq!(s.len(), k);
            let mut sorted = s.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), k, "{s:?}");
            assert!(s.iter().all(|&x| x < n));
        }
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut r = SplitMix64::new(3);
        let mut v: Vec<u32> = (0..50).collect();
        r.shuffle(&mut v);
        let mut s = v.clone();
        s.sort_unstable();
        assert_eq!(s, (0..50).collect::<Vec<_>>());
        assert_ne!(v, s);
    }

    #[test]
    fn floats_are_in_unit_interval() {
        let mut r = SplitMix64::new(9);
        for _ in 0..1000 {
            let f = r.next_f64();
            assert!((0.0..1.0).contains(&f));
            let u = r.uniform(0.2, 0.25);
            assert!((0.2..0.25).contains(&u));
        }
    }
}
