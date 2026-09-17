# shellcheck shell=bash disable=SC2034
shopt -s inherit_errexit
IMAGE=${IMAGE:-ghcr.io/akarasso/opencargo}
REPO=${REPO:-akarasso/opencargo}
ISSUER=https://token.actions.githubusercontent.com
IDENTITY_CI=${IDENTITY_CI:-https://github.com/$REPO/.github/workflows/ci.yml@refs/heads/main}
CYCLONEDX_PREDICATE=https://cyclonedx.org/bom

identity_release() {
  echo "${IDENTITY_RELEASE:-https://github.com/$REPO/.github/workflows/release.yml@refs/tags/$1}"
}

die() {
  echo "$(basename "$0"): $*" >&2
  exit 1
}

registry_digest() {
  docker buildx imagetools inspect "$1" --format '{{json .Manifest}}' | jq -r .digest
}

require_digest() {
  [[ $1 =~ ^sha256:[0-9a-f]{64}$ ]] || die "'$1' is not a sha256 digest"
}

# The digest docker push reports for the local image it pushed; never a registry lookup.
push_digest() {
  local ref=$1 tag=${1##*:} log matches
  log=$(mktemp)
  docker push "$ref" | tee "$log" >&2
  matches=$(grep -E "^$tag: digest: sha256:[0-9a-f]{64} size: [0-9]+$" "$log" | sed -E 's/.* digest: (sha256:[0-9a-f]{64}) .*/\1/' || true)
  rm -f "$log"
  [[ -n $matches && $(wc -l <<< "$matches") -eq 1 ]] || die "expected one '$tag: digest:' line in docker push output, got '${matches//$'\n'/ }'"
  echo "$matches"
}
