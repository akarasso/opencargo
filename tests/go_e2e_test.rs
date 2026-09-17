mod common;

use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::Ordering;

use tempfile::TempDir;

use common::upstream_tap::{self, Tap};
use common::{
    client_bin, group, hosted, proxy, publish_go_module, run_cmd, spawn_server, SpawnOpts,
    TestServer,
};
use opencargo::config::{RepositoryFormat, Visibility};

const MODULE: &str = "example.com/hello";
const VERSION: &str = "v1.0.0";

/// `go` against the group only: no `direct`, no sumdb, every cache and
/// config path under `home`, so the build proves the proxy chain alone.
async fn run_go(
    go: &str,
    args: &[&str],
    project: &Path,
    home: &Path,
    goproxy: &str,
) -> (bool, String, String) {
    let modcache = home.join("modcache");
    let gopath = home.join("gopath");
    let gocache = home.join("gocache");
    let env: [(&str, &OsStr); 9] = [
        ("HOME", home.as_os_str()),
        ("GOPROXY", OsStr::new(goproxy)),
        ("GOSUMDB", OsStr::new("off")),
        ("GOFLAGS", OsStr::new("-mod=mod")),
        ("GOTOOLCHAIN", OsStr::new("local")),
        ("GOENV", OsStr::new("off")),
        ("GOMODCACHE", modcache.as_os_str()),
        ("GOPATH", gopath.as_os_str()),
        ("GOCACHE", gocache.as_os_str()),
    ];
    run_cmd(go, args, project, &env).await
}

fn write_project(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("go.mod"),
        format!("module example.com/app\n\ngo 1.21\n\nrequire {MODULE} {VERSION}\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join("main.go"),
        format!(
            "package main\n\nimport (\n\t\"fmt\"\n\n\t\"{MODULE}\"\n)\n\nfunc main() {{\n\tfmt.Println(hello.Hello())\n}}\n"
        ),
    )
    .unwrap();
}

/// A second opencargo holding `example.com/hello`, behind a tap, and an
/// instance A whose group is `[go-local, go-proxy -> tap]`.
async fn spawn_chain() -> (TestServer, TestServer, Tap) {
    let b = spawn_server(SpawnOpts {
        repositories: vec![hosted(
            "go-hosted",
            RepositoryFormat::Go,
            Visibility::Public,
        )],
        ..Default::default()
    })
    .await;
    publish_go_module(
        &reqwest::Client::new(),
        &b.base_url,
        "go-hosted",
        MODULE,
        VERSION,
    )
    .await;
    let tap = upstream_tap::start(&b.base_url).await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("go-local", RepositoryFormat::Go, Visibility::Public),
            proxy(
                "go-proxy",
                RepositoryFormat::Go,
                &format!("{}/go-hosted", tap.base_url),
            ),
            group("go-group", RepositoryFormat::Go, &["go-local", "go-proxy"]),
        ],
        ..Default::default()
    })
    .await;
    (a, b, tap)
}

#[tokio::test]
async fn go_mod_download_and_build_through_group() {
    let Some(go) = client_bin("GO_BIN") else {
        return;
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        go_mod_download_and_build_through_group_inner(&go),
    )
    .await;
    assert!(result.is_ok(), "test timed out after 180s");
}

async fn go_mod_download_and_build_through_group_inner(go: &str) {
    let (a, _b, tap) = spawn_chain().await;
    let goproxy = format!("{}/go-group", a.base_url);
    let work = TempDir::new().unwrap();
    let project = work.path().join("app");
    write_project(&project);

    let home = work.path().join("home1");
    let (ok, _, stderr) = run_go(go, &["mod", "download"], &project, &home, &goproxy).await;
    assert!(ok, "go mod download through the group failed:\n{stderr}");
    let (ok, _, stderr) = run_go(go, &["build", "-o", "app", "."], &project, &home, &goproxy).await;
    assert!(ok, "go build through the group failed:\n{stderr}");
    let (ok, stdout, stderr) = run_cmd(
        &project.join("app").display().to_string(),
        &[],
        &project,
        &[],
    )
    .await;
    assert!(ok, "built binary failed:\n{stderr}");
    assert_eq!(stdout.trim(), "hello");

    for file in ["info", "mod", "zip"] {
        let path = format!("/go-hosted/{MODULE}/@v/{VERSION}.{file}");
        assert_eq!(
            tap.count(&path),
            1,
            "{path}: fetched from the upstream exactly once"
        );
    }
    assert!(
        tap.hits
            .lock()
            .unwrap()
            .iter()
            .all(|(_, p)| p.starts_with("/go-hosted/")),
        "every upstream request addresses the proxy member's upstream repository"
    );

    // A fresh module cache with the upstream down: A's cache carries the build.
    tap.fail.store(true, Ordering::SeqCst);
    let home = work.path().join("home2");
    std::fs::remove_file(project.join("app")).unwrap();
    let (ok, _, stderr) = run_go(go, &["build", "-o", "app", "."], &project, &home, &goproxy).await;
    assert!(
        ok,
        "go build from A's cache with the upstream down failed:\n{stderr}"
    );
    for file in ["info", "mod", "zip"] {
        let path = format!("/go-hosted/{MODULE}/@v/{VERSION}.{file}");
        assert_eq!(
            tap.count(&path),
            1,
            "{path}: served from A's cache, no new upstream hit"
        );
    }
}
