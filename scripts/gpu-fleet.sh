#!/bin/bash
# Supervise the native GPU-assisted workers.
#
# These cannot run in the container: Metal is a macOS userspace framework and the worker image is
# Linux/aarch64 with no GPU device nodes. So the fleet is split — Docker runs the CPU-only workers,
# this runs the hybrid ones, and together they fill both the cores and the GPU.
#
# Re-swept 2026-10-08 on the M4 Max (16 logical cores, 40-core GPU) with 6 Docker CPU workers,
# campaign 10 stages/s in 4-minute windows (after the row selector and worker#136/#137):
#   6 CPU + 10 GPU  7.03 (two windows)      6 CPU + 16 GPU  7.83 (+11%)   <- deployed
#   6 CPU + 12 GPU  7.41 (+5%)              6 CPU + 18 GPU  7.77
#   6 CPU + 14 GPU  7.64 (+9%)              6 CPU + 20 GPU  7.94 (three windows; within noise of 16)
# The fleet is now stage-turnover bound, not compute bound: the host stays 36-52% idle and the GPU
# ~50% busy throughout. So the old rule ("keep CPU + GPU = core count", measured Aug 2026 when 20
# workers were -10%) no longer holds. Extra Docker workers did NOT help (+1%): they split the same
# work. 16 is the knee; more adds less than the ~2% noise and costs CPU headroom.
set -u
cd "$(dirname "$0")/.."

GPU_WORKERS="${GPU_WORKERS:-16}"
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
