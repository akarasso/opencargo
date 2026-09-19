#!/usr/bin/env bash
# Measures what one opencargo process costs, scenario by scenario.
# `scripts/bench.sh --help` for the options, docs/performance.md for the method.
set -euo pipefail
export LC_ALL=C

BENCH_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$BENCH_ROOT"

# shellcheck source=scripts/bench/sampler.sh
source scripts/bench/sampler.sh
# shellcheck source=scripts/bench/http.sh
source scripts/bench/http.sh
# shellcheck source=scripts/bench/report.sh
source scripts/bench/report.sh
# shellcheck source=scripts/bench/server.sh
source scripts/bench/server.sh
# shellcheck source=scripts/bench/scenarios.sh
source scripts/bench/scenarios.sh

BENCH_BINARY=${BENCH_BINARY:-$BENCH_ROOT/target/release/opencargo}
BENCH_OUT=${BENCH_OUT:-$BENCH_ROOT/bench-results}
BENCH_SETTLE=${BENCH_SETTLE:-15}
BENCH_CONCURRENCY=${BENCH_CONCURRENCY:-50}
BENCH_READS=${BENCH_READS:-1000}
BENCH_PUBLISHES=${BENCH_PUBLISHES:-50}
BENCH_VERSIONS=${BENCH_VERSIONS:-10000}
BENCH_PAYLOAD=${BENCH_PAYLOAD:-16384}
BENCH_INTERVAL=${BENCH_INTERVAL:-0.1}
BENCH_FORMATS=${BENCH_FORMATS:-"npm cargo pypi maven nuget oci"}
BENCH_WITH_IMAGE=0
BENCH_KEEP=0
BENCH_SCENARIOS=

ALL_SCENARIOS=(idle npm-install cargo-fetch oci-pull publish concurrent-reads s3 growth)
SMOKE_SCENARIOS=(idle publish)

usage() {
  cat <<'USAGE'
Usage: scripts/bench.sh [options]

  --out DIR           where results.json, results.csv and report.md land
  --binary PATH       the opencargo to measure (default target/release/opencargo)
  --scenarios LIST    comma-separated subset of: idle npm-install cargo-fetch oci-pull
                      publish concurrent-reads s3 growth
  --smoke             the two-record shape check: idle + one publish, tiny and offline
  --settle SECONDS    steady-state window (default 15)
  --concurrency N     concurrent warm readers (default 50)
  --reads N           warm proxy reads (default 1000)
  --publishes N       publishes per format (default 50)
  --formats LIST      space-separated formats for the publish scenario
  --versions N        versions for the growth scenario (default 10000)
  --with-image        also build the container image and record its size
  --keep              keep the scenario work directories
  --list              print the scenario names and exit
USAGE
}

while [[ $# -gt 0 ]]; do
  case $1 in
    --out) BENCH_OUT=$2; shift 2 ;;
    --binary) BENCH_BINARY=$2; shift 2 ;;
    --scenarios) BENCH_SCENARIOS=$2; shift 2 ;;
    --settle) BENCH_SETTLE=$2; shift 2 ;;
    --concurrency) BENCH_CONCURRENCY=$2; shift 2 ;;
    --reads) BENCH_READS=$2; shift 2 ;;
    --publishes) BENCH_PUBLISHES=$2; shift 2 ;;
    --formats) BENCH_FORMATS=$2; shift 2 ;;
    --versions) BENCH_VERSIONS=$2; shift 2 ;;
    --with-image) BENCH_WITH_IMAGE=1; shift ;;
    --keep) BENCH_KEEP=1; shift ;;
    --list) printf '%s\n' "${ALL_SCENARIOS[@]}"; exit 0 ;;
    --smoke)
      BENCH_SCENARIOS=${BENCH_SCENARIOS:-$(IFS=,; echo "${SMOKE_SCENARIOS[*]}")}
      BENCH_SETTLE=1 BENCH_CONCURRENCY=4 BENCH_READS=20 BENCH_PUBLISHES=3
      BENCH_VERSIONS=50 BENCH_INTERVAL=0.02 BENCH_SMOKE=1 BENCH_FORMATS=npm
      shift ;;
    -h | --help) usage; exit 0 ;;
    *) echo "bench: unknown option $1" >&2; usage >&2; exit 2 ;;
  esac
