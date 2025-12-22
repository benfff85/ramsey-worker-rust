# Ramsey Worker (Rust)

High-performance worker for the Ramsey distributed computing system. Consumes work items from Redis queue, processes them using the Bron-Kerbosch algorithm, and tracks improvements for automatic stage progression.

## Architecture

```
┌───────────┐     ┌─────────────────┐     ┌────────────┐
│   Redis   │────▶│   Rust Worker   │────▶│   MySQL    │
│   Queue   │     │  (Bron-Kerbosch)│     │  (results) │
│           │◀────│                 │     │  optional  │
│best_result│     │ (tracks best)   │     │            │
│proc_count │◀────│ (tracks count)  │     │            │
└───────────┘     └─────────────────┘     └────────────┘
```

## How It Works

1. **Registration**: Worker registers with middleware as a CLIQUECHECKER client
2. **Stage Discovery**: Fetches active stage and base graph clique count from middleware
3. **Work Loop**:
   - Pops work items from Redis queue (`RPOP work_queue:{stageId}`)
   - Processes each item using optimized Bron-Kerbosch clique counting
   - **If result has fewer cliques than base graph**: Updates `best_result:{stageId}` in Redis
   - Submits results to middleware (`POST /api/ramsey/results`)
4. **Heartbeat**: Periodically updates last phone home time

## Best Result Tracking

When a worker finds a result with fewer cliques than the current base graph:

1. Compares result clique count to cached `base_graph_clique_count`
2. If better, calls `update_best_if_better()` which:
   - Gets current `best_result:{stageId}` from Redis
   - Only updates if new result is better than existing best
   - Stores: `{baseGraphId, stageId, edgesToFlip, cliqueCount}`

The Queue Manager's `StageProgressionMonitor` polls this key and triggers stage progression when improvements are detected.

## Performance Optimizations

- **Fixed-size BitMatrix**: Uses compile-time sized arrays with `Copy` semantics
- **In-place Bron-Kerbosch**: Minimizes allocations during recursion
- **Native CPU targeting**: Uses `target-cpu=native` for local builds
- **LTO + single codegen unit**: Maximum optimization for release builds

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `RAMSEY_API_URL` | Middleware API base URL | `http://localhost:4040/api/ramsey` |
| `RAMSEY_CAMPAIGN_ID` | Campaign ID to process | `1` |
| `REDIS_HOST` | Redis server hostname | `localhost` |
| `REDIS_PORT` | Redis server port | `6379` |
| `WORK_UNIT_FETCH_COUNT` | Items to pop per cycle | `50000` |
| `WORK_UNIT_PUBLISH_COUNT` | Batch size for result submission | `50000` |
| `WORK_UNIT_POLL_FREQ` | Polling interval (ms) when queue empty | `1000` |
| `CLIENT_PHONE_HOME_FREQ` | Heartbeat interval (ms) | `60000` |
| `PUBLISH_RESULTS` | Whether to submit results to MySQL | `true` |

## Building

### Local Development
```bash
cargo build --release
```

### Docker
```bash
docker build -t benferenchak/ramsey-worker-rust:develop .
```

## Running

### Local
```bash
export RAMSEY_API_URL=http://localhost:4040/api/ramsey
export REDIS_HOST=localhost
cargo run --release
```

### Docker Compose
```bash
docker compose -f ./docker/ramsey-compose.yml -p ramsey up --scale ramsey-worker-rust=12 -d
```

## Project Structure

```
src/
├── main.rs           # Entry point, environment config
├── worker.rs         # Main worker loop, Redis integration, best result tracking
├── redis_client.rs   # Redis queue operations, best result updates
├── client.rs         # Middleware HTTP client
├── algorithm.rs      # Bron-Kerbosch implementations
├── graph.rs          # Graph representation
├── bitset.rs         # Fixed-size BitMatrix
├── clique_collection.rs  # Clique storage and lookup
└── model.rs          # Data structures (WorkResult, Stage, GraphData, etc.)
```