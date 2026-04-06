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
    /// Atomically adds and trims to keep only the best N results (lowest clique counts).
    /// Returns true if this result is currently in the top N, false otherwise.
    pub async fn add_to_top_results(
        &mut self,
        stage_id: i32,
        base_graph_id: i32,
        edges_to_flip: &[WorkUnitEdge],
        clique_count: i32,
        max_results: usize,
    ) -> Result<bool, Box<dyn Error>> {
        let key = format!("best_results:{}", stage_id);

        let result = BestResult {
            base_graph_id,
            stage_id,
            edges_to_flip: edges_to_flip.to_vec(),
            clique_count,
        };
        let json = serde_json::to_string(&result)?;

        // Lua script: ZADD, then ZREMRANGEBYRANK to keep only top N (lowest scores)
        // Returns 1 if this result is still in the set after trim, 0 otherwise
        let script = redis::Script::new(
            r#"
            redis.call('ZADD', KEYS[1], ARGV[1], ARGV[2])
            redis.call('ZREMRANGEBYRANK', KEYS[1], ARGV[3], -1)
            local rank = redis.call('ZRANK', KEYS[1], ARGV[2])
            if rank then
                return 1
            else
                return 0
            end
            "#,
        );

        let kept: i32 = script
            .key(&key)
            .arg(clique_count)
            .arg(&json)
            .arg(max_results as i64) // Keep indices 0 to max_results-1, remove from max_results onward
            .invoke_async(&mut self.connection)
            .await?;

        if kept == 1 {
            log_info!(
                "Added to top-{} results for stage {}: clique_count={}",
                max_results,
                stage_id,
                clique_count
            );
        }

        Ok(kept == 1)
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
    ) -> Result<bool, Box<dyn Error>> {
        let key = format!("best_results:{}", stage_id);

        let result = SaBestResult {
            base_graph_id,
            stage_id,
            graph_bitstring: graph_bitstring.to_string(),
            clique_count,
        };
        let json = serde_json::to_string(&result)?;

        // Same Lua script as add_to_top_results: ZADD + trim + check rank
        let script = redis::Script::new(
            r#"
            redis.call('ZADD', KEYS[1], ARGV[1], ARGV[2])
            redis.call('ZREMRANGEBYRANK', KEYS[1], ARGV[3], -1)
            local rank = redis.call('ZRANK', KEYS[1], ARGV[2])
            if rank then
                return 1
            else
                return 0
            end
            "#,
        );

        let kept: i32 = script
            .key(&key)
            .arg(clique_count)
            .arg(&json)
            .arg(max_results as i64)
            .invoke_async(&mut self.connection)
            .await?;

        if kept == 1 {
            log_info!(
                "SA: Added to top-{} results for stage {}: clique_count={}",
                max_results,
                stage_id,
                clique_count
            );
        }

        Ok(kept == 1)
    }

    /// Get the threshold score (worst/highest score in top-N) for a stage.
    /// Returns None if there are fewer than max_results entries (any result would be accepted).
    /// Returns Some(threshold) if the set is "full" - only results better than this should be submitted.
    pub async fn get_top_results_threshold(
        &mut self,
        stage_id: i32,
        max_results: usize,
    ) -> Result<Option<i32>, Box<dyn Error>> {
        let key = format!("best_results:{}", stage_id);

        // Get the count of entries in the set
        let count: i64 = redis::cmd("ZCARD")
            .arg(&key)
            .query_async(&mut self.connection)
            .await?;

        if (count as usize) < max_results {
            // Set isn't full yet - accept any result
            return Ok(None);
        }

        // Get the score of the last (worst) entry: index max_results-1
        let scores: Vec<(String, f64)> = redis::cmd("ZRANGE")
            .arg(&key)
            .arg((max_results - 1) as i64)
            .arg((max_results - 1) as i64)
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
}
