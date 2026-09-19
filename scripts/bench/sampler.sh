#!/usr/bin/env bash
# Samples one process through /proc into a phase-tagged TSV, and summarises it.
# Columns: epoch_seconds phase rss_kb hwm_kb cpu_ticks read_bytes write_bytes threads

sampler_now() {
  if [[ -n ${EPOCHREALTIME:-} ]]; then
    printf '%s' "${EPOCHREALTIME}"
  else
    date +%s.%N
  fi
}

# sampler_start <pid> <tsv> <phase_file> [interval_seconds]
sampler_start() {
  local pid=$1 tsv=$2 phase_file=$3 interval=${4:-0.1}
  local fifo
  fifo=$(mktemp -u)
  mkfifo "$fifo"
  : >"$tsv"
  (
    exec 9<>"$fifo"
    rm -f "$fifo"
    local phase line rss hwm utime stime rbytes wbytes threads f
    while [[ -r /proc/$pid/status ]]; do
      phase=load
      [[ -r $phase_file ]] && read -r phase <"$phase_file"
      rss=0 hwm=0 threads=0
      while read -r line; do
        case $line in
          VmRSS:*) rss=${line//[^0-9]/} ;;
          VmHWM:*) hwm=${line//[^0-9]/} ;;
          Threads:*) threads=${line//[^0-9]/} ;;
        esac
      done </proc/$pid/status
      # /proc/pid/stat fields 14 and 15 are utime and stime, after a comm that may hold spaces.
      read -r line </proc/$pid/stat || break
      line=${line#*) }
      # shellcheck disable=SC2086
      set -- $line
      utime=${12:-0} stime=${13:-0}
      rbytes=0 wbytes=0
      if [[ -r /proc/$pid/io ]]; then
        while read -r f line; do
          case $f in
            read_bytes:) rbytes=$line ;;
            write_bytes:) wbytes=$line ;;
          esac
        done </proc/$pid/io
      fi
      printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$(sampler_now)" "$phase" "$rss" "$hwm" "$((utime + stime))" "$rbytes" "$wbytes" "$threads" >>"$tsv"
      read -r -t "$interval" -u 9 _ || true
    done
  ) &
  SAMPLER_PID=$!
}

sampler_stop() {
  [[ -n ${SAMPLER_PID:-} ]] || return 0
  kill "$SAMPLER_PID" 2>/dev/null || true
  wait "$SAMPLER_PID" 2>/dev/null || true
  SAMPLER_PID=
}

# sampler_summary <tsv> <steady_phase> -- emits `key=value` lines
sampler_summary() {
  local tsv=$1 steady=${2:-settle}
  awk -v steady="$steady" -v tck="$(getconf CLK_TCK)" '
    NR == 1 { t0 = $1; c0 = $5; r0 = $6; w0 = $7 }
    {
      n++
      t = $1; rss = $3; hwm = $4; cpu = $5
      if (rss > rss_peak) rss_peak = rss
      if (hwm > hwm_peak) hwm_peak = hwm
      if ($8 > threads_peak) threads_peak = $8
      if (n > 1 && t > tp) {
        pct = (cpu - cp) / tck / (t - tp) * 100
        if (pct > cpu_peak) cpu_peak = pct
      }
      if ($2 == steady) {
        s_n++
        if (s_n == 1) { s_cpu_t0 = t; s_cpu_c0 = cpu }
        s_cpu_t1 = t; s_cpu_c1 = cpu
      }
      tp = t; cp = cpu
      t1 = t; c1 = cpu; r1 = $6; w1 = $7
    }
    END {
      if (n == 0) { print "samples=0"; exit }
      wall = t1 - t0
      cpu_s = (c1 - c0) / tck
      printf "samples=%d\n", n
      printf "window_s=%.3f\n", wall
      printf "rss_peak_bytes=%d\n", (hwm_peak > rss_peak ? hwm_peak : rss_peak) * 1024
      printf "rss_last_bytes=%d\n", rss * 1024
      printf "cpu_seconds=%.3f\n", cpu_s
      printf "cpu_avg_pct=%.1f\n", (wall > 0 ? cpu_s / wall * 100 : 0)
      printf "cpu_peak_pct=%.1f\n", cpu_peak
      printf "read_bytes=%d\n", r1 - r0
      printf "write_bytes=%d\n", w1 - w0
      printf "threads_peak=%d\n", threads_peak
      if (s_n > 0) {
        sw = s_cpu_t1 - s_cpu_t0
        printf "cpu_steady_pct=%.1f\n", (sw > 0 ? (s_cpu_c1 - s_cpu_c0) / tck / sw * 100 : 0)
      }
    }
  ' "$tsv"
  local median
  median=$(awk -v steady="$steady" '$2 == steady { print $3 }' "$tsv" | sort -n | median_of)
  [[ -n $median ]] && printf 'rss_steady_bytes=%d\n' "$((median * 1024))"
  return 0
}

# median_of -- sorted numbers on stdin, median on stdout
median_of() {
  awk '{ v[n++] = $1 } END { if (n) print v[int((n - 1) / 2)] }'
}
