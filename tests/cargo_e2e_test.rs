#![allow(clippy::disallowed_types, clippy::disallowed_methods)]
//! SQLite-only by design: these assertions guarantee the schema, not the
//! ports (designs-next/ports-and-adapters.md 7.4).

mod common;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::StatusCode;
use serde_json::Value;
use tempfile::TempDir;

use common::{
    client_bin, group, hosted, proxy, proxy_with, run_cmd, spawn_server, ProxyOpts, SpawnOpts,
    TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryConfig, RepositoryFormat, Visibility};

const TIMEOUT: Duration = Duration::from_secs(300);

/// One isolated cargo: its own `CARGO_HOME` (credentials, index cache) and
/// target directory, so nothing leaks in from the developer's setup.
struct Cargo {
    bin: String,
    home: PathBuf,
    target: PathBuf,
}

impl Cargo {
    fn new(bin: &str, root: &Path, tag: &str) -> Self {
        let home = root.join(format!("cargo-home-{tag}"));
        let target = root.join(format!("target-{tag}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        // cargo refuses an auth-required registry without an explicit provider.
        std::fs::write(
            home.join("config.toml"),
            "[registry]\nglobal-credential-providers = [\"cargo:token\"]\n",
        )
        .unwrap();
        Self {
            bin: bin.to_string(),
            home,
            target,
        }
    }

    /// The only place a token ever lives: `$CARGO_HOME/credentials.toml`.
    fn credentials(&self, registries: &[&str]) {
        let body: String = registries
            .iter()
            .map(|r| format!("[registries.{r}]\ntoken = \"Bearer {STATIC_TOKEN}\"\n"))
            .collect();
        std::fs::write(self.home.join("credentials.toml"), body).unwrap();
    }

    async fn run(&self, args: &[&str], cwd: &Path) -> (bool, String, String) {
        let env: [(&str, &OsStr); 3] = [
            ("CARGO_HOME", self.home.as_os_str()),
            ("CARGO_TARGET_DIR", self.target.as_os_str()),
            ("CARGO_TERM_COLOR", OsStr::new("never")),
        ];
        run_cmd(&self.bin, args, cwd, &env).await
    }

    async fn publish(&self, dir: &Path, registry: &str) {
        let (ok, stdout, stderr) = self
            .run(&["publish", "--registry", registry, "--no-verify"], dir)
            .await;
        assert!(
            ok,
            "cargo publish --registry {registry} failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

fn sparse_index(server: &TestServer, repo: &str) -> String {
    format!("sparse+{}/{repo}/index/", server.base_url)
}

/// A library crate whose `.cargo/config.toml` names `registries` (index
/// URLs only, never a token) and whose deps come from `registry = "all"`.
fn write_crate(root: &Path, name: &str, deps: &[&str], registries: &[(&str, String)]) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join(".cargo")).unwrap();
    let deps: String = deps
        .iter()
        .map(|d| format!("{d} = {{ version = \"0.1.0\", registry = \"all\" }}\n"))
        .collect();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{deps}"
        ),
    )
    .unwrap();
    let uses: String = deps_of(&deps)
        .map(|d| format!("pub use {d}::NAME as {d}_name;\n"))
        .collect();
    std::fs::write(
        dir.join("src/lib.rs"),
        format!("pub const NAME: &str = \"{name}\";\n{uses}"),
    )
    .unwrap();
    let config: String = registries
        .iter()
        .map(|(r, index)| format!("[registries.{r}]\nindex = \"{index}\"\n"))
        .collect();
    std::fs::write(dir.join(".cargo/config.toml"), config).unwrap();
    dir
}

fn deps_of(deps_toml: &str) -> impl Iterator<Item = &str> {
    deps_toml
        .lines()
        .filter_map(|l| l.split_once(" = ").map(|(d, _)| d))
}

/// `(kind, cache_key)` of every cache row `server` holds.
async fn cache_keys(server: &TestServer) -> Vec<(String, String)> {
    let db_path = server.tmp.path().join("opencargo.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", db_path.display()))
        .await
        .expect("failed to open the server database");
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT kind, cache_key FROM proxy_cache_entries ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("failed to read cache rows");
    pool.close().await;
    rows
}

