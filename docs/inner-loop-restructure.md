# Inner Loop Restructure & Evaluation Throughput Optimization

**Date:** 2026-08-23  
**Status:** Implemented, verified on standalone equivalence harness (10,000,000 units), piloted in live production on M1 fleet, and ready for audit.

---

## 1. Problem & Context

Under the `SEQUENTIAL_WITH_SINGLES` strategy on $R(8,8)$ ($N=288$ vertices, $392.5\text{M}$ pairs), each batch was previously evaluated via a generic scalar loop (`worker.rs:1011..1184`):
1. **Dynamic Dispatch & Modulo Arithmetic:** For every single unit, `enumerator.index_to_work_unit(idx)` was called through dynamic trait dispatch (`Box<dyn WorkEnumerator>`), executing 64-bit integer division and modulo (`idx / blue_count`, `idx % blue_count`) on every iteration.
2. **Repeated Invariant Lookups:** Across all 19,810 blue partner edges for a given red edge $r=(rx, ry)$, the properties of $r$ ($D_r$, broken red clique counts in `edge_counts`, and adjacency rows `adj[rx]`, `adj[ry]`) were repeatedly re-fetched and re-computed.
3. **Branchy Cross-Pair Checks:** Every unit passed through `cross_pairs()`, which iterated 4 times with 5 conditional guards per iteration to filter out degenerate / collapsed products, even though **98.6% of pairs are completely disjoint**.

---

## 2. Restructured Architecture

When `hoist` is engaged and the active strategy is `SEQUENTIAL_WITH_SINGLES`:

### A. Chunked Iteration by Red Edges
* The claimed range $[start\_index, end\_index)$ is partitioned into:
  1. **Singles Block:** $[start\_index, \min(end\_index, singles\_count))$, evaluated as direct table lookups.
  2. **Pairs Block:** $[p\_start, p\_end)$, where $p\_start = \max(start\_index, singles\_count) - singles\_count$.
* For each red edge $r = (rx, ry)$:
  * Invariants are loaded into registers/locals **once**:
    $$D_r = \text{tables.single\_created}(graph, k, rx, ry)$$
    $$red\_broken = \text{edge\_counts}[rx \cdot N + ry]$$
    $$row\_rx = graph.adjacency[rx], \quad row\_ry = graph.adjacency[ry]$$
  * The inner loop iterates linearly over the contiguous slice $[b\_from, b\_to)$ of blue edges.

### B. Fast Disjoint Cross-Pair Classification
* For the 98.6% disjoint pairs ($rx \neq bx \land rx \neq by \land ry \neq bx \land ry \neq by$):
  $$rx\_bx = row\_rx.get(bx), \quad rx\_by = row\_rx.get(by)$$
  $$ry\_bx = row\_ry.get(bx), \quad ry\_by = row\_ry.get(by)$$
  * If all 4 bits are 1 $\implies$ `AllRed`. If $D_r > early\_limit$, skip immediately!
  * If all 4 bits are 0 $\implies$ `AllBlue`. If $C_b > early\_limit$, skip immediately!
  * If mixed $\implies$ `Mixed`, $created = C_b + D_r$.
* For the remaining 1.4% shared-vertex pairs, fallback to exact `cross_pairs()`.

### C. Zero-Overhead Submissions & Early Abandonment
* Uses stack-allocated arrays `[WorkUnitEdge; 2]` for candidate flips, completely avoiding heap allocations on candidate evaluation.
* Periodically checks relaxed atomic stage announcements (`STAGE_CHECK_INTERVAL_UNITS = 4096`) to abandon superseded stages promptly.
* Maintains full fallback scalar path for unhoisted stages or non-sequential strategies.

---

## 3. Microbenchmark & Equivalence Proof

Benchmarked using `src/bin/inner_loop_bench.rs` on an active campaign graph ($N=288, k=8$, 10,000,000 units) on an Apple M1 P-core in release mode:

### Speedup

| Path | 10M Units Wall-Clock | Per-Unit Evaluation Time | Throughput | Speedup |
| :--- | :--- | :--- | :--- | :--- |
| **Baseline Loop** | 63.69 ms | 6.37 ns / unit | 157.00 M units / sec | 1.00x |
| **Restructured Loop** | **15.06 ms** | **1.51 ns / unit** | **663.79 M units / sec** | **4.23x** |

### Decision Equivalence
* `assert_eq!(acc_base, acc_restruct)` $\implies$ **20 accepted units (100% match)**
* `assert_eq!(best_base, best_restruct)` $\implies$ **8,255,788 minimum clique count (exact match)**
* `assert_eq!(dec_base, dec_restruct)` $\implies$ **All unit decisions and novelty hashes identical**
* Test suite: **98 / 98 unit tests passed with 0 failures** (`cargo test`).

---

## 4. Live Production Pilot

- Deployed the release binary as a live pilot on 1 core alongside 7 background workers on fleet `m1`.
- Verified live interaction with Redis (`www.setminusx.cloud:36002`) and middleware (`http://ramsey-mw.setminusx.cloud/api/ramsey`):
  - Clean stage transitions (stages 975964 through 975978).
  - Accurate novel candidate hashing and top-50 submissions.
  - Zero crashes or drift.
