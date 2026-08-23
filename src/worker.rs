use crate::algorithm::{get_all_cliques, get_cliques_comprehensive, get_new_cliques_with_limit};
use crate::client::MiddlewareClient;
use crate::clique_collection::CliqueCollection;
use crate::enumeration::{WorkEnumerator, WorkUnit, create_enumerator};
use crate::graph::{Graph, WorkUnitEdge};
use crate::hoist::HoistTables;
use crate::model::{StageConfig, WorkResult, WorkUnitAnalysisType};
use crate::redis_client::{RedisClient, StageAnnouncements, watch_stage_advances};
use std::sync::Arc;
use crate::sa::{SaConfig, run_sa};
use crate::tabu::{TabuConfig, run_tabu};
use crate::vds::{VdsConfig, run_vds};
use crate::{log_debug, log_error, log_info};
use chrono::Utc;
use std::collections::HashMap;
use std::error::Error;
use std::time::Duration;
use tokio::time::sleep;

/// TTL for shared per-edge clique counts. Only needs to outlive the window in which the fleet
/// picks up a given stage (seconds), so a few minutes is generous; the short life keeps Redis
/// bounded — at ~318 KB per 282-vertex graph only the last handful of stages are ever resident.
const SHARED_EDGE_COUNTS_TTL_SECONDS: u64 = 300;
/// Lifetime of the "I am building the counts" election lock. Comfortably longer than a build
/// (~0.6s on a 1.7M-clique graph) so peers wait rather than duplicating, but short enough that
/// a crashed builder doesn't stall the next stage.
const EDGE_COUNTS_BUILD_LOCK_TTL_SECONDS: u64 = 30;
/// How peers wait for the elected builder's result: poll interval and max polls (~3s total).
const EDGE_COUNTS_POLL_INTERVAL_MS: u64 = 25;
const EDGE_COUNTS_WAIT_POLLS: usize = 120;
/// Most flips we will chase incrementally rather than rebuilding the per-edge counts.
///
/// The bound is against the worker's OWN cached graph, not the previous stage, so it has to cover
/// however many stages a worker skipped — not just the 1-2 edges a single advance moves. During a
/// post-kick descent stages turn over ~2.7x/sec while a worker's cycle is ~0.7-1.1s, so skipping
/// 2-5 stages is routine and a cap of 2 sent half of all setups down the rebuild path: measured
/// 51% fallback, 19% of them full rebuilds, ~6% of total worker CPU.
///
/// A flip costs 0.82ms against 509ms for a full rebuild, so break-even is ~620 flips; 16 stays far
/// below that while still being far below a perturbation kick (1,920 pairs = 3,840 flips), which
/// genuinely is cheaper to rebuild. The derive verifies the reconstructed bitstring against the
/// stage's and falls back on any mismatch, so a wrong guess here costs time, never correctness.
const MAX_INCREMENTAL_FLIPS: usize = 16;
/// Graphs/collections retained per worker. The incremental path needs only the previous one.
const GRAPH_CACHE_MAX: usize = 3;
/// Fleet-wide claimed index past which a worker builds its hoist tables.
///
/// Set just past the SINGLES block. The work space is `[0, 39_621)` single-edge flips followed by
/// ~392.5M pair flips, so once the fleet is past ~50k indices every remaining unit is a pair — and
/// pairs are what the hoisted path exists for. Singles need no gate either way: `created` for a
/// single flip IS the table entry, so evaluating one and memoising it are the same work.
///
/// Sized by the asymmetry, which is lopsided in both regimes. Measured post-kick: a stage that does
/// NOT engage early spends **35.4 s** grinding the seeded kernel, while a fill that turns out wasted
/// costs **0.61 s** — 58x, so break-even is a 1.7% chance the stage is worth it. Near the floor it is
/// gentler (2.6 s vs 0.41 s) but points the same way. When the downside is 58x smaller than the
/// upside, the correct gate is "almost always engage", not a careful predictor.
///
/// History, so this is not re-litigated. This was 5_000_000, chosen from *hoisted* throughput
/// ("a wall stage crosses 5M in ~0.24s") — but every unit before the gate is by definition
/// unhoisted, so the real cost was 2.6-41 s per stage. Two successive attempts to keep the high gate
/// and predict around it both failed:
///   * predicting from stage depth measured against this gate — engaging made a stage ~20x faster
///     and so end sooner, below the gate, which disarmed the next stage (42% of stages paid full
///     ramp);
///   * predicting from the QM's exhausted/improved outcome — semantically clean but the wrong
///     question. Exhaustion proxies REGIME; the gate needs AMORTISATION. At high clique count a
///     stage that advances on an improvement still churns millions of units and amortises fine.
///     Measured 100% of stages paying full ramp — worse than the thing it replaced.
/// Lowering the gate removes the need for any predictor at all.
const HOIST_MIN_STAGE_INDEX: i64 = 50_000;
/// Slices the per-edge fill is split into across the fleet.
///
/// Every worker on a graph would otherwise fill the whole table itself — ~2.9s of uncapped
/// traversals, done 14 times over in parallel. Each worker instead claims one slice, publishes it,
/// and adopts its peers', so the fleet pays the fill once between them. Kept a little above the
/// usual worker count so slices stay distinct; a fleet smaller than this just leaves some slices
/// unpublished, which costs nothing beyond filling those edges on demand as before.
const HOIST_FILL_SLICES: i64 = 16;
/// How often the work loop checks whether its stage has been superseded.
///
/// A stage advance invalidates everything after it: results are written to keys the queue manager
/// clears on the switch, so a batch that runs on past one is wasted outright. Polling the fleet
/// endpoint once per cycle left workers a large fraction of a stage behind at current rates, which
/// both wasted that work and scattered the fleet across graphs so it could not pool the per-edge
/// fill. This is a relaxed atomic load, so checking often is nearly free; the interval only needs
/// to be coarse enough that the check is not a measurable share of a unit.
const STAGE_CHECK_INTERVAL_UNITS: i64 = 4096;
/// How long a worker will wait for peers' hoist slices before starting its unit loop.
///
/// The co-operative fill only pays off if a worker's table is populated BEFORE it starts looping.
/// It is not: coverage at engage is ~56%, and because blue is the inner enumeration index the very
/// first batch touches every blue edge, so the misses are all paid up front. Measured, the fleet
/// does 4.4x the necessary fill work and recomputes each blue entry ~5.2x over.
///
/// Sized against the slice fill itself (~0.4s): peers publish within roughly that window, so a few
/// hundred ms captures most of them. Bounded, and abandoned early when coverage stops improving or
/// the stage is superseded, so the downside on a short stage is small and self-limiting.
const HOIST_COVERAGE_WAIT: Duration = Duration::from_millis(300);
/// Gap between coverage polls while waiting. Each poll is one MGET of the slice keys.
const HOIST_COVERAGE_POLL: Duration = Duration::from_millis(25);
/// Consecutive polls that add nothing before giving up — peers have published all they will.
const HOIST_COVERAGE_STAGNANT_POLLS: u8 = 2;
/// Pause before retrying a cycle that found nothing to do for a TRANSIENT reason — the stage
/// advanced between resolving it and reading its config, or its work was fully claimed.
///
/// Those are races, not idleness, and they are routine now that stages turn over faster than a
/// worker can set one up. Falling back to the full poll interval for them left workers asleep for
/// a second at a time while the search moved on: measured 19% CPU across the fleet, most of it
/// spent in one-second sleeps between "Stage config missing" messages.
const TRANSIENT_RETRY_MILLIS: u64 = 25;

/// Wall-clock work to aim for per cycle. Every cycle carries a fixed cost — one fleet
/// active-stage HTTP call plus three Redis round trips, measured at ~22 ms — so a batch much
/// smaller than this spends most of the cycle on overhead. Bigger is not free either: a worker
/// cannot notice a stage change until its batch ends.
const TARGET_BATCH_LOOP_MILLIS: u128 = 200;
/// Ceiling on the adaptive batch, so one cycle can never run away.
///
/// Raised 1M -> 4M once the correction bound landed. At 1M this stopped being a safety ceiling and
/// became the BINDING constraint: per-worker throughput reached 8-12M units/sec, at which
/// [`TARGET_BATCH_LOOP_MILLIS`] wants 1.6-2.4M units, so batches ran ~77 ms instead of 200 ms and
/// the fixed ~17 ms per-cycle cost was paid ~2.6x as often. Measured: busy fell 95% -> 82%, i.e.
/// per-cycle overhead went 5% -> 18% of worker wall-clock.
///
/// The ceiling is sized in UNITS but the thing that matters is DURATION, and the controller already
/// targets that — so this only needs to stay above what the target implies at plausible throughput.
/// 4M covers ~20M units/sec, comfortably above today's 12M.
///
/// Cost of a larger batch is stage-TAIL latency, not wasted work: `claim_work_range` clamps the end
/// index to `total_pairs`, and a stage cannot finish until its last full batch does, so the tail is
/// ~one batch duration (~200 ms of a ~5 s stage). Abandonment is unaffected — a worker drops a
/// superseded stage every [`STAGE_CHECK_INTERVAL_UNITS`], not at batch boundaries.
const MAX_FETCH_SIZE: i32 = 4_000_000;
/// Most the batch may grow in a single step, so one unusually fast batch cannot overshoot.
const MAX_FETCH_GROWTH: i64 = 4;
/// How often a worker summarizes its throughput, replacing the per-batch line. Batches run several
/// times a second, so per-batch logging scaled with the fleet's speed rather than with anything
/// worth reading.
const STATS_INTERVAL_SECS: u64 = 30;

