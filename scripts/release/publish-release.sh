#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=scripts/release/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

tag=${1:?usage: publish-release.sh <tag> <prerelease> <latest> <digest> <dir>}
prerelease=${2:?prerelease}
latest=${3:?latest}
digest=${4:?digest}
dir=${5:?dir}
expected=${EXPECTED_ASSETS:-10}
require_digest "$digest"
if [[ $prerelease == true ]]; then latest=false; fi

shopt -s nullglob
assets=("$dir"/*)
(( ${#assets[@]} == expected )) || die "expected $expected assets in $dir, found ${#assets[@]}"

notes=$(mktemp)
trap 'rm -f "$notes"' EXIT
{
  "$(dirname "${BASH_SOURCE[0]}")/changelog-section.sh" "${tag#v}" CHANGELOG.md
  printf '\nContainer image: %s@%s\n\nVerify: https://github.com/%s#verifying-a-release\n' "$IMAGE" "$digest" "$REPO"
} > "$notes"

drafts() {
  gh api --paginate "repos/$REPO/releases" -q ".[] | select(.draft and .tag_name == \"$tag\") | .id"
}
leftover=$(drafts)
for id in $leftover; do
  echo "deleting leftover draft $id"
  gh api -X DELETE "repos/$REPO/releases/$id"
done

flags=(--latest="$latest")
if [[ $prerelease == true ]]; then flags+=(--prerelease); fi
gh release create "$tag" -R "$REPO" --verify-tag --draft --title "$tag" --notes-file "$notes" "${flags[@]}" "${assets[@]}"

created=$(drafts)
mapfile -t ids <<< "$created"
(( ${#ids[@]} == 1 )) && [[ -n ${ids[0]} ]] || die "expected one draft for $tag, found ${#ids[@]}"
count=$(gh api "repos/$REPO/releases/${ids[0]}" -q '.assets | length')
(( count == expected )) || die "draft ${ids[0]} has $count assets, expected $expected"

gh api -X PATCH "repos/$REPO/releases/${ids[0]}" -F draft=false -F prerelease="$prerelease" -f make_latest="$latest" \
  -q '"published \(.html_url) prerelease=\(.prerelease) assets=\(.assets | length)"'
