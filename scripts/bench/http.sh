#!/usr/bin/env bash
# One curl process drives a whole batch, so a measured latency is a request's
# and not a process spawn's. Each transfer is its own `next` block, because
# curl would otherwise concatenate the bodies of a batch into one request.

CURL_WRITE_OUT='%{time_total}\t%{http_code}\t%{size_download}\n'

curl_conf_reset() {
  printf 'silent\nshow-error\nno-buffer\nretry = 0\nmax-time = 600\n' >"$1"
  CURL_CONF_N=0
}

# curl_add <conf> <method> <url> [body_file] [content_type]
curl_add() {
  local conf=$1 method=$2 url=$3 body=${4:-} ctype=${5:-}
  {
    ((CURL_CONF_N++)) && printf 'next\n'
    printf 'url = "%s"\noutput = "/dev/null"\n' "$url"
    printf 'write-out = "%s"\n' "$CURL_WRITE_OUT"
    printf 'request = "%s"\n' "$method"
    [[ -n ${BENCH_TOKEN:-} ]] && printf 'header = "Authorization: Bearer %s"\n' "$BENCH_TOKEN"
    [[ -n $ctype ]] && printf 'header = "Content-Type: %s"\n' "$ctype"
    [[ -n $body ]] && printf 'data-binary = "@%s"\n' "$body"
  } >>"$conf"
}

# curl_run <conf> <out_tsv> [concurrency]
curl_run() {
  local conf=$1 out=$2 concurrency=${3:-1}
  if ((concurrency > 1)); then
    curl --parallel --parallel-immediate --parallel-max "$concurrency" -K "$conf" >"$out"
  else
    curl -K "$conf" >"$out"
  fi
}

# http_summary <tsv> -- emits `key=value` lines
http_summary() {
  local tsv=$1
  awk '
    { n++; if ($2 < 200 || $2 >= 400) err++; bytes += $3 }
    END { printf "requests=%d\nerrors=%d\nbytes_downloaded=%d\n", n, err + 0, bytes + 0 }
  ' "$tsv"
  awk '{ printf "%.6f\n", $1 }' "$tsv" | sort -n | awk '
    { v[n++] = $1; s += $1 }
    END {
      if (!n) exit
      printf "p50_ms=%.2f\n", v[int(n * 0.50)] * 1000
      printf "p95_ms=%.2f\n", v[int(n * 0.95)] * 1000
      printf "p99_ms=%.2f\n", v[int(n * 0.99)] * 1000
      printf "max_ms=%.2f\n", v[n - 1] * 1000
      printf "mean_ms=%.2f\n", s / n * 1000
    }
  '
}

# metrics_requests <base_url> -- total HTTP requests the server has answered
metrics_requests() {
  curl -fsS --max-time 10 "$1/metrics" 2>/dev/null |
    awk '/^opencargo_http_requests_total/ { s += $NF } END { printf "%d", s + 0 }'
}