/// Which stage to actually work, given the middleware's answer and the newest announced stage.
///
/// The middleware serves its active-stage answer from a short-lived cache, so it can name a stage
/// that has already been superseded — and asking Redis for that stage's config then misses,
/// because the queue manager deletes it as soon as the successor is live. Announcements are
/// published only after the new stage is ACTIVE and its config is seeded, so a *newer* announced
/// stage is always workable and is better information than a stale cache entry.
///
/// Only ever moves forward: an announcement older than the middleware's answer is ignored, so a
/// stale or missing announcement can never drag a worker back onto a dead stage.
fn effective_stage_id(mw_stage_id: i32, announced: Option<i32>) -> i32 {
    match announced {
        Some(a) if a > mw_stage_id => a,
        _ => mw_stage_id,
    }
}

/// Next batch size, from the previous batch's measured cost.
///
/// One setting cannot serve both regimes: a hoisted unit costs ~0.26 us and an unhoisted one
/// ~4 us, a 15x spread. Sizing by measured throughput instead of a constant resolves it — the
/// same target duration yields ~800k units once the hoisted path is live, and ~50k during a
/// stage's warmup or a fast post-kick descent (where it never engages), which is exactly where a
/// worker needs to stay responsive to stage changes.
fn next_fetch_size(current: i32, floor: i32, units: i64, loop_nanos: u128) -> i32 {
    if units <= 0 || loop_nanos == 0 {
        return current;
    }
    let ceiling = (current as i64)
        .saturating_mul(MAX_FETCH_GROWTH)
        .min(MAX_FETCH_SIZE as i64);
    let per_unit_nanos = loop_nanos / units as u128;
    let ideal = if per_unit_nanos == 0 {
        ceiling // immeasurably fast: grow by the max step
    } else {
        (TARGET_BATCH_LOOP_MILLIS * 1_000_000 / per_unit_nanos) as i64
    };
    ideal.min(ceiling).clamp(floor as i64, MAX_FETCH_SIZE as i64) as i32
}

pub struct Worker {
    mw_client: MiddlewareClient,
    redis_client: Option<RedisClient>,
    clique_size: usize,
    vertex_count: usize,
    graph_cache: HashMap<i32, Graph>,
    clique_collection_cache: HashMap<i32, CliqueCollection>,
    /// Memoised per-edge `created` counts per base graph, backing the hoisted pair-move
    /// evaluation. Keyed by GRAPH id and pruned with the other per-graph caches, so it can never
    /// outlive the graph it describes.
    hoist_cache: HashMap<i32, HoistTables>,
    /// Kill switch for the hoisted path (env HOIST_ENABLED). Off falls back to the seeded kernel,
    /// which computes exactly the same values.
    hoist_enabled: bool,
    /// Newest stage announced per campaign, kept current by a background subscriber. Lets the work
    /// loop abandon a superseded stage in milliseconds instead of at its next poll.
    stage_announcements: Arc<StageAnnouncements>,
    /// Set when a cycle came back empty because of a race rather than because there is nothing to
    /// do, so the next attempt waits milliseconds instead of a full poll interval.
    retry_soon: bool,
    /// Rolling throughput accumulator. Logging every batch fired several times a second per worker
    /// and reported a figure nobody reads directly; one periodic line reports units/sec instead.
    stats_window_start: std::time::Instant,
    stats_units: u64,
    stats_batches: u64,
    stats_busy_nanos: u128,
    /// On-demand hoist-table fills and their wall-clock, accumulated over the stats window.
    /// These happen INSIDE the unit loop, so they are counted as "busy" and are otherwise
    /// indistinguishable from real evaluation work in the throughput line.
    stats_fills: u64,
    stats_fill_nanos: u128,
    stats_fills_red: u64,
    stats_fills_blue: u64,
    stats_fills_slice: u64,
    /// Adaptive batch size, retuned from each batch's measured cost (see `next_fetch_size`).
    /// `fetch_size` is its floor and its reset value on a stage change.
    current_fetch_size: i32,
    /// Base graph of the stage we most recently set up, so the next stage (one flip away) can be
    /// derived from it instead of rebuilt.
    last_base_graph_id: Option<i32>,
    /// (parent graph id, edges the advance flipped) for the graph just derived, so the hoist
    /// engage path can carry the parent's per-edge table forward instead of rebuilding it.
    /// Consumed once, at engage.
    hoist_carry: Option<(i32, Vec<(usize, usize)>)>,
    poll_interval: Duration,
    fetch_size: i32,
    publish_size: i32,
    campaign_id: i32,
    /// Fleet abstraction: when Some(platform), the worker resolves its stage via
    /// GET /fleets/{platform}/active-stage each cycle (repoint/pause is a DB
    /// update, no redeploy). When None, falls back to the pinned campaign_id.
    fleet: Option<String>,
    stage_id: Option<i32>,
    base_graph_clique_count: Option<i32>,
    publish_results: bool,
    top_results_count: usize,
    // Counter-based mode state
    stage_config: Option<StageConfig>,
    enumerator: Option<Box<dyn WorkEnumerator + Send>>,
    // Simulated annealing mode config
    sa_mode: bool,
    sa_config: SaConfig,
    // Variable-depth search mode config
    vds_mode: bool,
    vds_config: VdsConfig,
    // Tabu search mode config
    tabu_mode: bool,
    tabu_config: TabuConfig,
}

impl Worker {
    pub fn new(
        base_url: String,
        vertex_count: usize,
        clique_size: usize,
        campaign_id: i32,
        fleet: Option<String>,
        poll_interval_ms: u64,
        fetch_size: i32,
        publish_size: i32,
        publish_results: bool,
        top_results_count: usize,
        sa_mode: bool,
        sa_max_iterations: u64,
        sa_initial_temp: f64,
        sa_cooling_rate: f64,
        sa_min_pairs: usize,
        sa_max_pairs: usize,
        vds_mode: bool,
        vds_max_depth: usize,
        vds_top_first_edges: usize,
        vds_branching_factor: usize,
        vds_worsening_tolerance: i32,
        vds_random_seed: Option<u64>,
        vds_start_depth: usize,
        tabu_mode: bool,
        tabu_max_iterations: u64,
        tabu_base_tenure: usize,
        tabu_max_tenure: usize,
        tabu_restart_after: u64,
        tabu_candidate_pool_size: usize,
        tabu_diversification_pairs: usize,
        tabu_random_seed: Option<u64>,
        hoist_enabled: bool,
    ) -> Self {
        Worker {
            mw_client: MiddlewareClient::new(base_url),
            redis_client: None,
            vertex_count,
            clique_size,
            graph_cache: HashMap::new(),
            clique_collection_cache: HashMap::new(),
            hoist_cache: HashMap::new(),
            hoist_enabled,
            stage_announcements: Arc::new(StageAnnouncements::default()),
            retry_soon: false,
            stats_window_start: std::time::Instant::now(),
            stats_units: 0,
            stats_batches: 0,
            stats_busy_nanos: 0,
            stats_fills: 0,
            stats_fill_nanos: 0,
            stats_fills_red: 0,
            stats_fills_blue: 0,
            stats_fills_slice: 0,
            current_fetch_size: fetch_size,
            last_base_graph_id: None,
            hoist_carry: None,
            poll_interval: Duration::from_millis(poll_interval_ms),
            fetch_size,
            publish_size,
            campaign_id,
            fleet,
            stage_id: None,
            base_graph_clique_count: None,
            publish_results,
            top_results_count,
            stage_config: None,
            enumerator: None,
            sa_mode,
            sa_config: SaConfig {
                max_iterations: sa_max_iterations,
                initial_temp: sa_initial_temp,
                cooling_rate: sa_cooling_rate,
                min_pairs: sa_min_pairs,
                max_pairs: sa_max_pairs,
            },
            vds_mode,
            vds_config: VdsConfig {
                max_depth: vds_max_depth,
                top_first_edges: vds_top_first_edges,
                branching_factor: vds_branching_factor,
                worsening_tolerance: vds_worsening_tolerance,
                random_seed: vds_random_seed,
                start_depth: vds_start_depth,
            },
            tabu_mode,
            tabu_config: TabuConfig {
                max_iterations: tabu_max_iterations,
                base_tabu_tenure: tabu_base_tenure,
                max_tabu_tenure: tabu_max_tenure,
                restart_after: tabu_restart_after,
                candidate_pool_size: tabu_candidate_pool_size,
                diversification_pair_count: tabu_diversification_pairs,
                random_seed: tabu_random_seed,
            },
        }
    }

