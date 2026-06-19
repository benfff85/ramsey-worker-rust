//! Derived-graph hashing — byte-for-byte compatible with the queue manager's
//! `GraphHashUtil` (SHA-256 of the edge-data bitstring, lowercase hex). Used to
//! filter already-visited graphs out of the per-stage best-results cache so the
//! early-exit threshold tracks the best *novel* result.

use crate::graph::WorkUnitEdge;
use sha2::{Digest, Sha256};

/// SHA-256 (lowercase hex) of a graph's edge-data bitstring.
/// Mirror of `GraphHashUtil.computeHash`.
pub fn graph_hash(bitstring: &str) -> String {
    hex_lower(Sha256::digest(bitstring.as_bytes()).as_slice())
}

/// SHA-256 (lowercase hex) of the bitstring that results from flipping `edges`
/// on `base_bitstring`. Mirror of `GraphHashUtil.computeDerivedGraphHash`:
/// flip each edge's char in the upper-triangular bitstring, then hash.
pub fn derived_graph_hash(
    base_bitstring: &str,
    vertex_count: usize,
    edges: &[WorkUnitEdge],
) -> String {
    let mut bytes = base_bitstring.as_bytes().to_vec();
    for e in edges {
        let i = edge_index(e.vertex_one as usize, e.vertex_two as usize, vertex_count);
        bytes[i] = if bytes[i] == b'1' { b'0' } else { b'1' };
    }
    hex_lower(Sha256::digest(&bytes).as_slice())
}

/// Index of edge `(v1, v2)` in the upper-triangular bitstring layout shared with
/// the QM (`GraphHashUtil.getEdgeIndex` / `Graph::from_bitstring`).
fn edge_index(v1: usize, v2: usize, vertex_count: usize) -> usize {
    let (a, b) = if v1 < v2 { (v1, v2) } else { (v2, v1) };
    a * (vertex_count - 1) - a * (a + 1) / 2 + b - 1
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(a: u16, b: u16) -> WorkUnitEdge {
        WorkUnitEdge {
            vertex_one: a,
            vertex_two: b,
        }
    }

    // Vector independently computed with Python hashlib over the same bytes the
    // QM's GraphHashUtil hashes. 5-vertex base "1010101010"; flipping edge (1,3)
    // toggles upper-triangular index 5 -> "1010111010".
    #[test]
    fn derived_hash_single_edge_flip_matches_qm() {
        let h = derived_graph_hash("1010101010", 5, &[edge(1, 3)]);
        assert_eq!(
            h,
            "a883b77243b9e63ca15f5399b50849ab6a73edd8d574687d8aebd8d371a94493"
        );
    }

    // Two flips: (1,3) -> index 5, (0,4) -> index 3 -> "1011111010".
    #[test]
    fn derived_hash_multi_edge_flip_matches_qm() {
        let h = derived_graph_hash("1010101010", 5, &[edge(1, 3), edge(0, 4)]);
        assert_eq!(
            h,
            "2693da8118a62cd8cd0e1d7c88aa9b4d204981bfe11c8727574164a01b05dee9"
        );
    }

    // graph_hash mirrors GraphHashUtil.computeHash over the raw bitstring.
    #[test]
    fn graph_hash_matches_qm() {
        assert_eq!(
            graph_hash("1010101010"),
            "179b1a3e9e319cff0ca6671e07a62dad7d087b0f2a89eef0202612c3cb46baf9"
        );
    }

    // Edge order is irrelevant; flipping the same edge twice is a no-op.
    #[test]
    fn flipping_same_edge_twice_returns_base_hash() {
        assert_eq!(
            derived_graph_hash("1010101010", 5, &[edge(1, 3), edge(3, 1)]),
            graph_hash("1010101010")
        );
    }

    // The string-flip hash must equal hashing the actual flipped Graph's
    // bitstring — i.e. the hash matches the graph the QM will reconstruct.
    #[test]
    fn derived_hash_matches_graph_flip_roundtrip() {
        use crate::graph::Graph;
        let base = "1011001010";
        let edges = [edge(0, 2), edge(3, 4)];
        let mut g = Graph::from_bitstring(base, 5);
        g.flip_edges(&edges);
        assert_eq!(derived_graph_hash(base, 5, &edges), graph_hash(&g.to_bitstring()));
    }
}
