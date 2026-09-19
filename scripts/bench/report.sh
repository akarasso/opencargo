#!/usr/bin/env bash
# Turns the per-scenario `key=value` files into results.json, results.csv and report.md.

REPORT_COLUMNS=(name storage status wall_ms rss_peak_bytes rss_steady_bytes cpu_avg_pct cpu_peak_pct cpu_seconds write_bytes db_bytes storage_bytes requests errors p50_ms p95_ms notes)

# kv_json -- `key=value` lines on stdin, one JSON object on stdout
kv_json() {
  jq -R -s '
    split("\n") | map(select(length > 0 and (startswith("#") | not)))
    | map(split("=") | {(.[0]): (.[1:] | join("="))}) | add // {}
    | with_entries(.value |= (
        if . == "null" or . == "" then null
        elif test("^-?[0-9]+(\\.[0-9]+)?$") then tonumber
        else . end))'
}

machine_json() {
  local cpu threads mem kernel fstype
  cpu=$(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo)
  threads=$(nproc)
  mem=$(awk '/^MemTotal:/ { print $2 * 1024 }' /proc/meminfo)
  kernel=$(uname -sr)
  fstype=$(df -T "${1:-.}" | awk 'NR == 2 { print $2 }')
  jq -n \
    --arg cpu "$cpu" --argjson threads "$threads" --argjson mem "$mem" \
    --arg kernel "$kernel" --arg fstype "$fstype" \
    --arg rustc "$(rustc --version 2>/dev/null || echo unknown)" \
    --arg cargo "$(cargo --version 2>/dev/null || echo unknown)" \
    --arg docker "$(docker --version 2>/dev/null || echo absent)" \
    --arg pnpm "$(pnpm --version 2>/dev/null || echo absent)" \
    --arg curl "$(curl --version 2>/dev/null | head -1)" \
    --arg loadavg "${BENCH_LOADAVG_START:-$(cut -d' ' -f1-3 /proc/loadavg)}" \
    '{cpu_model: $cpu, cpu_threads: $threads, mem_total_bytes: $mem, kernel: $kernel,
      work_filesystem: $fstype, rustc: $rustc, cargo: $cargo, docker: $docker, pnpm: $pnpm,
      curl: $curl, loadavg_at_start: $loadavg}'
}

# report_write <run_dir> <meta_json> -- meta holds commit, binary, machine, artifacts
report_write() {
  local dir=$1 meta=$2
  local scenarios="$dir/scenarios.json"
  : >"$scenarios"
  local f
  for f in "$dir"/scenarios/*.kv; do
    [[ -e $f ]] || continue
    kv_json <"$f" >>"$scenarios"
  done
  jq -s '.' "$scenarios" >"$dir/scenarios.array.json"
  jq --slurpfile s "$dir/scenarios.array.json" '. + {scenarios: $s[0]}' "$meta" >"$dir/results.json"
  rm -f "$scenarios" "$dir/scenarios.array.json"

  local cols
  cols=$(printf '%s\n' "${REPORT_COLUMNS[@]}" | jq -R . | jq -s .)
  jq -r --argjson cols "$cols" '
    ($cols | @csv), (.scenarios[] | [$cols[] as $c | .[$c]] | @csv)
  ' "$dir/results.json" >"$dir/results.csv"

  report_markdown "$dir/results.json" >"$dir/report.md"
}

report_markdown() {
  jq -r '
    def mib: if . == null then "n/a" else (. / 1048576 * 10 | round / 10 | tostring) end;
    def kib: if . == null then "n/a" else (. / 1024 | round | tostring) end;
    def num: if . == null then "n/a" else tostring end;
    "# opencargo benchmark",
    "",
    "- commit: `\(.commit)`\(if .dirty then " (dirty tree)" else "" end), version \(.version)",
    "- binary: `\(.binary.path)` (\(.binary.profile), \(.binary.size_bytes | mib) MiB)",
    "- machine: \(.machine.cpu_model), \(.machine.cpu_threads) threads, \(.machine.mem_total_bytes | mib) MiB RAM, \(.machine.kernel), work dir on \(.machine.work_filesystem)",
    "- toolchain: \(.machine.rustc), docker \(.machine.docker)",
    "- started: \(.started_at), load average \(.machine.loadavg_at_start) at the start and \(.machine.loadavg_at_end) at the end",
    "- settings: \(.settings | to_entries | map("\(.key)=\(.value)") | join(", "))",
    "",
    "| scenario | storage | wall (s) | peak RSS (MiB) | steady RSS (MiB) | CPU avg % | CPU peak % | CPU (s) | written (MiB) | db (KiB) | storage (MiB) | reqs | err | p50 (ms) | p95 (ms) |",
    "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|",
    (.scenarios[] | select(.status == "ok") |
      "| \(.name) | \(.storage // "-") | \((.wall_ms // 0) / 1000 * 100 | round / 100) | \(.rss_peak_bytes | mib) | \(.rss_steady_bytes | mib) | \(.cpu_avg_pct | num) | \(.cpu_peak_pct | num) | \(.cpu_seconds | num) | \(.write_bytes | mib) | \(.db_bytes | kib) | \(.storage_bytes | mib) | \(.requests | num) | \(.errors | num) | \(.p50_ms | num) | \(.p95_ms | num) |"),
    "",
    (if ([.scenarios[] | select(.throughput_rps)] | length) > 0 then
      "## Throughput\n\n| scenario | requests/s | downloaded (MiB) |\n|---|---|---|",
      (.scenarios[] | select(.throughput_rps) |
        "| \(.name) | \(.throughput_rps) | \(.bytes_downloaded | mib) |"),
      ""
     else empty end),
    (if ([.scenarios[] | select(.status != "ok")] | length) > 0 then
      "## Not measured\n\n| scenario | status | reason |\n|---|---|---|",
      (.scenarios[] | select(.status != "ok") | "| \(.name) | \(.status) | \(.skip_reason // .notes // "-") |"),
      ""
     else empty end),
    (if (.scenarios | map(select(.notes != null and .status == "ok")) | length) > 0 then
      "## Notes\n",
      (.scenarios[] | select(.status == "ok" and .notes != null) | "- **\(.name)**: \(.notes)"),
      ""
     else empty end),
    "## Artifacts",
    "",
    "| artifact | bytes |",
    "|---|---|",
    "| release binary | \(.artifacts.binary_bytes | num) |",
    "| container image | \(.artifacts.image_bytes | num) |",
    (if .artifacts.image_ref then "| image ref | `\(.artifacts.image_ref)` |" else empty end)
  ' "$1"
}