    /// Connect to Redis
    pub async fn connect_redis(&mut self, host: &str, port: u16) -> Result<(), Box<dyn Error>> {
        let redis_client = RedisClient::new(host, port).await?;
        self.redis_client = Some(redis_client);
        // A subscribed connection cannot serve commands, so the watcher gets its own.
        let announcements = Arc::clone(&self.stage_announcements);
        let (host, port) = (host.to_string(), port);
        tokio::spawn(async move { watch_stage_advances(&host, port, announcements).await });
        Ok(())
    }

    pub async fn initialize(&mut self) -> Result<(), Box<dyn Error>> {
        // Fleet mode: campaign is resolved dynamically from the fleet mapping, so
        // vertex_count/clique_size are set lazily the first time a stage is seen
        // (see get_or_fetch_stage_id). Nothing to fetch up front.
        if let Some(fleet) = &self.fleet {
            log_info!("Initializing worker for fleet: {}", fleet);
            return Ok(());
        }

        log_info!("Initializing worker for campaign ID: {}", self.campaign_id);

        let campaign = self.mw_client.get_campaign(self.campaign_id).await?;
        log_info!("Campaign Info: {:?}", campaign);
        self.vertex_count = campaign.vertex_count as usize;
        self.clique_size = campaign.subgraph_size as usize;

        Ok(())
    }

    pub async fn run(&mut self) {
        if let Err(e) = self.initialize().await {
            log_error!("Failed to initialize: {}", e);
            return;
        }

        log_info!("Worker started for campaign: {}", self.campaign_id);

        loop {
            let cycle_start = std::time::Instant::now();
            match self.cycle().await {
                Ok(count) => {
                    if count == 0 {
                        // A race (stage advanced under us) means work is waiting right now; only a
                        // genuinely idle fleet should wait out the poll interval.
                        let wait = if self.retry_soon {
                            self.retry_soon = false;
                            Duration::from_millis(TRANSIENT_RETRY_MILLIS)
                        } else {
                            self.poll_interval
                        };
                        sleep(wait).await;
                    } else {
                        let batch = cycle_start.elapsed();
                        log_debug!(
                            "Processed {} work items in {}ms",
                            count,
                            batch.as_millis()
                        );
                        self.stats_units += count as u64;
                        self.stats_batches += 1;
                        self.stats_busy_nanos += batch.as_nanos();
                        for t in self.hoist_cache.values_mut() {
                            let (f, n, red, blue, slice) = t.take_fill_stats();
                            self.stats_fills += f;
                            self.stats_fill_nanos += n;
                            self.stats_fills_red += red;
                            self.stats_fills_blue += blue;
                            self.stats_fills_slice += slice;
                        }
                        self.maybe_log_throughput();
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[{}] Error in worker cycle: {}",
                        Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"),
                        e
                    );
                    sleep(self.poll_interval).await;
                }
            }
        }
    }

    async fn cycle(&mut self) -> Result<usize, Box<dyn Error>> {
        // Get stage_id first. None => nothing to work (fleet paused / unmapped /
        // no active stage) => idle via the poll interval.
        let stage_id = match self.get_or_fetch_stage_id().await? {
            Some(id) => id,
            None => return Ok(0),
        };

        // Verify counter-based mode is available
        let redis_client = self.redis_client.as_mut().ok_or("Redis not connected")?;
        let has_counter = redis_client.has_stage_config(stage_id).await?;

        if !has_counter {
            // Stage config missing - likely stage progressed externally
            // Clear cache so we re-fetch the active stage on next cycle
            // Routine race now that stages advance sub-second, not an anomaly worth INFO.
            log_debug!(
                "Stage config missing for stage {} - stage may have progressed, refreshing...",
                stage_id
            );
            self.clear_stage_cache();
            self.retry_soon = true;
            return Ok(0);
        }

        if self.sa_mode {
            self.cycle_simulated_annealing(stage_id).await
        } else if self.vds_mode {
            self.cycle_variable_depth_search(stage_id).await
        } else if self.tabu_mode {
            self.cycle_tabu_search(stage_id).await
        } else {
            self.cycle_counter_based(stage_id).await
        }
    }

    async fn cycle_tabu_search(&mut self, stage_id: i32) -> Result<usize, Box<dyn Error>> {
        if self.stage_config.is_none() || self.stage_config.as_ref().unwrap().stage_id != stage_id {
            let redis_client = self.redis_client.as_mut().ok_or("Redis not connected")?;
            if let Some(config) = redis_client.get_stage_config(stage_id).await? {
                log_info!(
                    "Tabu: Loaded stage config: baseGraphId={}, strategy={:?}",
                    config.base_graph_id,
                    config.strategy
                );
                self.stage_config = Some(config);
            } else {
                return Err(
                    format!("Stage config not found in Redis for stage {}", stage_id).into(),
                );
            }
        }

        let config = self.stage_config.as_ref().unwrap();
        let base_graph_id = config.base_graph_id;
        let campaign_id_for_counter = self.campaign_id;

        let mut base_graph =
            Graph::from_bitstring(&config.graph.edge_data, config.graph.vertex_count);
        let all_cliques = get_all_cliques(&mut base_graph, self.clique_size);
        let mut clique_collection = CliqueCollection::new(self.vertex_count);
        clique_collection.set_cliques(all_cliques, self.vertex_count);
        get_cliques_comprehensive(&mut base_graph, self.clique_size);

        let threshold: Option<i32> = {
            let redis = self.redis_client.as_mut().ok_or("Redis not connected")?;
            redis
                .get_top_results_threshold(stage_id, self.top_results_count)
                .await
                .unwrap_or(None)
        };

        let result = run_tabu(
            &base_graph,
            self.clique_size,
            &self.tabu_config,
            &clique_collection,
            threshold,
        );

        // Submit if it beats the threshold (matches SA pattern: bitstring submission).
        let should_submit = match threshold {
            None => true,
            Some(t) => result.best_clique_count < t,
        };

        if should_submit {
            if let Some(redis) = self.redis_client.as_mut() {
                let _ = redis
                    .add_sa_result_to_top_results(
                        stage_id,
                        base_graph_id,
                        &result.best_graph_bitstring,
                        result.best_clique_count,
                        self.top_results_count,
                    )
                    .await;
            }
        }

        if let Some(redis) = self.redis_client.as_mut() {
            let _ = redis.increment_processed_count(stage_id, campaign_id_for_counter, 1).await;
        }

        Ok(1)
    }

