//! Standalone benchmark and decision-equivalence verification harness.
//! Compares the baseline evaluation loop against the restructured inner loop.

use ramsey_worker_rust::clique_collection::CliqueCollection;
use ramsey_worker_rust::enumeration::{
    SequentialWithSinglesEnumerator, WorkEnumerator, WorkUnit,
};
use ramsey_worker_rust::graph::{Graph, WorkUnitEdge};
use ramsey_worker_rust::hoist::{cross_pairs, CrossPairs, HoistTables};
use std::time::Instant;

fn bits(n: usize, seed: u64) -> String {
    let mut s = seed;
    let mut out = String::new();
    for _ in 0..(n * (n - 1) / 2) {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push(if (s >> 60) & 1 == 1 { '1' } else { '0' });
    }
    out
}

/// Baseline evaluation loop over [start_index, end_index)
fn run_baseline(
    graph: &mut Graph,
    clique_collection: &CliqueCollection,
    tables: &mut HoistTables,
    enumerator: &dyn WorkEnumerator,
    clique_size: usize,
    start_index: i64,
    end_index: i64,
    mut top_threshold: Option<i32>,
) -> (usize, i32, Vec<(i64, i32)>) {
    let mut accepted = 0;
    let mut best_count = i32::MAX;
    let mut decisions = Vec::new();
    let base_total = clique_collection.total() as i32;

    for idx in start_index..end_index {
        let unit = enumerator.index_to_work_unit(idx);
        let mut edge_buf = [
            WorkUnitEdge { vertex_one: 0, vertex_two: 0 },
            WorkUnitEdge { vertex_one: 0, vertex_two: 0 },
        ];
        let edges_to_flip: &[WorkUnitEdge] = match &unit {
            WorkUnit::SingleFlip(edge) => {
                edge_buf[0] = edge.clone();
                &edge_buf[..1]
            }
            WorkUnit::PairFlip(red_edge, blue_edge) => {
                edge_buf[0] = red_edge.clone();
                edge_buf[1] = blue_edge.clone();
                &edge_buf[..2]
            }
        };

        let broken = clique_collection.get_count_of_cliques_containing_edges(edges_to_flip);

        let early_limit = match top_threshold {
            Some(threshold) => {
                let max_new = (threshold - 1) - base_total + broken;
                if max_new < 0 {
                    continue;
                }
                max_new
            }
            None => i32::MAX,
        };

        let (new, exceeded) = match &unit {
            WorkUnit::SingleFlip(edge) => {
                let created = tables.single_created(
                    graph,
                    clique_size,
                    edge.vertex_one as usize,
                    edge.vertex_two as usize,
                );
                (created, created > early_limit)
            }
            WorkUnit::PairFlip(red_edge, blue_edge) => {
                match tables.pair_created_bounded(
                    graph,
                    clique_size,
                    (red_edge.vertex_one as usize, red_edge.vertex_two as usize),
                    (blue_edge.vertex_one as usize, blue_edge.vertex_two as usize),
                    early_limit,
                ) {
                    Some(created) => (created, false),
                    None => (0, true),
                }
            }
        };

        if exceeded {
            continue;
        }

        let count = base_total - broken + new;
        accepted += 1;
        best_count = best_count.min(count);
        decisions.push((idx, count));

        if let Some(t) = top_threshold {
            if count < t {
                top_threshold = Some(count);
            }
        }
    }

    (accepted, best_count, decisions)
}

