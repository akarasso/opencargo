#!/usr/bin/env bash
# The scenarios. Each one boots its own server, and writes one table row per
# `measure_start`/`measure_stop` pair.

BENCH_FIXTURES=scripts/bench/fixtures.py
# ~200 MB of layers, pinned by digest so a retag upstream cannot move the number.
BENCH_OCI_IMAGE=${BENCH_OCI_IMAGE:-library/postgres@sha256:485935f94cc7165afa896978809c37b592dc07f0a37d2c8f645f12412d0212c8}
BENCH_MINIO_IMAGE=${BENCH_MINIO_IMAGE:-quay.io/minio/minio:RELEASE.2025-09-07T16-13-09Z}
# The npm tree scenario (b) resolves, so its size is reported, not assumed.
BENCH_NPM_DEPS=${BENCH_NPM_DEPS:-'"express": "4.19.2", "chalk": "4.1.2", "rimraf": "5.0.7", "semver": "7.6.2"'}
BENCH_CARGO_DEPS=${BENCH_CARGO_DEPS:-'serde = { version = "=1.0.210", features = ["derive"] }
tokio = { version = "=1.40.0", features = ["full"] }
clap = { version = "=4.5.17", features = ["derive"] }'}

MINIO_NAME=

scenarios_cleanup() {
  [[ -n $MINIO_NAME ]] && docker rm -f "$MINIO_NAME" >/dev/null 2>&1
  return 0
}

need() {
  command -v "$1" >/dev/null && return 0
  record_skip "$2" "$1 is not installed"
  return 1
}

# Reachability, not success: a registry that answers 401 to an anonymous probe
# is up.
online() {
  curl -sS --max-time 5 -o /dev/null "$1" 2>/dev/null && return 0
  record_skip "$2" "no reachable upstream at $1"
  return 1
}

# ---------------------------------------------------------------------------

scenario_idle() {
  server_up idle
  local booted
  booted=$(awk '/^VmHWM:/ { print $2 * 1024 }' "/proc/$SERVER_PID/status")
  measure_start idle
  kv "boot_peak_rss_bytes=$booted"
  measure_stop "no traffic: the whole record is the settle window, so peak and steady are the same reading"
}

# ---------------------------------------------------------------------------

scenario_npm_install() {
  need pnpm npm-install || return 0
  online https://registry.npmjs.org/ npm-install || return 0
  server_up npm-install
  local project=$SCN_DIR/project
  mkdir -p "$project"
  printf '{"name":"bench","version":"1.0.0","private":true,"dependencies":{%s}}\n' \
    "$BENCH_NPM_DEPS" >"$project/package.json"

  local pass
  for pass in cold warm; do
    rm -rf "$project/node_modules" "$project/pnpm-lock.yaml" \
      "$SCN_DIR/pnpm-$pass" "$SCN_DIR/cache-$pass"
    mkdir -p "$SCN_DIR/pnpm-$pass" "$SCN_DIR/cache-$pass"
    local before after
    before=$(metrics_requests "$BASE")
    measure_start "npm-install-$pass"
    env HOME="$SCN_DIR" XDG_CACHE_HOME="$SCN_DIR/cache-$pass" XDG_DATA_HOME="$SCN_DIR/cache-$pass" \
      XDG_STATE_HOME="$SCN_DIR/cache-$pass" \
      pnpm install --dir "$project" --registry "$BASE/npm-proxy/" \
      --store-dir "$SCN_DIR/pnpm-$pass" --ignore-scripts --no-frozen-lockfile \
      --reporter=silent >>"$SCN_DIR/pnpm.log" 2>&1 || record_fail "pnpm install failed, see pnpm.log"
    measure_load_done
    after=$(metrics_requests "$BASE")
    kv "requests=$((after - before))" "errors=0"
    kv "packages=$(find "$project/node_modules/.pnpm" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | wc -l)"
    measure_stop "pnpm $(pnpm --version) against the npm proxy; the client store is wiped between the two passes, so only the server cache is warm"
  done
}

# ---------------------------------------------------------------------------

scenario_cargo_fetch() {
  need cargo cargo-fetch || return 0
  online https://index.crates.io/config.json cargo-fetch || return 0
  server_up cargo-fetch
  local project=$SCN_DIR/project
  mkdir -p "$project/src" "$project/.cargo"
  printf 'fn main() {}\n' >"$project/src/main.rs"
  {
    printf '[workspace]\n\n[package]\nname = "bench"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n'
    printf '%s\n' "$BENCH_CARGO_DEPS"
  } >"$project/Cargo.toml"
  cat >"$project/.cargo/config.toml" <<TOML
[source.crates-io]
replace-with = "bench"

[source.bench]
registry = "sparse+$BASE/cargo-proxy/index/"
TOML

  local pass
  for pass in cold warm; do
    rm -rf "$SCN_DIR/cargo-home-$pass" "$project/Cargo.lock"
    local before after
    before=$(metrics_requests "$BASE")
    measure_start "cargo-fetch-$pass"
    # From inside the project: cargo reads .cargo/config.toml from the working
    # directory, so --manifest-path alone would leave source replacement off
    # and fetch straight from crates.io.
    (cd "$project" && exec env CARGO_HOME="$SCN_DIR/cargo-home-$pass" cargo fetch) \
      >>"$SCN_DIR/cargo.log" 2>&1 || record_fail "cargo fetch failed, see cargo.log"
    measure_load_done
    after=$(metrics_requests "$BASE")
    kv "requests=$((after - before))" "errors=0"
    kv "crates=$(find "$SCN_DIR/cargo-home-$pass/registry/cache" -name '*.crate' 2>/dev/null | wc -l)"
    measure_stop "cargo fetch through source replacement; CARGO_HOME is wiped between the two passes"
  done
}

# ---------------------------------------------------------------------------

scenario_oci_pull() {
  need docker oci-pull || return 0
  online https://registry-1.docker.io/v2/ oci-pull || return 0
  server_up oci-pull
  local ref="127.0.0.1:$SERVER_PORT/oci-proxy/$BENCH_OCI_IMAGE"
  local pass
  for pass in cold warm; do
    docker rmi -f "$ref" >/dev/null 2>&1 || true
    local before after
    before=$(metrics_requests "$BASE")
    measure_start "oci-pull-$pass"
    if ! docker pull -q "$ref" >>"$SCN_DIR/docker.log" 2>&1; then
      measure_stop "docker pull failed, see docker.log"
      kv "status=failed"
      return 0
    fi
    measure_load_done
    after=$(metrics_requests "$BASE")
    kv "requests=$((after - before))" "errors=0"
    kv "image_bytes=$(docker image inspect "$ref" --format '{{.Size}}' 2>/dev/null || echo 0)"
    measure_stop "docker pull of $BENCH_OCI_IMAGE through the OCI proxy; the local image is removed before each pass, so both passes really transfer the layers"
  done
  docker rmi -f "$ref" >/dev/null 2>&1 || true
}

# ---------------------------------------------------------------------------

BENCH_PUBLISH_LIMIT=30

publish_count() {
  case $1 in
    npm | pypi) ((BENCH_PUBLISHES > BENCH_PUBLISH_LIMIT)) && echo "$BENCH_PUBLISH_LIMIT" || echo "$BENCH_PUBLISHES" ;;
    *) echo "$BENCH_PUBLISHES" ;;
  esac
}

