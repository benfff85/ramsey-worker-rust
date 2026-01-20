use crate::graph::WorkUnitEdge;
use crate::model::StageConfig;
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::{Deserialize, Serialize};
use std::error::Error;

/// Work queue item - parsed from compact format: baseGraphId|v1,v2|v1,v2
#[derive(Debug, Clone)]
pub struct WorkQueueItem {
    pub base_graph_id: i32,
    pub edges_to_flip: Vec<WorkUnitEdge>,
}

/// Parse compact format: baseGraphId|v1,v2|v1,v2
fn parse_compact_work_item(compact: &str) -> Result<WorkQueueItem, Box<dyn Error + Send + Sync>> {
    let parts: Vec<&str> = compact.split('|').collect();
    if parts.len() < 3 {
        return Err(format!("Invalid format, expected at least 3 parts: {}", compact).into());
    }

    let base_graph_id: i32 = parts[0].parse()?;
    let mut edges_to_flip = Vec::with_capacity(parts.len() - 1);

    for edge_str in &parts[1..] {
        let vertices: Vec<&str> = edge_str.split(',').collect();
        if vertices.len() != 2 {
            return Err(format!("Invalid edge format: {}", edge_str).into());
        }
        edges_to_flip.push(WorkUnitEdge {
            vertex_one: vertices[0].parse()?,
            vertex_two: vertices[1].parse()?,
        });
    }

    Ok(WorkQueueItem {
        base_graph_id,
        edges_to_flip,
    })
}

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

/// Redis client for work queue operations
pub struct RedisClient {
    connection: ConnectionManager,
}

impl RedisClient {
    /// Create a new Redis client with timeout configuration
    pub async fn new(host: &str, port: u16) -> Result<Self, Box<dyn Error>> {
        let redis_url = format!("redis://{}:{}", host, port);
        println!("Connecting to Redis at {}", redis_url);

        let client = redis::Client::open(redis_url)?;

        // ConnectionManager handles automatic reconnection
        // Retry connection with backoff for initial setup
        let mut last_error = None;
        for attempt in 1..=3 {
            match ConnectionManager::new(client.clone()).await {
                Ok(conn) => {
                    println!("Connected to Redis");
                    return Ok(RedisClient { connection: conn });
                }
                Err(e) => {
                    let delay_ms = 1000 * attempt;
                    eprintln!(
                        "Redis connection attempt {}/3 failed: {}. Retrying in {}ms...",
                        attempt, e, delay_ms
                    );
                    last_error = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
            }
        }

        Err(Box::new(last_error.unwrap()))
    }

    // ========== Counter-Based Work Distribution Methods ==========

    /// Claim a range of work indices atomically using INCRBY.
    /// Returns Some((start_index, end_index)) if work is available, None if all work claimed.
    /// Includes retry logic for transient network failures.
    pub async fn claim_work_range(
        &mut self,
        stage_id: i32,
        batch_size: i64,
        total_pairs: i64,
    ) -> Result<Option<(i64, i64)>, Box<dyn Error>> {
        let index_key = format!("stage_work_index:{}", stage_id);

        // INCRBY with inline retry
        let mut end_index: i64 = 0;
        for attempt in 0..3u32 {
            match self
                .connection
                .incr::<_, _, i64>(&index_key, batch_size)
                .await
            {
                Ok(val) => {
                    end_index = val;
                    break;
                }
                Err(e) => {
                    if attempt == 2 {
                        return Err(Box::new(e));
                    }
                    let delay = 500 * 2u64.pow(attempt);
                    eprintln!(
                        "Redis claim_work_range failed (attempt {}/3): {}. Retrying in {}ms...",
                        attempt + 1,
                        e,
                        delay
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            }
        }

        let start_index = end_index - batch_size;

        // If start_index is already >= total_pairs, all work has been claimed
        if start_index >= total_pairs {
            return Ok(None);
        }

        // Clamp end_index to total_pairs
        let clamped_end = end_index.min(total_pairs);

        Ok(Some((start_index, clamped_end)))
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
                    eprintln!(
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
                    eprintln!(
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

    // ========== Queue-Based Work Distribution Methods (existing) ==========

    /// Pop work items from the queue using atomic Lua script
    /// This prevents race conditions when multiple workers pop simultaneously
    pub async fn pop_work_items(
        &mut self,
        stage_id: i32,
        count: usize,
    ) -> Result<Vec<WorkQueueItem>, Box<dyn Error>> {
        let queue_key = format!("work_queue:{}", stage_id);

        // Lua script that atomically:
        // 1. Gets items from the end of the list
        // 2. Trims the list to remove those items
        // 3. Returns the items
        // This is atomic - no other command can interleave
        let lua_script = r#"
            local key = KEYS[1]
            local count = tonumber(ARGV[1])
            local len = redis.call('LLEN', key)
            if len == 0 then
                return {}
            end
            local actual_count = math.min(count, len)
            local start = -actual_count
            local items = redis.call('LRANGE', key, start, -1)
            if #items > 0 then
                redis.call('LTRIM', key, 0, -(#items + 1))
            end
            return items
        "#;

        let items_json: Vec<String> = redis::cmd("EVAL")
            .arg(lua_script)
            .arg(1) // number of keys
            .arg(&queue_key)
            .arg(count)
            .query_async(&mut self.connection)
            .await?;

        if items_json.is_empty() {
            return Ok(Vec::new());
        }

        // Parse compact format: baseGraphId|v1,v2|v1,v2
        // Reverse to maintain FIFO order since LRANGE returns oldest last
        let mut items = Vec::with_capacity(items_json.len());
        for compact in items_json.into_iter().rev() {
            match parse_compact_work_item(&compact) {
                Ok(item) => items.push(item),
                Err(e) => {
                    eprintln!("Failed to parse work queue item: {} - {}", compact, e);
                }
            }
        }

        Ok(items)
    }

    /// Get the queue depth (O(1) operation)
    pub async fn get_queue_depth(&mut self, stage_id: i32) -> Result<i64, Box<dyn Error>> {
        let queue_key = format!("work_queue:{}", stage_id);
        let size: i64 = self.connection.llen(&queue_key).await?;
        Ok(size)
    }

    /// Update best result if the new result is better (lower clique count)
    /// Returns true if updated, false otherwise
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
            println!(
                "New best result for stage {}: clique_count={}",
                stage_id, clique_count
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Get the current best result for a stage
    pub async fn get_best_result(
        &mut self,
        stage_id: i32,
    ) -> Result<Option<BestResult>, Box<dyn Error>> {
        let key = format!("best_result:{}", stage_id);
        let result: Option<String> = self.connection.get(&key).await?;

        match result {
            Some(json) => {
                let best = serde_json::from_str::<BestResult>(&json)?;
                Ok(Some(best))
            }
            None => Ok(None),
        }
    }

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
