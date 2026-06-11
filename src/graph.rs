use crate::bitset::{BitMatrix, BITSET_SIZE};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorkUnitEdge {
    #[serde(rename = "vertexOne")]
    pub vertex_one: u16,
    #[serde(rename = "vertexTwo")]
    pub vertex_two: u16,
}

pub struct Graph {
    pub vertex_count: usize,
    pub adjacency: Vec<BitMatrix>,
}

impl Graph {
    pub fn new(vertex_count: usize) -> Self {
        debug_assert!(vertex_count <= BITSET_SIZE);
        let mut adjacency = Vec::with_capacity(vertex_count);
        for _ in 0..vertex_count {
            adjacency.push(BitMatrix::new());
        }
        Graph {
            vertex_count,
            adjacency,
        }
    }

    pub fn from_bitstring(bit_string: &str, vertex_count: usize) -> Self {
        let mut graph = Graph::new(vertex_count);
        let mut edge_index = 0;
        let chars: Vec<char> = bit_string.chars().collect();

        for i in 0..vertex_count {
            for j in (i + 1)..vertex_count {
                if edge_index < chars.len() && chars[edge_index] == '1' {
                    graph.adjacency[i].set(j);
                    graph.adjacency[j].set(i);
                }
                edge_index += 1;
            }
        }
        graph
    }

    /// Inverts the adjacency matrix using word-level XOR operations.
    /// This is O(n * words) instead of O(n²) individual bit flips.
    #[inline]
    pub fn invert(&mut self) {
        for i in 0..self.vertex_count {
            // Invert all bits at word level
            self.adjacency[i].invert_all();
            // Clear the self-loop bit (diagonal)
            self.adjacency[i].clear(i);
            // Clear any "neighbors" beyond vertex_count: a vertex slot that doesn't
            // exist must not appear as a neighbor of an existing vertex. invert_all()
            // already cleared the BITSET padding (288..320), so we only need this when
            // vertex_count < BITSET_SIZE — a no-op for production (vertex_count = 288).
            if self.vertex_count < BITSET_SIZE {
                self.adjacency[i].clear_above(self.vertex_count);
            }
        }
    }

    /// Convert graph adjacency matrix back to bitstring format.
    /// Inverse of `from_bitstring()`.
    pub fn to_bitstring(&self) -> String {
        let edge_count = self.vertex_count * (self.vertex_count - 1) / 2;
        let mut bits = String::with_capacity(edge_count);
        for i in 0..self.vertex_count {
            for j in (i + 1)..self.vertex_count {
                if self.adjacency[i].get(j) {
                    bits.push('1');
                } else {
                    bits.push('0');
                }
            }
        }
        bits
    }

    #[inline]
    pub fn flip_edges(&mut self, edges: &[WorkUnitEdge]) {
        for edge in edges {
            let u = edge.vertex_one as usize;
            let v = edge.vertex_two as usize;
            self.adjacency[u].flip(v);
            self.adjacency[v].flip(u);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// K5 fully connected: all 10 edges set.
    fn k5_bitstring() -> String {
        "1".repeat(10)
    }

    #[test]
    fn from_bitstring_k5_is_fully_connected() {
        let g = Graph::from_bitstring(&k5_bitstring(), 5);
        for i in 0..5 {
            for j in 0..5 {
                if i == j {
                    assert!(!g.adjacency[i].get(j), "self-loop {i}");
                } else {
                    assert!(g.adjacency[i].get(j), "edge ({i},{j}) should be set");
                }
            }
        }
    }

    #[test]
    fn bitstring_roundtrip_k5() {
        let bits = k5_bitstring();
        let g = Graph::from_bitstring(&bits, 5);
        assert_eq!(g.to_bitstring(), bits);
    }

    #[test]
    fn bitstring_roundtrip_mixed() {
        // P3: vertex 0-1, 1-2 connected, 0-2 not. Edges in lex order: (0,1), (0,2), (1,2)
        let bits = "101";
        let g = Graph::from_bitstring(bits, 3);
        assert!(g.adjacency[0].get(1));
        assert!(!g.adjacency[0].get(2));
        assert!(g.adjacency[1].get(2));
        assert_eq!(g.to_bitstring(), bits);
    }

    #[test]
    fn invert_complements_k5_to_empty() {
        let mut g = Graph::from_bitstring(&k5_bitstring(), 5);
        g.invert();
        for i in 0..5 {
            for j in 0..5 {
                assert!(
                    !g.adjacency[i].get(j),
                    "edge ({i},{j}) should be cleared after invert"
                );
            }
        }
    }

    #[test]
    fn invert_is_self_inverse() {
        let original = "101100110100"; // arbitrary 6-vertex graph (15 edges)
        let bits = format!("{}{}", original, "0".repeat(15 - original.len()));
        let mut g = Graph::from_bitstring(&bits, 6);
        let before = g.to_bitstring();
        g.invert();
        g.invert();
        assert_eq!(g.to_bitstring(), before);
    }

    #[test]
    fn flip_edges_toggles_both_directions() {
        let mut g = Graph::new(5);
        g.flip_edges(&[WorkUnitEdge {
            vertex_one: 1,
            vertex_two: 3,
        }]);
        assert!(g.adjacency[1].get(3));
        assert!(g.adjacency[3].get(1));
        g.flip_edges(&[WorkUnitEdge {
            vertex_one: 1,
            vertex_two: 3,
        }]);
        assert!(!g.adjacency[1].get(3));
        assert!(!g.adjacency[3].get(1));
    }
}
