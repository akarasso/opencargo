#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=scripts/release/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

local_image=${1:?usage: push-latest.sh <local image> <ref:latest> <signed digest>}
ref=${2:?ref}
signed=${3:?signed digest}
sha=${GITHUB_SHA:?GITHUB_SHA}
require_digest "$signed"

main=$(git ls-remote "${MAIN_REMOTE:-https://github.com/$REPO}" refs/heads/main | cut -f1)
[[ -n $main ]] || die "could not read main's head"
if [[ $main != "$sha" ]]; then
  echo "$sha is not main's head ($main), latest left alone"
  exit 0
fi

docker tag "$local_image" "$ref"
digest=$(push_digest "$ref")
[[ $digest == "$signed" ]] || die "docker push of $ref reported $digest, signed digest is $signed"
echo "$ref -> $digest"
