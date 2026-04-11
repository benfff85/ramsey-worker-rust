use crate::bitset::{BITSET_SIZE, BitMatrix};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
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