    /// Counter-based work cycle: claim index ranges and enumerate locally
    async fn cycle_counter_based(&mut self, stage_id: i32) -> Result<usize, Box<dyn Error>> {
        // Ensure we have stage config cached
        if self.stage_config.is_none() || self.stage_config.as_ref().unwrap().stage_id != stage_id {
            let redis_client = self.redis_client.as_mut().ok_or("Redis not connected")?;
            if let Some(config) = redis_client.get_stage_config(stage_id).await? {
                log_debug!(
                    "Loaded stage config: strategy={:?}, totalPairs={}, baseGraphId={}",
                    config.strategy,
                    config.total_pairs,
                    config.base_graph_id
                );

                // Check if we already have this graph cached (reuse across stages!)
                if !self.graph_cache.contains_key(&config.base_graph_id) {
                    let graph_id = config.base_graph_id;

                    // FAST PATH: consecutive stages differ by a single edge flip, so derive this
                    // stage's graph AND counts from the previous stage's cached ones — a seeded
                    // traversal of one edge's neighbourhood (~0.4ms) instead of a whole-graph pass
                    // (~420ms at 800k cliques; measured 1075x). This matters because the counts are
                    // the serialized head of every stage: until they exist no worker can evaluate
                    // anything, so it set the floor on stage duration for the whole fleet.
                    if self.derive_from_previous_stage(&config) {
                        self.last_base_graph_id = Some(graph_id);
                        self.prune_graph_caches(graph_id);
                    } else {
                    log_info!(
                        "Building graph from stage_config (first time for graph {})",
                        config.base_graph_id
                    );
                    let graph =
                        Graph::from_bitstring(&config.graph.edge_data, config.graph.vertex_count);
                    self.graph_cache.insert(config.base_graph_id, graph);

                    // Per-edge clique cardinalities for this graph. This path reads only the
                    // per-edge counts and the total (see `broken` / `base_total` below), so we
                    // build counts-only — no clique list, no edge->cliques index. Cost scales
                    // with the clique count (~1s at 1.6M cliques), and every worker would
                    // otherwise pay it for the SAME graph on every stage, so the first one to
                    // build it shares it via Redis and the rest skip the traversal entirely.
                    let mut shared = match self.redis_client.as_mut() {
                        Some(redis) => redis.get_shared_edge_counts(graph_id).await.unwrap_or(None),
                        None => None,
                    };
                    // On a miss, elect ONE builder: every worker sees a new stage within
                    // milliseconds, so without this they all miss and all traverse in parallel
                    // (no sharing at all). Losers wait for the winner — they would have been
                    // busy building anyway, and freeing those cores lets the winner finish
                    // sooner. If the winner dies or is slow, the lock TTL expires / the wait
                    // times out and they build locally.
                    if shared.is_none() {
                        let i_build = match self.redis_client.as_mut() {
                            Some(redis) => redis
                                .try_acquire_edge_counts_build_lock(
                                    graph_id,
                                    EDGE_COUNTS_BUILD_LOCK_TTL_SECONDS,
                                )
                                .await
                                .unwrap_or(true),
                            None => true,
                        };
                        if !i_build {
                            for _ in 0..EDGE_COUNTS_WAIT_POLLS {
                                sleep(Duration::from_millis(EDGE_COUNTS_POLL_INTERVAL_MS)).await;
                                if let Some(redis) = self.redis_client.as_mut() {
                                    if let Some(v) =
                                        redis.get_shared_edge_counts(graph_id).await.unwrap_or(None)
                                    {
                                        shared = Some(v);
                                        break;
                                    }
                                }
                            }
                            if shared.is_none() {
                                log_info!(
                                    "Waited for shared edge counts for graph {} without success; building locally",
                                    graph_id
                                );
                            }
                        }
                    }
                    let cc = match shared {
                        Some((counts, total)) => {
                            log_info!(
                                "Reusing shared edge counts for graph {} (total cliques {})",
                                graph_id,
                                total
                            );
                            CliqueCollection::from_shared_counts(self.vertex_count, counts, total)
                        }
                        None => {
                            let graph = self.graph_cache.get_mut(&graph_id).unwrap();
                            let mut cc = CliqueCollection::new(self.vertex_count);
                            cc.build_counts_only(graph, self.clique_size);
                            log_info!(
                                "Built edge counts for graph {} (total cliques {}); sharing",
                                graph_id,
                                cc.total()
                            );
                            if let Some(redis) = self.redis_client.as_mut() {
                                if let Err(e) = redis
                                    .set_shared_edge_counts(
                                        graph_id,
                                        cc.edge_counts(),
                                        cc.total(),
                                        SHARED_EDGE_COUNTS_TTL_SECONDS,
                                    )
                                    .await
                                {
                                    log_error!("Could not share edge counts for graph {graph_id}: {e}");
                                }
                            }
                            cc
                        }
                    };
                    self.clique_collection_cache.insert(graph_id, cc);
                    self.last_base_graph_id = Some(graph_id);
                    self.prune_graph_caches(graph_id);
                    }
                } else {
                    log_debug!(
                        "Reusing cached graph {} for new stage {}",
                        config.base_graph_id,
                        stage_id
                    );
                }

                // Create enumerator for this stage (uses cached graph)
                let graph = self.graph_cache.get(&config.base_graph_id).unwrap();
                let enumerator = create_enumerator(&config.strategy, graph);
                if enumerator.total_work_units() != config.total_pairs {
                    return Err(format!(
                        "Enumerator total_work_units {} != stage config totalPairs {} for stage {} (strategy {:?}) — worker and queue manager disagree on the work space; refusing to process",
                        enumerator.total_work_units(),
                        config.total_pairs,
                        stage_id,
                        config.strategy
                    )
                    .into());
                }
                self.enumerator = Some(enumerator);
                self.stage_config = Some(config);
            } else {
                return Err(
                    format!("Stage config not found in Redis for stage {}", stage_id).into(),
                );
            }
        }

        let config = self.stage_config.as_ref().unwrap();
        let batch_size = self.current_fetch_size as i64;
        let total_pairs = config.total_pairs;
        let base_graph_id = config.base_graph_id;
        // Base graph bitstring + vertex count for derived-graph hashing (the novelty
        // filter). Captured once per batch — constant for the stage's base graph.
        // vertex_count must equal the QM's (config value) so the hashes agree.
        let base_bitstring = config.graph.edge_data.clone();
        let derived_vertex_count = config.graph.vertex_count;

        // Claim work range
        let redis_client = self.redis_client.as_mut().ok_or("Redis not connected")?;
        let range = redis_client
            .claim_work_range(stage_id, batch_size, total_pairs)
            .await?;

        let (start_index, end_index) = match range {
            Some(r) => r,
            None => {
                log_debug!("All work claimed for stage {}, clearing cache...", stage_id);
                self.clear_stage_cache();
                self.retry_soon = true;
                return Ok(0);
            }
        };

        let work_count = (end_index - start_index) as usize;
        let enumerator = self.enumerator.as_ref().unwrap();

        let clique_size = self.clique_size;
        let publish_results = self.publish_results;
        let campaign_id_for_counter = self.campaign_id;
        let graph_vertex_count = self.graph_cache[&base_graph_id].vertex_count;
        let engage = self.hoist_enabled && start_index >= HOIST_MIN_STAGE_INDEX;
        let first_time = engage && !self.hoist_cache.contains_key(&base_graph_id);
        if first_time {
            log_info!(
                "Hoist ENGAGED for graph {} at stage work index {} ({})",
                base_graph_id,
                start_index,
                if start_index < 200_000 { "past singles" } else { "late claim" }
            );
        }

        let announcements_for_wait = Arc::clone(&self.stage_announcements);
        // Co-operative fill: claim one slice of the edge space, publish it, and adopt whatever
        // peers have published. Anything still missing is computed on demand exactly as before, so
        // a crashed peer or an expired key costs a little time and nothing else.
        if first_time {
            // Seed from the previous stage's table where we have one. The co-operative fill below
            // is deliberately unchanged: `fill_slice` returns memoised values instantly for
            // carried entries, so our published slice is still complete and peers still cover the
            // rest -- carrying only removes work, never sharing.
            let mut tables = HoistTables::new(graph_vertex_count);
            let mut carried_kept = 0usize;
            if let Some((parent_id, flipped)) = self.hoist_carry.take() {
                if self.hoist_cache.contains_key(&parent_id)
                    && self.graph_cache.contains_key(&parent_id)
                    && self.graph_cache.contains_key(&base_graph_id)
                {
                    let mut t = self.hoist_cache[&parent_id].clone();
                    let invalidated = t.carry_forward(
                        &self.graph_cache[&parent_id],
                        &self.graph_cache[&base_graph_id],
                        &flipped,
                    );
                    carried_kept = t.filled();
                    tables = t;
                    log_info!(
                        "Hoist carried graph {} -> {}: kept {} of {} entries, invalidated {} from {} flip(s)",
                        parent_id,
                        base_graph_id,
                        carried_kept,
                        graph_vertex_count * (graph_vertex_count - 1) / 2,
                        invalidated,
                        flipped.len()
                    );
                }
            }
            let mut adopted = 0usize;
            if self.redis_client.is_some() {
                let graph_for_fill = self.graph_cache.get_mut(&base_graph_id).unwrap();
                // Seed from peers first so the slice we then fill is genuinely new work.
                if let Some(redis) = self.redis_client.as_mut() {
                    if let Ok(slices) = redis.get_hoist_slices(base_graph_id, HOIST_FILL_SLICES).await {
                        for (s, values) in slices {
                            tables.adopt_slice(s as usize, HOIST_FILL_SLICES as usize, &values);
                        }
                        adopted = tables.filled(); // seeded before we filled our own slice
                    }
                }
                let slice = match self.redis_client.as_mut() {
                    Some(redis) => redis
                        .claim_hoist_slice(base_graph_id, HOIST_FILL_SLICES)
                        .await
                        .unwrap_or(0),
                    None => 0,
                };
                let values = tables.fill_slice(
                    graph_for_fill,
                    clique_size,
                    slice as usize,
                    HOIST_FILL_SLICES as usize,
                );
                if let Some(redis) = self.redis_client.as_mut() {
                    if let Err(e) = redis.put_hoist_slice(base_graph_id, slice, &values).await {
                        log_error!("Could not publish hoist slice {slice} for graph {base_graph_id}: {e}");
                    }
                }
                // One more sweep for slices that landed while we were filling ours.
                if let Some(redis) = self.redis_client.as_mut() {
                    if let Ok(slices) = redis.get_hoist_slices(base_graph_id, HOIST_FILL_SLICES).await {
                        for (s, values) in slices {
                            tables.adopt_slice(s as usize, HOIST_FILL_SLICES as usize, &values);
                        }
                    }
                }
                // Report BOTH sweeps separately. The first races peers — they reach the gate when
                // we do, so it is near-zero by construction — while the second is where the
                // fleet's work actually shows up. Reporting only the first reads as "no sharing"
                // even when the table came back mostly filled by peers.
                // Wait briefly for peers before entering the loop.
                //
                // Measured: the fleet performs 4.4x more fill work than the information-theoretic
                // minimum, and blue entries alone are computed 5.2x over -- 14 workers each
                // recomputing the same values because they start looping before peers publish.
                // The waste is front-loaded and cannot be recovered by polling afterwards: blue is
                // the inner enumeration index, so a worker's FIRST batch touches all 19,810 blue
                // edges while coverage is still ~56%. 74% of on-demand misses are blue.
                //
                // Bounded three ways so this can never stall a stage: a hard deadline, an
                // early exit once coverage stops improving, and an immediate bail if the stage is
                // superseded -- which is what makes it safe on a mispredicted eager engage during
                // a descent, since such a stage advances and the wait ends on the spot.
                let wait_started = std::time::Instant::now();
                let mut last_known = tables.filled();
                let mut stagnant = 0u8;
                let mut present: std::collections::HashSet<i64> = std::collections::HashSet::new();
                while wait_started.elapsed() < HOIST_COVERAGE_WAIT
                    && stagnant < HOIST_COVERAGE_STAGNANT_POLLS
                {
                    if announcements_for_wait
                        .latest_for(campaign_id_for_counter)
                        .is_some_and(|announced| announced != stage_id)
                    {
                        break; // stage superseded — nothing here is worth waiting for
                    }
                    tokio::time::sleep(HOIST_COVERAGE_POLL).await;
                    if let Some(redis) = self.redis_client.as_mut() {
                        if let Ok(slices) =
                            redis.get_hoist_slices(base_graph_id, HOIST_FILL_SLICES).await
                        {
                            for (sl, vals) in slices {
                                present.insert(sl);
                                tables.adopt_slice(sl as usize, HOIST_FILL_SLICES as usize, &vals);
                            }
                        }
                    }
                    let now_known = tables.filled();
                    if now_known == last_known {
                        stagnant += 1;
                    } else {
                        stagnant = 0;
                        last_known = now_known;
                    }
                }
                let waited_ms = wait_started.elapsed().as_millis();

                // Self-healing gap fill.
                //
                // The claim is `(INCR - 1) mod HOIST_FILL_SLICES`, so a fleet SMALLER than the
                // slice count leaves the tail slices unclaimed forever: 14 workers against 16
                // slices means 4,953 edges are never published by anyone, and every worker refills
                // them on demand, every stage. That capped coverage at 87.5% and was the binding
                // constraint once the wait started collecting properly.
                //
                // Pinning the constant to the fleet size would break the moment the M1 resumes or
                // a burst joins, so instead take another turn of the SAME counter: claims
                // N+1.. land on exactly the slices the first round missed, and the INCR keeps two
                // workers from picking the same one. Skipped when the slice is already published,
                // and bounded to one extra slice per worker per graph.
                let mut extra_filled: Option<i64> = None;
                if !tables.is_complete() {
                    let extra = match self.redis_client.as_mut() {
                        Some(redis) => redis
                            .claim_hoist_slice(base_graph_id, HOIST_FILL_SLICES)
                            .await
                            .ok(),
                        None => None,
                    };
                    if let Some(extra) = extra {
                        if !present.contains(&extra) {
                            let vals = tables.fill_slice(
                                graph_for_fill,
                                clique_size,
                                extra as usize,
                                HOIST_FILL_SLICES as usize,
                            );
                            if let Some(redis) = self.redis_client.as_mut() {
                                if let Err(e) =
                                    redis.put_hoist_slice(base_graph_id, extra, &vals).await
                                {
                                    log_error!(
                                        "Could not publish gap slice {extra} for graph {base_graph_id}: {e}"
                                    );
                                }
                            }
                            extra_filled = Some(extra);
                        }
                    }
                }

                let edges = graph_vertex_count * (graph_vertex_count - 1) / 2;
                let known = tables.filled();
                log_info!(
                    "Hoist fill for graph {}: slice {} ({} edges computed here), {} seeded before + {} after publishing, {} of {} known ({}% from peers), waited {}ms, gap slice {:?}",
                    base_graph_id,
                    slice,
                    values.len(),
                    adopted,
                    known.saturating_sub(adopted + values.len()),
                    known,
                    edges,
                    100 * known.saturating_sub(values.len()) / edges.max(1),
                    waited_ms,
                    extra_filled
                );
            }
            self.hoist_cache.insert(base_graph_id, tables);
        }

        // Collect slices peers published after our own sweep. They reach the gate when we do and
        // publish at roughly the same moment, so the sweep taken right after filling our slice
        // usually races them; picking the rest up over the next few batches is what actually makes
        // the fill co-operative rather than 14 workers each doing the whole thing.
        if engage && !first_time {
            let wants = self
                .hoist_cache
                .get(&base_graph_id)
                .is_some_and(|t| t.wants_refresh());
            if wants {
                let fetched = match self.redis_client.as_mut() {
                    Some(redis) => redis
                        .get_hoist_slices(base_graph_id, HOIST_FILL_SLICES)
                        .await
                        .unwrap_or_default(),
                    None => Vec::new(),
                };
                if let Some(tables) = self.hoist_cache.get_mut(&base_graph_id) {
                    for (s, values) in fetched {
                        tables.adopt_slice(s as usize, HOIST_FILL_SLICES as usize, &values);
                    }
                    tables.note_refresh();
                }
            }
        }

        let graph = self.graph_cache.get_mut(&base_graph_id).unwrap();
        let clique_collection = self.clique_collection_cache.get(&base_graph_id).unwrap();
        let mut hoist = if engage {
            self.hoist_cache.get_mut(&base_graph_id)
        } else {
            None
        };

        // Fetch the current threshold for top-N results (None = accept anything).
        // Declared mut so it can be tightened in-loop as the sorted set fills up,
        // eliminating the burst of unfiltered submissions when a fresh stage starts.
        let mut top_threshold: Option<i32> = {
            let redis = self.redis_client.as_mut().ok_or("Redis not connected")?;
            redis
                .get_top_results_threshold(stage_id, self.top_results_count)
                .await
                .unwrap_or(None)
        };

        let mut processed_results: Vec<WorkResult> = Vec::new();

        // Process each work unit in the range
        let loop_started = std::time::Instant::now();
        let announcements = Arc::clone(&self.stage_announcements);
        let mut units_done: i64 = 0;
        let mut abandoned = false;
        for idx in start_index..end_index {
            // Abandon promptly when this stage has been superseded — everything computed past that
            // point is written to keys the queue manager has already cleared.
            if units_done % STAGE_CHECK_INTERVAL_UNITS == 0
                && units_done > 0
                && announcements
                    .latest_for(campaign_id_for_counter)
                    .is_some_and(|announced| announced != stage_id)
            {
                abandoned = true;
                break;
            }
            units_done += 1;
            let unit = enumerator.index_to_work_unit(idx);
            // Stack buffer, not a per-unit heap allocation. This was profiled at 0.2% and
            // deliberately left alone in the 2026-07-15 kernel round — correctly, when a unit cost
            // 13.5us of Bron-Kerbosch. The hoist and then the correction bound removed everything
            // that dwarfed it, and the same ~15ns is now 63-70% of what remains: measured
            // 0.0268 -> 0.0123 us/unit near the floor and 0.0368 -> 0.0177 mid-descent, i.e. ~2.2x
            // on the evaluation half of a worker's time. Nothing about the allocation changed —
            // only its share did.
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
            let base_total = clique_collection.total() as i32;

            // Early termination when not publishing: stop counting if result can't be in top-N.
            // If threshold exists: max_new = threshold - (base_total - broken) - 1, the most new
            // cliques that would still beat it.
            let early_limit = if !publish_results {
                match top_threshold {
                    Some(threshold) => {
                        let max_new = (threshold - 1) - base_total + broken;
                        if max_new < 0 {
                            // base_total - broken >= threshold: this flip cannot beat the
                            // threshold even if it creates ZERO new cliques, so the kernel can
                            // only confirm what the per-edge counts already prove. Skip it
                            // outright instead of paying two flip_edges plus a seeded traversal.
                            continue;
                        }
                        max_new
                    }
                    None => i32::MAX, // No threshold, count everything
                }
            } else {
                i32::MAX
            };

            // `created` is ~all of a work unit's cost. The hoisted path derives it algebraically
            // from memoised per-edge counts (see hoist.rs) instead of running a seeded traversal,
            // and is EXACT — the two branches agree unit for unit, so which one runs changes only
            // the cost, never a result, a threshold, or a stage transition.
            let (new, exceeded) = match hoist.as_deref_mut() {
                Some(tables) => match &unit {
                    WorkUnit::SingleFlip(edge) => {
                        // A single flip's `created` IS the table entry, so there is nothing to
                        // abort; the kernel's early exit becomes a comparison.
                        let created = tables.single_created(
                            graph,
                            clique_size,
                            edge.vertex_one as usize,
                            edge.vertex_two as usize,
                        );
                        (created, created > early_limit)
                    }
                    // Pairs take the bounded form: `created >= D_r` (cross pairs all red) or
                    // `>= C_b` (all blue), both already in hand, so most units that cannot beat
                    // the limit are rejected without computing the correction — which is 98% of
                    // this loop's cost. Exact, not a prune: see `pair_created_bounded`.
                    WorkUnit::PairFlip(red_edge, blue_edge) => {
                        match tables.pair_created_bounded(
                            graph,
                            clique_size,
                            (red_edge.vertex_one as usize, red_edge.vertex_two as usize),
                            (blue_edge.vertex_one as usize, blue_edge.vertex_two as usize),
                            early_limit,
                        ) {
                            Some(created) => (created, false),
                            // Provably over the limit. The count is never read once `exceeded`
                            // is set — the loop continues immediately.
                            None => (0, true),
                        }
                    }
                },
                None => {
                    graph.flip_edges(edges_to_flip);
                    let out =
                        get_new_cliques_with_limit(graph, clique_size, edges_to_flip, early_limit);
                    graph.flip_edges(edges_to_flip);
                    out
                }
            };

            if exceeded {
                continue;
            }
            let count = base_total - broken + new;

            // Track as a potential best result (stored in top-N sorted set)
            // Submit if: threshold is None (set not full) OR count < threshold (better than worst)
            let should_submit = match top_threshold {
                None => true, // Set is not full, accept any result
                Some(threshold) => count < threshold,
            };
            if should_submit {
                // Derived-graph hash so the set stays novel-only (slot 0 = best novel).
                // Only computed for record-breakers (count < best novel), so it's rare.
                let hash = crate::hash::derived_graph_hash(
                    &base_bitstring,
                    derived_vertex_count,
                    edges_to_flip,
                );
                if let Some(redis) = self.redis_client.as_mut() {
                    if let Ok((kept, new_threshold)) = redis
                        .add_to_top_results(
                            stage_id,
                            base_graph_id,
                            edges_to_flip,
                            count,
                            &hash,
                            self.top_results_count,
                        )
                        .await
                    {
                        // Only a real insert is news; a rejected (already-visited) candidate
                        // changes nothing for the QM. Fire-and-forget — the QM keeps a polling
                        // fallback, so a dropped message costs latency, not correctness.
                        if kept {
                            let _ = redis.publish_best_result(stage_id, count).await;
                        }
                        // Update threshold in-place so early termination tightens
                        // within this batch rather than staying stale for all 250K units.
                        if let Some(t) = new_threshold {
                            top_threshold = Some(match top_threshold {
                                Some(current) => current.min(t),
                                None => t,
                            });
                        }
                    }
                }
            }

            // Collect results for publishing
            if self.publish_results {
                let result = WorkResult {
                    id: None,
                    base_graph_id,
                    stage_id,
                    edges_to_flip: edges_to_flip.to_vec(),
                    clique_count: count,
                    work_unit_analysis_type: WorkUnitAnalysisType::TARGETED,
                };
                processed_results.push(result);

                if processed_results.len() >= self.publish_size as usize {
                    self.mw_client.submit_results(&processed_results).await?;
                    processed_results.clear();
                }
            }
        }

        let loop_elapsed = loop_started.elapsed();
        if abandoned {
            log_info!(
                "Abandoned stage {} after {} of {} units — a newer stage was announced",
                stage_id,
                units_done,
                work_count
            );
        }
        // Size the next batch, and report progress, from units ACTUALLY processed. An abandoned
        // batch did not do the rest of its claim, and counting it would both inflate throughput
        // and make the batch look faster per unit than it was.
        self.current_fetch_size = next_fetch_size(
            self.current_fetch_size,
            self.fetch_size,
            units_done,
            loop_elapsed.as_nanos(),
        );

        // Submit remaining results
        if self.publish_results && !processed_results.is_empty() {
            self.mw_client.submit_results(&processed_results).await?;
        }

        // Update processed count
        if units_done > 0 {
            if let Some(redis) = self.redis_client.as_mut() {
                let _ = redis
                    .increment_processed_count(stage_id, campaign_id_for_counter, units_done)
                    .await;
            }
        }

        Ok(work_count)
    }

