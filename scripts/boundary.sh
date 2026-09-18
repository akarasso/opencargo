#!/usr/bin/env bash
# The boundary ratchet of designs-next/ports-and-adapters.md section 4.2: every row counts
# OCCURRENCES of one leak pattern in one scope. A count above its max fails; a count below it
# nags, so the maxima come down with the code instead of standing still.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

rows=()
declare_row() { local IFS=$'\x1f'; rows+=("$*"); }

# declare_row <name> <max|min|eq> <bound> <pattern> <plain|strip> <roots> <glob> [excluded paths...]
# Bounds are measured occurrences at 6c9747a, never lines: `\bdb::` is 232 occurrences over 231
# lines, so a max fed by a line count licenses one free violation. A raise is an edit to this
# block, in the commit that needs it, with the reason on the line -- never a silent bump.
declare_row db-calls       max 227 '\bdb::'                  plain src '*.rs' src/db src/adapters # 232 -> 227: db::migrate and its five call sites are gone
declare_row pool-field     max 219 '\.db\b'                  strip src '*.rs' src/db src/adapters # strip: `"...opencargo.db"` is a filename, not a pool
declare_row stray-sql      max  96 'sqlx::query'             plain src '*.rs' src/db src/adapters # covers _as and _scalar
declare_row pool-leak      max  73 'SqlitePool|Pool<Sqlite>' plain src '*.rs' src/db src/adapters # 72 -> 73: server::migrate, the composition root's one migration entry point, is how the temp-DB fixtures reach the adapter without importing it
declare_row context-bypass max  25 'cx\.state\b'             plain src '*.rs'                     # word boundary: `cx.state` is also passed whole
declare_row dialect-rs     max  22 'datetime\(|julianday\(|strftime\(|AUTOINCREMENT|INSERT OR ' plain src '*.rs' src/db src/adapters/sqlite
declare_row dialect-sql    max  49 "AUTOINCREMENT|CHECK\(|fts5|CREATE TRIGGER|datetime\('now'\)" plain 'src/db/migrations src/adapters/sqlite/migrations' '*.sql' # scoped, not eliminated: SQLite DDL belongs in a SQLite directory
declare_row concrete-fs    eq    0 'FilesystemStorage'       plain src '*.rs' src/storage src/adapters/fs # eq, not max: clippy cannot see `FilesystemStorage::new(..)` in expression position, so this row is the real check (4.1)
declare_row storage-error  eq    0 '(crate|opencargo)::error' plain 'src/storage src/adapters/fs' '*.rs' # the storage port answers with StorageError; AppError is the layer above's word (2.1)
declare_row adapter-import max   0 '(crate|opencargo)::adapters::' plain src '*.rs' src/adapters src/server.rs src/main.rs # the composition root is those two files
declare_row domain-paths   max   0 '(crate|opencargo)::(error|server|db|api|registry|proxy|storage|telemetry|auth|app|adapters)\b' plain src/domain '*.rs'
declare_row domain-names   max   0 'AppError|AppResult|sqlx|axum|reqwest' plain src/domain '*.rs'
declare_row tests-raw-sql  max  41 'sqlx::query|SqlitePool'  plain tests '*.rs' tests/common/contract.rs # ratchet-only (7.4); the contract suite is the one exclusion
declare_row unit-tests     min 203 '#\[(tokio::)?test\]'     plain src '*.rs'                     # floors: the suite may be rebalanced, not shrunk (7.5 rule 3)
declare_row integ-tests    min 268 '#\[(tokio::)?test\]'     plain tests '*.rs'

