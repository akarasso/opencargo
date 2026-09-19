//! Backup, check and restore through the composition-root functions the
//! subcommands call, plus the doors the restore lock keeps shut.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use reqwest::{Client, StatusCode};

use common::{
    build_cargo_publish_body, build_crate_data, build_go_module_zip, build_npm_publish_body,
    build_tarball, hosted, push_blob, respawn, spawn_server, SpawnOpts, TestServer, STATIC_TOKEN,
};
use opencargo::backup::lock::{marker_path, restore_lock_guard, Lock};
use opencargo::backup::manifest::{DATABASE, KEYS, MANIFEST, STORAGE_DIR};
use opencargo::backup::{incomplete_snapshots, SpaceProbe, StatvfsProbe, LAST_BACKUP_AT, LAST_BACKUP_WAL};
use opencargo::config::{Config, RepositoryFormat, Visibility};
use opencargo::server::{self, BackupArgs};

fn opts() -> SpawnOpts {
    SpawnOpts {
        repositories: vec![
            hosted("npm-hosted", RepositoryFormat::Npm, Visibility::Public),
            hosted("cargo-hosted", RepositoryFormat::Cargo, Visibility::Public),
            hosted("go-hosted", RepositoryFormat::Go, Visibility::Public),
            hosted("oci-hosted", RepositoryFormat::Oci, Visibility::Public),
        ],
        ..Default::default()
    }
}

fn db_path(server: &TestServer) -> PathBuf {
    server.tmp.path().join("opencargo.db")
}

fn args(to: &Path, storage: bool) -> BackupArgs {
    BackupArgs {
        to: to.to_path_buf(),
        storage,
        force: false,
        keep: 7,
        space: Arc::new(StatvfsProbe),
    }
}

struct Free(u64);

impl SpaceProbe for Free {
    fn free_bytes(&self, _: &Path) -> std::io::Result<u64> {
        Ok(self.0)
    }
}

