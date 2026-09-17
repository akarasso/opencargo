#!/usr/bin/env bash
set -euo pipefail

CYCLONEDX_URL=https://github.com/CycloneDX/cyclonedx-rust-cargo/releases/download/cargo-cyclonedx-0.5.9/cargo-cyclonedx-x86_64-unknown-linux-gnu.tar.xz
CYCLONEDX_SHA256=fb8dbee9f182173e062a64a387b21a0badc6fab8b2abf9294973f012972bf6d8
TARGETS=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)

version=${1:?usage: sbom.sh <version> <out>}
out=${2:?out}

tool=$(mktemp -d)
trap 'rm -rf "$tool"' EXIT
curl -sSfL --retry 3 -o "$tool/cdx.tar.xz" "$CYCLONEDX_URL"
echo "$CYCLONEDX_SHA256  $tool/cdx.tar.xz" | sha256sum --strict -c
tar -xJf "$tool/cdx.tar.xz" -C "$tool" --strip-components=1
mkdir -p "$out"

for t in "${TARGETS[@]}"; do
  name=opencargo-$version-$t
  "$tool/cargo-cyclonedx" cyclonedx --manifest-path Cargo.toml -q -f json --spec-version 1.5 \
    --no-build-deps --target "$t" --override-filename "$name.cdx"
  mv "$name.cdx.json" "$out/$name.cdx.json"
  got=$(jq -r '.metadata.component.version' "$out/$name.cdx.json")
  if [[ $got != "$version" ]]; then
    echo "sbom: $name.cdx.json root version is '$got', expected '$version'" >&2
    exit 1
  fi
  echo "sbom: $out/$name.cdx.json ($(jq '.components | length' "$out/$name.cdx.json") components)"
done
