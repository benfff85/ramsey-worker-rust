/// A fixed-size bitset optimized for graph operations on R(8,8) with 288 vertices.
/// Uses a fixed array of 5 u64s (320 bits) to enable Copy semantics - no heap allocation.
pub const BITSET_WORDS: usize = 5; // ceil(288/64) = 5
pub const BITSET_SIZE: usize = 288;

#[derive(Clone, Copy, Debug)]
pub struct BitMatrix {
    data: [u64; BITSET_WORDS],
}

impl BitMatrix {
    #[inline]
    pub const fn new() -> Self {
        BitMatrix {
            data: [0; BITSET_WORDS],
        }
    }

    #[inline]
    pub fn set(&mut self, bit: usize) {
        debug_assert!(bit < BITSET_SIZE);
        self.data[bit / 64] |= 1u64 << (bit % 64);
    }

    #[inline]
    pub fn clear(&mut self, bit: usize) {
        debug_assert!(bit < BITSET_SIZE);
        self.data[bit / 64] &= !(1u64 << (bit % 64));
    }

    #[inline]
    pub fn get(&self, bit: usize) -> bool {
        debug_assert!(bit < BITSET_SIZE);
        (self.data[bit / 64] & (1u64 << (bit % 64))) != 0
    }

    #[inline]
    pub fn flip(&mut self, bit: usize) {
        debug_assert!(bit < BITSET_SIZE);
        self.data[bit / 64] ^= 1u64 << (bit % 64);
    }

    /// Inverts all bits in the bitset using word-level XOR.
    /// Clears the padding bits (288-319) after inversion.
    #[inline]
    pub fn invert_all(&mut self) {
        for word in &mut self.data {
            *word = !*word;
        }
        // Clear padding bits beyond 288: 288 % 64 = 32
        // Mask keeps only bits 0-31 of the last word
        self.data[BITSET_WORDS - 1] &= (1u64 << (BITSET_SIZE % 64)) - 1;
    }

    /// Clear all bits at indices >= `threshold`. No-op if threshold >= BITSET_SIZE.
    /// Used after `invert_all()` when the active vertex count is smaller than BITSET_SIZE,
    /// so unused vertex slots don't appear as neighbors.
    #[inline]
    pub fn clear_above(&mut self, threshold: usize) {
        if threshold >= BITSET_SIZE {
            return;
        }
        let word_idx = threshold / 64;
        let bit_in_word = threshold % 64;
        if bit_in_word > 0 {
            // Keep only bits below `bit_in_word` in this word.
            let mask = (1u64 << bit_in_word) - 1;
            self.data[word_idx] &= mask;
            for i in (word_idx + 1)..BITSET_WORDS {
                self.data[i] = 0;
            }
        } else {
            for i in word_idx..BITSET_WORDS {
                self.data[i] = 0;
            }
        }
    }

    #[inline]
    pub fn cardinality(&self) -> u32 {
        self.data[0].count_ones()
            + self.data[1].count_ones()
            + self.data[2].count_ones()
            + self.data[3].count_ones()
            + self.data[4].count_ones()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data[0] == 0
            && self.data[1] == 0
            && self.data[2] == 0
            && self.data[3] == 0
            && self.data[4] == 0
    }

    /// Finds the index of the next set bit starting from `from_index`.
    #[inline]
    pub fn next_set_bit(&self, from_index: usize) -> Option<usize> {
        let mut word_idx = from_index / 64;

        if word_idx >= BITSET_WORDS {
            return None;
        }

        // Handle the first word (mask out lower bits)
        let bit_in_word = from_index % 64;
        let mask = !((1u64 << bit_in_word) - 1);
        let word = self.data[word_idx] & mask;

        if word != 0 {
            return Some(word_idx * 64 + word.trailing_zeros() as usize);
        }

        // Check subsequent words
        word_idx += 1;
        while word_idx < BITSET_WORDS {
            let w = self.data[word_idx];
            if w != 0 {
                return Some(word_idx * 64 + w.trailing_zeros() as usize);
            }
            word_idx += 1;
        }

        None
    }

    /// Perform logical AND with another BitMatrix in place (unrolled for 5 words)
    #[inline]
    pub fn and_assign(&mut self, other: &BitMatrix) {
        self.data[0] &= other.data[0];
        self.data[1] &= other.data[1];
        self.data[2] &= other.data[2];
        self.data[3] &= other.data[3];
        self.data[4] &= other.data[4];
    }

