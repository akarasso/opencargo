# Security policy

opencargo sits on the critical path of your builds and holds credentials for
your developers and CI. It is maintained by one person; reports are read
within days, not weeks.

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Use GitHub's private vulnerability reporting on this repository
(*Security* tab, *Report a vulnerability*). If that is not possible, email
`a.karassouloff@gmail.com` with `[opencargo security]` in the subject.

You will get an acknowledgement within 3 business days and a fix or a
mitigation plan within 30 days for confirmed issues. Reporters are credited
in the changelog unless they prefer otherwise.

## Supported versions

Only the latest release and the `main` branch receive security fixes.
Until the first tagged release, the `latest` container image on GHCR tracks
`main`.

## Scope

In scope: anything reachable through the HTTP API, the registry protocols
(npm, Cargo, OCI, Go), the WebSocket event stream, the embedded web UI, the
container image and the Helm chart defaults.

Out of scope: vulnerabilities in packages you host or proxy (that is what
the vulnerability scanner is for), and issues that require an already
compromised admin token.

## What the codebase already does

- Passwords hashed with Argon2; API tokens stored hashed, shown once at creation.
- Rate limiting on login and token endpoints.
- Path traversal guards on every storage path (`safe_path`, covered by tests).
- Per-user, per-repository permission matrix, enforced server-side and on the
  WebSocket stream (events are scoped public / authenticated / admin).
- Static "break-glass" tokens are empty by default and documented as such.
- Weak admin passwords in config (`admin`, `changeme`) are rejected; a random
  one is generated and must be changed at first login.
- Container runs as an unprivileged user (uid 10001); only `/data` needs to
  be writable, so `readOnlyRootFilesystem` can be enabled in Kubernetes.
- `cargo audit` and a Trivy image scan run in CI.

## Hardening checklist for operators

- Terminate TLS (native `[server.tls]` or an ingress) before exposing the
  registry beyond localhost.
- Set `anonymous_read = false` unless you intentionally serve public packages.
- Provide the admin password through `OPENCARGO_ADMIN_PASSWORD` from a secret
  store rather than the config file.
- Pin the image by digest in production.
- Back up `/data` (SQLite database + storage) regularly.