/// A second opencargo holding `helper 0.1.0`, published there by cargo.
async fn spawn_upstream(cargo: &Cargo, root: &Path) -> TestServer {
    let b = spawn_server(SpawnOpts {
        repositories: vec![hosted(
            "cargo-up",
            RepositoryFormat::Cargo,
            Visibility::Public,
        )],
        ..Default::default()
    })
    .await;
    let helper = write_crate(root, "helper", &[], &[("up", sparse_index(&b, "cargo-up"))]);
    cargo.publish(&helper, "up").await;
    b
}

fn proxy_to(b: &TestServer, dl_allow_private: bool) -> RepositoryConfig {
    let upstream = format!("{}/cargo-up/index", b.base_url);
    if dl_allow_private {
        proxy_with(
            "cargo-proxy",
            RepositoryFormat::Cargo,
            &upstream,
            ProxyOpts {
                dl_allow_private: true,
                ..Default::default()
            },
        )
    } else {
        proxy("cargo-proxy", RepositoryFormat::Cargo, &upstream)
    }
}

#[tokio::test]
async fn cargo_publish_then_fetch_through_group() {
    let Some(cargo) = client_bin("CARGO_BIN") else {
        return;
    };
    let result = tokio::time::timeout(TIMEOUT, publish_then_fetch_through_group(&cargo)).await;
    assert!(result.is_ok(), "test timed out after {TIMEOUT:?}");
}

async fn publish_then_fetch_through_group(bin: &str) {
    let tmp = TempDir::new().unwrap();
    let cargo = Cargo::new(bin, tmp.path(), "all");
    cargo.credentials(&["up", "hosted"]);
    let b = spawn_upstream(&cargo, tmp.path()).await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("cargo-hosted", RepositoryFormat::Cargo, Visibility::Public),
            proxy_to(&b, true),
            group(
                "cargo-all",
                RepositoryFormat::Cargo,
                &["cargo-hosted", "cargo-proxy"],
            ),
        ],
        ..Default::default()
    })
    .await;

    // greeter depends on helper, so its hosted index line carries a dependency
    // that cargo must be able to parse and resolve through the group.
    let greeter = write_crate(
        tmp.path(),
        "greeter",
        &["helper"],
        &[
            ("hosted", sparse_index(&a, "cargo-hosted")),
            ("all", sparse_index(&a, "cargo-all")),
        ],
    );
    cargo.publish(&greeter, "hosted").await;

    let consumer = write_crate(
        tmp.path(),
        "consumer",
        &["greeter", "helper"],
        &[("all", sparse_index(&a, "cargo-all"))],
    );
    let (ok, stdout, stderr) = cargo.run(&["fetch"], &consumer).await;
    assert!(
        ok,
        "cargo fetch through the group failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let (ok, stdout, stderr) = cargo.run(&["build", "--offline"], &consumer).await;
    assert!(
        ok,
        "cargo build --offline failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let keys = cache_keys(&a).await;
    assert!(
        keys.contains(&("cargo-crate".to_string(), "helper/0.1.0".to_string())),
        "helper came through the proxy member: {keys:?}"
    );
    assert!(
        !keys.contains(&("cargo-crate".to_string(), "greeter/0.1.0".to_string())),
        "greeter was downloaded from the hosted member, never proxied: {keys:?}"
    );
}

/// What cargo relies on before it holds a token: `config.json` answers a
/// tokenless request with `auth-required`, while the index stays gated.
async fn assert_anonymous_bootstrap(a: &TestServer) {
    let resp = reqwest::get(format!("{}/cargo-all/index/config.json", a.base_url))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "config.json is the anonymous bootstrap"
    );
    let config: Value = resp.json().await.unwrap();
    assert_eq!(config["auth-required"], true);
    assert_eq!(
        config["dl"],
        format!("{}/cargo-all/api/v1/crates", a.base_url)
    );
    let resp = reqwest::get(format!("{}/cargo-all/index/gr/ee/greeter", a.base_url))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "the index itself stays gated"
    );
}

