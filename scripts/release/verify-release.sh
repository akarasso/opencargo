#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=scripts/release/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

v=${1:?usage: verify-release.sh <version> [commit]}
tag=v$v
commit=${2:-$(gh api "repos/$REPO/commits/$tag" -q .sha)}
rel=$(identity_release "$tag")
rc=false
[[ $v == *-rc.* ]] && rc=true

state=$(gh release view "$tag" -R "$REPO" --json isPrerelease,isDraft,assets -q '[.isPrerelease,.isDraft,(.assets|length)]|@csv')
[[ $state == "$rc,false,10" ]] || die "release $tag: prerelease,draft,assets = $state, expected $rc,false,10"
echo "release OK: $tag prerelease=$rc, 10 assets"

status=$(gh api "repos/$REPO/compare/main...$commit" -q .status)
[[ $status == identical || $status == behind ]] || die "$commit is $status relative to main"
echo "commit OK: $commit on main ($status)"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
gh release download "$tag" -R "$REPO"
sha256sum --strict -c SHA256SUMS
mapfile -t files < <(awk '{print $2}' SHA256SUMS)
for f in "${files[@]}" SHA256SUMS; do
  cosign verify-blob --bundle "$f.sigstore.json" --certificate-identity "$rel" --certificate-oidc-issuer "$ISSUER" "$f"
done
for f in "${files[@]}"; do
  [[ $f == *.cdx.json ]] && continue
  gh attestation verify "$f" -R "$REPO" --cert-identity "$rel" --cert-oidc-issuer "$ISSUER" > /dev/null
  gh attestation verify "$f" -R "$REPO" --cert-identity "$rel" --cert-oidc-issuer "$ISSUER" --predicate-type "$CYCLONEDX_PREDICATE" > /dev/null
  [[ $(jq -r .metadata.component.version "$f.cdx.json") == "$v" ]] || die "$f.cdx.json root version is not $v"
  echo "attestations OK: $f"
done
if [[ $(uname -m) == x86_64 ]]; then
  chmod +x "opencargo-$v-x86_64-unknown-linux-musl"
  [[ $(./"opencargo-$v-x86_64-unknown-linux-musl" --version) == "opencargo $v" ]] || die "--version mismatch"
  echo "binary OK: opencargo $v"
fi

cosign verify --new-bundle-format=false "$IMAGE:$v" --certificate-identity "$rel" --certificate-oidc-issuer "$ISSUER" \
  --certificate-github-workflow-sha "$commit" > /dev/null
cosign verify --new-bundle-format=false "$IMAGE:sha-$commit" --certificate-identity "$IDENTITY_CI" --certificate-oidc-issuer "$ISSUER" \
  --certificate-github-workflow-sha "$commit" > /dev/null
d=$(gh attestation verify "oci://$IMAGE:sha-$commit" -R "$REPO" --cert-identity "$IDENTITY_CI" --cert-oidc-issuer "$ISSUER" \
  --source-digest "$commit" --format json -q '.[0].verificationResult.statement.subject[0].digest.sha256')
got=$(gh attestation verify "oci://$IMAGE:$v" -R "$REPO" --cert-identity "$rel" --cert-oidc-issuer "$ISSUER" \
  --predicate-type "$CYCLONEDX_PREDICATE" --format json \
  -q '[([.[].verificationResult.statement.predicate.metadata.component.name]|unique|length),([.[].verificationResult.statement.subject[0].digest.sha256]|unique)]|tojson')
[[ $got == "[2,[\"$d\"]]" ]] || die "image SBOM attestations: $got, expected [2,[\"$d\"]]"
echo "image OK: $IMAGE:$v = sha-$commit = sha256:$d, signed by release.yml and ci.yml, 2 SBOMs"

if [[ $rc == true ]]; then
  if cosign verify --new-bundle-format=false "$IMAGE:${v%%.*}.$(cut -d. -f2 <<< "$v")" \
      --certificate-identity "$rel" --certificate-oidc-issuer "$ISSUER" > /dev/null 2>&1; then
    die "an rc moved a floating tag"
  fi
  echo "floating tags OK: untouched by rc"
fi
echo "release $tag verified"