    async fn cycle_simulated_annealing(&mut self, stage_id: i32) -> Result<usize, Box<dyn Error>> {
        // Ensure we have stage config cached
        if self.stage_config.is_none() || self.stage_config.as_ref().unwrap().stage_id != stage_id {
            let redis_client = self.redis_client.as_mut().ok_or("Redis not connected")?;
            if let Some(config) = redis_client.get_stage_config(stage_id).await? {
                log_info!(
                    "SA: Loaded stage config: baseGraphId={}, strategy={:?}",
                    config.base_graph_id,
                    config.strategy
                );
                self.stage_config = Some(config);
            } else {
                return Err(
                    format!("Stage config not found in Redis for stage {}", stage_id).into(),
                );
            }
        }

        let config = self.stage_config.as_ref().unwrap();
        let base_graph_id = config.base_graph_id;
        let campaign_id_for_counter = self.campaign_id;

        // Build graph and CliqueCollection from stage config.
        // The clique collection provides per-edge participation scores for guided edge selection.
        let mut base_graph =
            Graph::from_bitstring(&config.graph.edge_data, config.graph.vertex_count);
        let all_cliques = get_all_cliques(&mut base_graph, self.clique_size);
        let mut clique_collection = CliqueCollection::new(self.vertex_count);
        clique_collection.set_cliques(all_cliques, self.vertex_count);

        // Recount cliques after get_all_cliques (which may mutate graph state)
        get_cliques_comprehensive(&mut base_graph, self.clique_size);

        // Get current threshold for top-N filtering
        let threshold: Option<i32> = {
            let redis = self.redis_client.as_mut().ok_or("Redis not connected")?;
            redis
                .get_top_results_threshold(stage_id, self.top_results_count)
                .await
                .unwrap_or(None)
        };

        // Run one complete SA schedule
        let result = run_sa(
            &base_graph,
            self.clique_size,
            &self.sa_config,
            &clique_collection,
            threshold,
        );

        // Submit best result to Redis if it's worth tracking
        let should_submit = match threshold {
            None => true,
            Some(t) => result.best_clique_count < t,
        };

        if should_submit {
            if let Some(redis) = self.redis_client.as_mut() {
                let _ = redis
                    .add_sa_result_to_top_results(
                        stage_id,
                        base_graph_id,
                        &result.best_graph_bitstring,
                        result.best_clique_count,
                        self.top_results_count,
                    )
                    .await;
            }
        }

        // Update processed count (1 per SA run)
        if let Some(redis) = self.redis_client.as_mut() {
            let _ = redis.increment_processed_count(stage_id, campaign_id_for_counter, 1).await;
        }

        // Return 1 to indicate work was done (avoids poll sleep)
        Ok(1)
    }

