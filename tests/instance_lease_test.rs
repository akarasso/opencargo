//! One instance per database: the writer lease at startup, through
//! `opencargo migrate`, and across a restart. Reads `sqlite_master` to prove
//! a refused instance wrote nothing.

mod common;

use std::path::Path;
use tokio::process::Command;
use std::time::{Duration, Instant};

use chrono::Utc;
use common::{respawn, spawn_server, SpawnOpts};
use opencargo::app::lease::LeaseGuard;
use opencargo::config::Config;
use opencargo::ports::leases::{Acquired, WRITER};
use opencargo::server;
use tempfile::TempDir;

fn config_in(tmp: &TempDir, lease: bool) -> Config {
    let mut config = Config::default();
    config.server.storage_path = tmp.path().join("storage").to_string_lossy().into_owned();
    config.database.url = format!("sqlite:{}?mode=rwc", db_path(tmp).display());
    config.server.lease = lease;
    common::short_lease(&mut config.server);
    config
}

fn db_path(tmp: &TempDir) -> std::path::PathBuf {
    tmp.path().join("opencargo.db")
}

async fn tables(tmp: &TempDir) -> Vec<String> {
    common::table_names(&db_path(tmp)).await
}

struct Wall;

impl opencargo::ports::clock::Clock for Wall {
    fn now(&self) -> chrono::DateTime<Utc> {
        Utc::now()
    }
}

/// A live holder that renews, as a running instance does.
async fn live(config: &Config, owner: &str) -> LeaseGuard {
    let store = server::lease_store(config).await.unwrap();
    let terms = config.server.lease_terms().unwrap().unwrap();
    LeaseGuard::take(store, std::sync::Arc::new(Wall), owner, "9.9.9", terms)
        .await
        .unwrap()
}

/// A holder that never renews: what a crashed instance leaves.
async fn hold(config: &Config, owner: &str) {
    let store = server::lease_store(config).await.unwrap();
    let taken = store
        .acquire(WRITER, owner, "9.9.9", Utc::now(), Duration::from_secs(3))
        .await
        .unwrap();
    assert!(matches!(taken, Acquired::Taken(_)));
}

fn write_config(tmp: &TempDir, name: &str, body: &str) -> std::path::PathBuf {
    let path = tmp.path().join(name);
    std::fs::write(&path, body).unwrap();
    path
}

fn toml_for(tmp: &TempDir) -> String {
    format!(
        "[server]\nstorage_path = \"{}\"\nlease_wait = \"4s\"\nlease_stale_after = \"3s\"\nlease_renew = \"1s\"\n\
         [database]\nurl = \"sqlite:{}?mode=rwc\"\n",
        tmp.path().join("storage").display(),
        db_path(tmp).display()
    )
}

async fn opencargo(args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_opencargo"))
        .args(args)
        .current_dir(cwd)
        .env_remove("OPENCARGO_CONFIG")
        .env_remove("OPENCARGO_LEASE_WAIT")
        .env("RUST_LOG", "off")
        .output()
        .await
        .expect("the built binary runs")
}

#[tokio::test]
async fn second_instance_on_one_database_refuses_to_start() {
    let tmp = TempDir::new().unwrap();
    let mut config = config_in(&tmp, true);
    let _first = live(&config, "first-instance").await;
    let started = Instant::now();
    let err = common::start(&mut config).await.err().expect("the second instance is refused");
    assert!(started.elapsed() >= Duration::from_secs(3), "it waited for the lease");
    let text = err.to_string();
    assert!(text.contains("first-instance") && text.contains("9.9.9"), "{text}");
    assert_eq!(tables(&tmp).await, vec!["server_leases".to_string()], "the loser wrote nothing");
}

#[tokio::test]
async fn second_instance_starts_after_the_first_releases() {
    let first = spawn_server(SpawnOpts { lease: true, ..Default::default() }).await;
    let mut config = common::config_of(&first);
    config.server.lease = true;
    let mut first = first;
    let releaser = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        first.stop().await;
    };
    let (second, ()) = tokio::join!(common::start(&mut config), releaser);
    let second = second.expect("taken once the first released");
    assert!(second.lease.is_some());
}

