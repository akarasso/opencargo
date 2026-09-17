#!/usr/bin/env bash
set -euo pipefail

RUST_IMAGE=rust:1.93.0-alpine@sha256:69d7b9d9aeaf108a1419d9a7fcf7860dcc043e9dbd1ab7ce88e44228774d99e9

target=${1:?usage: build-musl.sh <target> <version> <out>}
version=${2:?version}
out=${3:?out}

case $target in
  x86_64-unknown-linux-musl | aarch64-unknown-linux-musl) ;;
  *) echo "build-musl: unsupported target $target" >&2; exit 1 ;;
esac
if [[ $(uname -m) != "${target%%-*}" ]]; then
  echo "build-musl: $target needs a native $(uname -m) != ${target%%-*} host" >&2
  exit 1
fi
if [[ ! -f frontend/dist/index.html ]]; then
  echo "build-musl: frontend/dist/index.html missing, build the frontend first" >&2
  exit 1
fi

mkdir -p target/musl "$out"
# Non-root keeps the tree owned by the caller; RUSTUP_TOOLCHAIN uses the image's compiler instead of a rust-toolchain.toml sync.
docker run --rm --user "$(id -u):$(id -g)" \
  -v "$PWD:/src" -w /src \
  -e HOME=/src/target/musl/home \
  -e CARGO_HOME=/src/target/musl/cargo-home \
  -e CARGO_TARGET_DIR=/src/target/musl/build \
  -e RUSTUP_TOOLCHAIN=1.93.0 \
  "$RUST_IMAGE" \
  sh -euc 'cargo build --release --locked --target "$1"' sh "$target"

bin=target/musl/build/$target/release/opencargo
if readelf -lW "$bin" | grep -q INTERP; then
  echo "build-musl: $bin has an INTERP segment, not static" >&2
  exit 1
fi
install -m 0755 "$bin" "$out/opencargo-$version-$target"
echo "built $out/opencargo-$version-$target"
