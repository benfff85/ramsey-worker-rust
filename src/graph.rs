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
    /// The complement color's adjacency, maintained in lockstep with `adjacency`
    /// by `from_bitstring`/`flip_edges` so `invert()` is an O(1) swap instead of
    /// an O(n·words) rebuild (it used to run twice per work unit). Code that
    /// mutates `adjacency` directly must call `resync_complement()` afterwards.
    pub complement_adjacency: Vec<BitMatrix>,
}

impl Graph {
    pub fn new(vertex_count: usize) -> Self {
        debug_assert!(vertex_count <= BITSET_SIZE);
        let mut graph = Graph {
            vertex_count,
            adjacency: vec![BitMatrix::new(); vertex_count],
            complement_adjacency: vec![BitMatrix::new(); vertex_count],
        };
        graph.resync_complement();
        graph
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
                    graph.complement_adjacency[i].clear(j);
                    graph.complement_adjacency[j].clear(i);
                }
                edge_index += 1;
            }
        }
        graph
    }

    /// Recompute `complement_adjacency` from `adjacency`. Only needed after
    /// mutating `adjacency` directly (tools/experiments); the supported mutators
    /// (`from_bitstring`, `flip_edges`, `invert`) keep the two in lockstep.
    pub fn resync_complement(&mut self) {
        for i in 0..self.vertex_count {
            let mut row = self.adjacency[i];
            row.invert_all();
            row.clear(i);
            if self.vertex_count < BITSET_SIZE {
                row.clear_above(self.vertex_count);
            }
            self.complement_adjacency[i] = row;
        }
    }

    /// Swaps which color `adjacency` refers to. O(1): both colors' matrices are
    /// maintained continuously, so this is a pointer swap, not a rebuild.
    #[inline]
    pub fn invert(&mut self) {
        std::mem::swap(&mut self.adjacency, &mut self.complement_adjacency);
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
            self.complement_adjacency[u].flip(v);
            self.complement_adjacency[v].flip(u);
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

    fn assert_complement_in_lockstep(g: &Graph) {
        for i in 0..g.vertex_count {
            for j in 0..g.vertex_count {
                if i == j {
                    assert!(!g.adjacency[i].get(j), "self-loop in adjacency at {i}");
                    assert!(
                        !g.complement_adjacency[i].get(j),
                        "self-loop in complement at {i}"
                    );
                } else {
                    assert_ne!(
                        g.adjacency[i].get(j),
                        g.complement_adjacency[i].get(j),
                        "complement out of lockstep at ({i},{j})"
                    );
                }
            }
        }
    }

    #[test]
    fn complement_stays_in_lockstep_through_all_mutators() {
        let mut g = Graph::from_bitstring("101100110100110", 6);
        assert_complement_in_lockstep(&g);
        g.flip_edges(&[
            WorkUnitEdge { vertex_one: 0, vertex_two: 3 },
            WorkUnitEdge { vertex_one: 2, vertex_two: 5 },
        ]);
        assert_complement_in_lockstep(&g);
        g.invert();
        assert_complement_in_lockstep(&g);
        g.flip_edges(&[WorkUnitEdge { vertex_one: 1, vertex_two: 4 }]);
        assert_complement_in_lockstep(&g);
        g.invert();
        assert_complement_in_lockstep(&g);
    }

    #[test]
    fn resync_complement_repairs_direct_mutation() {
        let mut g = Graph::new(5);
        g.adjacency[0].set(1);
        g.adjacency[1].set(0);
        g.resync_complement();
        assert_complement_in_lockstep(&g);
    }

    #[test]
    fn invert_matches_resynced_complement_semantics() {
        // The O(1) swap must be indistinguishable from the old O(n) rebuild.
        let bits = "101100110100110";
        let mut swapped = Graph::from_bitstring(bits, 6);
        swapped.invert();
        let mut rebuilt = Graph::from_bitstring(bits, 6);
        let complement: Vec<BitMatrix> = rebuilt.complement_adjacency.clone();
        rebuilt.adjacency = complement;
        rebuilt.resync_complement();
        for i in 0..6 {
            for j in 0..6 {
                assert_eq!(swapped.adjacency[i].get(j), rebuilt.adjacency[i].get(j));
            }
        }
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
