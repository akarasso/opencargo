# Postgres migrations

Deliberately empty. The ports exist; a Postgres adapter does not, and will not
until a trigger fires (`designs-next/ha-options.md`, option 4). This directory
is what makes "Postgres is reachable" a statement about missing files rather
than about a missing plan: the same ids as
`src/adapters/sqlite/migrations/README.md`, written in the other dialect.

The SQLite files next door are SQLite-specific on purpose — `AUTOINCREMENT`,
`CHECK(.. IN (..))`, the FTS5 virtual table and its three triggers — and
`scripts/boundary.sh` keeps that dialect scoped to that directory rather than
pretending it will shrink.
