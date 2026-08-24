use crate::graph::WorkUnitEdge;
use crate::model::StageConfig;
use crate::{log_error, log_info};
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::{Deserialize, Serialize};
use std::error::Error;

/// Lifetime of a published hoist slice. Only needs to outlive the window in which a fleet is
/// working one graph — seconds to minutes — and the short life keeps Redis bounded, since a new
/// graph appears on every stage advance.
const HOIST_SHARD_TTL_SECONDS: u64 = 300;

/// Channel the queue manager announces stage advances on. One channel carries every campaign and
/// subscribers filter, so a fleet being repointed needs no resubscribe.
pub const STAGE_ADVANCED_CHANNEL: &str = "stage_advanced";

/// The newest stage a campaign has been announced on, shared between the subscriber task and the
/// work loop. Stored as one atomic so a reader can never see a campaign and stage from different
/// announcements; the work loop reads it every few thousand units, so it has to be cheap.
#[derive(Debug, Default)]
pub struct StageAnnouncements {
    packed: std::sync::atomic::AtomicI64,
}

impl StageAnnouncements {
    pub fn set(&self, campaign_id: i32, stage_id: i32) {
        let packed = ((campaign_id as i64) << 32) | (stage_id as i64 & 0xFFFF_FFFF);
        self.packed
            .store(packed, std::sync::atomic::Ordering::Relaxed);
    }

    /// The latest announced stage for `campaign_id`, if the last announcement was for it.
    #[inline]
    pub fn latest_for(&self, campaign_id: i32) -> Option<i32> {
        let packed = self.packed.load(std::sync::atomic::Ordering::Relaxed);
        if packed == 0 {
            return None;
        }
        let announced_campaign = (packed >> 32) as i32;
        if announced_campaign != campaign_id {
            return None;
        }
        Some(packed as i32)
    }
}

