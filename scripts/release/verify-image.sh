#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=scripts/release/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

tag=${1:?usage: verify-image.sh <tag> <digest> <sha>}
digest=${2:?digest}
sha=${3:?sha}
require_digest "$digest"
rel=$(identity_release "$tag")
ref=$IMAGE@$digest
run_uri=${RUN_INVOCATION_URI:-$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID/attempts/$GITHUB_RUN_ATTEMPT}

for id in "$rel" "$IDENTITY_CI"; do
  cosign verify --new-bundle-format=false "$ref" \
    --certificate-identity "$id" --certificate-oidc-issuer "$ISSUER" \
    --certificate-github-workflow-sha "$sha" > /dev/null
  echo "signature OK: $ref by $id"
done

# Attestations are append-only: count only this run's SBOMs so a re-run is judged on its own two.
names=$(gh attestation verify "oci://$ref" -R "$REPO" --cert-identity "$rel" --cert-oidc-issuer "$ISSUER" \
  --predicate-type "$CYCLONEDX_PREDICATE" --format json \
  | jq -c --arg uri "$run_uri" '[.[] | select(.verificationResult.signature.certificate.runInvocationURI == $uri)
      | .verificationResult.statement.predicate.metadata.component.name]')
[[ $(jq length <<< "$names") == 2 && $(jq 'unique | length' <<< "$names") == 2 ]] \
  || die "expected 2 distinct SBOM attestations from $run_uri, got $names"
jq -e 'index("opencargo") != null' <<< "$names" > /dev/null || die "no opencargo SBOM among $names"
echo "sbom attestations OK: $names"

gh attestation verify "oci://$ref" -R "$REPO" --cert-identity "$IDENTITY_CI" --cert-oidc-issuer "$ISSUER" \
  --source-digest "$sha" > /dev/null
echo "provenance OK: $ref by $IDENTITY_CI for $sha"
