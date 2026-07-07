# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added
- Real-time event WebSocket at `/api/v1/events/ws` (first-frame token auth,
  server-side visibility scoping public/authenticated/admin, heartbeat,
  periodic token re-validation, `resync` marker on lag)
- `GET /api/v1/me/permissions`: effective per-repository rights for the
  caller, with the rule that produced them (`admin`/`grant`/`role`/`anonymous`)
- `whoami` now returns `role` and `must_change_password`
- Audit entries for previously silent mutations: user update, repository
  create/update/delete/purge-cache, webhook create/update/delete,
  permission set/remove
- Web UI: per-user × per-repository permission matrix editor, "My access"
  page, repository CRUD, webhook CRUD + test delivery, live audit stream,
  command palette (Cmd+K), live dashboard manifest fed by the WebSocket

### Changed
- Web UI redesigned end to end (new design system, self-hosted IBM Plex /
  Space Grotesk, inline SVG icons, skeleton loaders, mobile drawer); frontend
  split into a framework-agnostic `core/` layer (typed API client, WebSocket
  client, reactive stores) and rendering components
- `GET /api/v1/repositories` returns `type`, `format`, `visibility` and
  `upstream`, and no longer lists private repositories to anonymous callers
- Dashboard stats apply the same visibility rules as the package list
  (anonymous callers no longer see private version/download/repo counts)

### Fixed
- Production CSP silently blocked Google Fonts and Material Symbols; fonts
  are now bundled and icons inlined, so typography and iconography render
  under the strict CSP

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
