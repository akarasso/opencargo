#!/usr/bin/env bash
set -euo pipefail

meta=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/meta.sh
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
unset GITHUB_OUTPUT

printf '#!/bin/sh\necho sha256:%s\n' "$(printf 'a%.0s' {1..64})" > "$root/found"
printf '#!/bin/sh\necho MANIFEST_UNKNOWN >&2\nexit 1\n' > "$root/missing"
chmod +x "$root/found" "$root/missing"

fails=0 n=0

# fixture <name> <cargo version> <lock version> <changelog version>: repo with origin/main at one commit
fixture() {
  local d=$root/$1
  git init -q --bare -b main "$d.git"
  git init -q -b main "$d"
  cd "$d"
  printf '[package]\nname = "opencargo"\nversion = "%s"\n\n[dependencies]\nversion-dep = { version = "9.9.9" }\n' "$2" > Cargo.toml
  printf 'version = 4\n\n[[package]]\nname = "aaa"\nversion = "9.9.9"\n\n[[package]]\nname = "opencargo"\nversion = "%s"\n' "$3" > Cargo.lock
  printf '# Changelog\n\n## [Unreleased]\n\n## [%s] - 2026-09-17\n\n### Added\n- thing\n\n## [0.0.1] - 2026-01-01\n- old\n' "$4" > CHANGELOG.md
  git add -A && git commit -qm init
  git remote add origin "$d.git"
  git push -q origin main
}

expect() {
  local want=$1 label=$2 out rc=0
  shift 2
  n=$((n + 1))
  out=$("$@" 2>&1) || rc=$?
  if [[ $want == fail:* && $rc -ne 0 && $out == *"${want#fail:}"* ]] || [[ $want != fail:* && $rc -eq 0 && $out == *"$want"* ]]; then
    echo "ok   $label"
  else
    echo "FAIL $label (rc=$rc)"
    printf '%s\n' "$out" | sed 's/^/     /'
    fails=$((fails + 1))
  fi
}

run() {
  IMAGE_INSPECT_CMD=$root/found "$meta" "$@"
}
run_missing() {
  IMAGE_INSPECT_CMD=$root/missing "$meta" "$@"
}

fixture final 0.1.0 0.1.0 0.1.0
git tag v0.1.0
expect $'version=0.1.0\nprerelease=false\nlatest=true\ntags=0.1.0,0.1,0\nsource_digest=sha256:aaaa' "accepts v0.1.0" run v0.1.0
expect "version=0.1.0" "no tag: version from Cargo.toml" run
for bad in v0.1 v01.0.0 v0.1.00 v0.1.0-rc.0 v0.1.0-rc.01 v0.1.0-beta.1 0.1.0 v0.1.0-rc.1+b; do
  git tag "$bad" 2>/dev/null || true
  expect "fail:is not vX.Y.Z" "rejects $bad" run "$bad"
done
git tag v0.2.0
expect "fail:does not match Cargo.toml" "rejects tag/Cargo mismatch" run v0.2.0
expect "fail:sha-" "rejects a missing image" run_missing v0.1.0

fixture rc 0.1.0-rc.1 0.1.0-rc.1 0.1.0-rc.1
git tag v0.1.0-rc.1
expect $'version=0.1.0-rc.1\nprerelease=true\nlatest=false\ntags=0.1.0-rc.1\n' "accepts v0.1.0-rc.1" run v0.1.0-rc.1
git tag v0.1.0-rc.2
expect "fail:does not match Cargo.toml" "rejects rc.2 against Cargo rc.1" run v0.1.0-rc.2

fixture lock 0.1.0 0.0.9 0.1.0
git tag v0.1.0
expect "fail:Cargo.lock has" "rejects Cargo.lock mismatch" run v0.1.0

fixture nosection 0.1.0 0.1.0 0.0.9
git tag v0.1.0
expect "fail:no section" "rejects a missing CHANGELOG section" run v0.1.0

fixture offmain 0.1.0 0.1.0 0.1.0
git checkout -qb side && git commit -q --allow-empty -m side && git tag v0.1.0
expect "fail:is not on origin/main" "rejects a commit off main" run v0.1.0

fixture nothead 0.1.0 0.1.0 0.1.0
git tag v0.1.0 && git commit -q --allow-empty -m next && git push -q origin main
expect "fail:checkout is" "rejects a tag that is not HEAD" run v0.1.0

fixture older 0.1.1 0.1.1 0.1.1
git tag v0.2.0 && git tag v0.1.1
expect $'latest=false\ntags=0.1.1,0.1\n' "higher minor exists: no X, not latest" run v0.1.1

fixture oldermajor 0.3.0 0.3.0 0.3.0
git tag v1.0.0 && git tag v0.3.0-rc.1 && git tag v0.3.0
expect $'latest=false\ntags=0.3.0,0.3,0\n' "older major moves only its own floating tags" run v0.3.0

fixture olderpatch 0.3.1 0.3.1 0.3.1
git tag v0.3.2 && git tag v0.3.1
expect $'latest=false\ntags=0.3.1\n' "older patch moves no floating tag" run v0.3.1

echo "$((n - fails))/$n passed"
(( fails == 0 ))