#[tokio::test]
async fn cargo_fetch_private_group_with_token_only_in_cargo_home() {
    let Some(cargo) = client_bin("CARGO_BIN") else {
        return;
    };
    let result = tokio::time::timeout(TIMEOUT, fetch_private_group(&cargo)).await;
    assert!(result.is_ok(), "test timed out after {TIMEOUT:?}");
}

async fn fetch_private_group(bin: &str) {
    let tmp = TempDir::new().unwrap();
    let cargo = Cargo::new(bin, tmp.path(), "private");
    cargo.credentials(&["up", "hosted", "all"]);
    let b = spawn_upstream(&cargo, tmp.path()).await;
    let private = |cfg: RepositoryConfig| RepositoryConfig {
        visibility: Visibility::Private,
        ..cfg
    };
    let a = spawn_server(SpawnOpts {
        anonymous_read: false,
        repositories: vec![
            hosted("cargo-hosted", RepositoryFormat::Cargo, Visibility::Private),
            private(proxy_to(&b, true)),
            private(group(
                "cargo-all",
                RepositoryFormat::Cargo,
                &["cargo-hosted", "cargo-proxy"],
            )),
        ],
        ..Default::default()
    })
    .await;

    let greeter = write_crate(
        tmp.path(),
        "greeter",
        &[],
        &[("hosted", sparse_index(&a, "cargo-hosted"))],
    );
    cargo.publish(&greeter, "hosted").await;
    assert_anonymous_bootstrap(&a).await;

    let consumer = write_crate(
        tmp.path(),
        "consumer",
        &["greeter", "helper"],
        &[("all", sparse_index(&a, "cargo-all"))],
    );
    let (ok, stdout, stderr) = cargo.run(&["fetch"], &consumer).await;
    assert!(
        ok,
        "cargo fetch through the private group failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let (ok, stdout, stderr) = cargo.run(&["build", "--offline"], &consumer).await;
    assert!(
        ok,
        "cargo build --offline failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let anonymous = Cargo::new(bin, tmp.path(), "anonymous");
    let (ok, stdout, stderr) = anonymous.run(&["fetch"], &consumer).await;
    assert!(
        !ok,
        "without credentials the private group must refuse.\nstdout:\n{stdout}"
    );
    assert!(
        stderr.contains("token"),
        "cargo names the missing token: {stderr}"
    );
}

#[tokio::test]
async fn cargo_fetch_refused_when_dl_points_at_private_literal_without_optin() {
    let Some(cargo) = client_bin("CARGO_BIN") else {
        return;
    };
    let result = tokio::time::timeout(TIMEOUT, fetch_refused_without_optin(&cargo)).await;
    assert!(result.is_ok(), "test timed out after {TIMEOUT:?}");
}

async fn fetch_refused_without_optin(bin: &str) {
    let tmp = TempDir::new().unwrap();
    let cargo = Cargo::new(bin, tmp.path(), "strict");
    cargo.credentials(&["up"]);
    let b = spawn_upstream(&cargo, tmp.path()).await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            proxy_to(&b, false),
            group("cargo-all", RepositoryFormat::Cargo, &["cargo-proxy"]),
        ],
        ..Default::default()
    })
    .await;

    let consumer = write_crate(
        tmp.path(),
        "consumer",
        &["helper"],
        &[("all", sparse_index(&a, "cargo-all"))],
    );
    let (ok, stdout, stderr) = cargo.run(&["fetch"], &consumer).await;
    assert!(
        !ok,
        "a dl on 127.0.0.1 is refused without dl_allow_private.\nstdout:\n{stdout}"
    );
    assert!(stderr.contains("502"), "cargo surfaces the 502: {stderr}");

    let resp = reqwest::get(format!(
        "{}/cargo-all/api/v1/crates/helper/0.1.0/download",
        a.base_url
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let keys = cache_keys(&a).await;
    assert!(
        !keys.iter().any(|(kind, _)| kind == "cargo-crate"),
        "nothing was downloaded or stored: {keys:?}"
    );
}