    async fn cycle_variable_depth_search(
        &mut self,
        stage_id: i32,
    ) -> Result<usize, Box<dyn Error>> {
        // Ensure we have stage config cached
        if self.stage_config.is_none() || self.stage_config.as_ref().unwrap().stage_id != stage_id {
            let redis_client = self.redis_client.as_mut().ok_or("Redis not connected")?;
            if let Some(config) = redis_client.get_stage_config(stage_id).await? {
                log_info!(
                    "VDS: Loaded stage config: baseGraphId={}, strategy={:?}",
                    config.base_graph_id,
                    config.strategy
                );
                self.stage_config = Some(config);
            } else {
                return Err(
                    format!("Stage config not found in Redis for stage {}", stage_id).into(),
                );
            }
        }

        let config = self.stage_config.as_ref().unwrap();
        let base_graph_id = config.base_graph_id;
        let campaign_id_for_counter = self.campaign_id;
        let base_bitstring = config.graph.edge_data.clone();
        let derived_vertex_count = config.graph.vertex_count;

        // Build graph and CliqueCollection from stage config.
        let mut base_graph =
            Graph::from_bitstring(&config.graph.edge_data, config.graph.vertex_count);
        let all_cliques = get_all_cliques(&mut base_graph, self.clique_size);
        let base_clique_count = all_cliques.len() as i32;
        let mut clique_collection = CliqueCollection::new(self.vertex_count);
        clique_collection.set_cliques(all_cliques, self.vertex_count);

        // Recount cliques after get_all_cliques (which may mutate graph state)
        get_cliques_comprehensive(&mut base_graph, self.clique_size);

        // Get current threshold for top-N filtering
        let threshold: Option<i32> = {
            let redis = self.redis_client.as_mut().ok_or("Redis not connected")?;
            redis
                .get_top_results_threshold(stage_id, self.top_results_count)
                .await
                .unwrap_or(None)
        };

        // Run one complete VDS
        let result = run_vds(
            &base_graph,
            self.clique_size,
            &self.vds_config,
            &clique_collection,
            base_clique_count,
        );

        // Submit best result to Redis if it improved and beats the threshold
        let should_submit = result.improved
            && match threshold {
                None => true,
                Some(t) => result.final_clique_count < t,
            };

        if should_submit {
            let hash = crate::hash::derived_graph_hash(
                &base_bitstring,
                derived_vertex_count,
                &result.edges_to_flip,
            );
            if let Some(redis) = self.redis_client.as_mut() {
                let _ = redis
                    .add_to_top_results(
                        stage_id,
                        base_graph_id,
                        &result.edges_to_flip,
                        result.final_clique_count,
                        &hash,
                        self.top_results_count,
                    )
                    .await;
            }
        }

        // Update processed count (1 per VDS run)
        if let Some(redis) = self.redis_client.as_mut() {
            let _ = redis.increment_processed_count(stage_id, campaign_id_for_counter, 1).await;
        }

        Ok(1)
    }

