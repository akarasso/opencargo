# Changelog

All notable changes to this project will be documented in this file.

## [0.1.0] - 2026-03-23

### Added
- npm package registry (publish, install, search, dist-tags)
- Cargo crate registry (sparse protocol, publish, download, yank/unyank)
- OCI/Docker container registry (blobs, manifests, tags)
- Go module registry (GOPROXY protocol)
- Proxy repositories with transparent caching (npmjs.org, etc.)
- Group repositories (merge multiple repos behind one URL)
- Package promotion between hosted repos (dev -> prod workflow)
- Full authentication system (users, API tokens, roles)
- Secure initial admin password (random generation, file or env var)
- Rate limiting on sensitive endpoints
- Dependency graph tracking and impact analysis
- Webhooks for package events (publish, promote)
- Vulnerability scanning via OSV.dev
- Web UI (SolidJS SPA with Stitch-designed dark theme)
- Prometheus metrics endpoint
- Full-text search (SQLite FTS5)
- Automatic cleanup policies
- TLS native support (rustls)
- Health check endpoints (liveness + readiness)
- Helm chart and Kubernetes manifests
- CI sidecar mode for build caching
- Audit logging
- Docker multi-stage build
- 60+ integration tests including pnpm E2E
