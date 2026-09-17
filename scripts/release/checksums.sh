#!/usr/bin/env bash
set -euo pipefail

dir=${1:?usage: checksums.sh <dir>}
cd "$dir"
shopt -s nullglob
files=()
for f in opencargo-*; do
  [[ $f == *.sigstore.json ]] || files+=("$f")
done
if (( ${#files[@]} == 0 )); then
  echo "checksums: no opencargo-* file in $dir" >&2
  exit 1
fi
printf '%s\n' "${files[@]}" | LC_ALL=C sort | xargs -d '\n' sha256sum > SHA256SUMS
sha256sum --strict -c SHA256SUMS
