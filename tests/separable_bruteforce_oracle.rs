//! Brute-force oracle for the exact row selector, independent of every counting kernel.
//!
//! For hundreds of random small graphs, every red/blue pair flip is recounted by literal subset
//! enumeration — "every k-subset whose pairs all have one colour after both flips" — with no use of
//! Bron-Kerbosch, `CliqueCollection`, or the hoist tables. The selector may only retire a pair whose
//! true count cannot beat the threshold (insertion is `count < threshold`), and its Mixed identity
//! must equal the true count exactly.

use ramsey_worker_rust::algorithm::get_all_cliques;
use ramsey_worker_rust::clique_collection::CliqueCollection;
use ramsey_worker_rust::graph::Graph;
use ramsey_worker_rust::hoist::{cross_pairs, CrossPairs, HoistTables};
use ramsey_worker_rust::separable::{RowSelectorScratch, SeparableRowPlan};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Monochromatic k-subsets of a colouring given as a plain matrix, by enumeration.
fn brute_force_count(red: &[Vec<bool>], k: usize) -> i32 {
    let n = red.len();
    let mut count = 0;
    let mut subset: Vec<usize> = (0..k).collect();
    loop {
        let colour = red[subset[0]][subset[1]];
        let mono = (0..k).all(|i| ((i + 1)..k).all(|j| red[subset[i]][subset[j]] == colour));
        if mono {
            count += 1;
        }
        // Next combination in lexicographic order.
        let mut i = k;
        loop {
            if i == 0 {
                return count;
            }
            i -= 1;
            if subset[i] != i + n - k {
                break;
            }
            if i == 0 {
                return count;
            }
        }
        subset[i] += 1;
        for j in (i + 1)..k {
            subset[j] = subset[j - 1] + 1;
        }
    }
}

fn random_bitstring(rng: &mut Rng, n: usize) -> String {
    (0..n * (n - 1) / 2)
        .map(|_| if rng.next() & 1 == 1 { '1' } else { '0' })
        .collect()
}

#[test]
fn selector_never_retires_a_pair_that_beats_the_threshold() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let mut checked_pairs = 0u64;
    let mut retired_pairs = 0u64;
    let mut retired_non_mixed = 0u64;
    for &(n, k) in &[(7usize, 3usize), (8, 3), (8, 4), (9, 4), (10, 4), (11, 5)] {
        let graphs = if k == 5 { 40 } else { 120 };
        for _ in 0..graphs {
            let bits = random_bitstring(&mut rng, n);
            let mut graph = Graph::from_bitstring(&bits, n);
            let red: Vec<Vec<bool>> = (0..n)
                .map(|u| (0..n).map(|v| u != v && graph.adjacency[u].get(v)).collect())
                .collect();
            let base_total = brute_force_count(&red, k);

            let mut collection = CliqueCollection::new(n);
            collection.set_cliques(get_all_cliques(&mut graph, k), n);
            assert_eq!(collection.total() as i32, base_total, "base count disagrees with brute force");
            let mut tables = HoistTables::new(n);
            tables.fill_slice(&mut graph, k, 0, 1);
            assert!(tables.is_complete());
            let plan = SeparableRowPlan::build(&mut graph, k, &collection, &mut tables);
            if plan.red_len() == 0 || plan.blue_len() == 0 {
                continue;
            }
            let mut scratch = RowSelectorScratch::new(plan.blue_len());

            // True count of every pair, by enumeration on the doubly flipped colouring.
            let mut truth = vec![vec![0i32; plan.blue_len()]; plan.red_len()];
            for (ri, row) in truth.iter_mut().enumerate() {
                let r = plan.red_edge(ri);
                for (bi, cell) in row.iter_mut().enumerate() {
                    let b = plan.blue_edge(bi);
                    let mut flipped = red.clone();
                    flipped[r.0][r.1] = false;
                    flipped[r.1][r.0] = false;
                    flipped[b.0][b.1] = true;
                    flipped[b.1][b.0] = true;
                    *cell = brute_force_count(&flipped, k);
                    if cross_pairs(&graph.adjacency, r, b) == CrossPairs::Mixed {
                        assert_eq!(
                            plan.exact_mixed_score(ri, bi),
                            *cell,
                            "Mixed identity wrong: n={n} k={k} bits={bits} r={r:?} b={b:?}"
                        );
                    }
                }
            }

            // Every threshold at which some pair's decision can change, plus the extremes.
            let mut thresholds: Vec<i32> = truth.iter().flatten().flat_map(|&c| [c, c + 1]).collect();
            thresholds.extend([i32::MIN / 2, -1, 0, base_total, base_total + 1, i32::MAX / 2]);
            thresholds.sort_unstable();
            thresholds.dedup();

            for &threshold in &thresholds {
                for ri in 0..plan.red_len() {
                    plan.select_row(&graph, ri, threshold, &mut scratch);
                    for bi in 0..plan.blue_len() {
                        checked_pairs += 1;
                        if scratch.contains(bi) {
                            continue;
                        }
                        retired_pairs += 1;
                        let (r, b) = (plan.red_edge(ri), plan.blue_edge(bi));
                        if cross_pairs(&graph.adjacency, r, b) != CrossPairs::Mixed {
                            retired_non_mixed += 1;
                        }
                        assert!(
                            truth[ri][bi] >= threshold,
                            "retired a qualifying pair: n={n} k={k} bits={bits} r={r:?} b={b:?} \
                             count={} threshold={threshold}",
                            truth[ri][bi]
                        );
                    }
                }
            }
        }
    }
    eprintln!(
        "brute-force oracle: {checked_pairs} pair decisions, {retired_pairs} retired \
         ({retired_non_mixed} via the all-red/all-blue/shared-vertex lower bounds), all safe"
    );
    assert!(retired_non_mixed > 0, "lower-bound retirements were never exercised");
}
