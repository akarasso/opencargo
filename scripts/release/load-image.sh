#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=scripts/release/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

tar=${1:?usage: load-image.sh <image tar> <local image> <scanned image id>}
local_image=${2:?local image}
scanned=${3:?scanned image id}
require_digest "$scanned"

# The ID is content-addressed and docker load checks every blob against it: same ID, same bytes Trivy saw.
docker load -i "$tar"
got=$(docker image inspect --format '{{.Id}}' "$local_image")
[[ $got == "$scanned" ]] || die "$local_image loaded as $got, the scanned image was $scanned"
echo "$local_image = $scanned (scanned)"
