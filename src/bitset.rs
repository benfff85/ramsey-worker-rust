/// A fixed-size bitset optimized for graph operations.
/// We use a vector of u64 to store the bits.
/// For R(8,8) searching 288 vertices, we need 288 bits, which is 5 u64s (320 bits).
#[derive(Clone, Debug)]
pub struct BitMatrix {
    size: usize,
    data: Vec<u64>,
}

impl BitMatrix {
    pub fn new(size: usize) -> Self {
        let num_u64 = (size + 63) / 64;
        BitMatrix {
            size,
            data: vec![0; num_u64],
        }
    }

    pub fn set(&mut self, bit: usize) {
        if bit < self.size {
            self.data[bit / 64] |= 1 << (bit % 64);
        }
    }

    pub fn clear(&mut self, bit: usize) {
        if bit < self.size {
            self.data[bit / 64] &= !(1 << (bit % 64));
        }
    }

    pub fn get(&self, bit: usize) -> bool {
        if bit < self.size {
            (self.data[bit / 64] & (1 << (bit % 64))) != 0
        } else {
            false
        }
    }

    pub fn flip(&mut self, bit: usize) {
        if bit < self.size {
            self.data[bit / 64] ^= 1 << (bit % 64);
        }
    }

    pub fn cardinality(&self) -> u32 {
        self.data.iter().map(|&x| x.count_ones()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.data.iter().all(|&x| x == 0)
    }

    /// Finds the index of the next set bit starting from `from_index`.
    /// Returns None if no bit is set at or after `from_index`.
    pub fn next_set_bit(&self, from_index: usize) -> Option<usize> {
        let idx = from_index;
        let mut word_idx = idx / 64;

        if word_idx >= self.data.len() {
            return None;
        }

        // Handle the first word (potentially partial)
        let bit_in_word = idx % 64;
        let mask = !((1u64 << bit_in_word) - 1); // Mask out lower bits
        let mut word = self.data[word_idx] & mask;

        if word != 0 {
            return Some(word_idx * 64 + word.trailing_zeros() as usize);
        }

        // Check subsequent words
        word_idx += 1;
        while word_idx < self.data.len() {
            word = self.data[word_idx];
            if word != 0 {
                return Some(word_idx * 64 + word.trailing_zeros() as usize);
            }
            word_idx += 1;
        }

        None
    }

    // Perform logical AND with another BitMatrix in place
    pub fn and_assign(&mut self, other: &BitMatrix) {
        for (a, b) in self.data.iter_mut().zip(other.data.iter()) {
            *a &= *b;
        }
    }

    pub fn to_indices(&self) -> Vec<usize> {
        let mut indices = Vec::new();
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