    /// Try to derive this stage's graph and per-edge clique counts from the previous stage's,
    /// which differ by only the edge(s) the search just flipped. Returns false when there is
    /// nothing to derive from (cold start) or the graphs are too far apart, leaving the caller to
    /// do a full build.
    fn derive_from_previous_stage(&mut self, config: &StageConfig) -> bool {
        let graph_id = config.base_graph_id;
        let Some(prev_id) = self.last_base_graph_id else {
            return false;
        };
        if prev_id == graph_id
            || !self.graph_cache.contains_key(&prev_id)
            || !self.clique_collection_cache.contains_key(&prev_id)
        {
            return false;
        }

        let new_bits = &config.graph.edge_data;
        let prev_bits = self.graph_cache[&prev_id].to_bitstring();
        if prev_bits.len() != new_bits.len() {
            return false;
        }
        let flipped: Vec<usize> = prev_bits
            .chars()
            .zip(new_bits.chars())
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .collect();
        if flipped.is_empty() || flipped.len() > MAX_INCREMENTAL_FLIPS {
            return false;
        }

        let mut graph = self.graph_cache[&prev_id].clone();
        let mut cc = self.clique_collection_cache[&prev_id].clone();
        for &bit in &flipped {
            match Graph::edge_for_bit_index(bit, config.graph.vertex_count) {
                Some((u, v)) => cc.apply_edge_flip(&mut graph, self.clique_size, u, v),
                None => return false,
            }
        }
        // Cheap belt-and-braces: the derived graph must be exactly the stage's graph. If this ever
        // fails we fall back to a full build rather than search a wrong graph.
        if graph.to_bitstring() != *new_bits {
            log_error!(
                "Derived graph {} does not match stage config; falling back to full build",
                graph_id
            );
            return false;
        }

        log_info!(
            "Derived edge counts for graph {} from {} via {} flip(s) (total cliques {})",
            graph_id,
            prev_id,
            flipped.len(),
            cc.total()
        );
        // Remember what this advance changed so the hoist engage path can carry the parent's
        // per-edge table forward. ~90% of its 39,621 entries survive a 1-2 edge advance, and
        // rebuilding all of them is the largest fixed cost of a short stage.
        let flipped_edges: Vec<(usize, usize)> = flipped
            .iter()
            .filter_map(|&bit| Graph::edge_for_bit_index(bit, config.graph.vertex_count))
            .collect();
        self.hoist_carry = Some((prev_id, flipped_edges));
        self.graph_cache.insert(graph_id, graph);
        self.clique_collection_cache.insert(graph_id, cc);
        true
    }

    /// Keep only the newest few graphs/collections. Without this the caches grow by ~360 KB per
    /// stage forever (thousands of stages per descent), and the incremental path only ever needs
    /// the immediately preceding one.
    fn prune_graph_caches(&mut self, keep: i32) {
        if self.graph_cache.len() <= GRAPH_CACHE_MAX {
            return;
        }
        let mut ids: Vec<i32> = self.graph_cache.keys().copied().collect();
        ids.sort_unstable();
        let drop_count = ids.len().saturating_sub(GRAPH_CACHE_MAX);
        for id in ids.into_iter().take(drop_count) {
            if id != keep {
                self.graph_cache.remove(&id);
                self.clique_collection_cache.remove(&id);
                self.hoist_cache.remove(&id);
            }
        }
    }

    /// Emit one throughput line per `STATS_INTERVAL_SECS` and reset the window.
    ///
    /// Reports units/sec — the figure that actually gets read — plus the busy fraction, which
    /// tells whether the worker is compute-bound or waiting on Redis/HTTP.
    fn maybe_log_throughput(&mut self) {
        let window = self.stats_window_start.elapsed();
        if window.as_secs() < STATS_INTERVAL_SECS {
            return;
        }
        let secs = window.as_secs_f64();
        let busy_secs = self.stats_busy_nanos as f64 / 1e9;
        log_info!(
            "Throughput: {} units in {:.0}s ({:.2}M units/sec) over {} batches, avg {:.0}ms/batch, {:.0}% busy, {} on-demand fills costing {:.2}s ({:.0}% of busy) [slice {} | misses {} = {} red + {} blue]",
            self.stats_units,
            secs,
            self.stats_units as f64 / secs / 1e6,
            self.stats_batches,
            busy_secs * 1000.0 / self.stats_batches.max(1) as f64,
            100.0 * busy_secs / secs,
            self.stats_fills,
            self.stats_fill_nanos as f64 / 1e9,
            100.0 * (self.stats_fill_nanos as f64 / 1e9) / busy_secs.max(1e-9),
            self.stats_fills_slice,
            self.stats_fills.saturating_sub(self.stats_fills_slice),
            self.stats_fills_red,
            self.stats_fills_blue
        );
        self.stats_window_start = std::time::Instant::now();
        self.stats_units = 0;
        self.stats_batches = 0;
        self.stats_fills = 0;
        self.stats_fill_nanos = 0;
        self.stats_fills_red = 0;
        self.stats_fills_blue = 0;
        self.stats_fills_slice = 0;
        self.stats_busy_nanos = 0;
    }

    fn clear_stage_cache(&mut self) {
        self.stage_id = None;
        // A new stage starts unhoisted, so shrink back to the responsive size rather than
        // carrying the previous stage's hoisted batch into its warmup.
        self.current_fetch_size = self.fetch_size;
        self.base_graph_clique_count = None;
        self.stage_config = None;
        self.enumerator = None;
        // Clear graph caches to prevent memory leak on stage progression
    }

    /// Drop the cross-stage graph/collection caches. These are keyed by GRAPH id and are exactly
    /// what lets the next stage be derived from the previous one, so they must survive an ordinary
    /// stage advance — only a campaign change (different vertex_count/clique_size, which would
    /// make cached collections the wrong shape) or going idle should clear them.
    fn clear_graph_caches(&mut self) {
        self.graph_cache.clear();
        self.clique_collection_cache.clear();
        self.hoist_cache.clear();
        self.last_base_graph_id = None;
    }

