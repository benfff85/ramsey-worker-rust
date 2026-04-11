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

    let worker_mode = env::var("WORKER_MODE")
        .unwrap_or_else(|_| "EXHAUSTIVE".to_string())
        .to_uppercase();
    let sa_mode: bool = worker_mode == "SIMULATED_ANNEALING";
    let vds_mode: bool = worker_mode == "VARIABLE_DEPTH_SEARCH";

    let sa_max_iterations: u64 = env::var("SA_MAX_ITERATIONS")
        .unwrap_or_else(|_| "100000".to_string())
        .parse()
        .expect("SA_MAX_ITERATIONS must be a number");

    let sa_initial_temp: f64 = env::var("SA_INITIAL_TEMP")
        .unwrap_or_else(|_| "1000.0".to_string())
        .parse()
        .expect("SA_INITIAL_TEMP must be a number");

    let sa_cooling_rate: f64 = env::var("SA_COOLING_RATE")
        .unwrap_or_else(|_| "0.999".to_string())
        .parse()
        .expect("SA_COOLING_RATE must be a number");

    let sa_min_pairs: usize = env::var("SA_MIN_PAIRS")
        .unwrap_or_else(|_| "2".to_string())
        .parse()
        .expect("SA_MIN_PAIRS must be a number");

    let sa_max_pairs: usize = env::var("SA_MAX_PAIRS")
        .unwrap_or_else(|_| "5".to_string())
        .parse()
        .expect("SA_MAX_PAIRS must be a number");

    let vds_max_depth: usize = env::var("VDS_MAX_DEPTH")
        .unwrap_or_else(|_| "4".to_string())
        .parse()
        .expect("VDS_MAX_DEPTH must be a number");

    let vds_top_first_edges: usize = env::var("VDS_TOP_FIRST_EDGES")
        .unwrap_or_else(|_| "500".to_string())
        .parse()
        .expect("VDS_TOP_FIRST_EDGES must be a number");

    let vds_branching_factor: usize = env::var("VDS_BRANCHING_FACTOR")
        .unwrap_or_else(|_| "20".to_string())
        .parse()
        .expect("VDS_BRANCHING_FACTOR must be a number");

    let vds_worsening_tolerance: i32 = env::var("VDS_WORSENING_TOLERANCE")
        .unwrap_or_else(|_| "100".to_string())
        .parse()
        .expect("VDS_WORSENING_TOLERANCE must be a number");

    let vds_random_seed: Option<u64> = env::var("VDS_RANDOM_SEED").ok().and_then(|s| s.parse().ok());

    log_info!("Publish results to MySQL: {}", publish_results);
    log_info!("Tracking top {} results per stage", top_results_count);

    if sa_mode {
        log_info!("Worker mode: SIMULATED_ANNEALING");
        log_info!("  max_iterations={}, initial_temp={}, cooling_rate={}, min_pairs={}, max_pairs={}",
            sa_max_iterations, sa_initial_temp, sa_cooling_rate, sa_min_pairs, sa_max_pairs);
    } else if vds_mode {
        log_info!("Worker mode: VARIABLE_DEPTH_SEARCH");
        log_info!("  max_depth={}, top_first_edges={}, branching_factor={}, worsening_tolerance={}, random_seed={:?}",
            vds_max_depth, vds_top_first_edges, vds_branching_factor, vds_worsening_tolerance, vds_random_seed);
    } else {
        log_info!("Worker mode: EXHAUSTIVE");
    }

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
        sa_mode,
        sa_max_iterations,
        sa_initial_temp,
        sa_cooling_rate,
        sa_min_pairs,
        sa_max_pairs,
        vds_mode,
        vds_max_depth,
        vds_top_first_edges,
        vds_branching_factor,
        vds_worsening_tolerance,
        vds_random_seed,
    );

    // Connect to Redis
    if let Err(e) = worker.connect_redis(&redis_host, redis_port).await {
        log_error!("Failed to connect to Redis: {}", e);
        return;
    }

    worker.run().await;
}