# The artifacts and the curl script are built before the clock starts: a
# publish scenario measures the server, not python.
publish_prepare() {
  local format=$1 n=$2 payload=$3 conf=$SCN_DIR/$1.conf i out
  local fx=$SCN_DIR/fx-$format
  rm -rf "$fx"
  curl_conf_reset "$conf"
  for ((i = 0; i < n; i++)); do
    local dir=$fx/$i
    case $format in
      npm)
        out=$(python3 "$BENCH_FIXTURES" npm "$dir" "bench-npm-$i" 1.0.0 "$payload")
        curl_add "$conf" PUT "$BASE/npm-hosted/bench-npm-$i" "$(kvget "$out" body)" application/json
        ;;
      cargo)
        out=$(python3 "$BENCH_FIXTURES" cargo "$dir" "bench-cargo-$i" 1.0.0 "$payload")
        curl_add "$conf" PUT "$BASE/cargo-hosted/api/v1/crates/new" "$(kvget "$out" body)" application/octet-stream
        ;;
      pypi)
        out=$(python3 "$BENCH_FIXTURES" pypi "$dir" "bench-pypi-$i" 1.0.0 "$payload")
        curl_add "$conf" POST "$BASE/pypi-hosted/legacy/" "$(kvget "$out" body)" "$(kvget "$out" content_type)"
        ;;
      nuget)
        out=$(python3 "$BENCH_FIXTURES" nuget "$dir" "Bench.Nuget$i" 1.0.0 "$payload")
        curl_add "$conf" PUT "$BASE/nuget-hosted/v3/package" "$(kvget "$out" body)" "$(kvget "$out" content_type)"
        ;;
      maven)
        out=$(python3 "$BENCH_FIXTURES" maven "$dir" "lib$i" 1.0 "$payload")
        local base=$BASE/maven/maven-hosted/org/example/lib$i/1.0/lib$i-1.0
        printf '%s' "$(kvget "$out" jar_sha1)" >"$dir/jar.sha1"
        curl_add "$conf" PUT "$base.jar" "$(kvget "$out" jar)" application/java-archive
        curl_add "$conf" PUT "$base.jar.sha1" "$dir/jar.sha1" text/plain
        curl_add "$conf" PUT "$base.pom" "$(kvget "$out" pom)" application/xml
        ;;
      *) echo "bench: no publish batch for $format" >&2; return 1 ;;
    esac
  done
}