/// Restructured hoisted inner loop over [start_index, end_index)
fn run_restructured(
    graph: &mut Graph,
    clique_collection: &CliqueCollection,
    tables: &mut HoistTables,
    enumerator: &SequentialWithSinglesEnumerator,
    clique_size: usize,
    start_index: i64,
    end_index: i64,
    mut top_threshold: Option<i32>,
) -> (usize, i32, Vec<(i64, i32)>) {
    let mut accepted = 0;
    let mut best_count = i32::MAX;
    let mut decisions = Vec::new();
    let base_total = clique_collection.total() as i32;
    let vertex_count = graph.vertex_count;
    let edge_counts = clique_collection.edge_counts();

    let singles = enumerator.singles();
    let red_edges = enumerator.red_edges();
    let blue_edges = enumerator.blue_edges();
    let singles_count = singles.len() as i64;
    let blue_count = blue_edges.len() as i64;

    // 1. Singles block
    if start_index < singles_count {
        let s_end = end_index.min(singles_count) as usize;
        for idx in (start_index as usize)..s_end {
            let edge = &singles[idx];
            let u = edge.vertex_one as usize;
            let v = edge.vertex_two as usize;
            let (min_v, max_v) = if u < v { (u, v) } else { (v, u) };
            let broken = edge_counts[min_v * vertex_count + max_v];

            let early_limit = match top_threshold {
                Some(threshold) => {
                    let max_new = (threshold - 1) - base_total + broken;
                    if max_new < 0 {
                        continue;
                    }
                    max_new
                }
                None => i32::MAX,
            };

            let created = tables.single_created(graph, clique_size, u, v);
            if created > early_limit {
                continue;
            }

            let count = base_total - broken + created;
            accepted += 1;
            best_count = best_count.min(count);
            decisions.push((idx as i64, count));

            if let Some(t) = top_threshold {
                if count < t {
                    top_threshold = Some(count);
                }
            }
        }
    }

    // 2. Pairs block
    if end_index > singles_count {
        let p_start = (start_index.max(singles_count) - singles_count) as usize;
        let p_end = (end_index - singles_count) as usize;

        let start_red = p_start / (blue_count as usize);
        let start_blue = p_start % (blue_count as usize);
        let end_red = (p_end - 1) / (blue_count as usize);
        let end_blue = (p_end - 1) % (blue_count as usize);

        for red_idx in start_red..=end_red {
            let r_edge = &red_edges[red_idx];
            let rx = r_edge.vertex_one as usize;
            let ry = r_edge.vertex_two as usize;
            let (min_r, max_r) = if rx < ry { (rx, ry) } else { (ry, rx) };
            let red_broken = edge_counts[min_r * vertex_count + max_r];
            let d_r = tables.single_created(graph, clique_size, rx, ry);
            let row_rx = graph.adjacency[rx];
            let row_ry = graph.adjacency[ry];

            let b_from = if red_idx == start_red { start_blue } else { 0 };
            let b_to = if red_idx == end_red { end_blue + 1 } else { blue_count as usize };

            for blue_idx in b_from..b_to {
                let global_idx = singles_count + (red_idx as i64) * blue_count + (blue_idx as i64);
                let b_edge = &blue_edges[blue_idx];
                let bx = b_edge.vertex_one as usize;
                let by = b_edge.vertex_two as usize;
                let (min_b, max_b) = if bx < by { (bx, by) } else { (by, bx) };
                let blue_broken = edge_counts[min_b * vertex_count + max_b];
                let broken = red_broken + blue_broken;

                let early_limit = match top_threshold {
                    Some(threshold) => {
                        let max_new = (threshold - 1) - base_total + broken;
                        if max_new < 0 {
                            continue;
                        }
                        max_new
                    }
                    None => i32::MAX,
                };

                let c_b = tables.single_created(graph, clique_size, bx, by);
                let base = c_b + d_r;

                // Check disjoint vs shared vertices
                let is_disjoint = rx != bx && rx != by && ry != bx && ry != by;
                let created = if is_disjoint {
                    let rx_bx = row_rx.get(bx);
                    let rx_by = row_rx.get(by);
                    let ry_bx = row_ry.get(bx);
                    let ry_by = row_ry.get(by);

                    if rx_bx && rx_by && ry_bx && ry_by {
                        // AllRed
                        if d_r > early_limit {
                            continue;
                        }
                        base - tables.compute_correction(&graph.adjacency, (rx, ry), (bx, by), clique_size)
                    } else if !rx_bx && !rx_by && !ry_bx && !ry_by {
                        // AllBlue
                        if c_b > early_limit {
                            continue;
                        }
                        base - tables.compute_correction(&graph.complement_adjacency, (rx, ry), (bx, by), clique_size)
                    } else {
                        // Mixed
                        base
                    }
                } else {
                    match cross_pairs(&graph.adjacency, (rx, ry), (bx, by)) {
                        CrossPairs::Mixed => base,
                        CrossPairs::AllRed => {
                            if d_r > early_limit {
                                continue;
                            }
                            base - tables.compute_correction(&graph.adjacency, (rx, ry), (bx, by), clique_size)
                        }
                        CrossPairs::AllBlue => {
                            if c_b > early_limit {
                                continue;
                            }
                            base - tables.compute_correction(&graph.complement_adjacency, (rx, ry), (bx, by), clique_size)
                        }
                    }
                };

                if created > early_limit {
                    continue;
                }

                let count = base_total - broken + created;
                accepted += 1;
                best_count = best_count.min(count);
                decisions.push((global_idx, count));

                if let Some(t) = top_threshold {
                    if count < t {
                        top_threshold = Some(count);
                    }
                }
            }
        }
    }

    (accepted, best_count, decisions)
}

