#!/usr/bin/env bash
set -euo pipefail

ACTIONLINT_VERSION=v1.7.12
SHELLCHECK_URL=https://github.com/koalaman/shellcheck/releases/download/v0.11.0/shellcheck-v0.11.0.linux.x86_64.tar.xz
SHELLCHECK_SHA256=8c3be12b05d5c177a04c29e3c78ce89ac86f1595681cab149b65b97c4e227198

cd "$(dirname "${BASH_SOURCE[0]}")/.."
bin=$(mktemp -d)
trap 'rm -rf "$bin"' EXIT

GOBIN=$bin go install "github.com/rhysd/actionlint/cmd/actionlint@$ACTIONLINT_VERSION"
curl -sSfL --retry 3 -o "$bin/sc.tar.xz" "$SHELLCHECK_URL"
echo "$SHELLCHECK_SHA256  $bin/sc.tar.xz" | sha256sum --strict -c
tar -xJf "$bin/sc.tar.xz" -C "$bin" --strip-components=1 shellcheck-v0.11.0/shellcheck
export PATH=$bin:$PATH

shellcheck --version | grep '^version:'
# actionlint 1.7.12 does not know concurrency.queue yet.
actionlint -ignore 'unexpected key "queue" for "concurrency" section'
shellcheck -x scripts/*.sh scripts/release/*.sh
echo "workflows and scripts: clean"