    /// Resolve the stage to work. Ok(None) means "nothing to do right now"
    /// (fleet paused / unmapped / no active stage) so the caller should idle.
    async fn get_or_fetch_stage_id(&mut self) -> Result<Option<i32>, Box<dyn Error>> {
        // ---- Fleet mode: re-resolve each cycle so repoints/pauses take effect
        // within one poll, with no redeploy. ----
        if let Some(fleet) = self.fleet.clone() {
            let stage = match self.mw_client.get_fleet_active_stage(&fleet).await? {
                None => {
                    // Paused / unmapped / no active stage. Drop any cached stage
                    // so we re-init cleanly when work reappears, then idle.
                    if self.stage_id.is_some() {
                        log_info!("Fleet {} has no active stage — idling", fleet);
                        self.clear_stage_cache();
                        self.clear_graph_caches();
                    }
                    return Ok(None);
                }
                Some(s) => s,
            };

            // The middleware caches its active-stage answer, so it can name a stage that has
            // already been superseded — and by the time we ask Redis for that stage's config the
            // queue manager has deleted it, costing a wasted cycle and a retry sleep.
            //
            // A stage-advance announcement is published only after the new stage is ACTIVE *and*
            // its config is seeded, so an announced stage is always workable. When it is newer
            // than what the cache returned, it is strictly better information — prefer it.
            let announced = self.stage_announcements.latest_for(stage.campaign_id);
            let stage_id = effective_stage_id(stage.stage_id, announced);
            if stage_id != stage.stage_id {
                log_debug!(
                    "Middleware named stale stage {} for campaign {}; using announced stage {}",
                    stage.stage_id,
                    stage.campaign_id,
                    stage_id
                );
            }

            if self.stage_id != Some(stage_id) {
                // Fleet repointed or the stage progressed → reset per-stage state. NOTE: the
                // graph/collection caches deliberately survive, so the new stage (one flip away)
                // can be derived from the previous one instead of rebuilt from scratch.
                self.clear_stage_cache();
                // Different campaign → refresh vertex/clique params (as initialize
                // does in campaign mode), so CliqueCollection sizing stays correct.
                let repointed = self.campaign_id != stage.campaign_id;
                if repointed {
                    self.clear_graph_caches(); // cached collections are sized for the old campaign
                    self.campaign_id = stage.campaign_id;
                    if let Ok(campaign) = self.mw_client.get_campaign(stage.campaign_id).await {
                        self.vertex_count = campaign.vertex_count as usize;
                        self.clique_size = campaign.subgraph_size as usize;
                    }
                }
                self.stage_id = Some(stage_id);
                // Only the middleware's own answer carries a base graph id; when the announcement
                // superseded it that field describes the older stage, so don't report it.
                let base_graph = if stage_id == stage.stage_id {
                    stage.base_graph_id.to_string()
                } else {
                    "from stage config".to_string()
                };
                // A repoint is rare and operationally significant, so it stays at INFO. Following
                // the stage counter is neither — it happens more than once a second per worker.
                if repointed {
                    log_info!(
                        "Fleet {} REPOINTED to campaign {} → stage {} (base_graph_id {})",
                        fleet,
                        stage.campaign_id,
                        stage_id,
                        base_graph
                    );
                } else {
                    log_debug!(
                        "Fleet {} → stage {} (campaign {}, base_graph_id {})",
                        fleet,
                        stage_id,
                        stage.campaign_id,
                        base_graph
                    );
                }
            }
            return Ok(Some(stage_id));
        }

        // ---- Campaign mode (legacy fallback, RAMSEY_CAMPAIGN_ID) ----
        if let Some(stage_id) = self.stage_id {
            return Ok(Some(stage_id));
        }

        let stages = self
            .mw_client
            .get_stages_by_campaign(self.campaign_id, "ACTIVE")
            .await?;

        if stages.is_empty() {
            return Err("No active stage found for campaign".into());
        }

        if stages.len() > 1 {
            log_error!(
                "Warning: Multiple active stages for campaign {}, using first",
                self.campaign_id
            );
        }

        let stage = &stages[0];
        log_info!(
            "Using stage {} for campaign {} (base_graph_id: {})",
            stage.stage_id,
            self.campaign_id,
            stage.base_graph_id
        );

        let graph_data = self.mw_client.get_graph(stage.base_graph_id).await?;
        self.base_graph_clique_count = graph_data.clique_count;
        log_info!(
            "Base graph clique count: {:?}",
            self.base_graph_clique_count
        );

        self.stage_id = Some(stage.stage_id);
        Ok(Some(stage.stage_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u128 = 1_000_000;

    /// The gate sits just past the singles block, so every pair unit is hoisted.
    ///
    /// Replaced a 5M gate plus two generations of predictor. The predictors failed because the
    /// asymmetry is 58:1 in favour of engaging (35.4 s of unhoisted ramp versus a 0.61 s wasted
    /// fill) — at that ratio the right answer is "almost always engage", and any predictor is a
    /// liability rather than an optimisation.
    #[test]
    fn gate_opens_immediately_after_the_singles_block() {
        // 282 vertices -> C(282,2) = 39,621 single flips, then the pair space.
        let singles = 282 * 281 / 2;
        assert_eq!(singles, 39_621);
        assert!(
            HOIST_MIN_STAGE_INDEX > singles as i64,
            "gate must clear the singles block so it does not fire mid-singles"
        );
        assert!(
            HOIST_MIN_STAGE_INDEX < 200_000,
            "gate must be a rounding error against the ~392.5M pair space; was 5M, which cost              2.6-41s of unhoisted kernel per stage"
        );
    }

    /// The middleware's cached answer can name a stage the queue manager has already retired,
    /// whose Redis config is therefore gone. A newer announcement is authoritative — it is only
    /// published once the stage is ACTIVE and seeded — so prefer it.
    #[test]
    fn prefers_a_newer_announced_stage_over_a_stale_cached_one() {
        assert_eq!(effective_stage_id(100, Some(103)), 103);
    }

    /// Guard rails: nothing may drag a worker backwards onto a dead stage.
    #[test]
    fn never_moves_backwards_or_sideways() {
        assert_eq!(effective_stage_id(100, None), 100, "no announcement yet");
        assert_eq!(effective_stage_id(100, Some(100)), 100, "agreement");
        assert_eq!(
            effective_stage_id(100, Some(97)),
            100,
            "a stale announcement must not override a newer middleware answer"
        );
    }

    /// Hoisted units (~0.26us) should drive the batch toward ~200ms of work, i.e. ~770k units,
    /// but only 4x per step so one fast batch cannot overshoot.
    #[test]
    fn grows_toward_the_target_when_units_are_cheap() {
        let floor = 25_000;
        let mut size = floor;
        // 0.26us/unit: a batch of `size` takes size * 260ns.
        for _ in 0..6 {
            let nanos = size as u128 * 260;
            size = next_fetch_size(size, floor, size as i64, nanos);
        }
        assert!(size > 700_000 && size <= MAX_FETCH_SIZE, "settled at {size}");
    }

    /// Regression guard for the ceiling silently becoming the binding constraint.
    ///
    /// Once the correction bound landed, a unit cost ~0.083us (8-12M units/sec), so the 200ms
    /// target wants ~2.4M units — more than the old 1M ceiling allowed. The controller pinned
    /// there, batches ran ~77ms instead of 200ms, and the fixed ~17ms per-cycle cost was paid 2.6x
    /// too often: measured live as busy 95% -> 82%, i.e. per-cycle overhead 5% -> 18%.
    ///
    /// Nothing failed when that happened — the ceiling is meant to be hit occasionally, so there
    /// was no signal. This asserts it is NOT hit at production throughput, so the next speedup
    /// trips a test instead of quietly costing 10% of the fleet.
    #[test]
    fn ceiling_does_not_bind_at_post_bound_throughput() {
        let floor = 2_000;
        let mut size = floor;
        for _ in 0..12 {
            let nanos = size as u128 * 83; // 0.083us/unit ~= 12M units/sec
            size = next_fetch_size(size, floor, size as i64, nanos);
        }
        assert!(size > 2_000_000, "settled at {size}, short of the 200ms target");
        assert!(
            size < MAX_FETCH_SIZE,
            "pinned at the ceiling ({size}) — MAX_FETCH_SIZE is binding again, raise it"
        );
    }

    #[test]
    fn growth_is_capped_at_four_x_per_step() {
        let floor = 25_000;
        // Absurdly fast batch: still only 4x.
        assert_eq!(next_fetch_size(25_000, floor, 25_000, 1), 100_000);
    }

    /// Unhoisted units (~4us) belong in small batches so a worker stays responsive to stage
    /// changes — this is the fast-descent case.
    #[test]
    fn shrinks_when_units_are_expensive() {
        let floor = 25_000;
        let size = 1_000_000;
        // 4us/unit
        let next = next_fetch_size(size, floor, size as i64, size as u128 * 4_000);
        assert_eq!(next, 50_000, "200ms / 4us = 50k units");
    }

    #[test]
    fn never_leaves_the_floor_ceiling_band() {
        let floor = 25_000;
        // Extremely slow units would ask for < floor.
        assert_eq!(next_fetch_size(500_000, floor, 1_000, 1_000 * MS), floor);
        // Extremely fast units are capped by MAX_FETCH_SIZE, not just the growth step.
        assert_eq!(next_fetch_size(MAX_FETCH_SIZE, floor, 1_000_000, 1), MAX_FETCH_SIZE);
    }

    #[test]
    fn degenerate_inputs_leave_the_size_untouched() {
        let floor = 25_000;
        assert_eq!(next_fetch_size(123_456, floor, 0, 5 * MS), 123_456);
        assert_eq!(next_fetch_size(123_456, floor, 100, 0), 123_456);
    }

    /// A batch already at the target duration should stay put rather than oscillate.
    #[test]
    fn holds_steady_at_the_target_duration() {
        let floor = 25_000;
        let size = 800_000;
        let nanos = TARGET_BATCH_LOOP_MILLIS * MS; // exactly the target
        let next = next_fetch_size(size, floor, size as i64, nanos);
        assert_eq!(next, size, "should not move when already on target");
    }
}
