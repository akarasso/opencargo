#!/usr/bin/env bash
# One opencargo process per scenario: its own work directory, its own port,
# its own cold caches.

BENCH_REPOS_TOML='
[[repositories]]
name = "npm-hosted"
type = "hosted"
format = "npm"
visibility = "private"

[[repositories]]
name = "npm-proxy"
type = "proxy"
format = "npm"
visibility = "public"
upstream = "https://registry.npmjs.org"

[[repositories]]
name = "cargo-hosted"
type = "hosted"
format = "cargo"
visibility = "private"

[[repositories]]
name = "cargo-proxy"
type = "proxy"
format = "cargo"
visibility = "public"
upstream = "https://index.crates.io/"

[[repositories]]
name = "oci-hosted"
type = "hosted"
format = "oci"
visibility = "public"

[[repositories]]
name = "oci-proxy"
type = "proxy"
format = "oci"
visibility = "public"
upstream = "https://registry-1.docker.io"
token_realms = ["https://auth.docker.io/token"]

[[repositories]]
name = "pypi-hosted"
type = "hosted"
format = "pypi"
visibility = "private"

[[repositories]]
name = "maven-hosted"
type = "hosted"
format = "maven"
visibility = "private"

[[repositories]]
name = "nuget-hosted"
type = "hosted"
format = "nuget"
visibility = "private"

[[repositories]]
name = "go-proxy"
type = "proxy"
format = "go"
visibility = "public"
upstream = "https://proxy.golang.org"
'

# Below the ephemeral range: a port picked inside it can be taken by any other
# process between the probe and the bind, and the health probe would then
# answer from whatever took it.
free_port() {
  local port low
  low=$(awk '{ print $1 }' /proc/sys/net/ipv4/ip_local_port_range 2>/dev/null || echo 32768)
  while :; do
    port=$((low - 12000 + RANDOM % 10000))
    (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null || {
      printf '%s' "$port"
      return 0
    }
  done
}

# server_config <dir> <port> <fs|s3>
server_config() {
  local dir=$1 port=$2 storage=$3
  mkdir -p "$dir/data/db" "$dir/data/storage"
  {
    cat <<TOML
[server]
bind = "127.0.0.1:$port"
base_url = "http://127.0.0.1:$port"
storage_path = "$dir/data/storage"

[database]
url = "sqlite:$dir/data/db/opencargo.db"

[auth]
anonymous_read = true
static_tokens = ["$BENCH_TOKEN"]

[auth.admin]
username = "admin"

[proxy]
default_ttl = "24h"
negative_cache_ttl = "1h"
connect_timeout = "10s"

[cleanup]
enabled = false

[vuln_scan]
enabled = false
TOML
    if [[ $storage == s3 ]]; then
      cat <<TOML

[storage]
backend = "s3"

[storage.s3]
bucket = "$BENCH_S3_BUCKET"
region = "us-east-1"
endpoint = "$BENCH_S3_ENDPOINT"
allow_http = true
TOML
    fi
    printf '%s\n' "$BENCH_REPOS_TOML"
  } >"$dir/config.toml"
}

# server_start <dir> -- sets SERVER_PID, SERVER_PORT, BASE
server_start() {
  local dir=$1 storage=${2:-fs}
  SERVER_PORT=$(free_port)
  BASE="http://127.0.0.1:$SERVER_PORT"
  server_config "$dir" "$SERVER_PORT" "$storage"
  local -a env=(
    "HOME=$dir" "XDG_CONFIG_HOME=$dir/.config"
    "OPENCARGO_ADMIN_PASSWORD=$BENCH_TOKEN"
    "RUST_LOG=${BENCH_RUST_LOG:-opencargo=info,tower_http=info}"
  )
  if [[ $storage == s3 ]]; then
    env+=("OPENCARGO_S3_ACCESS_KEY_ID=$BENCH_S3_USER" "OPENCARGO_S3_SECRET_ACCESS_KEY=$BENCH_S3_PASS")
  fi
  (cd "$dir" && exec env -i PATH="$PATH" "${env[@]}" "$BENCH_BINARY" --config "$dir/config.toml") \
    >"$dir/server.log" 2>&1 &
  SERVER_PID=$!
  server_ready "$dir"
}

# server_ready <dir> -- a deadline, not a sleep. The log line comes first: a
# health probe alone would accept an answer from someone else's server.
server_ready() {
  local dir=$1 deadline=$((SECONDS + 60))
  while :; do
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
      echo "bench: server exited before it was ready" >&2
      tail -20 "$dir/server.log" >&2
      return 1
    fi
    if grep -q "Listening on 127.0.0.1:$SERVER_PORT" "$dir/server.log" 2>/dev/null &&
      curl -fsS --max-time 2 "$BASE/health/ready" >/dev/null 2>&1; then
      return 0
    fi
    if ((SECONDS >= deadline)); then
      echo "bench: server was not ready within 60s" >&2
      tail -20 "$dir/server.log" >&2
      return 1
    fi
    sleep 0.2
  done
}

server_stop() {
  [[ -n ${SERVER_PID:-} ]] || return 0
  kill "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
  SERVER_PID=
}
