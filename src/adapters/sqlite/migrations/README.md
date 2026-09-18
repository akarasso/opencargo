# SQLite migrations

Every file here is applied at most once, in id order, by
`src/adapters/sqlite/migrate.rs`. A database that predates the version table is
**probed**, not assumed: each id whose sentinel object already exists is
recorded as applied, each id whose sentinel does not is executed.

**Ids are keys, so a shipped file is immutable.** Renaming, renumbering or
deleting one makes installs that already ran it run it again — a second strict
`ALTER TABLE` and a boot failure — and editing one in place reaches only the
installs that had not yet run it, silently. The only legal way to change a
shipped file's effect is to take the next free id. `scripts/boundary.sh` pins
every present file by SHA-256 against the table below and fails CI otherwise.

An allocated id with no file yet is legal: the table is the allocation for
every design in `designs-next/`, made once so that two of them cannot claim the
same number. A file that appears must take its allocated id and record its
checksum in the same commit.

| id | file | owner | sha256 |
|---|---|---|---|
| 001 | 001_initial.sql | shipped before the migrator | ac2be605c7aefeba5de4ee58a85254cf3302c8cdff45e1aff3c18052f358ea67 |
| 002 | 002_proxy_cache.sql | shipped before the migrator | b94d0bae2bd9b2447aea848ac8176c8b993af584913139fb0e01f89e08ef4d18 |
| 003 | 003_auth.sql | shipped before the migrator | 43c8b6c292ffc1f9368c955a50b481cea9115d1b98ccd220d8eaca25e7454191 |
| 004 | 004_cargo.sql | shipped before the migrator | 273c61e2a7077f29a08f6325f9a69b9ca24f66555c04d47bffd776fd52fa24e8 |
| 005 | 005_must_change_password.sql | shipped before the migrator | 92f283ba98ce4e2ba1b128ba941993ca4d7233373bbd570e3172317eb99a926e |
| 006 | 006_oci.sql | shipped before the migrator | 27264d5a2b9ae1fd37f06ff1d97ab795cb7f6ae0135b871e624ac96b3a4541aa |
| 007 | 007_fts5.sql | shipped before the migrator | b425b2841b151af4fabff407bfe7cdd59692d8ec779ea462eff2836d3ddd7cef |
| 008 | 008_deps.sql | shipped before the migrator | 3c6656cbf5b36ef6c7f24521d6dc28753b727a86da8b11304d8f9deb838e633e |
| 009 | 009_vulns.sql | shipped before the migrator | 18bbabef00aecc399c820608ea30db0224581ab4af70ddfa35566c173e0a9aca |
| 010 | 010_dynamic_config.sql | shipped before the migrator | a07c49c8016cfbbbcda35630bc002e5be9413f959a1019078f7541fd10b486aa |
| 011 | 011_download_counts.sql | shipped before the migrator | 0633205d67f496af18ece8d4976f87cb94bc2acf2ab39fb4138b32ce7b3ebd5c |
| 012 | 012_oci_manifest_blobs.sql | shipped before the migrator | a46b247fc05121fbe358bab7fbb5d29e77776839813613e1801d5da0eb81eab0 |
| 013 | 013_proxy_cache_entries.sql | shipped before the migrator | 651c9452a8b87c6303e0db902bbdea7db702d35765691ec2d4f56723e2a00414 |
| 014 | 014_policy.sql | shipped before the migrator | e434fd9b03944f09f108e5b5325dd994834c9e77c1dccafcdaf784cbd169d0aa |
| 015 | 015_fts_rebuild.sql | ports-and-adapters.md 8a | b90a86c09e6b6c263cd75735e2ebe253eb1d15a73266be08bb5b9fd7db18aaf5 |
| 016 | - | (free, slack) | - |
| 017 | 017_storage_multipart.sql | s3.md (storage_multipart) | d33bf7772baee4f379535f7983837a5c56e0f707211e3e4cbf00b359efe0f6d2 |
| 018 | 018_oci_upload_progress.sql | s3.md S4 (oci_upload_progress, physical keys on OCI rows) | de0ec789fca377873106f27a2b88af5517e797d28dd38aaf7840277371bba44a |
| 019 | 019_pypi.sql | pypi.md (pypi_files) | d301499e72c72ddbb460fee30d6c39dbf0b4cbdf2e7a609ba2ee0b0d91f5108e |
| 020 | - | nuget.md (nuget_format: a `Step::Rust` through the shared `rebuild::widen_formats`, no file) | - |
| 021 | 021_sso.sql | sso.md (sso, server_secrets) | 56921514f38896bf6ac419d884a164839bc5521afa1bca372fdcf8376185d62c |
| 022 | 022_server_leases.sql | ha-options.md (server_leases, server_state; server_secrets when 021 has not run) | 5a9163cb82081825640cf537a25dcf872b17edfdd140728cbd18029ff1ee0a67 |
| 023 | 023_mcp.sql | mcp-governance.md (mcp format through the shared rebuild helper `rebuild::widen_formats`, governance tables) | af1ab372ba05dcfb2207e210759a835c4a66870e90ec683a454c19d3e6482eb3 |
| 024 | 024_maven.sql | maven.md (maven format through the shared rebuild helper `rebuild::widen_formats`, port 18 tables) | 58ca2fdd771fe0dc453d1c21499855df9f303d520f85451b7b508c907170b95d |
| 025 | 025_reclaim.sql | s3.md S2r (reclamation, incarnations, retired prefixes) | aec372d3addae7af66c944632d4296f5041b659c5971a5f97569eef8aa8f2654 |
| 026 | 026_reclaim_epoch.sql | ha-profiles C-1/C-9 (restore epoch, installation identifier, high-water counter) | 109b9ff6860ee44be9d9528a6d99613b735675f0a6450fa03ef92b08e2fd30c9 |

A `Step::Rust` migration has no file and so no checksum: `020` is the
`repositories` CHECK widened by `migrate::widen_format_check`, the one helper
`023` and `024` reuse (existing formats plus one, never a literal list).

Ids from 018 on are order-independent: none reads or alters a table another
of them creates, and `025` ships before `018`-`024`; `026` reads none of them. The Postgres side of
this directory is `src/adapters/postgres/migrations/`, and it is empty.