#[tokio::test]
async fn second_instance_starts_once_the_lease_goes_stale() {
    let tmp = TempDir::new().unwrap();
    let mut config = config_in(&tmp, true);
    hold(&config, "crashed").await;
    let started = common::start(&mut config).await.expect("a stale lease is taken over");
    assert_eq!(started.state.lease.status().as_str(), "held");
}

#[tokio::test]
async fn lease_disabled_allows_two_instances() {
    let tmp = TempDir::new().unwrap();
    let mut config = config_in(&tmp, false);
    let a = common::start(&mut config).await.unwrap();
    let b = common::start(&mut config).await.unwrap();
    assert!(a.lease.is_none() && b.lease.is_none());
    assert_eq!(a.state.lease.status().as_str(), "disabled");
    assert!(Config::default().server.lease, "a config that omits it takes the lease");
}

#[tokio::test]
async fn respawn_releases_the_lease() {
    let server = spawn_server(SpawnOpts { lease: true, ..Default::default() }).await;
    let started = Instant::now();
    let server = respawn(server, SpawnOpts { lease: true, ..Default::default() }).await;
    assert!(started.elapsed() < Duration::from_secs(3), "taken at once, not after going stale");
    let resp = reqwest::get(format!("{}/health/ready", server.base_url)).await.unwrap();
    assert!(resp.status().is_success());
}

#[tokio::test]
async fn migrate_subcommand_refuses_while_a_server_holds_the_lease() {
    let tmp = TempDir::new().unwrap();
    let config = config_in(&tmp, true);
    let _serving = live(&config, "serving-instance").await;
    let path = write_config(&tmp, "c.toml", &toml_for(&tmp));
    let out = opencargo(&["--config", path.to_str().unwrap(), "migrate"], tmp.path()).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("serving-instance"), "{stderr}");
    assert_eq!(tables(&tmp).await, vec!["server_leases".to_string()]);
}

#[tokio::test]
async fn migrate_on_a_fresh_database_succeeds() {
    let tmp = TempDir::new().unwrap();
    let path = write_config(&tmp, "c.toml", &toml_for(&tmp));
    let out = opencargo(&["migrate", "--config", path.to_str().unwrap()], tmp.path()).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let tables = tables(&tmp).await;
    for table in ["server_leases", "server_state", "users", "schema_migrations"] {
        assert!(tables.contains(&table.to_string()), "{table}: {tables:?}");
    }
    let config = config_in(&tmp, true);
    let store = server::lease_store(&config).await.unwrap();
    assert_eq!(store.current(WRITER).await.unwrap(), None, "migrate released its lease");
}

#[tokio::test]
async fn validate_config_subcommand_reports_every_rule() {
    let tmp = TempDir::new().unwrap();
    let ambient = write_config(&tmp, "ambient.toml", "[server]\nlease_wait = \"1s\"\n");
    let named = write_config(
        &tmp,
        "named.toml",
        "[server]\nlease_renew = \"20s\"\nshutdown_grace = \"0s\"\nendpoint_drain = \"soon\"\n[proxy]\ndefault_ttl = \"1d\"\n",
    );
    let out = opencargo(
        &["--config", ambient.to_str().unwrap(), "validate-config", named.to_str().unwrap()],
        tmp.path(),
    )
    .await;
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for rule in ["three renewals", "shutdown_grace", "endpoint_drain"] {
        assert!(stdout.contains(rule), "{rule}: {stdout}");
    }
    assert!(!stdout.contains("default_ttl"), "the proxy strings keep their fallback");
    assert!(stdout.contains("named.toml") && !stdout.contains("ambient.toml"), "{stdout}");

    let good = write_config(&tmp, "good.toml", "");
    let out = opencargo(&["validate-config", good.to_str().unwrap(), "--config", ambient.to_str().unwrap()], tmp.path()).await;
    assert!(out.status.success(), "a global flag after the subcommand: {}", String::from_utf8_lossy(&out.stderr));
}