async fn publish_npm(base_url: &str, name: &str) -> Vec<u8> {
    let tarball = build_tarball(&format!(r#"{{"name":"{name}","version":"1.0.0"}}"#));
    let body = build_npm_publish_body(name, "1.0.0", "d", &tarball);
    let resp = Client::new()
        .put(format!("{base_url}/npm-hosted/{name}"))
        .bearer_auth(STATIC_TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tarball
}

async fn get_bytes(url: &str) -> (StatusCode, Vec<u8>) {
    let resp = Client::new().get(url).bearer_auth(STATIC_TOKEN).send().await.unwrap();
    let status = resp.status();
    (status, resp.bytes().await.unwrap().to_vec())
}

/// Every format's artifact, and the URL it is served at.
async fn seed(server: &TestServer) -> Vec<(String, Vec<u8>)> {
    let base = &server.base_url;
    let client = Client::new();
    let npm = publish_npm(base, "left-pad").await;
    let crate_data = build_crate_data();
    let meta = r#"{"name":"kept","vers":"0.1.0","deps":[],"features":{},"authors":[],"description":"d","license":"MIT"}"#;
    let resp = client
        .put(format!("{base}/cargo-hosted/api/v1/crates/new"))
        .bearer_auth(STATIC_TOKEN)
        .body(build_cargo_publish_body(meta, &crate_data))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    common::publish_go_module(&client, base, "go-hosted", "example.com/kept", "v1.0.0").await;
    let layer = b"layer that must survive".to_vec();
    let digest = push_blob(&client, base, "oci-hosted/app", &layer).await;
    vec![
        (format!("{base}/npm-hosted/left-pad/-/left-pad-1.0.0.tgz"), npm),
        (format!("{base}/cargo-hosted/api/v1/crates/kept/0.1.0/download"), crate_data),
        (
            format!("{base}/go-hosted/example.com/kept/@v/v1.0.0.zip"),
            build_go_module_zip("example.com/kept", "v1.0.0"),
        ),
        (format!("{base}/v2/oci-hosted/app/blobs/{digest}"), layer),
    ]
}

/// A lost machine: the database files gone and the store empty, whichever
/// backend holds the objects.
async fn wipe(server: &TestServer) {
    common::clear_storage(server).await;
    for suffix in ["", "-wal", "-shm"] {
        let mut name = db_path(server).into_os_string();
        name.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }
    let _ = std::fs::remove_dir_all(server.tmp.path().join("storage"));
}

async fn restore(config: &Config, from: &Path, force: bool) -> anyhow::Result<server::RestoreReport> {
    let db = server::database_path(config).unwrap();
    let lock = restore_lock_guard(&db, Lock::Exclusive(from))?;
    server::run_restore(config, from, force, lock).await
}

fn rebase(url: &str, from: &str, to: &str) -> String {
    url.replacen(from, to, 1)
}

#[tokio::test]
async fn backup_then_restore_serves_every_format_byte_for_byte() {
    let mut server = spawn_server(opts()).await;
    let artifacts = seed(&server).await;
    let config = common::config_of(&server);
    let to = server.tmp.path().join("backups");
    let snapshot = server::run_backup(&config, args(&to, true)).await.unwrap();
    assert!(snapshot.manifest.storage && snapshot.manifest.storage_keys >= 4);
    server::check_backup(&snapshot.dir).await.unwrap();

    server.stop().await;
    wipe(&server).await;
    let report = restore(&config, &snapshot.dir, false).await.unwrap();
    assert_eq!(report.objects, snapshot.manifest.storage_keys);
    assert!(!marker_path(&db_path(&server)).exists());

    let old = server.base_url.clone();
    let server = respawn(server, opts()).await;
    for (url, bytes) in artifacts {
        let (status, got) = get_bytes(&rebase(&url, &old, &server.base_url)).await;
        assert_eq!(status, StatusCode::OK, "{url}");
        assert_eq!(got, bytes, "{url}");
    }
    let verify = server::storage_verify(&common::config_of(&server), Default::default(), chrono::Utc::now()).await.unwrap();
    assert!(verify.missing.is_empty(), "{verify:?}");
}

#[tokio::test]
async fn backup_records_last_backup_at_and_truncates_the_wal() {
    let server = spawn_server(opts()).await;
    publish_npm(&server.base_url, "one").await;
    let to = server.tmp.path().join("backups");
    let snapshot = server::run_backup(&common::config_of(&server), args(&to, false)).await.unwrap();
    assert!(snapshot.wal_truncated);
    let stores = server::open_stores(&db_path(&server)).await.unwrap();
    let state = stores.server_state();
    assert!(state.get(LAST_BACKUP_AT).await.unwrap().is_some());
    assert_eq!(state.get(LAST_BACKUP_WAL).await.unwrap().as_deref(), Some("truncated"));
}

#[tokio::test]
async fn check_detects_a_tampered_db_file_and_artifact() {
    let server = spawn_server(opts()).await;
    publish_npm(&server.base_url, "tampered").await;
    let to = server.tmp.path().join("backups");
    let config = common::config_of(&server);

    let snapshot = server::run_backup(&config, args(&to, true)).await.unwrap();
    let db = snapshot.dir.join(DATABASE);
    let mut bytes = std::fs::read(&db).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&db, bytes).unwrap();
    let err = server::check_backup(&snapshot.dir).await.unwrap_err().to_string();
    assert!(err.contains("sha256"), "{err}");

    let snapshot = server::run_backup(&config, args(&to, true)).await.unwrap();
    let keys = std::fs::read_to_string(snapshot.dir.join(KEYS)).unwrap();
    let key = keys.lines().next().unwrap().splitn(3, ' ').nth(2).unwrap().to_string();
    let object = snapshot.dir.join(STORAGE_DIR).join(&key);
    let mut bytes = std::fs::read(&object).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(&object, bytes).unwrap();
    let err = server::check_backup(&snapshot.dir).await.unwrap_err().to_string();
    assert!(err.contains(&key), "the first mismatching key is named: {err}");
}

#[tokio::test]
async fn check_refuses_a_newer_manifest_version() {
    let mut server = spawn_server(opts()).await;
    let config = common::config_of(&server);
    let snapshot = server::run_backup(&config, args(&server.tmp.path().join("b"), true)).await.unwrap();
    let path = snapshot.dir.join(MANIFEST);
    let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    manifest["version"] = serde_json::json!(opencargo::backup::manifest::FORMAT_VERSION + 1);
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let err = server::check_backup(&snapshot.dir).await.unwrap_err().to_string();
    assert!(err.contains("version 2") && err.contains("version 1"), "{err}");
    server.stop().await;
    let err = restore(&config, &snapshot.dir, true).await.unwrap_err().to_string();
    assert!(err.contains("version 2"), "{err}");
}

#[tokio::test]
async fn restore_force_still_contends_with_a_live_server() {
    let server = spawn_server(opts()).await;
    let config = common::config_of(&server);
    let snapshot = server::run_backup(&config, args(&server.tmp.path().join("b"), true)).await.unwrap();
    let err = restore(&config, &snapshot.dir, true).await.unwrap_err().to_string();
    assert!(err.contains("open by a server"), "{err}");
    assert!(!marker_path(&db_path(&server)).exists(), "nothing written");
}

#[tokio::test]
async fn a_first_boot_creates_the_lock_and_never_refuses_itself() {
    let mut server = spawn_server(opts()).await;
    let lock = opencargo::backup::lock::lock_path(&db_path(&server));
    assert!(lock.exists());
    let config = common::config_of(&server);
    server.stop().await;
    let server = respawn(server, opts()).await;
    server::run_migrations(&config, false).await.unwrap();
    server::run_backup(&config, args(&server.tmp.path().join("b"), false)).await.unwrap();
}

#[tokio::test]
async fn a_server_refuses_to_start_while_a_restore_is_in_progress() {
    let mut server = spawn_server(opts()).await;
    server.stop().await;
    std::fs::write(marker_path(&db_path(&server)), "/snapshots/opencargo-x").unwrap();
    let err = common::start_error_in(&server, opts()).await;
    assert!(err.contains("restore-in-progress") && err.contains("opencargo restore --from /snapshots/opencargo-x"), "{err}");
}

#[tokio::test]
async fn backup_refuses_while_a_restore_is_in_progress() {
    let mut server = spawn_server(opts()).await;
    let config = common::config_of(&server);
    let to = server.tmp.path().join("b");
    let first = server::run_backup(&config, args(&to, true)).await.unwrap();
    server.stop().await;
    let held = restore_lock_guard(&db_path(&server), Lock::Exclusive(&first.dir)).unwrap();
    let err = server::run_backup(&config, args(&to, true)).await.unwrap_err().to_string();
    assert!(err.contains("a restore is running"), "{err}");
    drop(held);
    std::fs::write(marker_path(&db_path(&server)), first.dir.display().to_string()).unwrap();
    assert!(server::run_backup(&config, args(&to, true)).await.is_err(), "the marker refuses too");
    let dirs: Vec<_> = std::fs::read_dir(&to).unwrap().filter_map(Result::ok).filter(|e| e.path().is_dir()).collect();
    assert_eq!(dirs.len(), 1, "no new snapshot, and the good one not pruned");
}

#[tokio::test]
async fn migrate_refuses_while_a_restore_is_in_progress() {
    let mut server = spawn_server(opts()).await;
    server.stop().await;
    let db = db_path(&server);
    let _held = restore_lock_guard(&db, Lock::Exclusive(Path::new("/nowhere"))).unwrap();
    let mut wal = db.clone().into_os_string();
    wal.push("-wal");
    let _ = std::fs::remove_file(PathBuf::from(&wal));
    let config = server.tmp.path().join("c.toml");
    std::fs::write(
        &config,
        format!("[server]\nstorage_path = \"{}\"\n[database]\nurl = \"sqlite:{}\"\n", server.tmp.path().join("storage").display(), db.display()),
    )
    .unwrap();
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_opencargo"))
        .args(["--config", config.to_str().unwrap(), "migrate", "--force"])
        .env("RUST_LOG", "off")
        .output()
        .await
        .unwrap();
    assert!(!out.status.success(), "--force overrides a lease holder, never the restore lock");
    assert!(String::from_utf8_lossy(&out.stderr).contains("a restore is running"));
    assert!(!PathBuf::from(&wal).exists(), "nothing connected");
}

#[tokio::test]
async fn an_interrupted_restore_is_finished_by_re_running_it() {
    let mut server = spawn_server(opts()).await;
    let artifacts = seed(&server).await;
    let config = common::config_of(&server);
    let snapshot = server::run_backup(&config, args(&server.tmp.path().join("b"), true)).await.unwrap();
    server.stop().await;
    let _ = std::fs::remove_dir_all(server.tmp.path().join("storage"));
    std::fs::write(marker_path(&db_path(&server)), snapshot.dir.canonicalize().unwrap().display().to_string()).unwrap();

    let other = server.tmp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let err = restore(&config, &other, true).await.unwrap_err().to_string();
    assert!(err.contains("not finished"), "a different snapshot is refused: {err}");

    restore(&config, &snapshot.dir, false).await.expect("the identical command resumes");
    assert!(!marker_path(&db_path(&server)).exists());
    let old = server.base_url.clone();
    let server = respawn(server, opts()).await;
    for (url, bytes) in artifacts {
        assert_eq!(get_bytes(&rebase(&url, &old, &server.base_url)).await.1, bytes, "{url}");
    }
}

#[tokio::test]
async fn restore_over_a_dirty_wal_database_yields_the_snapshot() {
    let mut server = spawn_server(opts()).await;
    publish_npm(&server.base_url, "before").await;
    let config = common::config_of(&server);
    let snapshot = server::run_backup(&config, args(&server.tmp.path().join("b"), true)).await.unwrap();
    publish_npm(&server.base_url, "after").await;
    server.stop().await;
    restore(&config, &snapshot.dir, true).await.unwrap();
    let old = server.base_url.clone();
    let server = respawn(server, opts()).await;
    let url = |name: &str| format!("{}/npm-hosted/{name}", server.base_url);
    assert_eq!(get_bytes(&url("before")).await.0, StatusCode::OK);
    assert_eq!(get_bytes(&url("after")).await.0, StatusCode::NOT_FOUND, "no post-snapshot row: {old}");
}

#[tokio::test]
async fn restoring_a_database_only_snapshot_onto_an_empty_tree_is_refused() {
    let mut server = spawn_server(opts()).await;
    publish_npm(&server.base_url, "db-only").await;
    let config = common::config_of(&server);
    let snapshot = server::run_backup(&config, args(&server.tmp.path().join("b"), false)).await.unwrap();
    assert!(!snapshot.manifest.storage);
    server.stop().await;
    wipe(&server).await;
    let err = restore(&config, &snapshot.dir, false).await.unwrap_err().to_string();
    assert!(err.contains("storage: false"), "{err}");
    let report = restore(&config, &snapshot.dir, true).await.unwrap();
    assert_eq!(report.gate, "opencargo storage verify");
    let missing = server::storage_verify(&config, Default::default(), chrono::Utc::now()).await.unwrap().missing;
    assert!(!missing.is_empty(), "the gate is what shows the rows with no object");
}

#[tokio::test]
async fn restore_runs_with_an_invalid_backup_block() {
    let mut server = spawn_server(opts()).await;
    let config = common::config_of(&server);
    let snapshot = server::run_backup(&config, args(&server.tmp.path().join("b"), true)).await.unwrap();
    server.stop().await;
    let file = server.tmp.path().join("c.toml");
    std::fs::write(
        &file,
        format!(
            "[server]\nstorage_path = \"{}\"\n[database]\nurl = \"sqlite:{}\"\n[backup]\nenabled = true\nevery = \"7h\"\nat = \"soon\"\n",
            server.tmp.path().join("storage").display(),
            db_path(&server).display()
        ),
    )
    .unwrap();
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_opencargo"))
        .args(["--config", file.to_str().unwrap(), "restore", "--from", snapshot.dir.to_str().unwrap(), "--force"])
        .env("RUST_LOG", "off")
        .output()
        .await
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    for problem in ["enabled needs `to`", "does not divide the day", "HH:MM"] {
        assert!(stderr.contains(problem), "{problem}: {stderr}");
    }
}

#[tokio::test]
async fn scheduled_backup_refuses_when_free_space_is_below_the_estimate() {
    let server = spawn_server(opts()).await;
    seed(&server).await;
    let to = server.tmp.path().join("b");
    let err = server::run_backup(
        &common::config_of(&server),
        BackupArgs {
            space: Arc::new(Free(1024)),
            ..args(&to, true)
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("1024 are free") && err.contains("short"), "{err}");
    assert_eq!(incomplete_snapshots(&to).unwrap(), (0, 0), "nothing written");
    let snapshots = std::fs::read_dir(&to).unwrap().filter_map(Result::ok).filter(|e| e.path().is_dir()).count();
    assert_eq!(snapshots, 0);
}

#[tokio::test]
#[ignore = "needs a small filesystem mounted at OPENCARGO_SMALL_FS"]
async fn refuses_on_a_real_small_filesystem() {
    let to = PathBuf::from(std::env::var("OPENCARGO_SMALL_FS").expect("a mounted small filesystem"));
    let server = spawn_server(opts()).await;
    seed(&server).await;
    assert!(server::run_backup(&common::config_of(&server), args(&to, true)).await.is_err());
}

#[tokio::test]
async fn retention_keeps_keep_snapshots_and_reclaims_interrupted_runs() {
    let server = spawn_server(opts()).await;
    let config = common::config_of(&server);
    let to = server.tmp.path().join("b");
    for _ in 0..4 {
        server::run_backup(&config, BackupArgs { keep: 3, ..args(&to, false) }).await.unwrap();
    }
    let interrupted = to.join("opencargo-20000101T000000.000Z");
    std::fs::create_dir_all(interrupted.join(STORAGE_DIR)).unwrap();
    std::fs::write(interrupted.join(DATABASE), b"half").unwrap();
    assert_eq!(incomplete_snapshots(&to).unwrap().0, 1);
    server::run_backup(&config, BackupArgs { keep: 3, ..args(&to, false) }).await.unwrap();
    assert_eq!(incomplete_snapshots(&to).unwrap(), (0, 0), "reclaimed by the next run");
    let complete = std::fs::read_dir(&to)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().join(MANIFEST).exists())
        .count();
    assert_eq!(complete, 3);
}

#[tokio::test]
async fn two_runs_into_one_target_serialise_on_the_backup_lock() {
    let server = spawn_server(opts()).await;
    let to = server.tmp.path().join("b");
    let held = opencargo::backup::lock::backup_dir_lock(&to).unwrap();
    let err = server::run_backup(&common::config_of(&server), args(&to, false)).await.unwrap_err().to_string();
    assert!(err.contains("another backup is running"), "{err}");
    assert_eq!(std::fs::read_dir(&to).unwrap().filter_map(Result::ok).filter(|e| e.path().is_dir()).count(), 0);
    drop(held);
}

#[tokio::test]
async fn sink_objects_are_invisible_to_verify_orphans() {
    let server = spawn_server(opts()).await;
    publish_npm(&server.base_url, "synced").await;
    let mut config = common::config_of(&server);
    if common::storage_is_s3() {
        return;
    }
    let sink_root = server.tmp.path().join("dr");
    config.backup.sink = Some(opencargo::config::BackupSinkConfig {
        path: sink_root.display().to_string(),
        ..Default::default()
    });
    let snapshot = server::run_backup(&config, args(&server.tmp.path().join("b"), true)).await.unwrap();
    let name = snapshot.dir.file_name().unwrap();
    assert!(sink_root.join(name).join(MANIFEST).exists(), "the snapshot is off-box");
    let orphans = server::storage_verify(&common::config_of(&server), opencargo::app::storage_ops::Verify { orphans: true, ..Default::default() }, chrono::Utc::now() + chrono::TimeDelta::hours(3))
        .await
        .unwrap()
        .orphans;
    assert!(orphans.is_empty(), "{orphans:?}");
}

#[tokio::test]
async fn a_sink_resolving_onto_the_artifacts_is_caught_at_run_time() {
    let server = spawn_server(opts()).await;
    if common::storage_is_s3() {
        return;
    }
    let mut config = common::config_of(&server);
    let aliased = server.tmp.path().join("alias");
    std::os::unix::fs::symlink(&config.server.storage_path, &aliased).unwrap();
    config.backup.sink = Some(opencargo::config::BackupSinkConfig {
        path: aliased.display().to_string(),
        ..Default::default()
    });
    assert!(config.problems().is_empty(), "the declared paths differ");
    let err = server::run_backup(&config, args(&server.tmp.path().join("b"), false)).await.unwrap_err().to_string();
    assert!(err.contains("resolves to the artifact store"), "{err}");
}

#[tokio::test]
async fn an_unreachable_sink_fails_the_run_not_the_boot() {
    let server = spawn_server(opts()).await;
    let mut config = common::config_of(&server);
    let mut sink = opencargo::config::BackupSinkConfig::default();
    sink.storage.backend = opencargo::config::StorageKind::S3;
    sink.storage.s3.bucket = "dr".to_string();
    sink.storage.s3.endpoint = Some("http://127.0.0.1:9".to_string());
    sink.storage.s3.allow_http = true;
    sink.storage.s3.request_timeout = "2s".to_string();
    config.backup.sink = Some(sink);
    let mut boot = common::config_of(&server);
    boot.backup = config.backup.clone();
    common::build_state(&mut boot).await.expect("the sink is not built at boot");
    assert!(server::run_backup(&config, args(&server.tmp.path().join("b"), false)).await.is_err());
    let stores = server::open_stores(&db_path(&server)).await.unwrap();
    assert_eq!(stores.server_state().get(LAST_BACKUP_AT).await.unwrap(), None, "a failed run is not a backup");
}

#[tokio::test]
async fn incomplete_snapshots_counts_an_attended_run_directory() {
    let server = spawn_server(opts()).await;
    let to = server.tmp.path().join("attended");
    server::run_backup(&common::config_of(&server), args(&to, false)).await.unwrap();
    std::fs::create_dir_all(to.join("opencargo-20000101T000000.000Z")).unwrap();
    let body: serde_json::Value = Client::new()
        .get(format!("{}/api/v1/system/instance", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["incomplete_snapshots"], 1, "{body}");
    assert!(body["last_backup_at"].is_string());
    assert_eq!(body["last_backup_wal"], "truncated");
    assert!(!body.to_string().contains("attended"), "no path in the body");
}
