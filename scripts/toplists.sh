#!/usr/bin/env bash
# Regenerates the typosquat lists under src/policy/lists (sources and
# licences in NOTICE): the top tier compared against, and with --known the
# 20 000 known names that pass untouched plus the 20 001-40 000 holdout the
# false-positive test measures. Every name is normalised the way
# policy::distance::normalize does it: lowercase, `_` -> `-`, Go major
# suffixes stripped, PyPI runs of `-_.` one `-` (PEP 503), then deduplicated
# and sorted. `--only <eco>` regenerates one ecosystem.
set -euo pipefail

OUT=${OUT:-src/policy/lists}
UA="opencargo-toplists (https://github.com/alexandrekarassouloff/opencargo)"
ECO="https://packages.ecosyste.ms/api/v1/registries"
DATE=$(date -u +%Y-%m-%d)
TOP_NPM=5000
TOP_CRATES=5000
TOP_GO=2000
TOP_PYPI=5000
KNOWN=20000

normalize() {
  local eco=$1
  tr 'A-Z_' 'a-z-' | if [ "$eco" = go ]; then
    sed -E -e 's#/v([2-9]|[1-9][0-9]+)$##' -e 's#^(gopkg\.in/.*)\.v[0-9]+$#\1#'
  elif [ "$eco" = pypi ]; then
    sed -E 's#[-.]+#-#g'
  else
    cat
  fi
}

# write <file> <header> <limit>: normalised, deduplicated, sorted names from stdin
write() {
  local file=$1 header=$2 limit=$3
  { echo "# $header, fetched $DATE by scripts/toplists.sh"
    awk -v n="$limit" '!seen[$0]++ && ++kept <= n' | sort; } > "$file"
  echo "$file: $(($(wc -l < "$file") - 1)) names"
}

eco_pages() {
  local registry=$1 first=$2 last=$3 sort=${4:-dependent_packages_count} page
  for page in $(seq "$first" "$last"); do
    curl -sSf -A "$UA" "$ECO/$registry/packages?per_page=1000&page=$page&sort=$sort&order=desc" \
      | jq -r '.[].name'
  done
}

top_npm() {
  curl -sSfL "https://unpkg.com/npm-high-impact@latest/lib/top-download.js" \
    | sed -n "s/^  '\(.*\)',\{0,1\}$/\1/p" | normalize npm \
    | write "$OUT/npm.txt" "npm-high-impact topDownload, first $TOP_NPM" "$TOP_NPM"
}

top_crates() {
  local page
  for page in $(seq 1 $((TOP_CRATES / 100))); do
    curl -sSf -A "$UA" "https://crates.io/api/v1/crates?sort=downloads&per_page=100&page=$page" \
      | jq -r '.crates[].name'
    sleep 1
  done | normalize crates \
    | write "$OUT/crates.txt" "crates.io API sorted by downloads, first $TOP_CRATES" "$TOP_CRATES"
}

top_go() {
  eco_pages proxy.golang.org 1 3 | normalize go \
    | write "$OUT/go.txt" "ecosyste.ms proxy.golang.org by dependents, first $TOP_GO after major suffixes" "$TOP_GO"
}

top_pypi() {
  eco_pages pypi.org 1 $((TOP_PYPI / 1000)) downloads | normalize pypi \
    | write "$OUT/pypi.txt" "ecosyste.ms pypi.org by downloads, first $TOP_PYPI" "$TOP_PYPI"
}

known() {
  local eco=$1 registry=$2 pages=$((KNOWN / 1000)) all
  all=$(eco_pages "$registry" 1 $((pages * 2)) | normalize "$eco" | awk '!seen[$0]++')
  echo "$all" | write "$OUT/known/$eco.txt" "ecosyste.ms $registry by dependents, ranks 1-$KNOWN" "$KNOWN"
  echo "$all" | awk -v n="$KNOWN" 'NR > n' \
    | write "$OUT/holdout/$eco.txt" "ecosyste.ms $registry by dependents, ranks $((KNOWN + 1))-$((KNOWN * 2))" "$KNOWN"
}

mkdir -p "$OUT/known" "$OUT/holdout"
if [ "${1:-}" = --only ]; then
  case ${2:-} in
    pypi) top_pypi; known pypi pypi.org ;;
    *) echo "usage: $0 [--known | --only pypi]" >&2; exit 2 ;;
  esac
elif [ "${1:-}" = --known ]; then
  known npm npmjs.org
  known crates crates.io
  known go proxy.golang.org
  known pypi pypi.org
else
  top_npm
  top_go
  top_crates
  top_pypi
fi
