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
}
