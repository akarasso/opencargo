-- The backfill 007 never had, for the installs that ADOPT 007 rather than
-- apply it: their index exists and their triggers fire, but nothing ever
-- indexed the packages that predate the trigger. Same statement as 007's tail,
-- idempotent, and a no-op on a fresh install where 007 has just run it.
INSERT INTO packages_fts(packages_fts) VALUES('rebuild');
