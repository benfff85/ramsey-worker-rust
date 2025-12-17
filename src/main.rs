use dotenv::dotenv;
use ramsey_worker_rust::worker::Worker;
use std::env;

#[tokio::main]
async fn main() {
    dotenv().ok();

    let base_url =
        env::var("RAMSEY_API_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let client_id = env::var("RAMSEY_CLIENT_ID").unwrap_or_else(|_| "rust-worker-1".to_string());

    // Configurable via env or hardcoded/args for now
    let vertex_count = 288;
    let clique_size = 8;

    let campaign_id = env::var("RAMSEY_CAMPAIGN_ID")
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .expect("CAMPAIGN_ID must be a number");
    let mut worker = Worker::new(base_url, client_id, vertex_count, clique_size, campaign_id);

    worker.run().await;
}