/// Watch for stage-advance announcements, keeping `announcements` current.
///
/// Runs on its own connection because a subscribed Redis connection cannot serve commands. Loops
/// forever, reconnecting after a failure: workers still poll the fleet endpoint every cycle, so a
/// subscription that is down costs the latency this exists to remove and nothing more.
pub async fn watch_stage_advances(
    host: &str,
    port: u16,
    announcements: std::sync::Arc<StageAnnouncements>,
) {
    use futures_util::StreamExt;
    let url = format!("redis://{}:{}", host, port);
    loop {
        match redis::Client::open(url.as_str()) {
            Ok(client) => match client.get_async_pubsub().await {
                Ok(mut pubsub) => {
                    if pubsub.subscribe(STAGE_ADVANCED_CHANNEL).await.is_ok() {
                        log_info!("Watching '{}' for stage advances", STAGE_ADVANCED_CHANNEL);
                        let mut stream = pubsub.on_message();
                        while let Some(msg) = stream.next().await {
                            if let Ok(payload) = msg.get_payload::<String>() {
                                if let Some((campaign_id, stage_id)) = parse_stage_advance(&payload) {
                                    announcements.set(campaign_id, stage_id);
                                }
                            }
                        }
                    }
                }
                Err(e) => log_error!("Stage-advance subscribe failed: {e}"),
            },
            Err(e) => log_error!("Stage-advance client failed: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Pull the ids out of `{"campaignId":10,"stageId":42}` without pulling in a JSON parser for two
/// integers on a hot-ish path.
fn parse_stage_advance(payload: &str) -> Option<(i32, i32)> {
    let field = |name: &str| -> Option<i32> {
        let at = payload.find(name)? + name.len();
        let rest = &payload[at..];
        let start = rest.find(|c: char| c.is_ascii_digit() || c == '-')?;
        let end = rest[start..]
            .find(|c: char| !c.is_ascii_digit() && c != '-')
            .unwrap_or(rest.len() - start);
        rest[start..start + end].parse().ok()
    };
    Some((field("\"campaignId\"")?, field("\"stageId\"")?))
}

/// Pub/sub channel workers announce new best results on. The queue manager subscribes and arms
/// a settle timer, so it neither polls blindly nor adopts the very first (usually weakest)
/// improvement. Must match the QM's RedisListenerConfig.BEST_RESULT_CHANNEL.
pub const BEST_RESULT_CHANNEL: &str = "best_result_events";

/// Best result for a stage - stored in Redis for stage progression
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BestResult {
    #[serde(rename = "baseGraphId")]
    pub base_graph_id: i32,
    #[serde(rename = "stageId")]
    pub stage_id: i32,
    #[serde(rename = "edgesToFlip")]
    pub edges_to_flip: Vec<WorkUnitEdge>,
    #[serde(rename = "cliqueCount")]
    pub clique_count: i32,
}

/// SA best result - stored in Redis with full graph bitstring instead of edges to flip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaBestResult {
    #[serde(rename = "baseGraphId")]
    pub base_graph_id: i32,
    #[serde(rename = "stageId")]
    pub stage_id: i32,
    #[serde(rename = "graphBitstring")]
    pub graph_bitstring: String,
    #[serde(rename = "cliqueCount")]
    pub clique_count: i32,
}

/// Redis client for work queue operations
pub struct RedisClient {
    connection: ConnectionManager,
}

/// Lifetime for per-stage Redis keys, REFRESHED on every write.
///
/// The queue manager deletes a stage's keys when it advances, but a worker that claims or submits
/// against a just-retired stage RECREATES them afterwards, and nothing deletes them a second time.
/// Measured on the live instance: 713,374 keys / 653 MB had accumulated, and adding the missing
/// `processed_count` delete only cut its leak by 19% — stragglers recreated it on 81% of stages.
/// Deletion cannot win that race; expiry can.
///
/// Refreshed rather than set-once because a live stage must never lose its keys: stages are ~3.4 s
/// on campaign 3 today, but earlier eras ran 95-minute sweeps. Since workers write constantly while
/// a stage is live, the TTL is continually pushed out, and only a stage nobody has touched for an
/// hour expires.
const STAGE_KEY_TTL_SECS: i64 = 3600;

impl RedisClient {
    /// Create a new Redis client with timeout configuration
    pub async fn new(host: &str, port: u16) -> Result<Self, Box<dyn Error>> {
        let redis_url = format!("redis://{}:{}", host, port);
        log_info!("Connecting to Redis at {}", redis_url);

        let client = redis::Client::open(redis_url)?;

        // ConnectionManager handles automatic reconnection
        // Retry connection with backoff for initial setup
        let mut last_error = None;
        for attempt in 1..=3 {
            match ConnectionManager::new(client.clone()).await {
                Ok(conn) => {
                    log_info!("Connected to Redis");
                    return Ok(RedisClient { connection: conn });
                }
                Err(e) => {
                    let delay_ms = 1000 * attempt;
                    log_error!(
                        "Redis connection attempt {}/3 failed: {}. Retrying in {}ms...",
                        attempt,
                        e,
                        delay_ms
                    );
                    last_error = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
            }
        }

        Err(Box::new(last_error.unwrap()))
    }

    // ========== Counter-Based Work Distribution Methods ==========

    /// Claim a range of work indices atomically using a Lua script.
    /// Prevents counter from exceeding total_pairs by checking before incrementing.
    /// Returns Some((start_index, end_index)) if work is available, None if all work claimed.
    /// Includes retry logic for transient network failures.
    pub async fn claim_work_range(
        &mut self,
        stage_id: i32,
        batch_size: i64,
        total_pairs: i64,
    ) -> Result<Option<(i64, i64)>, Box<dyn Error>> {
        let index_key = format!("stage_work_index:{}", stage_id);

        // Lua script: atomically check and increment, never exceeding total_pairs
        // Returns: start_index if work available, -1 if exhausted
        let script = redis::Script::new(
            r#"
            local current = tonumber(redis.call('GET', KEYS[1]) or '0')
            local batch = tonumber(ARGV[1])
            local total = tonumber(ARGV[2])
            if current >= total then
                return -1
            end
            local new_end = current + batch
            redis.call('SET', KEYS[1], new_end)
            redis.call('EXPIRE', KEYS[1], ARGV[3])
            return current
            "#,
        );

        // Retry with backoff
        for attempt in 0..3u32 {
            match script
                .key(&index_key)
                .arg(batch_size)
                .arg(total_pairs)
                .arg(STAGE_KEY_TTL_SECS)
                .invoke_async::<i64>(&mut self.connection)
                .await
            {
                Ok(start_index) => {
                    if start_index < 0 {
                        // All work has been claimed
                        return Ok(None);
                    }

                    let end_index = start_index + batch_size;
                    // Clamp end_index to total_pairs
                    let clamped_end = end_index.min(total_pairs);

                    return Ok(Some((start_index, clamped_end)));
                }
                Err(e) => {
                    if attempt == 2 {
                        return Err(Box::new(e));
                    }
                    let delay = 500 * 2u64.pow(attempt);
                    log_error!(
                        "Redis claim_work_range failed (attempt {}/3): {}. Retrying in {}ms...",
                        attempt + 1,
                        e,
                        delay
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            }
        }

        Ok(None) // Unreachable
    }

    /// Get the stage configuration from Redis (includes graph data).
    /// This should be fetched once per stage and cached locally.
    /// Includes retry logic for transient network failures.
    pub async fn get_stage_config(
        &mut self,
        stage_id: i32,
    ) -> Result<Option<StageConfig>, Box<dyn Error>> {
        let config_key = format!("stage_config:{}", stage_id);

        // GET with inline retry
        let mut result: Option<String> = None;
        for attempt in 0..3u32 {
            match self.connection.get::<_, Option<String>>(&config_key).await {
                Ok(val) => {
                    result = val;
                    break;
                }
                Err(e) => {
                    if attempt == 2 {
                        return Err(Box::new(e));
                    }
                    let delay = 500 * 2u64.pow(attempt);
                    log_error!(
                        "Redis get_stage_config failed (attempt {}/3): {}. Retrying in {}ms...",
                        attempt + 1,
                        e,
                        delay
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            }
        }

        match result {
            Some(json) => {
                let config = serde_json::from_str::<StageConfig>(&json)?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    /// Check if stage config exists (indicates counter-based mode).
    /// Includes retry logic for transient network failures.
    pub async fn has_stage_config(&mut self, stage_id: i32) -> Result<bool, Box<dyn Error>> {
        let config_key = format!("stage_config:{}", stage_id);

        // EXISTS with inline retry
        for attempt in 0..3u32 {
            match self.connection.exists::<_, bool>(&config_key).await {
                Ok(exists) => return Ok(exists),
                Err(e) => {
                    if attempt == 2 {
                        return Err(Box::new(e));
                    }
                    let delay = 500 * 2u64.pow(attempt);
                    log_error!(
                        "Redis has_stage_config failed (attempt {}/3): {}. Retrying in {}ms...",
                        attempt + 1,
                        e,
                        delay
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            }
        }

        Ok(false) // Unreachable
    }

    // ========== Top-N Best Results Tracking (Sorted Set) ==========

    /// Add a result to the top-N sorted set for a stage.
    /// Uses Redis sorted set with score = clique_count.
    /// Atomically add a result to the per-stage best-results set, **filtering out
    /// graphs already in `processed_graph_hashes`** (visited / cycle-prevention) so
    /// the set holds only progressable graphs. Trims to the best `max_results`.
    /// Returns `(kept, best_novel)` where `best_novel` is the slot-0 (lowest) score —
    /// the best novel result so far and the tightest early-exit threshold — or None
    /// if the set is empty. `hash` is the derived-graph SHA-256 (see `crate::hash`).
    pub async fn add_to_top_results(
        &mut self,
        stage_id: i32,
        base_graph_id: i32,
        edges_to_flip: &[WorkUnitEdge],
        clique_count: i32,
        hash: &str,
        max_results: usize,
    ) -> Result<(bool, Option<i32>), Box<dyn Error>> {
        let key = format!("best_results:{}", stage_id);

        let result = BestResult {
            base_graph_id,
            stage_id,
            edges_to_flip: edges_to_flip.to_vec(),
            clique_count,
        };
        let json = serde_json::to_string(&result)?;

        // KEYS[1] = best_results:{stage}, KEYS[2] = processed_graph_hashes.
        // ARGV[1] = score, ARGV[2] = json, ARGV[3] = derived hash, ARGV[4] = max_results.
        // If the graph is already visited, reject (don't insert) but still return the
        // current best so the caller's threshold stays fresh. Otherwise ZADD + trim to
        // the best max_results. Returns {kept, best_score} where best_score is slot 0
        // (the best novel result), or -1 if the set is empty.
        let script = redis::Script::new(
            r#"
            if redis.call('SISMEMBER', KEYS[2], ARGV[3]) == 1 then
                redis.call('EXPIRE', KEYS[1], ARGV[5])
                local b = redis.call('ZRANGE', KEYS[1], 0, 0, 'WITHSCORES')
                if b[2] then return {0, tonumber(b[2])} else return {0, -1} end
            end
            redis.call('ZADD', KEYS[1], ARGV[1], ARGV[2])
            redis.call('ZREMRANGEBYRANK', KEYS[1], ARGV[4], -1)
            redis.call('EXPIRE', KEYS[1], ARGV[5])
            local rank = redis.call('ZRANK', KEYS[1], ARGV[2])
            local kept = 0
            if rank then kept = 1 end
            local b = redis.call('ZRANGE', KEYS[1], 0, 0, 'WITHSCORES')
            if b[2] then return {kept, tonumber(b[2])} else return {kept, -1} end
            "#,
        );

        let raw: Vec<redis::Value> = script
            .key(&key)
            .key("processed_graph_hashes")
            .arg(clique_count)
            .arg(&json)
            .arg(hash)
            .arg(max_results as i64)
            .arg(STAGE_KEY_TTL_SECS)
            .invoke_async(&mut self.connection)
            .await?;

        let kept = matches!(raw.first(), Some(redis::Value::Int(1)));
        let new_threshold = match raw.get(1) {
            Some(redis::Value::Int(n)) if *n >= 0 => Some(*n as i32),
            _ => None,
        };

        if kept {
            log_info!(
                "Added to top-{} novel results for stage {}: clique_count={}",
                max_results,
                stage_id,
                clique_count
            );
        }

        Ok((kept, new_threshold))
    }

    /// Add an SA result to the top-N sorted set for a stage.
    /// Uses the same sorted set key as exhaustive workers, but the member JSON
    /// contains a graphBitstring field instead of edgesToFlip.
    pub async fn add_sa_result_to_top_results(
        &mut self,
        stage_id: i32,
        base_graph_id: i32,
        graph_bitstring: &str,
        clique_count: i32,
        max_results: usize,
    ) -> Result<(bool, Option<i32>), Box<dyn Error>> {
        let key = format!("best_results:{}", stage_id);

        let result = SaBestResult {
            base_graph_id,
            stage_id,
            graph_bitstring: graph_bitstring.to_string(),
            clique_count,
        };
        let json = serde_json::to_string(&result)?;

        // Same Lua script as add_to_top_results: ZADD + trim + check rank + return threshold
        let script = redis::Script::new(
            r#"
            redis.call('ZADD', KEYS[1], ARGV[1], ARGV[2])
            redis.call('ZREMRANGEBYRANK', KEYS[1], ARGV[3], -1)
            local rank = redis.call('ZRANK', KEYS[1], ARGV[2])
            local kept = 0
            if rank then kept = 1 end
            local size = redis.call('ZCARD', KEYS[1])
            if tonumber(size) >= tonumber(ARGV[3]) then
                local worst = redis.call('ZRANGE', KEYS[1], tonumber(ARGV[3])-1, tonumber(ARGV[3])-1, 'WITHSCORES')
                return {kept, tonumber(worst[2])}
            else
                return {kept, -1}
            end
            "#,
        );

        let raw: Vec<redis::Value> = script
            .key(&key)
            .arg(clique_count)
            .arg(&json)
            .arg(max_results as i64)
            .invoke_async(&mut self.connection)
            .await?;

        let kept = matches!(raw.first(), Some(redis::Value::Int(1)));
        let new_threshold = match raw.get(1) {
            Some(redis::Value::Int(n)) if *n >= 0 => Some(*n as i32),
            _ => None,
        };

        if kept {
            log_info!(
                "SA: Added to top-{} results for stage {}: clique_count={}",
                max_results,
                stage_id,
                clique_count
            );
        }

        Ok((kept, new_threshold))
    }

    /// Threshold score for early termination = the best NOVEL result (slot 0, the
    /// lowest clique count). Active as soon as the set is non-empty, so early-exit
    /// kicks in after the first novel result instead of the max_results-th — the
    /// tightest correct threshold. `max_results` is unused here (the set is already
    /// novel-only and trimmed by `add_to_top_results`); kept for call-site parity.
    pub async fn get_top_results_threshold(
        &mut self,
        stage_id: i32,
        _max_results: usize,
    ) -> Result<Option<i32>, Box<dyn Error>> {
        let key = format!("best_results:{}", stage_id);

        // Slot 0 = lowest score = best novel result currently in the set.
        let scores: Vec<(String, f64)> = redis::cmd("ZRANGE")
            .arg(&key)
            .arg(0)
            .arg(0)
            .arg("WITHSCORES")
            .query_async(&mut self.connection)
            .await?;

        match scores.first() {
            Some((_, score)) => Ok(Some(*score as i32)),
            None => Ok(None),
        }
    }

    /// Get the current best (lowest clique count) result for a stage from the sorted set.
    /// This returns the result with rank 0 (lowest score).
    pub async fn get_best_result(
        &mut self,
        stage_id: i32,
    ) -> Result<Option<BestResult>, Box<dyn Error>> {
        let key = format!("best_results:{}", stage_id);

        // ZRANGE with LIMIT 0 1 gets the single lowest score
        let results: Vec<String> = redis::cmd("ZRANGE")
            .arg(&key)
            .arg(0)
            .arg(0)
            .query_async(&mut self.connection)
            .await?;

        match results.first() {
            Some(json) => {
                let best = serde_json::from_str::<BestResult>(json)?;
                Ok(Some(best))
            }
            None => Ok(None),
        }
    }

    // ========== Legacy Best Result (single key - for backward compatibility) ==========

    /// Update best result if the new result is better (lower clique count)
    /// DEPRECATED: Use add_to_top_results instead. Keeping for backward compatibility.
    pub async fn update_best_if_better(
        &mut self,
        stage_id: i32,
        base_graph_id: i32,
        edges_to_flip: &[WorkUnitEdge],
        clique_count: i32,
    ) -> Result<bool, Box<dyn Error>> {
        let key = format!("best_result:{}", stage_id);

        // Get current best
        let current: Option<String> = self.connection.get(&key).await?;

        let should_update = match current {
            Some(json) => {
                match serde_json::from_str::<BestResult>(&json) {
                    Ok(current_best) => clique_count < current_best.clique_count,
                    Err(_) => true, // Invalid JSON, overwrite
                }
            }
            None => true, // No current best, set it
        };

        if should_update {
            let new_best = BestResult {
                base_graph_id,
                stage_id,
                edges_to_flip: edges_to_flip.to_vec(),
                clique_count,
            };
            let json = serde_json::to_string(&new_best)?;
            self.connection.set::<_, _, ()>(&key, json).await?;
            log_info!(
                "New best result for stage {}: clique_count={}",
                stage_id,
                clique_count
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }

    // ========== Progress Tracking ==========

    /// Increment the processed work unit count for a stage
    /// Record work done, both against the stage and against the campaign.
    ///
    /// The per-stage counter is deleted when a stage advances, so anything differencing it loses
    /// every unit across a turnover — near the floor stages now advance faster than once a second,
    /// which made fleet throughput read as ~0. The campaign-scoped total never resets, so it can be
    /// differenced across stage boundaries. Both are pipelined into one round trip, so this stays
    /// as cheap as the single increment it replaces.
    pub async fn increment_processed_count(
        &mut self,
        stage_id: i32,
        campaign_id: i32,
        count: i64,
    ) -> Result<i64, Box<dyn Error>> {
        let stage_key = format!("processed_count:{}", stage_id);
        let campaign_key = format!("processed_total:{}", campaign_id);
        let (new_count, _, _): (i64, i64, i64) = redis::pipe()
            .atomic()
            .incr(&stage_key, count)
            .incr(&campaign_key, count)
            .expire(&stage_key, STAGE_KEY_TTL_SECS)
            .query_async(&mut self.connection)
            .await?;
        Ok(new_count)
    }

    // ========== Shared per-edge clique counts ==========
    //
    // Every worker otherwise recomputes the SAME per-edge clique cardinalities for each new
    // stage's base graph (the "new graph tax": a full Bron-Kerbosch traversal whose cost scales
    // with the clique count — ~1s on a 1.6M-clique graph, paid by every worker in parallel on
    // the same box). The first worker to build them shares them here; peers fetch and skip the
    // traversal. Keyed by GRAPH id (immutable content) with a short TTL so Redis stays bounded.
    //
    // Blob layout: [0..8) total clique count (u64 LE), then vertex_count^2 i32 LE counts.

    fn edge_counts_key(graph_id: i32) -> String {
        format!("clique_edge_counts:{}", graph_id)
    }

    /// Fetch shared per-edge clique counts for a graph. Returns (counts, total_clique_count).
    pub async fn get_shared_edge_counts(
        &mut self,
        graph_id: i32,
    ) -> Result<Option<(Vec<i32>, usize)>, Box<dyn Error>> {
        let key = Self::edge_counts_key(graph_id);
        let blob: Option<Vec<u8>> = self.connection.get(&key).await?;
        let Some(blob) = blob else {
            return Ok(None);
        };
        if blob.len() < 8 || (blob.len() - 8) % 4 != 0 {
            log_error!(
                "Shared edge counts for graph {} malformed ({} bytes); ignoring",
                graph_id,
                blob.len()
            );
            return Ok(None);
        }
        let total = u64::from_le_bytes(blob[0..8].try_into().unwrap()) as usize;
        let counts = blob[8..]
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        Ok(Some((counts, total)))
    }

    // ========== Co-operative hoist-table fill ==========
    //
    // Filling the per-edge hoist table costs an uncapped traversal per edge — ~2.9s for a
    // 282-vertex graph — and every worker on that graph would otherwise pay it in full, in
    // parallel, at the start of every deep stage. Splitting the edge space lets each worker fill
    // one slice and publish it, so the fleet does the work once between them.
    //
    // Keys are scoped by GRAPH id, which is globally unique, so fleets on different campaigns
    // never collide and fleets that happen to share a graph co-operate automatically.
    //
    // A slice is only ever a warm start: [`HoistTables::adopt_slice`] computes anything missing on
    // demand, so a crashed peer, an expired key or a slice that never lands costs time, never
    // correctness.

    fn hoist_shard_key(graph_id: i32, slice: i64) -> String {
        format!("hoist_shard:{}:{}", graph_id, slice)
    }

    /// Claim a slice of the edge space for this graph. Workers claiming concurrently get distinct
    /// slices until the count wraps, at which point a duplicate is harmless — it just recomputes
    /// what a peer already published.
    pub async fn claim_hoist_slice(
        &mut self,
        graph_id: i32,
        slices: i64,
    ) -> Result<i64, Box<dyn Error>> {
        let key = format!("hoist_claim:{}", graph_id);
        let n: i64 = self.connection.incr(&key, 1i64).await?;
        // Expire the counter with the slices themselves so a long-dead graph leaves nothing behind.
        let _: () = self.connection.expire(&key, HOIST_SHARD_TTL_SECONDS as i64).await?;
        Ok((n - 1).rem_euclid(slices))
    }

    /// Publish this worker's slice for peers to adopt.
    pub async fn put_hoist_slice(
        &mut self,
        graph_id: i32,
        slice: i64,
        values: &[i32],
    ) -> Result<(), Box<dyn Error>> {
        let mut blob = Vec::with_capacity(values.len() * 4);
        for v in values {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        let key = Self::hoist_shard_key(graph_id, slice);
        let _: () = self
            .connection
            .set_ex(&key, blob, HOIST_SHARD_TTL_SECONDS)
            .await?;
        Ok(())
    }

    /// Fetch every published slice for this graph in one round trip. Returns `(slice, values)` for
    /// those present; absent or malformed slices are simply omitted.
    pub async fn get_hoist_slices(
        &mut self,
        graph_id: i32,
        slices: i64,
    ) -> Result<Vec<(i64, Vec<i32>)>, Box<dyn Error>> {
        let keys: Vec<String> = (0..slices).map(|s| Self::hoist_shard_key(graph_id, s)).collect();
        let blobs: Vec<Option<Vec<u8>>> = self.connection.mget(&keys).await?;
        let mut out = Vec::new();
        for (s, blob) in blobs.into_iter().enumerate() {
            let Some(blob) = blob else { continue };
            if blob.is_empty() || blob.len() % 4 != 0 {
                log_error!(
                    "Hoist slice {} for graph {} malformed ({} bytes); ignoring",
                    s,
                    graph_id,
                    blob.len()
                );
                continue;
            }
            let values = blob
                .chunks_exact(4)
                .map(|c| i32::from_le_bytes(c.try_into().unwrap()))
                .collect();
            out.push((s as i64, values));
        }
        Ok(out)
    }

    /// Announce that a new best (novel, record-breaking) result landed for a stage, so the queue
    /// manager can start its settle timer immediately instead of discovering the improvement on
    /// its next poll. Fire-and-forget: Redis pub/sub has no delivery guarantee and the QM keeps a
    /// polling fallback, so a dropped message costs a little latency, never correctness.
    pub async fn publish_best_result(
        &mut self,
        stage_id: i32,
        clique_count: i32,
    ) -> Result<(), Box<dyn Error>> {
        let payload = format!(
            "{{\"stageId\":{},\"cliqueCount\":{}}}",
            stage_id, clique_count
        );
        let _: i64 = self
            .connection
            .publish(BEST_RESULT_CHANNEL, payload)
            .await?;
        Ok(())
    }

    /// Elect a single builder for a graph's edge counts: returns true for the one caller that
    /// wins the SET NX. Losers wait for the winner's result instead of all doing the same
    /// traversal at once (without this, every worker sees a new stage within milliseconds, all
    /// miss the cache, and all build in parallel — no sharing at all). The TTL means a builder
    /// that dies only delays peers, who then fall back to building locally.
    pub async fn try_acquire_edge_counts_build_lock(
        &mut self,
        graph_id: i32,
        ttl_seconds: u64,
    ) -> Result<bool, Box<dyn Error>> {
        let key = format!("clique_edge_counts_lock:{}", graph_id);
        let acquired: Option<String> = redis::cmd("SET")
            .arg(&key)
            .arg("1")
            .arg("NX")
            .arg("EX")
            .arg(ttl_seconds)
            .query_async(&mut self.connection)
            .await?;
        Ok(acquired.is_some())
    }

    /// Share per-edge clique counts for a graph with a TTL. Best-effort: on failure peers
    /// simply rebuild locally, so callers may ignore the error.
    pub async fn set_shared_edge_counts(
        &mut self,
        graph_id: i32,
        counts: &[i32],
        total_clique_count: usize,
        ttl_seconds: u64,
    ) -> Result<(), Box<dyn Error>> {
        let key = Self::edge_counts_key(graph_id);
        let mut blob = Vec::with_capacity(8 + counts.len() * 4);
        blob.extend_from_slice(&(total_clique_count as u64).to_le_bytes());
        for c in counts {
            blob.extend_from_slice(&c.to_le_bytes());
        }
        self.connection
            .set_ex::<_, _, ()>(&key, blob, ttl_seconds)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_queue_managers_announcement() {
        assert_eq!(
            parse_stage_advance("{\"campaignId\":10,\"stageId\":141248}"),
            Some((10, 141248))
        );
    }

    #[test]
    fn ignores_a_malformed_announcement() {
        assert_eq!(parse_stage_advance("not json"), None);
        assert_eq!(parse_stage_advance("{\"campaignId\":10}"), None);
    }

    /// A worker must only act on announcements for the campaign it is working. One channel serves
    /// every campaign, so another fleet's advance must not make this one abandon its stage.
    #[test]
    fn announcements_are_scoped_to_a_campaign() {
        let a = StageAnnouncements::default();
        assert_eq!(a.latest_for(10), None, "nothing announced yet");

        a.set(10, 500);
        assert_eq!(a.latest_for(10), Some(500));
        assert_eq!(a.latest_for(11), None, "another campaign must not match");

        a.set(11, 900);
        assert_eq!(a.latest_for(11), Some(900));
        assert_eq!(a.latest_for(10), None, "superseded by a different campaign");
    }

    /// Stage ids are well past 2^15 in production; make sure the packing survives large values.
    #[test]
    fn packing_survives_large_ids() {
        let a = StageAnnouncements::default();
        a.set(10, 2_000_000_000);
        assert_eq!(a.latest_for(10), Some(2_000_000_000));
    }
    // ===== Live-Redis integration tests (run with --ignored; needs Redis on 36002) =====
    //
    // Every per-stage key must carry a TTL. The queue manager deletes them on stage advance, but
    // stragglers still writing against a retired stage recreate them afterwards, so deletion
    // cannot win that race and the keyspace grows without bound (measured 3.90 keys/stage).
    // Expiry is what actually bounds it, so each write path is checked for a live TTL here.

    async fn test_client() -> RedisClient {
        RedisClient::new("127.0.0.1", 36002)
            .await
            .expect("live Redis on 127.0.0.1:36002 required for --ignored tests")
    }

    async fn ttl_of(c: &mut RedisClient, key: &str) -> i64 {
        redis::cmd("TTL")
            .arg(key)
            .query_async(&mut c.connection)
            .await
            .expect("TTL")
    }

    /// A unique stage id per run so concurrent fleets on this Redis can never collide with it.
    fn scratch_stage_id() -> i32 {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        -((nanos % 1_000_000) as i32) - 1 // negative: production stage ids are positive
    }

    #[tokio::test]
    #[ignore]
    async fn claiming_work_gives_the_index_key_a_ttl() {
        let mut c = test_client().await;
        let stage = scratch_stage_id();
        let key = format!("stage_work_index:{}", stage);

        c.claim_work_range(stage, 10, 1000).await.unwrap().unwrap();
        let ttl = ttl_of(&mut c, &key).await;

        let _: () = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut c.connection)
            .await
            .unwrap();
        assert!(ttl > 0, "stage_work_index must expire, got TTL {}", ttl);
    }

    #[tokio::test]
    #[ignore]
    async fn counting_processed_work_gives_the_counter_a_ttl_but_not_the_campaign_total() {
        let mut c = test_client().await;
        let stage = scratch_stage_id();
        let campaign = scratch_stage_id();
        let stage_key = format!("processed_count:{}", stage);
        let campaign_key = format!("processed_total:{}", campaign);

        c.increment_processed_count(stage, campaign, 5).await.unwrap();
        let stage_ttl = ttl_of(&mut c, &stage_key).await;
        let campaign_ttl = ttl_of(&mut c, &campaign_key).await;

        let _: () = redis::cmd("DEL")
            .arg(&stage_key)
            .arg(&campaign_key)
            .query_async(&mut c.connection)
            .await
            .unwrap();
        assert!(stage_ttl > 0, "processed_count must expire, got TTL {}", stage_ttl);
        assert_eq!(
            campaign_ttl, -1,
            "the campaign total is differenced across stage turnovers and must never expire"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn recording_a_result_gives_the_best_results_key_a_ttl() {
        let mut c = test_client().await;
        let stage = scratch_stage_id();
        let key = format!("best_results:{}", stage);

        c.add_to_top_results(stage, 1, &[], 12345, "ttl-test-novel-hash", 10)
            .await
            .unwrap();
        let ttl = ttl_of(&mut c, &key).await;

        let _: () = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut c.connection)
            .await
            .unwrap();
        assert!(ttl > 0, "best_results must expire, got TTL {}", ttl);
    }

    /// A stage that keeps rediscovering already-visited graphs still writes nothing new, but it is
    /// very much alive — its TTL must keep getting pushed out or the key vanishes underneath it.
    #[tokio::test]
    #[ignore]
    async fn a_repeat_visit_still_refreshes_the_best_results_ttl() {
        let mut c = test_client().await;
        let stage = scratch_stage_id();
        let key = format!("best_results:{}", stage);
        let hash = format!("ttl-test-visited-{}", stage);

        c.add_to_top_results(stage, 1, &[], 12345, "ttl-test-novel-hash", 10)
            .await
            .unwrap();
        let _: () = redis::cmd("SADD")
            .arg("processed_graph_hashes")
            .arg(&hash)
            .query_async(&mut c.connection)
            .await
            .unwrap();
        // Shrink the TTL, then submit a graph we have already visited.
        let _: () = redis::cmd("EXPIRE")
            .arg(&key)
            .arg(5)
            .query_async(&mut c.connection)
            .await
            .unwrap();
        let (kept, _) = c
            .add_to_top_results(stage, 1, &[], 999, &hash, 10)
            .await
            .unwrap();
        let ttl = ttl_of(&mut c, &key).await;

        let _: () = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut c.connection)
            .await
            .unwrap();
        let _: () = redis::cmd("SREM")
            .arg("processed_graph_hashes")
            .arg(&hash)
            .query_async(&mut c.connection)
            .await
            .unwrap();
        assert!(!kept, "an already-visited graph must be rejected");
        assert!(
            ttl > 5,
            "the repeat-visit path must push the TTL back out, got {}",
            ttl
        );
    }

}
