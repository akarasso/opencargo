#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=scripts/release/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

dir=${1:?usage: verify-assets.sh <dir> <tag>}
tag=${2:?tag}
id=$(identity_release "$tag")

cd "$dir"
sha256sum --strict -c SHA256SUMS
mapfile -t files < <(awk '{print $2}' SHA256SUMS)
(( ${#files[@]} > 0 )) || die "SHA256SUMS is empty"

for f in "${files[@]}" SHA256SUMS; do
  cosign verify-blob --bundle "$f.sigstore.json" \
    --certificate-identity "$id" --certificate-oidc-issuer "$ISSUER" "$f"
done
for f in "${files[@]}"; do
  gh attestation verify "$f" -R "$REPO" --cert-identity "$id" --cert-oidc-issuer "$ISSUER" > /dev/null
  echo "provenance OK: $f"
  if [[ $f != *.cdx.json ]]; then
    gh attestation verify "$f" -R "$REPO" --cert-identity "$id" --cert-oidc-issuer "$ISSUER" \
      --predicate-type "$CYCLONEDX_PREDICATE" > /dev/null
    echo "sbom OK: $f"
  fi
done
