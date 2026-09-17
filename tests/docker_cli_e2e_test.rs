mod common;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use reqwest::StatusCode;
use serde_json::json;

use common::{
    client_bin, create_user, group, hosted, proxy, run_cmd, spawn_server, SpawnOpts, TestServer,
    STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

const USER: &str = "docker-pusher";
const PASSWORD: &str = "docker-pass-123";
const IMAGE: &str = "team/app:1.0";

/// The docker binary, when its daemon answers; absence skips unless
/// `OPENCARGO_E2E_REQUIRE=1`.
fn docker_bin() -> Option<String> {
    let docker = client_bin("DOCKER_BIN")?;
    let up = std::process::Command::new(&docker)
        .arg("info")
        .output()
        .is_ok_and(|o| o.status.success());
    if up {
        return Some(docker);
    }
    assert!(
        std::env::var("OPENCARGO_E2E_REQUIRE").as_deref() != Ok("1"),
        "the docker daemon is required by OPENCARGO_E2E_REQUIRE=1 but does not answer"
    );
    println!("skipped: the docker daemon does not answer");
    None
}

struct Docker {
    bin: String,
    config_dir: PathBuf,
    cwd: PathBuf,
}

impl Docker {
    /// Run `docker` with an isolated credential store; panics on failure.
    async fn run(&self, args: &[&str]) -> String {
        let env = [("DOCKER_CONFIG", self.config_dir.as_os_str())];
        let (ok, stdout, stderr) = run_cmd(&self.bin, args, &self.cwd, &env).await;
        assert!(ok, "docker {} failed:\n{stdout}\n{stderr}", args.join(" "));
        stdout
    }

    async fn run_quietly(&self, args: &[&str]) {
        let env: [(&str, &OsStr); 1] = [("DOCKER_CONFIG", self.config_dir.as_os_str())];
        let _ = run_cmd(&self.bin, args, &self.cwd, &env).await;
    }

    async fn layer_id(&self, image: &str) -> String {
        self.run(&[
            "image",
            "inspect",
            "--format",
            "{{index .RootFS.Layers 0}}",
            image,
        ])
        .await
        .trim()
        .to_string()
    }
}

/// A one-file root filesystem for `docker import`: no build, no network.
fn write_rootfs_tar(dir: &Path) -> PathBuf {
    let path = dir.join("rootfs.tar");
    let mut builder = tar::Builder::new(std::fs::File::create(&path).unwrap());
    let content = b"hello from opencargo";
    let mut header = tar::Header::new_gnu();
    header.set_path("hello.txt").unwrap();
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append(&header, content.as_slice()).unwrap();
    builder.finish().unwrap();
    path
}

async fn set_password(base_url: &str, username: &str, password: &str) {
    let resp = reqwest::Client::new()
        .put(format!("{base_url}/api/v1/users/{username}/password"))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({ "new_password": password }))
        .send()
        .await
        .expect("change password request failed");
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn cache_kinds(server: &TestServer) -> Vec<String> {
    let db_path = server.tmp.path().join("opencargo.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", db_path.display()))
        .await
        .expect("failed to open the server database");
    let kinds = sqlx::query_scalar("SELECT DISTINCT kind FROM proxy_cache_entries ORDER BY kind")
        .fetch_all(&pool)
        .await
        .expect("failed to read cache rows");
    pool.close().await;
    kinds
}

#[tokio::test]
async fn docker_push_nested_then_pull_through_proxy_and_group() {
    let Some(bin) = docker_bin() else {
        return;
    };
    let b = spawn_server(SpawnOpts {
        repositories: vec![hosted(
            "oci-hosted",
            RepositoryFormat::Oci,
            Visibility::Public,
        )],
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    create_user(&client, &b.base_url, STATIC_TOKEN, USER, "publisher").await;
    set_password(&b.base_url, USER, PASSWORD).await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            proxy(
                "oci-proxy",
                RepositoryFormat::Oci,
                &format!("{}/oci-hosted", b.base_url),
            ),
            group("oci-group", RepositoryFormat::Oci, &["oci-proxy"]),
        ],
        ..Default::default()
    })
    .await;

    let work = tempfile::TempDir::new().unwrap();
    let docker = Docker {
        bin,
        config_dir: work.path().join("docker-config"),
        cwd: work.path().to_path_buf(),
    };
    std::fs::create_dir_all(&docker.config_dir).unwrap();
    let rootfs = write_rootfs_tar(work.path());
    let registry_b = format!("127.0.0.1:{}", b.port);
    let pushed = format!("{registry_b}/oci-hosted/{IMAGE}");
    let pulls = [
        format!("127.0.0.1:{}/oci-proxy/{IMAGE}", a.port),
        format!("127.0.0.1:{}/oci-group/{IMAGE}", a.port),
    ];

    docker
        .run(&["import", rootfs.to_str().unwrap(), &pushed])
        .await;
    let layer = docker.layer_id(&pushed).await;
    docker
        .run(&["login", &registry_b, "-u", USER, "-p", PASSWORD])
        .await;
    docker.run(&["push", &pushed]).await;
    docker.run(&["rmi", &pushed]).await;

    // Each pull starts from an empty local store, so the group pull really
    // fetches the layers rather than finding them "Already exists".
    for image in &pulls {
        docker.run(&["pull", image]).await;
        assert_eq!(docker.layer_id(image).await, layer, "{image}");
        docker.run(&["rmi", image]).await;
    }
    let kinds = cache_kinds(&a).await;
    for kind in ["oci-blob", "oci-manifest", "oci-tag"] {
        assert!(kinds.iter().any(|k| k == kind), "{kinds:?}");
    }
    docker.run_quietly(&["logout", &registry_b]).await;
}
