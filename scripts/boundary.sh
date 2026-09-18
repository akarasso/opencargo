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
declare_row db-calls       eq    0 '\bdb::'                  plain src '*.rs' src/adapters # eq, not max: `src/db/` is gone, so there is no free-function DAL left to call and a new one would have to be invented
declare_row pool-field     eq    0 '\.db\b'                  strip src '*.rs' src/adapters # eq, not max: the OCI push half went through OciStore and AppState lost its pool field, so there is no pool left to reach one dot away. strip: `"...opencargo.db"` is a filename, not a pool
declare_row stray-sql      eq    0 'sqlx::query'             plain src '*.rs' src/adapters # eq, not max: the last sixteen were src/registry/oci/ and one src/testing/fixture.rs seed, and both went through ports. Kept beside clippy.toml's disallowed-methods, which cannot be scoped. covers _as and _scalar
declare_row pool-leak      eq    0 'SqlitePool|Pool<Sqlite>' plain src '*.rs' src/adapters # eq, not max: nothing outside the adapter holds a pool -- the fixtures open a handle set instead. Kept beside clippy.toml's disallowed-types, which lists every spelling of the alias
declare_row context-bypass eq    0 'cx\.state\b'             plain src '*.rs'                     # eq, not max: Cx holds eleven ports and borrowed values, and AppState is projected onto them in one place. Word boundary, because `cx.state` used to be passed whole
declare_row dialect-rs     eq    0 'datetime\(|julianday\(|strftime\(|AUTOINCREMENT|INSERT OR ' plain src '*.rs' src/adapters/sqlite # eq, not max: every retention predicate binds the caller's clock and src/registry/oci/'s three upserts are now statements of the SQLite adapter
declare_row dialect-sql    max  49 "AUTOINCREMENT|CHECK\(|fts5|CREATE TRIGGER|datetime\('now'\)" plain src/adapters/sqlite/migrations '*.sql' # scoped, not eliminated: SQLite DDL belongs in a SQLite directory
declare_row concrete-fs    eq    0 'FilesystemStorage'       plain src '*.rs' src/storage src/adapters/fs src/server.rs # eq, not max: the composition root builds it and nobody else names it; clippy cannot see `FilesystemStorage::new(..)` in expression position, so this row is the real check (4.1)
declare_row storage-error  eq    0 '(crate|opencargo)::error' plain 'src/storage src/adapters/fs src/adapters/s3' '*.rs' # the storage port answers with StorageError; AppError is the layer above's word (2.1)
declare_row object-store   eq    0 'object_store'            plain src '*.rs' src/adapters/s3 # S3 v5 I1: the client crate is named by its adapter and nowhere else
declare_row adapter-import max   0 '(crate|opencargo)::adapters::' plain src '*.rs' src/adapters src/server.rs src/main.rs # the composition root is those two files
declare_row storage-delete max    2 'storage\.delete(_batch)?\(' plain src '*.rs' src/app/reclaim.rs src/app/place.rs src/app/place_tests.rs src/testing src/adapters # A1 C5bis: ReclaimOrphans deletes shared keys, place_shared its drafts, private owners their own (the placement tests stand in for a reclaimer, and the adapters implement delete rather than call it); the two left are OCI segments, private to their session: a lost chunk's own, and the completion winner's
declare_row digests-none   eq    5 'ExpectedDigests::none\(\)(,|$)' plain src/registry '*.rs' # A1 C3: the strategies that verify nothing, listed -- npm (tarball and packument), go (every file), cargo (config, index, API metadata), pypi (project pages, and a file whose page announced no sha256), nuget (documents, and a .nupkg neither its registration nor its catalog leaf hashes)
declare_row maven-fencing  eq    0 '\.pin\(|\.enqueue(_prefix)?\(|\.delete(_batch)?\(' plain 'src/registry/maven src/app/maven' '*.rs' src/app/maven/tests.rs # Maven v4 decision 5: its use cases place through place_shared and release through port 18's own transactions; they never pin, enqueue or delete
# The two `src/domain/` grep rows retired at step 9: the domain is a crate of
# its own, so what it may name is a resolution error and what it may depend on
# is `check_domain_deps` below -- stronger than any pattern, because a grep
# cannot see a dependency reached through a rename or a dev-dependency.
declare_row tests-switch   eq    0 'server::build_state\('  plain tests '*.rs' tests/common/mod.rs # S3 v5 S6: every test server goes through common::build_state, the storage switch, so an S3 run is never partly on disk
declare_row sso-claims     eq    0 '\btid\b|\bhd\b|[Ee]ntra|[Gg]oogle|[Gg]it[Ll]ab|email_verified|groups_direct' plain crates/domain/src '*.rs' # SSO: the domain sees a neutral projection of an identity, never a provider's claim names
declare_row tests-raw-sql  max  41 'sqlx::query|SqlitePool'  plain tests '*.rs' tests/common/contract.rs # ratchet-only (7.4); the contract suite is the one exclusion
declare_row unit-tests     min 604 '#\[(tokio::)?test\]'     plain 'src crates/domain/src' '*.rs' # 602 -> 604: the sweeps run only for the lease holder and record last_sweep_at (2). 593 -> 602: backup: the SQLite snapshot and swap (1), the restore and target locks (3), the manifest line (1), the schedule (2), the backup config rules and sink disjointness (2). 591 -> 593: the static-token fingerprint and its re-check (2). 578 -> 591: the lease adapter (6), the config rules (5), 021 and 022 declare one server_secrets (1), the disabled lease (1); the six paused-clock lease cases escape this pattern. 577 -> 578: a NuGet key that is not token-shaped is no credential (1). 576 -> 577: a fresh database admits all seven formats together (1). 533 -> 576: feat/sso (its own history: OIDC adapter and profiles, SSO use cases, sealed cookie, 021, sso_contract!, SSO over HTTP, Dex end to end) merged. 489 -> 533: feat/nuget (its own history: rules, feed read, merge cache, spool, use cases, nuget_feed_contract!, hosted, proxy, group, dotnet) merged. 442 -> 489: feat/maven (its own history: coordinates, 024, use cases, reconciler, maven_contract!, hosted, proxy, group, decide, mvn and gradle) merged. 414 -> 442: feat/pypi merged (its 368 -> 396 history: names, versions, filenames, use cases, adapter, strategy, memo, migration 019, C6 quarantine). 408 -> 414: verify, migrate and reclaim --prefix (5), the content digest a key names (1). 402 -> 408: real-wire faults behind a relay (6, skipped without an S3 endpoint). 379 -> 402: the S3 adapter's own cases over the in-process fake (17, storage_contract! runs on it too), storage config (4), locations (2). 365 -> 379: the OCI push path onto place_shared (18 use cases, 3 layout rules and migration 018, less the eight they replace). 353 -> 365: place_shared, publish and promote over it (15), less the three they replace. 345 -> 353: ReclaimOrphans (5), migration 025, the layout rules (2). 339 -> 345: the walk stops on our faults, the misconfigured member, and four engine cases on deadlines and warm hits. 334 -> 339: storage_contract! (11, run on FS and the memory fake), key rules (2) and the FS cases (5), less the eleven filesystem cases they replace. 332 -> 334: ServerSecretStore (2). 324 -> 332: Authenticate (9) and the client source (2), less the three middleware cases it absorbed. 319 -> 324: FormatRules per format (5). 314 -> 319: expected digests (domain 2, oci 1, engine 2). 278 -> 314: the four OCI write use cases (7, orphan set/shared layer/ledger/reference refusal), the three WS envelope pins, DomainEvent's three variants and the broadcast adapter's three, the audience fan-out and its unreadable-repository case (3), the row codec's three decode cases, the name rule and the corrupt-column startup guard (3), the nine admin use cases §1.2 claims (12, repositories/users/tokens/permissions) and the dist-tag and yank orderings (3) -- less the four that went with src/db/. The scope is both crates since step 9 moved the domain's 39 cases out of `src/`. Floors: the suite may be rebalanced, not shrunk (7.5 rule 3)
declare_row integ-tests    min 533 '#\[(tokio::)?test\]'     plain tests '*.rs' # 532 -> 533: the README carries no multi-replica claim and the example config validates (1). 530 -> 532: the instance status: admin only and path-free, an attended run's leftover counted (2). 527 -> 530: the restore Job, the scoped ownership pass, the access mode (3). 507 -> 527: backup, check and restore, the restore lock at every door, retention, free space, the sink (20). 494 -> 507: the drain through TestServer::drain (8), the one real SIGTERM (1), the grace period, WS close grace mirror, shutdown env and readiness guards (4). 490 -> 494: registry tokens across a restart and a second state, a removed static token revoking its tokens (4). 476 -> 490: the writer lease at startup, through migrate and across a restart (8), the storage commands under it (2), the manifest guards (4). 475 -> 476: a placeholder NuGet key beside valid Basic credentials (1). 453 -> 475: feat/sso (its own history: OIDC adapter and profiles, SSO use cases, sealed cookie, 021, sso_contract!, SSO over HTTP, Dex end to end) merged. 418 -> 453: feat/nuget (its own history: rules, feed read, merge cache, spool, use cases, nuget_feed_contract!, hosted, proxy, group, dotnet) merged. 384 -> 418: feat/maven (its own history: coordinates, 024, use cases, reconciler, maven_contract!, hosted, proxy, group, decide, mvn and gradle) merged. 355 -> 384: feat/pypi merged (pypi_contract!, hosted, proxy and group, policy, real twine/pip/uv/poetry, 5xx never quarantined). 354 -> 355: a docker push and pull of a multipart-sized layer. 352 -> 354: the storage status route and the reclamation backlog in reclaim_contract!. 349 -> 352: the storage subcommands end to end (3). 339 -> 349: cascade_contract!'s OCI half on sessions, leases, pins and enqueues (8 more, on the fake and on SQLite), the OCI range, status and unknown-blob routes (2). 336 -> 339: M1 in reclaim_contract! (3). 322 -> 336: reclaim_contract! (14, on the fake and on SQLite). 321 -> 322: stale Bearer with real pnpm. 317 -> 321: cascade_contract! gains the manifest cascade -- the orphan set, the unknown manifest, the re-push that replaces its layers and the upload ledger -- asserted against both halves

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

