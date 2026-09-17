#!/usr/bin/env bash
set -euo pipefail

v=${1:?usage: changelog-section.sh <version> [file]}
file=${2:-CHANGELOG.md}

section=$(awk -v h="## [$v]" '
  index($0, h) == 1 { on = 1; next }
  on && index($0, "## [") == 1 { exit }
  on { print }
' "$file")

if [[ -z "${section//[[:space:]]/}" ]]; then
  echo "changelog-section: no non-empty '## [$v]' section in $file" >&2
  exit 1
fi
printf '%s\n' "$section" | sed -e '/./,$!d' | tac | sed -e '/./,$!d' | tac