    pub fn to_indices(&self) -> Vec<usize> {
        let mut indices = Vec::with_capacity(8); // Cliques are size 8
        for (i, &word) in self.data.iter().enumerate() {
            if word != 0 {
                let mut temp = word;
                let base_idx = i * 64;
                while temp != 0 {
                    let zeros = temp.trailing_zeros();
                    indices.push(base_idx + zeros as usize);
                    temp &= temp - 1; // Clear lowest set bit
                }
            }
        }
        indices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_clear_at_word_boundaries() {
        let mut b = BitMatrix::new();
        for &i in &[0usize, 1, 63, 64, 65, 127, 128, 191, 192, 255, 256, 281, 287] {
            assert!(!b.get(i));
            b.set(i);
            assert!(b.get(i));
            b.clear(i);
            assert!(!b.get(i));
        }
    }

    #[test]
    fn flip_toggles_bit() {
        let mut b = BitMatrix::new();
        b.flip(64);
        assert!(b.get(64));
        b.flip(64);
        assert!(!b.get(64));
    }

    #[test]
    fn cardinality_counts_bits_across_words() {
        let mut b = BitMatrix::new();
        assert_eq!(b.cardinality(), 0);
        for &i in &[0usize, 63, 64, 100, 191, 192, 287] {
            b.set(i);
        }
        assert_eq!(b.cardinality(), 7);
    }

    #[test]
    fn next_set_bit_finds_across_word_boundaries() {
        let mut b = BitMatrix::new();
        b.set(63);
        b.set(64);
        b.set(127);
        b.set(287);
        assert_eq!(b.next_set_bit(0), Some(63));
        assert_eq!(b.next_set_bit(63), Some(63));
        assert_eq!(b.next_set_bit(64), Some(64));
        assert_eq!(b.next_set_bit(65), Some(127));
        assert_eq!(b.next_set_bit(128), Some(287));
        assert_eq!(b.next_set_bit(288), None);
    }

    #[test]
    fn next_set_bit_empty_returns_none() {
        let b = BitMatrix::new();
        assert_eq!(b.next_set_bit(0), None);
        assert_eq!(b.next_set_bit(287), None);
    }

    #[test]
    fn and_assign_intersects() {
        let mut a = BitMatrix::new();
        let mut c = BitMatrix::new();
        for &i in &[0usize, 64, 100, 287] {
            a.set(i);
        }
        for &i in &[0usize, 65, 100, 200] {
            c.set(i);
        }
        a.and_assign(&c);
        assert!(a.get(0));
        assert!(!a.get(64));
        assert!(!a.get(65));
        assert!(a.get(100));
        assert!(!a.get(200));
        assert!(!a.get(287));
        assert_eq!(a.cardinality(), 2);
    }

    #[test]
    fn invert_all_complements_and_clears_padding() {
        let mut b = BitMatrix::new();
        b.set(5);
        b.set(287);
        b.invert_all();
        assert!(!b.get(5));
        assert!(!b.get(287));
        assert!(b.get(0));
        // Bits beyond BITSET_SIZE must not be set after invert_all
        assert_eq!(b.cardinality(), (BITSET_SIZE as u32) - 2);
    }

    #[test]
    fn clear_above_removes_high_bits() {
        let mut b = BitMatrix::new();
        for i in 0..BITSET_SIZE {
            b.set(i);
        }
        b.clear_above(282);
        assert!(b.get(0));
        assert!(b.get(281));
        assert!(!b.get(282));
        assert!(!b.get(287));
        assert_eq!(b.cardinality(), 282);
    }

    #[test]
    fn clear_above_at_word_boundary() {
        let mut b = BitMatrix::new();
        for i in 0..BITSET_SIZE {
            b.set(i);
        }
        b.clear_above(192);
        assert!(b.get(191));
        assert!(!b.get(192));
        assert_eq!(b.cardinality(), 192);
    }

    #[test]
    fn is_empty_distinguishes_zero_from_nonzero() {
        let mut b = BitMatrix::new();
        assert!(b.is_empty());
        b.set(287);
        assert!(!b.is_empty());
        b.clear(287);
        assert!(b.is_empty());
    }
}
