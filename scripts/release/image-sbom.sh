#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=scripts/release/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

SYFT_IMAGE=anchore/syft:v1.52.0@sha256:500e2d872ac019436926e8322b4fc1f39441d94d21f6f4046c6ff29b30e8cb02

digest=${1:?usage: image-sbom.sh <digest> <out>}
out=${2:?out}
require_digest "$digest"

mkdir -p "$(dirname "$out")"
docker run --rm --network host -e SYFT_REGISTRY_INSECURE_USE_HTTP \
  "$SYFT_IMAGE" "$IMAGE@$digest" -q -o cyclonedx-json > "$out"
jq -e '.bomFormat == "CycloneDX" and (.serialNumber | length > 0) and (.specVersion | length > 0)' "$out" > /dev/null \
  || die "$out is not a CycloneDX document"
echo "image sbom: $out ($(jq '.components | length' "$out") components)"
