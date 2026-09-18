# opencargo

Self-hosted universal package registry (npm, Cargo, OCI, Go). Rust + axum + SQLite, SolidJS UI embedded in the binary.

## Style

- Minimal comments. No explanatory comments on what code does; a short one only for a non-obvious *why*. No comment blocks in Dockerfiles, YAML, or configs.
- Same rule for docs: say it once, in the README or `docs/`, not inline.

## Working method

Designs and large changes go through this loop; it is the default, not a ceremony to skip.

1. **Design at architecture level only**: decisions, interfaces, invariants, risks, the test matrix. No SQL columns, no test bodies, no bound arithmetic. Implementation detail belongs to the code loop, where a test settles it.
2. **Refutation cycles on a pinned tree**: the refuter reads a worktree fixed at a named commit, never a moving `main`; sibling designs are snapshotted; nothing edits a document while it is being read; work in progress elsewhere is out of scope and must not be objected to.
3. **Arbitration every five rounds**, by an agent on a different model (Fable when the loop runs on Opus): it verifies a sample of the open objections instead of trusting their severity label, classifies each one (real blocker, real major, unproven, out of scope, detail), and decides between refining further and freezing the design with the remainder converted into a review checklist. A design that cannot be settled on paper is frozen, not refuted again.
4. **Implementation in reviewed steps**: one commit per step, a reviewer agent per commit with the arbitration checklist in hand, the suite green at every step.
5. **Execution proof**: full suite, new suites run under full CPU load before proposing a merge, real clients end to end, and validation from a second machine before anything is claimed in the README.

Rule of thumb: refutation removes design errors, tests remove reality errors, and neither substitutes for the other. A refutation loop that stops converging is a signal to switch to code, not to add rounds.

## Layout

- `src/registry/{npm,cargo,oci,go}` protocol handlers, `src/proxy` upstream cache, `src/auth` (Argon2, hashed tokens, permission matrix, rate limit), `src/api` admin REST + WS, `src/web` SPA serving.
- `frontend/` SolidJS, `core/` is framework-agnostic (API client, WS client, stores).
- `tests/` integration tests over HTTP; `pnpm_e2e_test.rs` drives a real `pnpm` binary, the Docker E2E does not use a real client.
- `k8s/` Kustomize (`base/`, `scaleway/`, `sidecar/`), `helm/opencargo` chart.

## Commands

- `make dev`, `make test-quick` (no network), `make test`, `make check` (clippy + tests).
- Image: `ghcr.io/akarasso/opencargo:latest`, built by CI on push to `main`. Rollout to k8s is manual.

## Private files

`plan-produit.md` and `plan-lancement.md` are business documents, gitignored, never commit or publish them. Real deployment values (hostnames, gateway, kubeconfig) live in `~/workspaces/perso/opencargo-deploy/`, outside this repository; `k8s/` only carries generic examples.
