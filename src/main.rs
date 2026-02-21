use dotenv::dotenv;
use ramsey_worker_rust::{log_error, log_info, worker::Worker};
use std::env;

#[tokio::main]
async fn main() {
    dotenv().ok();

    let base_url = env::var("RAMSEY_API_URL")
        .unwrap_or_else(|_| "http://localhost:4040/api/ramsey".to_string());

    // Redis configuration
    let redis_host = env::var("REDIS_HOST").unwrap_or_else(|_| "localhost".to_string());
    let redis_port: u16 = env::var("REDIS_PORT")
        .unwrap_or_else(|_| "6379".to_string())
        .parse()
        .expect("REDIS_PORT must be a number");

    // Configurable via env or hardcoded/args for now
    let vertex_count = 288;
    let clique_size = 8;

    let campaign_id = env::var("RAMSEY_CAMPAIGN_ID")
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .expect("CAMPAIGN_ID must be a number");

    let poll_interval_ms: u64 = env::var("WORK_UNIT_POLL_FREQ")
        .unwrap_or_else(|_| "1000".to_string())
        .parse()
        .expect("WORK_UNIT_POLL_FREQ must be a number");

    let heartbeat_interval_ms: u64 = env::var("CLIENT_PHONE_HOME_FREQ")
        .unwrap_or_else(|_| "60000".to_string())
        .parse()
        .expect("CLIENT_PHONE_HOME_FREQ must be a number");

    let fetch_size: i32 = env::var("WORK_UNIT_FETCH_COUNT")
        .unwrap_or_else(|_| "50000".to_string())
        .parse()
        .expect("WORK_UNIT_FETCH_COUNT must be a number");

    let publish_size: i32 = env::var("WORK_UNIT_PUBLISH_COUNT")
        .unwrap_or_else(|_| "50000".to_string())
        .parse()
        .expect("WORK_UNIT_PUBLISH_COUNT must be a number");

    let publish_results: bool = env::var("PUBLISH_RESULTS")
        .unwrap_or_else(|_| "true".to_string())
        .to_lowercase()
        .parse()
        .unwrap_or(true);

    let top_results_count: usize = env::var("TOP_RESULTS_COUNT")
        .unwrap_or_else(|_| "10".to_string())
        .parse()
        .expect("TOP_RESULTS_COUNT must be a number");

    log_info!("Publish results to MySQL: {}", publish_results);
    log_info!("Tracking top {} results per stage", top_results_count);

    let mut worker = Worker::new(
        base_url,
        vertex_count,
        clique_size,
        campaign_id,
        poll_interval_ms,
        heartbeat_interval_ms,
        fetch_size,
        publish_size,
        publish_results,
        top_results_count,
    );

    // Connect to Redis
    if let Err(e) = worker.connect_redis(&redis_host, redis_port).await {
        log_error!("Failed to connect to Redis: {}", e);
        return;
    }

    worker.run().await;
}
