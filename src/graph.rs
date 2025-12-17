use crate::bitset::BitMatrix;

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
        let mut adjacency = Vec::with_capacity(vertex_count);
        for _ in 0..vertex_count {
            adjacency.push(BitMatrix::new(vertex_count));
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

    pub fn invert(&mut self) {
        for i in 0..self.vertex_count {
            for j in 0..self.vertex_count {
                if i != j {
                    self.adjacency[i].flip(j);
                }
            }
        }
    }

    pub fn flip_edges(&mut self, edges: &[WorkUnitEdge]) {
        for edge in edges {
            let u = edge.vertex_one as usize;
            let v = edge.vertex_two as usize;
            if u < self.vertex_count && v < self.vertex_count {
                self.adjacency[u].flip(v);
                self.adjacency[v].flip(u);
            }
        }
    }
}