publish_run() {
  local format=$1
  curl_run "$SCN_DIR/$format.conf" "$SCN_DIR/$format.tsv" 1
  measure_load_done
  http_summary "$SCN_DIR/$format.tsv"
}

kvget() { awk -F= -v k="$2" '$1 == k { print substr($0, length(k) + 2) }' <<<"$1"; }

# An OCI push is a session: for each blob a POST, then a PUT to the location
# the server chose, then the manifest. Five requests whose URLs are only known
# one at a time, so this one cannot be scripted into a single curl run.
oci_prepare() {
  local n=$1 payload=$2 i
  rm -rf "$SCN_DIR/fx-oci"
  mkdir -p "$SCN_DIR/fx-oci"
  for ((i = 0; i < n; i++)); do
    python3 "$BENCH_FIXTURES" oci "$SCN_DIR/fx-oci/$i" "bench-oci-$i" 1.0.0 "$payload" \
      >"$SCN_DIR/fx-oci/$i.kv"
  done
}

oci_push_run() {
  local n=$1 tsv=$SCN_DIR/oci.tsv i pair
  : >"$tsv"
  for ((i = 0; i < n; i++)); do
    local out loc t0 code=201 image=oci-hosted/bench/app$i
    out=$(cat "$SCN_DIR/fx-oci/$i.kv")
    t0=$(sampler_now)
    for pair in "layer:layer_digest" "config:config_digest"; do
      loc=$(curl -fsS -X POST -H "Authorization: Bearer $BENCH_TOKEN" \
        -D - -o /dev/null "$BASE/v2/$image/blobs/uploads/" |
        awk 'tolower($1) == "location:" { print $2 }' | tr -d '\r')
      [[ $loc == http* ]] || loc=$BASE$loc
      [[ $loc == *\?* ]] && loc="$loc&" || loc="$loc?"
      curl -fsS -X PUT -H "Authorization: Bearer $BENCH_TOKEN" \
        -H 'Content-Type: application/octet-stream' \
        --data-binary "@$(kvget "$out" "${pair%%:*}")" \
        -o /dev/null "${loc}digest=$(kvget "$out" "${pair##*:}")" || code=500
    done
    curl -fsS -X PUT -H "Authorization: Bearer $BENCH_TOKEN" \
      -H 'Content-Type: application/vnd.oci.image.manifest.v1+json' \
      --data-binary "@$(kvget "$out" manifest)" \
      -o /dev/null "$BASE/v2/$image/manifests/1.0.0" || code=500
    awk -v t0="$t0" -v t1="$(sampler_now)" -v c="$code" \
      'BEGIN { printf "%.6f\t%d\t0\n", t1 - t0, c }' >>"$tsv"
  done
  measure_load_done
  http_summary "$tsv"
}

# A server per format: the publish rate limiter lives in the process, so two
# formats measured against one server would share its 30-per-minute budget.
scenario_publish() {
  local format note n
  for format in $BENCH_FORMATS; do
    [[ $format == oci ]] && continue
    n=$(publish_count "$format")
    server_up "publish-$format"
    publish_prepare "$format" "$n" "$BENCH_PAYLOAD"
    measure_start "publish-$format"
    publish_run "$format" >>"$RECORD_KV"
    note="$n publishes of a $BENCH_PAYLOAD-byte artifact, sent over the wire protocol by one sequential curl process, not by the vendor client"
    [[ $format == maven ]] && note="$note; a Maven publish is three requests (jar, sha1, pom) and the latency is one of them"
    [[ $format == npm || $format == pypi ]] && note="$note; capped at the server's 30 publishes per minute and per user"
    measure_stop "$note"
    server_down
  done
  [[ " $BENCH_FORMATS " == *" oci "* ]] || return 0
  server_up publish-oci
  oci_prepare "$BENCH_PUBLISHES" "$BENCH_PAYLOAD"
  measure_start publish-oci
  oci_push_run "$BENCH_PUBLISHES" >>"$RECORD_KV"
  measure_stop "$BENCH_PUBLISHES pushes of a one-layer image; a latency here is a whole push session (five requests) and includes the curl process starts"
}

# ---------------------------------------------------------------------------

scenario_concurrent_reads() {
  online https://registry.npmjs.org/ concurrent-reads || return 0
  server_up concurrent-reads
  local pkgs=(express chalk semver debug lodash rimraf glob picomatch minimatch ms)
  local conf=$SCN_DIR/warm.conf p
  curl_conf_reset "$conf"
  for p in "${pkgs[@]}"; do curl_add "$conf" GET "$BASE/npm-proxy/$p"; done
  curl_run "$conf" "$SCN_DIR/warm.tsv" 1
  local warm_errors
  warm_errors=$(awk '$2 < 200 || $2 >= 400' "$SCN_DIR/warm.tsv" | wc -l)
  if [[ $warm_errors -gt 0 ]]; then
    record_skip concurrent-reads "the proxy could not warm $warm_errors of ${#pkgs[@]} packuments"
    return 0
  fi

  conf=$SCN_DIR/reads.conf
  curl_conf_reset "$conf"
  local i
  for ((i = 0; i < BENCH_READS; i++)); do
    curl_add "$conf" GET "$BASE/npm-proxy/${pkgs[i % ${#pkgs[@]}]}"
  done
  measure_start "warm-reads-c$BENCH_CONCURRENCY"
  curl_run "$conf" "$SCN_DIR/reads.tsv" "$BENCH_CONCURRENCY"
  measure_load_done
  http_summary "$SCN_DIR/reads.tsv" >>"$RECORD_KV"
  kv "throughput_rps=$(awk -v n="$BENCH_READS" -v ms="$RECORD_WALL" 'BEGIN { printf "%.0f", n / (ms / 1000) }')"
  measure_stop "$BENCH_READS reads of ${#pkgs[@]} warm packuments at concurrency $BENCH_CONCURRENCY, one curl process; the 24h proxy TTL means no upstream request"
}

# ---------------------------------------------------------------------------

storage_pair() {
  local storage=$1
  local n
  n=$(publish_count npm)
  server_up "storage-$storage" "$storage"
  publish_prepare npm "$n" "$BENCH_PAYLOAD"
  measure_start "$storage-publish"
  publish_run npm >>"$RECORD_KV"
  measure_stop "the same $n npm publishes, on $storage storage"

  local conf=$SCN_DIR/read.conf i
  curl_conf_reset "$conf"
  for ((i = 0; i < BENCH_READS; i++)); do
    local pkg=bench-npm-$((i % n))
    curl_add "$conf" GET "$BASE/npm-hosted/$pkg/-/$pkg-1.0.0.tgz"
  done
  measure_start "$storage-read"
  curl_run "$conf" "$SCN_DIR/read.tsv" "$BENCH_CONCURRENCY"
  measure_load_done
  http_summary "$SCN_DIR/read.tsv" >>"$RECORD_KV"
  kv "throughput_rps=$(awk -v n="$BENCH_READS" -v ms="$RECORD_WALL" 'BEGIN { printf "%.0f", n / (ms / 1000) }')"
  measure_stop "$BENCH_READS tarball reads at concurrency $BENCH_CONCURRENCY, on $storage storage"
  server_down
}

scenario_s3() {
  need docker s3 || return 0
  MINIO_NAME=opencargo-bench-minio-$$
  BENCH_S3_BUCKET=opencargo-bench
  BENCH_S3_USER=minio
  BENCH_S3_PASS=minio12345
  local port
  port=$(free_port)
  BENCH_S3_ENDPOINT=http://127.0.0.1:$port
  docker run -d --rm --name "$MINIO_NAME" -p "127.0.0.1:$port:9000" \
    -e MINIO_ROOT_USER=$BENCH_S3_USER -e MINIO_ROOT_PASSWORD=$BENCH_S3_PASS \
    "$BENCH_MINIO_IMAGE" server /data >/dev/null
  local deadline=$((SECONDS + 60))
  until curl -sf "$BENCH_S3_ENDPOINT/minio/health/live" >/dev/null; do
    if ((SECONDS >= deadline)); then
      record_skip s3 "minio did not come up"
      return 0
    fi
    sleep 0.2
  done
  curl -sf -X PUT --aws-sigv4 "aws:amz:us-east-1:s3" \
    --user "$BENCH_S3_USER:$BENCH_S3_PASS" "$BENCH_S3_ENDPOINT/$BENCH_S3_BUCKET" >/dev/null

  storage_pair fs
  storage_pair s3
  docker rm -f "$MINIO_NAME" >/dev/null 2>&1 || true
  MINIO_NAME=
}

# ---------------------------------------------------------------------------

# Cargo, not npm: an npm or PyPI publish is rate limited to 30 per minute per
# user (src/server.rs), which would measure the limiter and not the growth.
scenario_growth() {
  server_up growth
  local per=100 fx=$SCN_DIR/bulk conf=$SCN_DIR/bulk.conf i
  ((BENCH_VERSIONS < per)) && per=$BENCH_VERSIONS
  mkdir -p "$fx"
  python3 "$BENCH_FIXTURES" cargo-bulk "$fx" bench-bulk "$BENCH_VERSIONS" 1024 "$per" >/dev/null
  curl_conf_reset "$conf"
  for ((i = 0; i < BENCH_VERSIONS; i++)); do
    curl_add "$conf" PUT "$BASE/cargo-hosted/api/v1/crates/new" \
      "$(printf '%s/%05d.bin' "$fx" "$i")" application/octet-stream
  done
  measure_start "growth-$BENCH_VERSIONS-versions"
  curl_run "$conf" "$SCN_DIR/growth.tsv" 8
  measure_load_done
  http_summary "$SCN_DIR/growth.tsv" >>"$RECORD_KV"
  kv "throughput_rps=$(awk -v n="$BENCH_VERSIONS" -v ms="$RECORD_WALL" 'BEGIN { printf "%.0f", n / (ms / 1000) }')"
  kv "packages=$((BENCH_VERSIONS / per))" "versions=$BENCH_VERSIONS"
  measure_stop "$BENCH_VERSIONS crate versions over $((BENCH_VERSIONS / per)) crates, 1 KiB of payload each, published at concurrency 8; db_bytes and storage_bytes are what they cost"
}
