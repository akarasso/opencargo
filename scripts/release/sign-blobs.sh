#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=scripts/release/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

dir=${1:?usage: sign-blobs.sh <dir> <tag>}
tag=${2:?tag}
id=$(identity_release "$tag")

cd "$dir"
sha256sum --strict -c SHA256SUMS
mapfile -t files < <(awk '{print $2}' SHA256SUMS)
(( ${#files[@]} > 0 )) || die "SHA256SUMS is empty"
files+=(SHA256SUMS)

for f in "${files[@]}"; do
  rm -f "$f.sigstore.json"
  cosign sign-blob --yes --bundle "$f.sigstore.json" "$f"
  cosign verify-blob --bundle "$f.sigstore.json" \
    --certificate-identity "$id" --certificate-oidc-issuer "$ISSUER" "$f"
done
echo "signed and verified ${#files[@]} blobs as $id"