# files <roots> <glob> [excluded paths...] -- the scope, one path per line
files() {
  local roots=$1 glob=$2 f ex skip
  local -a roots_arr present=()
  shift 2
  read -r -a roots_arr <<<"$roots"
  for f in "${roots_arr[@]}"; do
    if [ -e "$f" ]; then present+=("$f"); fi # a scope may not exist yet, or not any more
  done
  if [ ${#present[@]} -eq 0 ]; then return 0; fi
  find "${present[@]}" -type f -name "$glob" | sort | while read -r f; do
    skip=
    for ex in "$@"; do
      case $f in "$ex" | "$ex"/*) skip=1 ;; esac
    done
    if [ -z "$skip" ]; then printf '%s\n' "$f"; fi
  done
}

# count <pattern> <plain|strip> -- scope on stdin. grep exits 1 when it matches nothing, so every
# count swallows that: a row that has reached 0 must not kill the script under set -e.
count() {
  if [ "$2" = strip ]; then
    xargs -r -d '\n' sed 's/"[^"]*"//g' -- | { grep -oE -e "$1" || true; } | wc -l
  else
    { xargs -r -d '\n' grep -roE -e "$1" -- || true; } | wc -l
  fi
}

fail=0
check() {
  local name=$1 cmp=$2 bound=$3 pat=$4 mode=$5 roots=$6 glob=$7 n verdict=ok hint=
  shift 7
  n=$(files "$roots" "$glob" "$@" | count "$pat" "$mode")
  if { [ "$cmp" = max ] && [ "$n" -gt "$bound" ]; } || { [ "$cmp" = min ] && [ "$n" -lt "$bound" ]; } ||
    { [ "$cmp" = eq ] && [ "$n" -ne "$bound" ]; }; then
    verdict=FAIL
    fail=1
  elif [ "$n" -ne "$bound" ]; then
    verdict=nag
    hint="  <- ratchet this row to $n"
  fi
  printf '%-4s %-14s %4d occurrences (%s %s)%s\n' "$verdict" "$name" "$n" "$cmp" "$bound" "$hint"
}

# Ids are keys: a shipped migration may never be renamed, deleted or edited, or installs that
# already ran it run it again. The README beside the files records the SHA-256 of every file
# present and leaves every allocated-but-absent id checksum-less, which is legal until its file
# lands. Anything else -- a file with no row, a file under an allocated id that never recorded a
# checksum, a changed checksum -- is the failure this check exists for.
MIGRATIONS=src/adapters/sqlite/migrations

trim() {
  local v=$1
  v=${v#"${v%%[![:space:]]*}"}
  printf '%s' "${v%"${v##*[![:space:]]}"}"
}

check_migrations() {
  local readme=$MIGRATIONS/README.md id file sha actual f listed=() n=0
  if [ ! -f "$readme" ]; then
    printf 'FAIL %-14s %s is missing: every migration id is allocated there\n' migrations "$readme"
    fail=1
    return
  fi
  while IFS='|' read -r _ id file _ sha _; do
    id=$(trim "$id")
    file=$(trim "$file")
    sha=$(trim "$sha")
    if [ "$sha" = - ]; then
      if compgen -G "$MIGRATIONS/${id}_*.sql" >/dev/null; then
        printf 'FAIL %-14s %s has a file but no checksum in the README\n' migrations "$id"
        fail=1
      fi
      continue
    fi
    listed+=("$file")
    if [ ! -f "$MIGRATIONS/$file" ]; then
      printf 'FAIL %-14s %s is recorded but absent: a shipped file is never renamed or deleted\n' migrations "$file"
      fail=1
      continue
    fi
    actual=$(sha256sum "$MIGRATIONS/$file" | cut -d" " -f1)
    if [ "$actual" != "$sha" ]; then
      printf 'FAIL %-14s %s changed: take the next free id instead of editing a shipped file\n' migrations "$file"
      fail=1
      continue
    fi
    n=$((n + 1))
  done < <(grep -E "^\| *[0-9]{3} *\|" "$readme")
  for f in "$MIGRATIONS"/*.sql; do
    f=$(basename "$f")
    case " ${listed[*]-} " in
    *" $f "*) ;;
    *)
      printf 'FAIL %-14s %s is not in the README allocation table\n' migrations "$f"
      fail=1
      ;;
    esac
  done
  if [ "$fail" -eq 0 ]; then
    printf '%-4s %-14s %4d files pinned by checksum\n' ok migrations "$n"
  fi
}

# An empty scope and a pattern that matches nothing are the two ways a ratcheted row reaches 0.
self_test() {
  local mode n
  for mode in plain strip; do
    for n in "$(files src/no-such-dir '*.rs' | count '.' "$mode")" \
      "$(files src '*.rs' | count 'zzz_no_such_token' "$mode")"; do
      if [ "$n" -ne 0 ]; then
        echo "boundary: self-test failed in $mode mode ($n)" >&2
        exit 2
      fi
    done
  done
}

self_test
check_migrations
for row in "${rows[@]}"; do
  IFS=$'\x1f' read -r -a fields <<<"$row"
  check "${fields[@]}"
done
if [ "$fail" -ne 0 ]; then
  echo "boundary: a count grew past its max -- fix the code, or raise the max in the declare block with a reason" >&2
  exit 1
fi
echo "boundary: clean"
