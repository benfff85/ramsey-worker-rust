use crate::graph::WorkUnitEdge;
use crate::model::WorkUnitAnalysisType;
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::{Deserialize, Serialize};
use std::error::Error;

/// Work queue item - matches the Java WorkQueueItem model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkQueueItem {
    #[serde(rename = "baseGraphId")]
    pub base_graph_id: i32,
    #[serde(rename = "stageId")]
    pub stage_id: i32,
    #[serde(rename = "edgesToFlip")]
    pub edges_to_flip: Vec<WorkUnitEdge>,
    #[serde(rename = "analysisType")]
    pub analysis_type: WorkUnitAnalysisType,
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
    /// Create a new Redis client
    pub async fn new(host: &str, port: u16) -> Result<Self, Box<dyn Error>> {
        let redis_url = format!("redis://{}:{}", host, port);
        println!("Connecting to Redis at {}", redis_url);

        let client = redis::Client::open(redis_url)?;
        let connection = ConnectionManager::new(client).await?;

        println!("Connected to Redis");
        Ok(RedisClient { connection })
    }

    /// Pop work items from the queue (RPOP for FIFO ordering)
    pub async fn pop_work_items(
        &mut self,
        stage_id: i32,
        count: usize,
    ) -> Result<Vec<WorkQueueItem>, Box<dyn Error>> {
        let queue_key = format!("work_queue:{}", stage_id);
        let mut items = Vec::with_capacity(count);

        for _ in 0..count {
            let result: Option<String> = self.connection.rpop(&queue_key, None).await?;
            match result {
                Some(json) => match serde_json::from_str::<WorkQueueItem>(&json) {
                    Ok(item) => items.push(item),
                    Err(e) => {
                        eprintln!("Failed to deserialize work queue item: {} - {}", json, e);
                    }
                },
                None => break, // Queue is empty
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
}
