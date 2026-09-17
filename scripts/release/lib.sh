# shellcheck shell=bash disable=SC2034
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