# The domain's whole rule, since step 9 made it a crate: it depends on vocabulary and on nothing
# else, so nothing it names can perform I/O. `--depth 1` is the check -- the transitive closure
# (serde_derive, syn, iana-time-zone, ...) is nobody's direct choice -- and the list is an upper
# bound: a crate on it that no module uses yet is simply absent from the output.
DOMAIN_ALLOWED="bytes chrono serde serde_json thiserror"

check_domain_deps() {
  local dep n=0
  while read -r dep _; do
    case " $DOMAIN_ALLOWED " in
    *" $dep "*) n=$((n + 1)) ;;
    *)
      printf 'FAIL %-14s %s is not vocabulary: the domain depends on nothing that performs I/O\n' domain-deps "$dep"
      fail=1
      ;;
    esac
  done < <(cargo tree -p opencargo-domain --depth 1 --prefix none | tail -n +2)
  if [ "$n" -eq 0 ]; then
    printf 'FAIL %-14s cargo tree resolved no dependency: the domain crate is missing or unreadable\n' domain-deps
    fail=1
  elif [ "$fail" -eq 0 ]; then
    printf '%-4s %-14s %4d direct dependencies, all vocabulary\n' ok domain-deps "$n"
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
check_domain_deps
for row in "${rows[@]}"; do
  IFS=$'\x1f' read -r -a fields <<<"$row"
  check "${fields[@]}"
done
if [ "$fail" -ne 0 ]; then
  echo "boundary: a count grew past its max -- fix the code, or raise the max in the declare block with a reason" >&2
  exit 1
fi
echo "boundary: clean"
