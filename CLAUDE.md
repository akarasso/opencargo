# opencargo

Self-hosted universal package registry (npm, Cargo, OCI, Go). Rust + axum + SQLite, SolidJS UI embedded in the binary.

## Style

- Minimal comments. No explanatory comments on what code does; a short one only for a non-obvious *why*. No comment blocks in Dockerfiles, YAML, or configs.
- Same rule for docs: say it once, in the README or `docs/`, not inline.

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
