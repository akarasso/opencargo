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

# Anonymous registry lookup (no docker needed): the tag's digest, or "absent" on 404; anything else fails.
tag_digest() {
  local host=${IMAGE%%/*} repo=${IMAGE#*/} hdr code token auth=()
  if [[ $host == ghcr.io ]]; then
    token=$(curl -sSf --retry 3 "https://ghcr.io/token?service=ghcr.io&scope=repository:$repo:pull" | jq -er .token) || die "no anonymous ghcr.io token for $repo"
    auth=(-H "Authorization: Bearer $token")
  fi
  hdr=$(curl -sS --retry 3 -I "${auth[@]}" -w '%{http_code}' \
    -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json' \
    "${REGISTRY_SCHEME:-https}://$host/v2/$repo/manifests/$1")
  code=${hdr##*$'\n'}
  case $code in
    200) grep -i '^docker-content-digest:' <<< "$hdr" | tr -d '\r' | awk '{print $2}' | grep -E '^sha256:[0-9a-f]{64}$' || die "$IMAGE:$1: no digest header" ;;
    404) echo absent ;;
    *) die "$IMAGE:$1: registry answered HTTP $code" ;;
  esac
}
