#!/usr/bin/env bash
set -euo pipefail

IMAGE=${IMAGE:-ghcr.io/akarasso/opencargo}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
tag=${1:-}

emit() {
  if [[ -n ${GITHUB_OUTPUT:-} ]]; then
    printf '%s=%s\n' "$1" "$2" >> "$GITHUB_OUTPUT"
  fi
  printf '%s=%s\n' "$1" "$2"
}
die() {
  echo "meta: $*" >&2
  exit 1
}

inspect_digest() {
  if [[ -n ${IMAGE_INSPECT_CMD:-} ]]; then
    local cmd
    read -ra cmd <<< "$IMAGE_INSPECT_CMD"
    "${cmd[@]}" "$1"
  else
    docker buildx imagetools inspect "$1" --format '{{json .Manifest}}' | jq -r .digest
  fi
}

cargo_version=$(awk '
  /^\[/ { in_pkg = ($0 == "[package]") }
  in_pkg && /^version[[:space:]]*=/ { gsub(/.*=[[:space:]]*"|".*/, ""); print; exit }
' Cargo.toml)
lock_version=$(awk '
  $0 == "name = \"opencargo\"" { found = 1; next }
  found && /^version = / { gsub(/^version = "|"$/, ""); print; exit }
' Cargo.lock)
[[ -n $cargo_version ]] || die "no [package] version in Cargo.toml"
[[ $lock_version == "$cargo_version" ]] || die "Cargo.lock has opencargo '$lock_version', Cargo.toml has '$cargo_version'"

if [[ -z $tag ]]; then
  emit version "$cargo_version"
  emit prerelease "$([[ $cargo_version == *-* ]] && echo true || echo false)"
  emit latest false
  emit tags ""
  emit source_digest ""
  exit 0
fi

final_re='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
rc_re='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-rc\.[1-9][0-9]*$'
if [[ $tag =~ $final_re ]]; then
  prerelease=false
elif [[ $tag =~ $rc_re ]]; then
  prerelease=true
else
  die "tag '$tag' is not vX.Y.Z or vX.Y.Z-rc.N (no leading zeros, N >= 1)"
fi
major=${BASH_REMATCH[1]} minor=${BASH_REMATCH[2]} patch=${BASH_REMATCH[3]}
version=${tag#v}

[[ $version == "$cargo_version" ]] || die "tag '$tag' does not match Cargo.toml version '$cargo_version'"

head=$(git rev-parse 'HEAD^{commit}')
tagged=$(git rev-parse "refs/tags/$tag^{commit}" 2>/dev/null) || die "tag '$tag' not found locally"
[[ $tagged == "$head" ]] || die "tag '$tag' points at $tagged, checkout is $head"

git fetch --quiet --no-tags origin main
git merge-base --is-ancestor "$head" refs/remotes/origin/main || die "commit $head is not on origin/main"

"$here/changelog-section.sh" "$version" CHANGELOG.md > /dev/null || die "CHANGELOG.md has no section for $version"

digest=$(inspect_digest "$IMAGE:sha-$head") || die "$IMAGE:sha-$head not found (did main CI push it?)"
[[ $digest =~ ^sha256:[0-9a-f]{64}$ ]] || die "$IMAGE:sha-$head resolved to '$digest'"

tags=$version latest=false
if [[ $prerelease == false ]]; then
  higher_minor=false higher_major=false higher_any=false
  while IFS= read -r t; do
    [[ $t =~ $final_re ]] || continue
    M=${BASH_REMATCH[1]} m=${BASH_REMATCH[2]} p=${BASH_REMATCH[3]}
    if (( M > major || (M == major && m > minor) || (M == major && m == minor && p > patch) )); then
      higher_any=true
      if (( M == major )); then
        higher_major=true
        if (( m == minor )); then higher_minor=true; fi
      fi
    fi
  done < <(git tag -l 'v*')
  [[ $higher_minor == true ]] || tags+=",$major.$minor"
  [[ $higher_major == true ]] || tags+=",$major"
  [[ $higher_any == true ]] || latest=true
fi

emit version "$version"
emit prerelease "$prerelease"
emit latest "$latest"
emit tags "$tags"
emit source_digest "$digest"
