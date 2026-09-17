#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=scripts/release/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

local_image=${1:?usage: sign-image.sh <local image> <ref:tag>}
ref=${2:?ref}
sha=${GITHUB_SHA:?GITHUB_SHA}
repo=${ref%:*}

docker tag "$local_image" "$ref"
digest=$(push_digest "$ref")
registry=$(registry_digest "$ref")
[[ $registry == "$digest" ]] || die "$ref is $registry in the registry, docker push reported $digest"

cosign sign --yes --new-bundle-format=false --use-signing-config=false "$repo@$digest"
cosign verify --new-bundle-format=false "$repo@$digest" \
  --certificate-identity "$IDENTITY_CI" --certificate-oidc-issuer "$ISSUER" \
  --certificate-github-workflow-sha "$sha" > /dev/null
echo "signed $repo@$digest ($ref)"
if [[ -n ${GITHUB_OUTPUT:-} ]]; then
  echo "digest=$digest" >> "$GITHUB_OUTPUT"
fi
