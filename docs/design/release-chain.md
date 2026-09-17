# Release chain: signed, attested, promoted by digest

Design contract for `feat/release-chain`. Goal: every published artifact (binary, SBOM, checksums, container image) is verifiable by an RSSI with one command, against an identity
pinned to this repository and workflow file. State on `main @ af5540c`: `release.yml` never ran (gnu/macOS matrix, docker job pushes nothing, unpinned actions, `contents: write` at top level);
`ci.yml` pushes `latest` and `sha-<commit>` after Trivy, unsigned, and its last `main` run (35246625909) failed on `policy::writer::tests::gather_timeout_writes_unknown_row`, so
`sha-af5540c…` does not exist on GHCR today (checked anonymously). Every "verified" below was run locally on 2026-09-17 or read in the tool's source/docs (section 13).

## 1. Decisions

1. **Targets: x86_64 and aarch64 `*-unknown-linux-musl`, both native.** Each is built by `docker run rust:1.93.0-alpine@sha256:69d7b9d9…` (a multi-arch index: amd64, arm64/v8) on
   `ubuntu-24.04` and `ubuntu-24.04-arm`. Arm64 hosted runners are GA and free for public repositories since 2025-08-07 [1], so aarch64 needs no cross linker, no `cross`, no QEMU: the
   same Alpine toolchain the Dockerfile already proves with `aws-lc-sys` and bundled SQLite. Verified locally: `cargo build --release --locked --target x86_64-unknown-linux-musl` in that image
   as a non-root user = 2m51s, `file` says `static-pie linked`, no `INTERP` segment, `--version` prints `opencargo 0.1.0`, `/health/ready` answers `{"status":"ok"}`. Running `docker run`
   from a script instead of `container:` keeps JavaScript actions on the glibc host and makes the build reproducible locally with the same command. No macOS, no gnu: out of scope for a server.
2. **Tag = Cargo version, pre-release included.** `v0.1.0-rc.1` requires `version = "0.1.0-rc.1"` (valid Cargo semver pre-release). The binary then self-identifies (`clap`'s `version`
   prints `opencargo 0.1.0-rc.1`), the SBOM root component carries the same version, and the sha image built by main CI for that commit contains the same string. Cost: one
   `chore(release)` commit per rc (version, `Cargo.lock`, CHANGELOG heading), which is also where the CHANGELOG section is written.
3. **Sign every asset, not only SHA256SUMS.** `cosign sign-blob --bundle <asset>.sigstore.json` per file: 5 bundles. One command per artifact, no `sha256sum` indirection for someone
   who downloaded one binary. SHA256SUMS is still signed and is the subject list of the provenance attestation.
4. **Binary SBOM: `cargo-cyclonedx` 0.5.9, prebuilt, sha256-pinned.** It reads `cargo metadata`, so it honours the target, features and omits dev-dependencies [2]. Verified: with
   `--target x86_64-unknown-linux-musl --no-build-deps --spec-version 1.5 -f json` it lists 313 components (no `windows-sys`, `cc`, `rcgen`, `tempfile`). The prebuilt
   `x86_64-unknown-linux-gnu` tarball (sha256 `fb8dbee9…c6ab8b2abf9294973f012972bf6d8`) avoids a 3-minute `cargo install`; it runs on the host and needs no compiled target.
5. **Image SBOM: two CycloneDX attestations on the digest.** `syft` v1.52.0 on the current image finds 98 components, all `pkg:apk/`: no Rust crate, because the binary does not embed
   `cargo auditable` data. So the image gets the syft SBOM (Alpine layer) and the x86_64 cargo-cyclonedx SBOM (the binary inside, same commit, same `Cargo.lock`; the Dockerfile's
   builder is x86_64 musl). The Dockerfile gains `--locked` so the lock cannot drift from that SBOM. `cargo auditable` in the Dockerfile is a follow-up, not needed here.
6. **Promotion = registry-side copy by digest.** `docker buildx imagetools create --prefer-index=false -t <new> <image>@<digest>`. The default `--prefer-index=true` wraps a single
   manifest in a new index [3]; verified against a local `registry:2`: default produced a new `image.index` digest, `--prefer-index=false` kept the source digest. CI pushes a single
   `application/vnd.docker.distribution.manifest.v2+json` (checked on `sha-393242a…`). The script re-reads every new tag and fails unless its digest equals the source.
7. **Main CI signs `latest`/`sha-<commit>` by digest and attests their provenance.** Every published image is then signed, and the release verifies the source digest against identity
   `ci.yml@refs/heads/main` **and** `--certificate-github-workflow-sha <tagged commit>` before promoting it. Identity alone is not enough: every main build has it, so a `packages: write`
   holder could repoint `sha-<M>` at a digest CI signed for an older commit. The workflow-sha extension (OID 1.3.6.1.4.1.57264.1.3, flag present in `cosign verify --help` v3.1.3) binds the
   digest to the commit that is being released; a `sha-*` tag pushed by anyone else, or pointing at another commit's build, cannot be promoted. **CI signs only the digest its own push
   returned**, never a registry lookup: between Trivy and signing, a `packages: write` holder (e.g. a branch workflow in this repo) could repoint `sha-<M>`, and a registry-resolved digest
   would get main's signature for commit M without ever being scanned. The digest comes from the `<tag>: digest: sha256:… size: N` line of `docker push` (the scanned local image); the
   registry is read afterwards only as a check, and a mismatch fails the job before any signature. Never `docker image inspect`: with the containerd image store `RepoDigests` shows
   the local index digest, not the pushed manifest (verified locally). The release side may read `sha-<M>` from the registry because it only accepts a digest main CI signed for M.
8. **Attestations: `actions/attest@v4` directly.** `attest-build-provenance@v4` is now a wrapper and new users are told to use `actions/attest` [4]. Provenance, SBOM (`sbom-path`,
   predicate `https://cyclonedx.org/bom` [5]) and `push-to-registry` in one action. `create-storage-record: false`: storage records need an organization-owned repository [4], so
   `artifact-metadata: write` is not requested.
