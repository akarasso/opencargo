# opencargo

Self-hosted universal package registry (npm, Cargo, OCI, Go). Rust + axum + SQLite, SolidJS UI embedded in the binary.

## Style

- Minimal comments. No explanatory comments on what code does; a short one only for a non-obvious *why*. No comment blocks in Dockerfiles, YAML, or configs.
- Same rule for docs: say it once, in the README or `docs/`, not inline.

## Layout

- `src/registry/{npm,cargo,oci,go}` protocol handlers, `src/proxy` upstream cache, `src/auth` (Argon2, hashed tokens, permission matrix, rate limit), `src/api` admin REST + WS, `src/web` SPA serving.
- `frontend/` SolidJS, `core/` is framework-agnostic (API client, WS client, stores).
- `tests/` integration tests, real `pnpm` and `docker` clients for E2E.
- `k8s/` Kustomize (`base/`, `scaleway/`, `sidecar/`), `helm/opencargo` chart.

## Commands

- `make dev`, `make test-quick` (no network), `make test`, `make check` (clippy + tests).
- Image: `ghcr.io/akarasso/opencargo:latest`, built by CI on push to `main`. Rollout to k8s is manual.

## Private files

`plan-produit.md` and `plan-lancement.md` are business documents, gitignored, never commit or publish them.
