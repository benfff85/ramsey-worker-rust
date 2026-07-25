use crate::graph::WorkUnitEdge;
use crate::model::StageConfig;
use crate::{log_error, log_info};
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::{Deserialize, Serialize};
use std::error::Error;

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
            return current
            "#,
        );

        // Retry with backoff
        for attempt in 0..3u32 {
            match script
                .key(&index_key)
                .arg(batch_size)
                .arg(total_pairs)
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
                local b = redis.call('ZRANGE', KEYS[1], 0, 0, 'WITHSCORES')
                if b[2] then return {0, tonumber(b[2])} else return {0, -1} end
            end
            redis.call('ZADD', KEYS[1], ARGV[1], ARGV[2])
            redis.call('ZREMRANGEBYRANK', KEYS[1], ARGV[4], -1)
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
    pub async fn increment_processed_count(
        &mut self,
        stage_id: i32,
        count: i64,
    ) -> Result<i64, Box<dyn Error>> {
        let key = format!("processed_count:{}", stage_id);
        let new_count: i64 = self.connection.incr(&key, count).await?;
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
