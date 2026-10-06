//! The client's `Random_*` generator (Park–Miller "minimal standard" with
//! multiplier 48271), including its wrap-around quirk.

pub const DEFAULT_SEED: u32 = 0x075B_D924;

#[derive(Debug, Clone)]
pub struct Random {
    state: u32,
}

impl Random {
    /// `Random_Init`: seed 0 selects [`DEFAULT_SEED`].
    pub fn new(seed: u32) -> Self {
        Self { state: if seed == 0 { DEFAULT_SEED } else { seed } }
    }

    /// `Random_NextUInt`.
    pub fn next_u32(&mut self) -> u32 {
        let s = self.state;
        let mut v = s.wrapping_mul(48271).wrapping_sub((s / 44488).wrapping_mul(0x7FFF_FFFF));
        if v > 0x7FFF_FFFF {
            // The client adds 2^31 here, not 2^31 - 1.
            v = v.wrapping_add(0x8000_0000);
        }
        if v == 0 {
            v = DEFAULT_SEED;
        }
        self.state = v;
        v
    }

    /// The client's Fisher–Yates variant: for every index from the end down
    /// to 0, swap with `next() % len` (not `% (i + 1)`).
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        let len = items.len() as u32;
        for i in (0..items.len()).rev() {
            let j = (self.next_u32() % len) as usize;
            items.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_minimal_standard_for_small_states() {
        // While no wrap-around happens this is plain 48271 * s mod (2^31 - 1).
        let mut r = Random::new(1);
        assert_eq!(r.next_u32(), 48271);
        assert_eq!(r.next_u32(), (48271u64 * 48271 % 0x7FFF_FFFF) as u32);
    }
}
