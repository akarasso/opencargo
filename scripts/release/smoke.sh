#!/usr/bin/env bash
set -euo pipefail

bin=$(realpath "${1:?usage: smoke.sh <binary> <version>}")
version=${2:?version}
port=${SMOKE_PORT:-16789}

got=$("$bin" --version)
if [[ $got != "opencargo $version" ]]; then
  echo "smoke: --version printed '$got', expected 'opencargo $version'" >&2
  exit 1
fi
echo "smoke: $got"

work=$(mktemp -d)
pid=
cleanup() {
  if [[ -n $pid ]]; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

(
  cd "$work"
  unset OPENCARGO_CONFIG OPENCARGO_BASE_URL
  exec env HOME="$work" XDG_CONFIG_HOME="$work/.config" \
    OPENCARGO_ADMIN_PASSWORD="smoke-$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')" \
    "$bin" --bind "127.0.0.1:$port"
) > "$work/server.log" 2>&1 &
pid=$!

body=
for _ in $(seq 60); do
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "smoke: server exited early" >&2
    cat "$work/server.log" >&2
    exit 1
  fi
  if body=$(curl -fsS --max-time 2 "http://127.0.0.1:$port/health/ready" 2>/dev/null); then
    break
  fi
  sleep 1
done

if [[ $body != '{"status":"ok"}' ]]; then
  echo "smoke: /health/ready answered '$body'" >&2
  cat "$work/server.log" >&2
  exit 1
fi
echo "smoke: /health/ready $body"