fn main() {
    let vertex_count = 288;
    let clique_size = 8;
    println!("Initializing benchmark on N={vertex_count}, k={clique_size}...");

    let bitstring = bits(vertex_count, 123456789);
    let mut graph = Graph::from_bitstring(&bitstring, vertex_count);

    let mut cc = CliqueCollection::new(vertex_count);
    cc.build_counts_only(&mut graph, clique_size);
    println!("Base cliques: {}", cc.total());

    let mut tables = HoistTables::new(vertex_count);
    println!("Pre-filling hoist table...");
    tables.fill_slice(&mut graph, clique_size, 0, 1);
    println!("Hoist table filled: {} entries", tables.filled());

    let enumerator = SequentialWithSinglesEnumerator::new(&graph);
    let total_units = enumerator.total_work_units();
    println!("Total work units: {total_units}");

    let unit_range = 0..10_000_000i64;
    let threshold = Some(cc.total() as i32 + 50);

    println!("\n--- 1. Testing Equivalence on 10,000,000 units ---");
    let mut t_base_copy = tables.clone();
    let (acc_base, best_base, dec_base) = run_baseline(
        &mut graph,
        &cc,
        &mut t_base_copy,
        &enumerator,
        clique_size,
        unit_range.start,
        unit_range.end,
        threshold,
    );

    let mut t_restruct_copy = tables.clone();
    let (acc_restruct, best_restruct, dec_restruct) = run_restructured(
        &mut graph,
        &cc,
        &mut t_restruct_copy,
        &enumerator,
        clique_size,
        unit_range.start,
        unit_range.end,
        threshold,
    );

    println!("Baseline:     accepted={acc_base}, best={best_base}, decisions={}", dec_base.len());
    println!("Restructured: accepted={acc_restruct}, best={best_restruct}, decisions={}", dec_restruct.len());

    assert_eq!(acc_base, acc_restruct, "Accepted count mismatch!");
    assert_eq!(best_base, best_restruct, "Best count mismatch!");
    assert_eq!(dec_base, dec_restruct, "Decisions list mismatch!");
    println!(">>> EQUIVALENCE VERIFIED: 100% BIT-FOR-BIT IDENTICAL DECISIONS <<<\n");

    println!("--- 2. Benchmarking Evaluation Speed (10,000,000 units) ---");
    let trials = 5;
    let mut base_times = Vec::new();
    for i in 0..trials {
        let mut t_base = tables.clone();
        let t0 = Instant::now();
        let _ = run_baseline(
            &mut graph,
            &cc,
            &mut t_base,
            &enumerator,
            clique_size,
            unit_range.start,
            unit_range.end,
            threshold,
        );
        let el = t0.elapsed();
        base_times.push(el);
        println!("Baseline trial {i}: {el:?} ({:.2}M u/s)", 10.0 / el.as_secs_f64());
    }

    let mut restruct_times = Vec::new();
    for i in 0..trials {
        let mut t_restruct = tables.clone();
        let t0 = Instant::now();
        let _ = run_restructured(
            &mut graph,
            &cc,
            &mut t_restruct,
            &enumerator,
            clique_size,
            unit_range.start,
            unit_range.end,
            threshold,
        );
        let el = t0.elapsed();
        restruct_times.push(el);
        println!("Restructured trial {i}: {el:?} ({:.2}M u/s)", 10.0 / el.as_secs_f64());
    }

    let min_base = base_times.iter().min().unwrap();
    let min_restruct = restruct_times.iter().min().unwrap();

    let base_m_ups = 10.0 / min_base.as_secs_f64();
    let restruct_m_ups = 10.0 / min_restruct.as_secs_f64();
    let speedup = restruct_m_ups / base_m_ups;

    println!("\n=== RESULTS ===");
    println!("Baseline Best:     {min_base:?} ({:.2} M units/sec, {:.2} ns/unit)", base_m_ups, (min_base.as_nanos() as f64) / 10_000_000.0);
    println!("Restructured Best: {min_restruct:?} ({:.2} M units/sec, {:.2} ns/unit)", restruct_m_ups, (min_restruct.as_nanos() as f64) / 10_000_000.0);
    println!(">>> SPEEDUP: {:.2}x <<<", speedup);
}