done
BENCH_SMOKE=${BENCH_SMOKE:-0}

for tool in curl jq python3 awk sort; do
  command -v "$tool" >/dev/null || { echo "bench: $tool is required" >&2; exit 1; }
done
[[ -x $BENCH_BINARY ]] || {
  echo "bench: $BENCH_BINARY is not executable -- run 'make release' or pass --binary" >&2
  exit 1
}

BENCH_BINARY=$(realpath "$BENCH_BINARY")
BENCH_TOKEN="bench_$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
RUN_ID=$(date -u +%Y%m%dT%H%M%SZ)
RUN_DIR=$BENCH_OUT/$RUN_ID
# Not /tmp: a tmpfs would turn every disk figure into a memory figure.
mkdir -p "${BENCH_WORK:=$BENCH_ROOT/target/bench-work}"
WORK_ROOT=$(mktemp -d "$BENCH_WORK/run.XXXXXX")
mkdir -p "$RUN_DIR/scenarios"

RECORD_SEQ=0
# Read now, not in the report: the report is written when the run is over.
BENCH_LOADAVG_START=$(cut -d' ' -f1-3 /proc/loadavg)
cleanup() {
  server_stop
  sampler_stop
  scenarios_cleanup
  [[ $BENCH_KEEP -eq 1 ]] || rm -rf "$WORK_ROOT"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Measurement framework
# ---------------------------------------------------------------------------

now_ms() { awk -v t="$(sampler_now)" 'BEGIN { printf "%d", t * 1000 }'; }

phase() { printf '%s\n' "$1" >"$SCN_PHASE"; }

kv() { printf '%s\n' "$@" >>"$RECORD_KV"; }

# server_up <name> [fs|s3] -- a process of its own, on cold caches
server_up() {
  SCN_NAME=$1
  SCN_STORAGE=${2:-fs}
  SCN_DIR=$WORK_ROOT/$SCN_NAME
  mkdir -p "$SCN_DIR"
  server_start "$SCN_DIR" "$SCN_STORAGE"
}

server_down() {
  server_stop
}

# measure_start <record> -- everything from here to measure_stop is one table row
measure_start() {
  RECORD_SEQ=$((RECORD_SEQ + 1))
  RECORD_NAME=$1
  RECORD_KV=$RUN_DIR/scenarios/$(printf '%02d' "$RECORD_SEQ")-$RECORD_NAME.kv
  : >"$RECORD_KV"
  SCN_PHASE=$SCN_DIR/phase
  phase load
  # 5 clears mm->hiwater_rss, so peak RSS is this record's and not the process's.
  echo 5 >"/proc/$SERVER_PID/clear_refs" 2>/dev/null || true
  sampler_start "$SERVER_PID" "$SCN_DIR/$RECORD_NAME.tsv" "$SCN_PHASE" "$BENCH_INTERVAL"
  RECORD_STATUS=ok
  RECORD_REASON=
  RECORD_WALL=
  RECORD_T0=$(now_ms)
}

# measure_load_done -- stops the clock before any summarising work
measure_load_done() { RECORD_WALL=$(($(now_ms) - RECORD_T0)); }

# measure_stop [note] -- closes the row: settle, sample summary, sizes
measure_stop() {
  local wall=${RECORD_WALL:-$(($(now_ms) - RECORD_T0))}
  phase settle
  sleep "$BENCH_SETTLE"
  sync
  sampler_stop
  kill -0 "$SERVER_PID" 2>/dev/null || record_fail "the server was not alive at the end of the record"
  kv "name=$RECORD_NAME" "storage=$SCN_STORAGE" "status=$RECORD_STATUS" "wall_ms=$wall"
  [[ $RECORD_STATUS == ok ]] || kv "skip_reason=$RECORD_REASON"
  sampler_summary "$SCN_DIR/$RECORD_NAME.tsv" settle >>"$RECORD_KV"
  # The database is the three WAL files together, or a publish still in the
  # write-ahead log reads as a database that never grew.
  kv "db_bytes=$(stat -c %s "$SCN_DIR"/data/db/opencargo.db* 2>/dev/null | awk '{ s += $1 } END { print s + 0 }')"
  kv "storage_bytes=$(storage_size)"
  kv "log_bytes=$(stat -c %s "$SCN_DIR/server.log" 2>/dev/null || echo 0)"
  [[ -n ${1:-} ]] && kv "notes=$1"
  return 0
}

# record_fail <reason> -- the row is kept, but out of the table
record_fail() {
  RECORD_STATUS=failed
  RECORD_REASON=${RECORD_REASON:-$1}
  echo "bench: $RECORD_NAME failed -- $1" >&2
}

# record_skip <name> <reason>
record_skip() {
  RECORD_SEQ=$((RECORD_SEQ + 1))
  local seq f
  seq=$(printf '%02d' "$RECORD_SEQ")
  f=$RUN_DIR/scenarios/$seq-$1.kv
  printf 'name=%s\nstatus=skipped\nskip_reason=%s\n' "$1" "$2" >"$f"
  echo "bench: skipping $1 -- $2" >&2
}

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------

selected=("${ALL_SCENARIOS[@]}")
if [[ -n $BENCH_SCENARIOS ]]; then
  IFS=',' read -r -a selected <<<"$BENCH_SCENARIOS"
fi

echo "bench: $BENCH_BINARY -> $RUN_DIR"
for name in "${selected[@]}"; do
  if ! declare -F "scenario_${name//-/_}" >/dev/null; then
    echo "bench: unknown scenario $name" >&2
    exit 2
  fi
done

for name in "${selected[@]}"; do
  echo "bench: scenario $name"
  "scenario_${name//-/_}" || {
    echo "bench: scenario $name failed" >&2
    record_skip "$name" "the scenario failed, see the run log"
  }
  server_down
done

image_bytes=null image_ref=
if [[ $BENCH_WITH_IMAGE -eq 1 ]]; then
  ref="opencargo-bench:$(git rev-parse --short HEAD)"
  echo "bench: building $ref"
  if docker build -q -t "$ref" . >/dev/null; then
    image_bytes=$(docker image inspect "$ref" --format '{{.Size}}')
    image_ref=$ref
  fi
fi

jq -n \
  --arg commit "$(git rev-parse HEAD)" \
  --argjson dirty "$([[ -n $(git status --porcelain) ]] && echo true || echo false)" \
  --arg version "$("$BENCH_BINARY" --version)" \
  --arg started "$RUN_ID" \
  --arg path "$BENCH_BINARY" \
  --arg profile "$([[ $BENCH_BINARY == */release/* ]] && echo release || echo other)" \
  --argjson size "$(stat -c %s "$BENCH_BINARY")" \
  --argjson machine "$(machine_json "$WORK_ROOT")" \
  --arg loadavg_end "$(cut -d' ' -f1-3 /proc/loadavg)" \
  --argjson image_bytes "$image_bytes" \
  --arg image_ref "$image_ref" \
  --argjson settings "$(jq -n \
    --argjson settle "$BENCH_SETTLE" --argjson concurrency "$BENCH_CONCURRENCY" \
    --argjson reads "$BENCH_READS" --argjson publishes "$BENCH_PUBLISHES" \
    --argjson versions "$BENCH_VERSIONS" --argjson payload "$BENCH_PAYLOAD" \
    --argjson smoke "$([[ $BENCH_SMOKE -eq 1 ]] && echo true || echo false)" \
    --arg rust_log "${BENCH_RUST_LOG:-opencargo=info,tower_http=info}" \
    --arg formats "$BENCH_FORMATS" \
    '{settle_s: $settle, concurrency: $concurrency, reads: $reads, publishes: $publishes,
      versions: $versions, payload_bytes: $payload, smoke: $smoke, rust_log: $rust_log,
      formats: $formats}')" \
  '{schema: 1, commit: $commit, dirty: $dirty, version: $version, started_at: $started,
    binary: {path: $path, profile: $profile, size_bytes: $size},
    machine: ($machine + {loadavg_at_end: $loadavg_end}), settings: $settings,
    artifacts: {binary_bytes: $size, image_bytes: $image_bytes,
                image_ref: (if $image_ref == "" then null else $image_ref end)}}' \
  >"$RUN_DIR/meta.json"

report_write "$RUN_DIR" "$RUN_DIR/meta.json"
rm -f "$RUN_DIR/meta.json"
echo "bench: wrote $RUN_DIR/results.json, results.csv and report.md"