9. **cosign v3.1.3 via `sigstore/cosign-installer@v4.1.2` with `cosign-release: v3.1.3`** (fixes GHSA-fx35-mq7g-6g98); **image signatures in the classic `.sig` format.** v3 stores
   image signatures as OCI 1.1 referring artifacts by default [6][7]; GHCR has no working referrers API (checked: `404` on `/v2/…/referrers/<digest>`), so cosign falls back to the
   `sha256-<hex>` tag [8], whose index go-containerregistry writes without the bundle annotations and, on GHCR, with `artifactType` `application/vnd.oci.empty.v1+json`
   (sigstore/cosign#4641 [14]); `cosign verify` then reports "no signatures found" on GHCR [15]. The fix (ggcr v0.22.1, cosign PR #5098, merged 2026-09-05) is in no release yet
   (latest v3.1.3 = 2026-08-06). So every image signature is written with `cosign sign --yes --new-bundle-format=false --use-signing-config=false` (hidden, deprecated, accepted by v3.1.3:
   checked), which pushes `sha256-<hex>.sig` and still uploads to Rekor, and stays out of the `sha256-<hex>` index that `actions/attest` maintains. **Every image `cosign verify`
   (scripts, README, section 10) also passes `--new-bundle-format=false`.** Without it, v3.1.3 `verify` first calls `GetBundles` on the referrers fallback tag and, if it finds any
   bundle, verifies only those (`cmd/cosign/cli/verify/verify.go` @v3.1.3, `if err == nil && c.NewBundleFormat` then `VerifyImageAttestations`), with no predicate-type filter: once
   `actions/attest` has pushed a provenance or SBOM bundle signed by the same identity, plain `cosign verify` exits 0 whether or not the `.sig` exists. With the flag, only the `.sig`
   path is read, so "signed" and "attested" are checked separately (`cosign verify` and `gh attestation verify`). Verified locally against `registry:2` with a key pair: legacy sign
   creates the `.sig` tag and `cosign verify --new-bundle-format=false` exits 0. Negative test (sections 8 and 12): an image carrying an attestation bundle but no `.sig` must make it
   exit non-zero. Blob signing keeps the v3 bundle (`--bundle`, no registry). Re-evaluate both flags once a cosign release contains #5098.
10. **Re-scan at release.** The promoted digest is scanned again with the same Trivy thresholds (`HIGH,CRITICAL`, `ignore-unfixed`, exit 1). A CVE published after the main build blocks
    the release; the fix is a new commit on main and a new rc, never a rebuild in the release workflow.
11. **GitHub release through `gh` on the runner, draft then publish.** No third-party release action. Draft, upload the 10 assets, publish: the order GitHub prescribes for immutable
    releases [9]. Enabling "immutable releases" in repository settings before the first tag is recommended (locks tag and assets once published).
12. **Least privilege by job.** Top level `permissions: {}`. The job holding `id-token: write` never holds `contents: write`; the job holding `contents: write` never holds `id-token`.
    `trivy-action` is pinned by SHA to v0.36.0: its tags 0.0.1 to 0.34.2 were force-pushed with a credential stealer on 2026-03-19 [10]; `ci.yml`'s `@0.35.0` tag is replaced too.

## 2. Published outputs and identities

| Output | Signed by (certificate SAN) | Attestations |
|---|---|---|
| `opencargo-<V>-{x86_64,aarch64}-unknown-linux-musl` | `…/workflows/release.yml@refs/tags/v<V>` | SLSA provenance, CycloneDX SBOM |
| `opencargo-<V>-{x86_64,aarch64}-unknown-linux-musl.cdx.json` | same | SLSA provenance (listed in SHA256SUMS) |
| `SHA256SUMS` | same | none (it is the subject list, not a subject): cosign bundle only |
| `ghcr.io/akarasso/opencargo:<V>` (+ `X.Y`, `X` when not rc) | same, plus `ci.yml@refs/heads/main` on the digest | 2 CycloneDX (release.yml), provenance (ci.yml) |
| `ghcr.io/akarasso/opencargo:{latest,sha-<commit>}` | `…/workflows/ci.yml@refs/heads/main` | provenance (ci.yml) |

Issuer everywhere: `https://token.actions.githubusercontent.com`. Asset bundles are `<asset>.sigstore.json`. Release notes = the `## [<V>]` CHANGELOG section plus the image digest line.

## 3. `release.yml`

Triggers, concurrency and jobs (`P` = push of a tag, `PR` = dry run):

```yaml
on:
  push:
    tags: ['v[0-9]+.[0-9]+.[0-9]+', 'v[0-9]+.[0-9]+.[0-9]+-rc.[0-9]+']
  pull_request:
    branches: [main]
    paths: [Cargo.toml, Cargo.lock, rust-toolchain.toml, 'src/**', 'frontend/**', 'scripts/release/**', .github/workflows/release.yml]
permissions: {}
concurrency:
  group: release-${{ github.ref }}
  cancel-in-progress: ${{ github.event_name == 'pull_request' }}
```

| Job | Runs | needs | permissions | Steps |
|---|---|---|---|---|
| `meta` | P, PR | | `contents: read` | checkout `fetch-depth: 0`; `scripts/release/meta.sh` (section 5) -> outputs `version`, `prerelease`, `latest`, `tags`, `source_digest` |
| `frontend` | P, PR | | `contents: read` | checkout; pnpm 10; node 22; `pnpm install --frozen-lockfile && pnpm build`; upload `frontend-dist` |
| `build` (matrix: x86_64/`ubuntu-24.04`, aarch64/`ubuntu-24.04-arm`) | P, PR | meta, frontend | `contents: read` | checkout; download `frontend-dist`; `build-musl.sh`; `smoke.sh`; upload `bin-<target>` |
| `sbom` | P, PR | meta | `contents: read` | checkout; `sbom.sh <V> dist` (both targets); upload `sbom` |
| `assemble` | P, PR | meta, build, sbom | none | download `bin-*`, `sbom` merged; `checksums.sh dist`; upload `release-dist` |
| `sign` | P | meta, assemble | `id-token: write`, `attestations: write`, `contents: read` | checkout; download `release-dist`; cosign; `sign-blobs.sh dist <tag>`; attest provenance + 2 SBOM; `gh attestation verify` self-check; upload `release-bundles` |
| `image` | P | meta, assemble, sign (no tag moves unless every blob is signed and attested) | `contents: read`, `packages: write`, `id-token: write`, `attestations: write` | section 3.3 |
| `release` | P | meta, sign, image | `contents: write` | checkout; download `release-dist`, `release-bundles`; notes; `gh release create --draft`; `gh release edit --draft=false` |

Every job has `timeout-minutes`, `runs-on: ubuntu-24.04` unless stated, and `persist-credentials: false` on checkout. Pinned actions: `actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1`,
`actions/setup-node@820762786026740c76f36085b0efc47a31fe5020 # v7.0.0`, `pnpm/action-setup@ea17c68df8912ef543352723c149a84f56e3d413 # v6.1.0`,
`actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1`, `actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1`,
`sigstore/cosign-installer@6f9f17788090df1f26f669e9d70d6ae9567deba6 # v4.1.2`, `actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6 # v4.2.2`,
`aquasecurity/trivy-action@ed142fd0673e97e23eac54620cfb913e5ce36c25 # v0.36.0` with `cache: 'false'`. No cache is restored anywhere in the release workflow. This must be explicit:
trivy-action v0.36.0 defaults to `cache: 'true'`, which restores the trivy binary (`setup-trivy`, `actions/cache/restore` key `trivy-binary-<ver>-<os>-<arch>`) and the DB
(`actions/cache` key `cache-trivy-*`) (checked in both `action.yaml` at the pinned SHAs). Tag runs read default-branch caches, and main's `test` job runs pnpm/cargo/go code able to
write them: a planted binary would run next to `id-token: write` and `packages: write`. With `cache: 'false'` the binary comes from setup-trivy's install script and the DB from its registry.

### 3.1 Build leg

```yaml
  build:
    needs: [meta, frontend]
    strategy:
      matrix:
        include:
          - { target: x86_64-unknown-linux-musl, runner: ubuntu-24.04 }
          - { target: aarch64-unknown-linux-musl, runner: ubuntu-24.04-arm }
    runs-on: ${{ matrix.runner }}
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1
        with:
          name: frontend-dist
          path: frontend/dist
      - run: scripts/release/build-musl.sh "$TARGET" "$VERSION" dist
        env: { TARGET: '${{ matrix.target }}', VERSION: '${{ needs.meta.outputs.version }}' }
      - run: scripts/release/smoke.sh "dist/opencargo-$VERSION-$TARGET" "$VERSION"
        env: { TARGET: '${{ matrix.target }}', VERSION: '${{ needs.meta.outputs.version }}' }
```

### 3.2 Sign job (blobs)

```yaml
      - uses: sigstore/cosign-installer@6f9f17788090df1f26f669e9d70d6ae9567deba6 # v4.1.2
        with:
          cosign-release: v3.1.3
      - run: scripts/release/sign-blobs.sh dist "$GITHUB_REF_NAME"
      - uses: actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6 # v4.2.2
        with:
          subject-checksums: dist/SHA256SUMS
      - uses: actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6 # v4.2.2
        with:
          subject-path: dist/opencargo-${{ needs.meta.outputs.version }}-x86_64-unknown-linux-musl
          sbom-path: dist/opencargo-${{ needs.meta.outputs.version }}-x86_64-unknown-linux-musl.cdx.json
      # same step for aarch64
      - run: scripts/release/verify-assets.sh dist "$GITHUB_REF_NAME"
        env:
          GH_TOKEN: ${{ github.token }}
```

`sign-blobs.sh` runs `cosign sign-blob --yes --bundle "$f.sigstore.json" "$f"` then `cosign verify-blob` with the README identity for each of the 5 files; `set -euo pipefail` makes any
failure fatal. `verify-assets.sh` runs the README's `gh attestation verify` commands. A failed Fulcio/Rekor call or attestation upload fails the step, and `release` never runs.

### 3.3 Image job

Order: verify source, scan, sign, attest, verify everything **by digest**, and only then move tags. A failure at any step leaves no public `V`, `X.Y` or `X` tag; Rekor entries for a `sha-*`-only digest are harmless.

```yaml
    env:
      SOURCE_DIGEST: ${{ needs.meta.outputs.source_digest }}
      TAGS: ${{ needs.meta.outputs.tags }}
      GH_TOKEN: ${{ github.token }}   # docker login and `gh attestation verify` (section 9)
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: sigstore/cosign-installer@6f9f17788090df1f26f669e9d70d6ae9567deba6 # v4.1.2
        with:
          cosign-release: v3.1.3
      - run: echo "$GH_TOKEN" | docker login ghcr.io -u "$GITHUB_ACTOR" --password-stdin
      - run: scripts/release/promote-image.sh verify-source "$SOURCE_DIGEST" "$GITHUB_SHA"
      - uses: aquasecurity/trivy-action@ed142fd0673e97e23eac54620cfb913e5ce36c25 # v0.36.0
        with:
          image-ref: ghcr.io/akarasso/opencargo@${{ needs.meta.outputs.source_digest }}
          exit-code: '1'
          ignore-unfixed: true
          severity: HIGH,CRITICAL
          cache: 'false'
      - run: cosign sign --yes --new-bundle-format=false --use-signing-config=false "ghcr.io/akarasso/opencargo@$SOURCE_DIGEST"
      - uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1
        with: { name: sbom, path: sbom }
      - run: scripts/release/image-sbom.sh "$SOURCE_DIGEST" sbom/image-os.cdx.json
      - uses: actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6 # v4.2.2
        with:
          subject-name: ghcr.io/akarasso/opencargo
          subject-digest: ${{ needs.meta.outputs.source_digest }}
          sbom-path: sbom/image-os.cdx.json
          push-to-registry: true
          create-storage-record: false
      # same step with sbom/opencargo-<V>-x86_64-unknown-linux-musl.cdx.json
      - run: scripts/release/verify-image.sh "$GITHUB_REF_NAME" "$SOURCE_DIGEST" "$GITHUB_SHA"
      - run: scripts/release/promote-image.sh tag "$SOURCE_DIGEST" "$TAGS"
```

`verify-image.sh <tag> <digest> <sha>` runs, on `ghcr.io/akarasso/opencargo@<digest>`, `cosign verify --new-bundle-format=false` with the release.yml identity and
`--certificate-github-workflow-sha <sha>`, the same with the ci.yml identity, `gh attestation verify --predicate-type https://cyclonedx.org/bom --format json | jq` keeping only entries whose
`.verificationResult.signature.certificate.runInvocationURI` = `$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID/attempts/$GITHUB_RUN_ATTEMPT` (Fulcio OID …57264.1.21,
flattened into sigstore-go's `certificate.Summary`), which must be exactly 2 (component names `opencargo` and the syft image name). Attestations are append-only and gh returns one entry per
attestation: an unfiltered `length == 2` would fail forever after any re-run past the attest steps; filtered, a re-run or re-pushed tag counts only its own 2. Plus the ci.yml provenance with `--source-digest <sha>`. `promote-image.sh tag`
then re-reads each moved tag and fails unless its digest equals `<digest>`; the signatures, bound to the digest, need no re-check per tag.

`image-sbom.sh` = `docker run --rm anchore/syft:v1.52.0@sha256:500e2d872ac019436926e8322b4fc1f39441d94d21f6f4046c6ff29b30e8cb02 <image>@<digest> -q -o cyclonedx-json`
(image pinned by digest, no action). Verified: its output carries `bomFormat`, `serialNumber`, `specVersion`, which `actions/attest` requires [5].

### 3.4 Release job

`changelog-section.sh "$V" CHANGELOG.md > notes.md` (fails if the section is missing or empty), append `Container image: ghcr.io/akarasso/opencargo@<digest>`, then
first delete any draft whose `tag_name` = `$GITHUB_REF_NAME` (left by a failed attempt), then `gh release create "$GITHUB_REF_NAME" --verify-tag --draft --title "$GITHUB_REF_NAME" --notes-file notes.md <flags> dist/*`, where rc = `--prerelease --latest=false`
and final = `--latest=$LATEST` (`meta` output); keep the created release id, require exactly 10 assets on it, then publish by id with the same flags (`make_latest` is ignored on drafts and defaults to true on publish) [11]. `GH_TOKEN: ${{ github.token }}`, `contents: write` only.

## 4. `ci.yml` changes

All actions pinned by SHA with the version comment (`dtolnay/rust-toolchain@283fb51ee3c8a49cd7c8ff30102f3fcf3cfbe0e1 # 1.93.0`, `Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2`,
`actions/setup-go@b7ad1dad31e06c5925ef5d2fc7ad053ef454303e # v7.0.0`, `docker/setup-buildx-action@f87e5991a6d7451dcb8d9637bfbc97413f497069 # v4.4.1`,
`docker/login-action@dbcb813823bdd20940b903addbd779551569679f # v4.6.0`, `docker/build-push-action@c3c9e263c25d99ce0380d002d59b67737d91b0dc # v7.4.0`, and the release set). New job
`workflow-lint` (`contents: read`) runs `scripts/lint-workflows.sh` and `scripts/release/test-meta.sh` (the tag guards are tested in CI, not only locally). The `docker` job gains `attestations: write`; its "Push the scanned image" step is replaced, after Trivy, by:

```yaml
      - uses: sigstore/cosign-installer@6f9f17788090df1f26f669e9d70d6ae9567deba6 # v4.1.2
        with:
          cosign-release: v3.1.3
      - id: sign   # pushes sha-<commit> itself and signs the digest that push returned
        run: scripts/release/sign-image.sh opencargo:ci "ghcr.io/akarasso/opencargo:sha-$GITHUB_SHA"
      - uses: actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6 # v4.2.2
        with:
          subject-name: ghcr.io/akarasso/opencargo
          subject-digest: ${{ steps.sign.outputs.digest }}
          push-to-registry: true
          create-storage-record: false
      - run: scripts/release/push-latest.sh opencargo:ci ghcr.io/akarasso/opencargo:latest "$DIGEST"
        env: { DIGEST: '${{ steps.sign.outputs.digest }}' }
```

`sign-image.sh <local> <ref>`: `docker tag`, `docker push "$ref" | tee push.log`; the digest is the single match of `^<tag>: digest: (sha256:[0-9a-f]{64}) size: [0-9]+$`
(zero or several matches: exit 1). Then `imagetools inspect "$ref" --format '{{json .Manifest}}'` must return that same digest, else exit 1 before signing (check only: the value signed
is always the push digest). Then `cosign sign --yes --new-bundle-format=false --use-signing-config=false <image>@<push digest>`, `cosign verify --new-bundle-format=false` of that digest with the
`ci.yml@refs/heads/main` identity and `--certificate-github-workflow-sha "$GITHUB_SHA"`, and `digest=<push digest>` to `$GITHUB_OUTPUT`. A repoint after the check changes nothing: the
signature stays on the scanned digest and the release's `verify-source` rejects any other. `push-latest.sh` pushes `latest` only after signing and attestation, and only if `git ls-remote https://github.com/akarasso/opencargo refs/heads/main` still equals `$GITHUB_SHA`
(otherwise logs "not main's head, latest left alone" and exits 0: a re-run on an older commit, or a late job, never moves `latest` backwards); same parse, and it fails unless its push
digest equals the signed one (it never re-reads `latest` from the registry). Verified locally (Docker 29.6.2, containerd
store, `registry:2`): the parsed push digest equals the registry's `Docker-Content-Digest`; repointing the tag between push and check makes the script exit 1 with both digests.
The docker job gains `concurrency: { group: ghcr-main, cancel-in-progress: false, queue: max }`: the default `queue: single` cancels a pending job when a newer one queues [13],
leaving that commit with no `sha-<commit>` (unreleasable); `queue: max` keeps up to 100 in arrival order (a re-run still arrives late, hence the head check; the group serializes check-then-push). actionlint
v1.7.12 rejects `queue` (`parseConcurrency` knows `group`, `cancel-in-progress`): `lint-workflows.sh` ignores exactly that message. Also its Trivy step gets the v0.36.0 SHA pin and
`cache: 'false'` (section 3).

## 5. Scripts (`scripts/release/`, bash, `set -euo pipefail`, each runnable locally)

| Script | Contract |
|---|---|
| `meta.sh [tag]` | With a tag: regex `^v(0\|[1-9][0-9]*)\.(0\|[1-9][0-9]*)\.(0\|[1-9][0-9]*)(-rc\.[1-9][0-9]*)?$`; equals `v` + `Cargo.toml` `[package] version` and the `opencargo` entry of `Cargo.lock`; `git merge-base --is-ancestor HEAD origin/main` after `git fetch --no-tags origin main`; non-empty CHANGELOG section; `sha-<HEAD>` resolvable, digest emitted; `tags` = `V` for rc, else `V,X.Y,X`, dropping `X.Y`/`X` when a higher final `vX.Y.*`/`vX.*` tag exists (never moves a floating tag backwards); `latest=false` for rc or when a higher final tag exists. Without a tag (PR): version from `Cargo.toml`, lock check only. Writes `key=value` lines to `$GITHUB_OUTPUT` or stdout. |
| `test-meta.sh` | Fixture repos in `mktemp -d`: accepts `v0.1.0`, `v0.1.0-rc.1`; rejects `v0.1`, `v01.0.0`, `v0.1.0-rc.0`, `v0.1.0-beta.1`, tag/Cargo mismatch, commit off main, missing section. Image check stubbed via `IMAGE_INSPECT_CMD`. |
| `build-musl.sh <target> <V> <out>` | asserts `frontend/dist/index.html`, host arch = target arch; `docker run --user uid:gid` the pinned rust image with `CARGO_HOME`/`CARGO_TARGET_DIR` under `target/`; `cargo build --release --locked`; fails if `readelf -lW` shows `INTERP`; copies to `<out>/opencargo-<V>-<target>`. |
| `smoke.sh <bin> <V>` | `--version` = `opencargo <V>`; starts it in a `mktemp -d` cwd on `127.0.0.1:16789` with a throwaway `OPENCARGO_ADMIN_PASSWORD`; polls `/health/ready` for 60 s; body must be `{"status":"ok"}`; kills by PID in a trap. |
| `sbom.sh <V> <out>` | downloads cargo-cyclonedx 0.5.9, `sha256sum -c`; for both targets `cargo cyclonedx -f json --spec-version 1.5 --no-build-deps --target <t> --override-filename opencargo-<V>-<t>.cdx`; asserts root version = `<V>`. |
| `checksums.sh <dir>` | `sha256sum` of binaries and SBOMs, sorted by name, into `SHA256SUMS`; `sha256sum -c`. |
| `sign-blobs.sh`, `verify-assets.sh`, `sign-image.sh`, `push-latest.sh`, `promote-image.sh`, `image-sbom.sh`, `verify-image.sh` | sections 3-4; `promote-image.sh verify-source <digest> <sha>` = `cosign verify --new-bundle-format=false <image>@<digest>` with the ci.yml identity and `--certificate-github-workflow-sha <sha>`; `tag` = `imagetools create --prefer-index=false` per tag, then digest equality per tag. |
| `changelog-section.sh <V> [file]` | prints lines between `## [<V>]` and the next `## [`; exit 1 if absent or blank. |
| `verify-release.sh <V>` | section 10 as a script, for anyone to replay. |
| `../lint-workflows.sh` | `GOBIN=<tmp> go install github.com/rhysd/actionlint/cmd/actionlint@v1.7.12`; shellcheck v0.11.0 tarball (sha256 `8c3be12b…4e227198`); `actionlint -ignore 'unexpected key "queue" for "concurrency" section'` (runs shellcheck on every `run:` block when on PATH [12]) and `shellcheck scripts/*.sh scripts/release/*.sh`. |

## 6. Guards (all fail closed)

Tag shape (trigger filter [13] and regex); tag = Cargo.toml = Cargo.lock; commit is on `main`; CHANGELOG section present; `sha-<commit>` exists and carries a `.sig` signature (not just an attestation) by `ci.yml@refs/heads/main` for that exact commit (workflow-sha), and main CI signs only the digest its own push of the scanned image returned (checked against the registry, mismatch fails);
Trivy re-scan clean (no cache restored); image signed, attested and verified by digest before any tag moves; promoted tags carry the source digest; every signature and attestation
re-verified in-job with the published commands before `release` runs; GitHub release
stays a draft until all assets are uploaded. The workflow file used on a tag is the tagged commit's, so the main check protects only together with branch protection on `main` and
a tag ruleset restricting `v*` creation to maintainers (repository settings, section 11). Only `GITHUB_TOKEN` is used.

## 7. Pull request dry run

On `pull_request` touching the paths of section 3, `meta` (no tag), `frontend`, `build` x2, `sbom`, `assemble` run with `contents: read` only; `sign`, `image`, `release` carry
`if: github.event_name == 'push'` and never start, so no OIDC token is minted. It proves: both musl binaries build `--locked` and are static, `--version` prints the Cargo version, the binary boots and answers `/health/ready`, both SBOMs and `SHA256SUMS` are generated and self-check. Not a required check (a paths-filtered required check stays pending).
Gap, stated: the PR run never executes cosign, `actions/attest`, `promote-image.sh`, the verify scripts or `gh release`. Their first GHCR run is the scratch proof of section 12
(commit 4); the first run of the full `image`/`release` jobs is the `v0.1.0-rc.1` tag, where a failure leaves at most Rekor entries and a draft, never a moved tag (section 3.3).

## 8. Local proof before any push

`scripts/lint-workflows.sh` exits 0; `scripts/release/test-meta.sh` passes; `scripts/release/build-musl.sh x86_64-unknown-linux-musl 0.1.0-rc.1 dist && scripts/release/smoke.sh
dist/opencargo-0.1.0-rc.1-x86_64-unknown-linux-musl 0.1.0-rc.1`; `sbom.sh` and `checksums.sh` on `dist`; `promote-image.sh tag` against `registry:2` on `localhost:5055`
(`IMAGE=localhost:5055/tt`; ggcr rejects one-character repository names), digest equality asserted; `sign-image.sh` push-digest parse against that registry, plus a repoint of the tag
between push and check that must exit 1 (both run on 2026-09-17 with a prototype); `cosign sign --key --tlog-upload=false --new-bundle-format=false
--use-signing-config=false` on that registry creates `sha256-<hex>.sig` and `cosign verify --new-bundle-format=false --key --insecure-ignore-tlog` exits 0 (run on 2026-09-17).
Negative test, to add: a second image with only `cosign attest --key --tlog-upload=false` (bundle, no `.sig`): plain `cosign verify` exits 0, `--new-bundle-format=false` must fail. Baseline today: actionlint reports one SC2086 in the old `release.yml:110`; `scripts/toplists.sh` is shellcheck-clean.

## 9. Docs

README, new section "Verifying a release" after "Deployment" (V=0.1.0-rc.1 shown; the identity is exact per tag):

```bash
V=0.1.0-rc.1; ISS=https://token.actions.githubusercontent.com
ID=https://github.com/akarasso/opencargo/.github/workflows/release.yml@refs/tags/v$V
cosign verify-blob --bundle opencargo-$V-x86_64-unknown-linux-musl.sigstore.json \
  --certificate-identity "$ID" --certificate-oidc-issuer "$ISS" opencargo-$V-x86_64-unknown-linux-musl
gh attestation verify opencargo-$V-x86_64-unknown-linux-musl -R akarasso/opencargo --cert-identity "$ID" --cert-oidc-issuer "$ISS"
cosign verify --new-bundle-format=false ghcr.io/akarasso/opencargo:$V --certificate-identity "$ID" --certificate-oidc-issuer "$ISS"
gh attestation verify oci://ghcr.io/akarasso/opencargo:$V -R akarasso/opencargo \
  --predicate-type https://cyclonedx.org/bom --cert-identity "$ID" --cert-oidc-issuer "$ISS"
```

plus the `ci.yml@refs/heads/main` variant for `sha-<commit>` (`cosign verify --new-bundle-format=false ghcr.io/akarasso/opencargo:sha-$C … --certificate-github-workflow-sha $C`, and
`gh attestation verify … --source-digest $C`), required versions "cosign >= v3.0 (tested v3.1.3; v2 cannot read the v3 blob bundles), gh >= 2.101.0", a note that the flag is required (without it cosign v3 accepts any attestation bundle as a signature), a note that `latest` can only be checked for identity, not for a commit, and "pin by digest". `gh attestation verify` needs `GH_TOKEN` (verified: unauthenticated it stops at "gh auth login").
The Try-it one-liner keeps `latest`. SECURITY.md "Supported versions": table `0.1.x` final = supported; latest `-rc.N` = until the next rc or final; `main`/`latest` = best effort; link to the
README section; "`cargo audit` and a Trivy image scan run in CI" gains "and again on the release digest". CHANGELOG: `## [Unreleased]` (empty) above `## [0.1.0-rc.1] - <date>`, which
takes today's Unreleased content plus an `### Added` line for signed/attested releases and a `### Changed` line for signed main images.

## 10. Post-merge verification (orchestrator)

Host, after the squash merge: `M=$(git rev-parse origin/main)`; `gh run watch "$(gh run list -w ci.yml -b main -c "$M" -L1 --json databaseId -q '.[0].databaseId')" --exit-status` -> exit 0
(re-run on the known flaky test). Pre-tag gate (today: 0 rulesets, 404), stop unless `gh api repos/akarasso/opencargo/rulesets -q 'map(select(.target=="tag"))|length'` >= 1 and `gh api repos/akarasso/opencargo/immutable-releases -q .enabled` = `true`. `git tag -a v0.1.0-rc.1 -m v0.1.0-rc.1 "$M" && git push origin v0.1.0-rc.1`; `gh run watch "$(gh run list -w release.yml -e push -L1 --json databaseId -q '.[0].databaseId')" --exit-status`
-> exit 0, 9 jobs green. Then `docker run --rm -it -e GH_TOKEN="$(gh auth token)" -e M="$M" ubuntu:24.04 bash`:

```bash
apt-get update -qq && apt-get install -y -qq --no-install-recommends curl ca-certificates jq >/dev/null
curl -sSfLo /usr/local/bin/cosign https://github.com/sigstore/cosign/releases/download/v3.1.3/cosign-linux-amd64
echo "4629c757b7618056f8ddd7e2625ae9fdd94c0372a65049520bc7d9df9efc7f71  /usr/local/bin/cosign" | sha256sum -c   # OK
curl -sSfL https://github.com/cli/cli/releases/download/v2.101.0/gh_2.101.0_linux_amd64.tar.gz -o gh.tgz
echo "9bca2d1c16825f109907a23307628a2f0698fbf99662b73a5cf0b020293072b8  gh.tgz" | sha256sum -c                   # OK
tar xzf gh.tgz && install gh_2.101.0_linux_amd64/bin/gh /usr/local/bin/ && chmod +x /usr/local/bin/cosign
V=0.1.0-rc.1 R=akarasso/opencargo I=ghcr.io/akarasso/opencargo ISS=https://token.actions.githubusercontent.com
REL=https://github.com/$R/.github/workflows/release.yml@refs/tags/v$V CI=https://github.com/$R/.github/workflows/ci.yml@refs/heads/main
gh release view v$V -R $R --json isPrerelease,isDraft,assets -q '[.isPrerelease,.isDraft,(.assets|length)]'   # [true,false,10]
gh api repos/$R/compare/main...$M -q .status                                                                    # identical (or behind)
mkdir rel && cd rel && gh release download v$V -R $R && sha256sum -c SHA256SUMS                                  # 4 lines ": OK"
for f in $(awk '{print $2}' SHA256SUMS) SHA256SUMS; do cosign verify-blob --bundle $f.sigstore.json \
  --certificate-identity $REL --certificate-oidc-issuer $ISS $f; done                                           # "Verified OK" x5
for t in x86_64 aarch64; do b=opencargo-$V-$t-unknown-linux-musl
  gh attestation verify $b -R $R --cert-identity $REL --cert-oidc-issuer $ISS && \
  gh attestation verify $b -R $R --cert-identity $REL --cert-oidc-issuer $ISS --predicate-type https://cyclonedx.org/bom; done  # exit 0, silent (no TTY)
chmod +x opencargo-$V-x86_64-unknown-linux-musl && ./opencargo-$V-x86_64-unknown-linux-musl --version           # opencargo 0.1.0-rc.1
jq -r .metadata.component.version opencargo-$V-x86_64-unknown-linux-musl.cdx.json                              # 0.1.0-rc.1
cosign verify --new-bundle-format=false $I:$V --certificate-identity $REL --certificate-oidc-issuer $ISS >/dev/null                        # stderr "Verification for ghcr.io/akarasso/opencargo:0.1.0-rc.1 --"
cosign verify --new-bundle-format=false $I:$V --certificate-identity $REL --certificate-oidc-issuer $ISS --certificate-github-workflow-sha $M >/dev/null  # exit 0
cosign verify --new-bundle-format=false $I:sha-$M --certificate-identity $CI --certificate-oidc-issuer $ISS --certificate-github-workflow-sha $M >/dev/null  # same header, exit 0
gh attestation verify oci://$I:sha-$M -R $R --cert-identity $CI --cert-oidc-issuer $ISS --format json -q '.[0].verificationResult.statement.subject[0].digest.sha256'   # D
gh attestation verify oci://$I:$V -R $R --cert-identity $REL --cert-oidc-issuer $ISS --predicate-type https://cyclonedx.org/bom --format json -q '[([.[].verificationResult.statement.predicate.metadata.component.name]|unique|length),([.[].verificationResult.statement.subject[0].digest.sha256]|unique)]'  # [2,["D"]] (distinct SBOMs, not entries: a re-run adds entries)
cosign verify --new-bundle-format=false $I:0.1 --certificate-identity $REL --certificate-oidc-issuer $ISS; echo $?                         # MANIFEST_UNKNOWN, 1 (rc moves no floating tag)
cosign verify-blob --bundle SHA256SUMS.sigstore.json --certificate-identity ${REL/rc.1/rc.9} --certificate-oidc-issuer $ISS SHA256SUMS; echo $?  # identity mismatch error, 1
```

## 11. Risks

- `main` CI is red today (flaky `gather_timeout_writes_unknown_row`); no sha image, no release. `meta` fails fast naming the tag; re-run CI on `M` (safe if `M` is no longer head).
- The tag's own `release.yml` runs the guards. Without a `v*` tag ruleset and branch protection, a writer could tag an unreviewed commit whose workflow skips them; the verifier's
  `compare` step and the `ci.yml@refs/heads/main` image signature still expose it. Action for the owner: tag ruleset + immutable releases before pushing `v0.1.0-rc.1`.
- GHCR has no referrers API: image signatures live under `sha256-<hex>.sig` tags (classic format, verified with `--new-bundle-format=false`, decision 9) and attestations under the `sha256-<hex>` fallback index, both visible in
  the package UI; `cosign clean` or package deletion removes them. The classic format is deprecated in cosign v3: a future cosign that drops it needs a release with #5098 first. `gh attestation verify
  oci://` resolves the digest anonymously for a public package; if that fails in the clean container, add `--bundle-from-oci` or verify by file.
- Trivy re-scan can block a release on a new upstream CVE with no fix in Alpine yet (`ignore-unfixed` limits this). Accepted: fail closed.
- The released binary and the image binary are two builds of the same commit (same image family, same lock), not bit-identical. The image SBOM is the x86_64 lock-derived SBOM, valid
  because both use `--locked` and the same target; `cargo auditable` would make it observable, follow-up. Gap, stated in the README: neither SBOM lists the npm packages rust-embed puts in the binary (`solid-js`, `@solidjs/router`); follow-up = a pinned `syft scan file:frontend/pnpm-lock.yaml` SBOM merged or attested as a third one.
- `rustup` in the rust image re-syncs `1.93.0` + clippy/rustfmt from `rust-toolchain.toml` (network, ~10 s, seen locally). Harmless; the image digest pins the compiler.
- Action major bumps in `ci.yml` (checkout v4->v7, build-push v6->v7, setup-go v5->v7): proven by the PR's own CI run; revert to the latest SHA of the current major if one breaks. Helm `appVersion` stays `0.1.0` (chart release out of scope); SQLite backup, `.gitlab-ci.yml`/legacy helm cleanup stay deferred as decided on 2026-08-12.

## 12. Delivery plan (`feat/release-chain`, each commit green on CI)

1. `ci: pin every action by commit SHA` - `ci.yml` pins only (trivy v0.36.0 included); `Dockerfile` `--locked`.
2. `build(release): musl build, smoke and SBOM scripts` - `scripts/release/{build-musl,smoke,sbom,checksums,changelog-section,meta,test-meta}.sh`, `scripts/lint-workflows.sh`, `workflow-lint` job.
3. `ci(release): tag-driven release workflow with PR dry run` - new `release.yml` (all jobs), `sign-blobs`, `verify-assets`, `promote-image`, `image-sbom`, `verify-image`, `verify-release` scripts. The PR run is the dry run.
4. `ci: sign and attest main images by digest` - `ci.yml` docker job + `sign-image.sh`, `push-latest.sh`. Proof before merge: a temporary `workflow_dispatch` on `feat/release-chain` runs
   `sign-image.sh` (push output parsed on the hosted runner's Docker), the attest step, `promote-image.sh verify-source/tag` and `verify-image.sh` against the scratch package `ghcr.io/akarasso/opencargo-dryrun`, identity
   `release.yml@refs/heads/feat/release-chain` / `ci.yml@refs/heads/feat/release-chain` passed to the scripts through `IDENTITY_*` env; `cosign verify --new-bundle-format=false` must
   print the verification header on GHCR. Negative test: delete the `.sig` package version (`gh api -X DELETE user/packages/container/opencargo-dryrun/versions/<id>`); that command must then fail, plain `cosign verify` pass. Then the dispatch trigger and the scratch package are deleted.
5. `docs: verifying a release, supported versions` - README, SECURITY.md, this file's as-built notes.
6. `chore(release): 0.1.0-rc.1` - `Cargo.toml`/`Cargo.lock` version, CHANGELOG section. Merge (squash), wait for main CI, then section 10.

## 13. Sources

[1] github.blog/changelog/2025-08-07-arm64-hosted-runners-for-public-repositories-are-now-generally-available · [2] github.com/CycloneDX/cyclonedx-rust-cargo `cargo-cyclonedx/README.md` @0.5.9 ·
[3] docs.docker.com/reference/cli/docker/buildx/imagetools/create (`--prefer-index`) · [4] github.com/actions/attest README @v4.2.2 and actions/attest-build-provenance README ("simply a wrapper") ·
[5] actions/attest `src/sbom.ts` @v4.2.2 · [6] github.com/sigstore/cosign/releases/tag/v3.0.1 and v3.1.1, `cosign sign --help` v3.1.3 · [7] github.com/sigstore/cosign/releases/tag/v3.1.3 ·
[8] github.com/sigstore/cosign/blob/main/specs/BUNDLE_SPEC.md (referrers tag schema fallback) · [9] docs.github.com/en/code-security/supply-chain-security/understanding-your-software-supply-chain/immutable-releases ·
[10] github.com/aquasecurity/trivy/security/advisories/GHSA-69fq-xp46-6x23 · [11] `gh release create/edit --help`, `gh attestation verify --help` (gh 2.101.0) · [12] github.com/rhysd/actionlint docs/checks.md (shellcheck integration) ·
[13] docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax (filter pattern cheat sheet, `permissions`) ·
[14] github.com/sigstore/cosign/issues/4641 and pull/5098 (merged 2026-09-05, unreleased) · [15] github.com/Sp0Q1/castle/pull/68 ("no signatures found" on GHCR).

## 14. As-built notes (2026-09-17)

Commits on `feat/release-chain`: `ci: pin every action by commit SHA`, `build(release): musl build, smoke and SBOM scripts`, `ci(release): tag-driven release workflow with PR dry run`,
`ci: sign and attest main images by digest`, `docs: verifying a release, supported versions`, `chore(release): 0.1.0-rc.1`. Differences from sections 1-12:

- **Truncated hashes resolved and re-checked**: `rust:1.93.0-alpine@sha256:69d7b9d9aeaf108a1419d9a7fcf7860dcc043e9dbd1ab7ce88e44228774d99e9`, cargo-cyclonedx tarball
  `fb8dbee9f182173e062a64a387b21a0badc6fab8b2abf9294973f012972bf6d8` (equals the upstream `.sha256`), shellcheck `8c3be12b05d5c177a04c29e3c78ce89ac86f1595681cab149b65b97c4e227198`,
  cosign v3.1.3 and gh 2.101.0 as in section 10. Every action SHA of sections 3-4 equals `git ls-remote` of its tag (`dtolnay/rust-toolchain` 1.93.0 is a branch, not a tag).
- **`build-musl.sh` sets `RUSTUP_TOOLCHAIN=1.93.0`**: the image's own toolchain is used as is, so the `rust-toolchain.toml` component sync (section 11) does not happen and the
  non-root container never writes to `/usr/local/rustup`. The image already ships `musl-dev` and `gcc`. Local build 2m50s, `static-pie linked`, no `INTERP`.
- **`assemble` has `contents: read` and a checkout**, not "none": it runs `scripts/release/checksums.sh`, and scripts come from the tree.
- **Extra scripts**: `lib.sh` (image, repo, identities, `registry_digest`, `push_digest`, sourced by the signing scripts) and `publish-release.sh` (section 3.4 as a script: notes,
  leftover drafts deleted, `gh release create --draft`, exactly one draft with 10 assets, then `PATCH draft=false` with `prerelease` and `make_latest`).
- **`meta.sh` extra guards**: the tag must point at the checked-out HEAD, and HEAD must equal `$GITHUB_SHA` when set. `test-meta.sh` has 21 cases, each failure asserted on its
  message: shapes (`v0.1`, `v01.0.0`, `v0.1.00`, `v0.1.0-rc.0`, `v0.1.0-rc.01`, `v0.1.0-beta.1`, `0.1.0`, `v0.1.0-rc.1+b`), tag/Cargo, rc.2 vs Cargo rc.1, Cargo.lock, CHANGELOG,
  off main, tag not HEAD, missing image, and floating tags (higher minor, higher major, higher patch).
- **`frontend` job**: `setup-node` gets `package-manager-cache: false` (no cache restored anywhere in the release workflow).
- **Old `release.yml:110` SC2086** quoted in commit 2 so `workflow-lint` is green on every commit; the file is replaced in commit 3.
- **`lint-workflows.sh` runs `shellcheck -x`** so the sourced `lib.sh` is followed; `workflow-lint` installs Go 1.25 (actionlint v1.7.12 needs go >= 1.25.0).
- **Commit 4 GHCR scratch proof not run**: this branch is never pushed by the implementer. Replaced by local proofs against `registry:2` (below). The first hosted run of
  `sign-image.sh`/`actions/attest`/`push-latest.sh` is main CI after merge; section 10 checks its result before the tag is pushed (`cosign verify … sha-$M`).

Local proof, 2026-09-17 (Docker 29.6.2, containerd store, `registry:2` on `localhost:5055`, cosign v3.1.3):

- `scripts/lint-workflows.sh`: `workflows and scripts: clean` after each commit; without the ignore, actionlint reports exactly `unexpected key "queue" for "concurrency" section`.
- `test-meta.sh`: `21/21 passed`. `meta.sh` without a tag: `version=0.1.0`, empty `tags` and `source_digest`.
- `build-musl.sh x86_64-unknown-linux-musl 0.1.0 dist` exit 0; `smoke.sh` prints `opencargo 0.1.0` and `/health/ready {"status":"ok"}`; with a wrong version it exits 1.
  `build-musl.sh aarch64-…` on x86_64 and `x86_64-apple-darwin` exit 1.
- `sbom.sh 0.1.0 dist`: 313 (x86_64) and 312 (aarch64) components, spec 1.5, no `rcgen`/`tempfile`/`cc`/`windows-sys`. `checksums.sh dist`: every line `OK`, sorted.
- `promote-image.sh tag <digest> 0.1.0,0.1,0` (`IMAGE=localhost:5055/tt`): three tags, each re-read equal to the source digest; a malformed digest exits 1.
- `push_digest`: parsed `sha-test: digest:` equals `imagetools inspect` and the registry's `Docker-Content-Digest`. `sign-image.sh` with a `docker` wrapper repointing the tag
  right after the push: exit 1 naming both digests, cosign never called; without the repoint: `cosign sign … @<push digest>`, `cosign verify … --certificate-github-workflow-sha`,
  `digest=` in `$GITHUB_OUTPUT`. `push-latest.sh`: not main's head -> "latest left alone", exit 0; head -> pushed, exit 0; signed digest differs -> exit 1.
- Legacy image signature: `cosign sign --key --tlog-upload=false --new-bundle-format=false --use-signing-config=false` creates `sha256-<hex>.sig`; `cosign verify
  --new-bundle-format=false --key --insecure-ignore-tlog` exit 0. **Negative test done**: a second image with only `cosign attest --key` (signing config without Rekor/TSA, since
  v3.1.3 refuses `--tlog-upload=false` with a signing config): plain `cosign verify` exit 0, `cosign verify --new-bundle-format=false` exit 10 "no signatures found".
- `image-sbom.sh` on the local image (`SYFT_REGISTRY_INSECURE_USE_HTTP=true`): CycloneDX 1.7, `bomFormat`/`serialNumber`/`specVersion` present.
- `publish-release.sh` with a stub `gh`: create draft, count 10, PATCH `draft=false prerelease=… make_latest=…`; leftover draft deleted first; 11 files -> exit 1 before any call.
- Not runnable locally (need GitHub OIDC): `sign-blobs.sh`, `verify-assets.sh`, `verify-image.sh`, `promote-image.sh verify-source`, `verify-release.sh`.
