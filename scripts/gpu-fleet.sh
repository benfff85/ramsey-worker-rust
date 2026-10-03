#!/bin/bash
# Supervise the native GPU-assisted workers.
#
# These cannot run in the container: Metal is a macOS userspace framework and the worker image is
# Linux/aarch64 with no GPU device nodes. So the fleet is split — Docker runs the CPU-only workers,
# this runs the hybrid ones, and together they fill both the cores and the GPU.
#
# Measured on the M4 Max (16 logical cores, 40-core GPU), fleet units/sec:
#   16 CPU +  0 GPU  189.6      6 CPU + 10 GPU  269.1   <- deployed
#   12 CPU +  4 GPU  221.6      4 CPU + 12 GPU  258.4
#   10 CPU +  6 GPU  252.7      0 CPU + 16 GPU  237.3
#    8 CPU +  8 GPU  262.7
# Totals other than 16 are worse: 14 workers -7%, 20 workers -10%. Keep CPU + GPU = core count.
set -u
cd "$(dirname "$0")/.."

GPU_WORKERS="${GPU_WORKERS:-10}"
export RAMSEY_API_URL="${RAMSEY_API_URL:-http://localhost:36000/api/ramsey}"
export RAMSEY_FLEET="${RAMSEY_FLEET:-m4-max}"
export REDIS_HOST="${REDIS_HOST:-localhost}"
export REDIS_PORT="${REDIS_PORT:-36002}"
export WORK_UNIT_FETCH_COUNT="${WORK_UNIT_FETCH_COUNT:-2000}"
export WORK_UNIT_PUBLISH_COUNT="${WORK_UNIT_PUBLISH_COUNT:-10000}"
export WORK_UNIT_POLL_FREQ="${WORK_UNIT_POLL_FREQ:-1000}"
export PUBLISH_RESULTS="${PUBLISH_RESULTS:-false}"
export HOIST_GPU_ENABLED=true
# Measured configuration (2026-10-03, campaign 10, mirrored A/B with m1 paused): everything on is
# +108% fleet throughput against the pre-selector build; within it dense-64 is +6.6% and the GPU
# set +4.1% (small, below the ~5% between-block spread). The worker defaults these on as well, so a
# launcher that passes no environment still gets them; set any to false for a restart-level A/B.
export HOIST_GPU_CHUNK="${HOIST_GPU_CHUNK:-65536}"
export GPU_BUCKETING="${GPU_BUCKETING:-true}"
export GPU_DENSE64="${GPU_DENSE64:-true}"
export ROW_SELECTOR="${ROW_SELECTOR:-true}"

# Leave the checked-out release binary as the default.  An absolute override lets a separately
# built candidate be tested without overwriting that known-good artifact.
BIN="${RAMSEY_GPU_WORKER_BIN:-./target/release/ramsey-worker-rust}"
[ -x "$BIN" ] || { echo "$(date -u +%FT%TZ) missing $BIN — cargo build --release first"; exit 1; }

pids=()
cleanup() { echo "$(date -u +%FT%TZ) stopping"; for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done; exit 0; }
trap cleanup TERM INT

echo "$(date -u +%FT%TZ) starting $GPU_WORKERS GPU-assisted workers"
for _ in $(seq 1 "$GPU_WORKERS"); do
  "$BIN" >/dev/null 2>&1 &
  pids+=($!)
done

# Restart individually rather than exiting: one worker dying should not take the fleet with it.
while :; do
  sleep 30
  for i in "${!pids[@]}"; do
    if ! kill -0 "${pids[$i]}" 2>/dev/null; then
      echo "$(date -u +%FT%TZ) worker ${pids[$i]} died, restarting"
      "$BIN" >/dev/null 2>&1 &
      pids[$i]=$!
    fi
  done
done
