#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=scripts/release/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

cmd=${1:?usage: promote-image.sh verify-source <digest> <sha> | tag <digest> <tag,tag,...>}
digest=${2:?digest}
require_digest "$digest"

case $cmd in
  verify-source)
    sha=${3:?sha}
    cosign verify --new-bundle-format=false "$IMAGE@$digest" \
      --certificate-identity "$IDENTITY_CI" --certificate-oidc-issuer "$ISSUER" \
      --certificate-github-workflow-sha "$sha" > /dev/null
    echo "source $IMAGE@$digest signed by $IDENTITY_CI for $sha"
    ;;
  tag)
    IFS=, read -ra tags <<< "${3:?tags}"
    (( ${#tags[@]} > 0 )) || die "no tag to promote"
    for t in "${tags[@]}"; do
      docker buildx imagetools create --prefer-index=false -t "$IMAGE:$t" "$IMAGE@$digest"
    done
    for t in "${tags[@]}"; do
      got=$(registry_digest "$IMAGE:$t")
      [[ $got == "$digest" ]] || die "$IMAGE:$t is $got, expected $digest"
      echo "$IMAGE:$t -> $digest"
    done
    ;;
  *) die "unknown command $cmd" ;;
esac
