//! SSO end to end against a pinned Dex (mock connector) run by docker:
//! discovery, PKCE, the handoff, then real clients with the token it
//! issued -- `cargo publish`, and `docker login`/`push`/`pull`. Skips
//! cleanly when docker (or its daemon) or cargo is absent.

mod common;

use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, Instant};

use common::{client_bin, hosted, run_cmd, spawn_server, SpawnOpts, TestServer};
use opencargo::config::{RepositoryFormat, SsoConfig, SsoProviderConfig, Visibility};
use reqwest::{redirect, Client, StatusCode};
use serde_json::{json, Value};

const DEX_IMAGE: &str = "ghcr.io/dexidp/dex:v2.41.1";
const TIMEOUT: Duration = Duration::from_secs(300);

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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A Dex container on the host network; removed on drop.
struct Dex {
    docker: String,
    name: String,
    issuer: String,
}

impl Drop for Dex {
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.docker)
            .args(["rm", "-f", &self.name])
            .output();
    }
}

async fn start_dex(docker: &str, work: &Path, port: u16, redirect: &str) -> Dex {
    let issuer = format!("http://127.0.0.1:{port}/dex");
    let config = format!(
        "issuer: {issuer}\nstorage:\n  type: memory\nweb:\n  http: 127.0.0.1:{port}\n\
         oauth2:\n  skipApprovalScreen: true\n  responseTypes: [\"code\"]\n\
         staticClients:\n- id: opencargo\n  secret: s3cret\n  name: opencargo\n  redirectURIs:\n  - '{redirect}'\n\
         connectors:\n- type: mockCallback\n  id: mock\n  name: Mock\n\
         enablePasswordDB: false\n"
    );
    let dir = work.join("dex");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), config).unwrap();
    let name = format!("opencargo-dex-{port}");
    let mount = format!("{}:/cfg:ro", dir.display());
    let (ok, _, stderr) = run_cmd(
        docker,
        &[
            "run",
            "-d",
            "--rm",
            "--network",
            "host",
            "--name",
            &name,
            "-v",
            &mount,
            DEX_IMAGE,
            "dex",
            "serve",
            "/cfg/config.yaml",
        ],
        work,
        &[],
    )
    .await;
    assert!(ok, "docker run dex failed: {stderr}");
    let dex = Dex {
        docker: docker.to_string(),
        name,
        issuer,
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    let discovery = format!("{}/.well-known/openid-configuration", dex.issuer);
    loop {
        if let Ok(r) = Client::new().get(&discovery).send().await {
            if r.status().is_success() {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "Dex did not come up at {discovery}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    dex
}

/// The browser's part: our start, every Dex hop, our callback, the SPA's
/// exchange.
async fn browser_login(server: &TestServer) -> Value {
    let client = Client::builder()
        .redirect(redirect::Policy::none())
        .build()
        .unwrap();
    let start = client
        .get(format!("{}/api/v1/auth/sso/dex/start", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(
        start.status(),
        StatusCode::SEE_OTHER,
        "{:?}",
        start.text().await
    );
    let cookie = start.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert!(cookie.starts_with("oc_sso="), "{cookie}");
    let mut next = url::Url::parse(start.headers()["location"].to_str().unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let landed = loop {
        assert!(
            Instant::now() < deadline,
            "the redirect chain never came back"
        );
        let ours = next.as_str().starts_with(&server.base_url);
        let mut req = client.get(next.clone());
        if ours {
            req = req.header("cookie", &cookie);
        }
        let resp = req.send().await.unwrap();
        if ours {
            break resp;
        }
        assert!(
            resp.status().is_redirection(),
            "Dex answered {} at {next}: {:?}",
            resp.status(),
            resp.text().await
        );
        let location = resp.headers()["location"].to_str().unwrap().to_string();
        next = next.join(&location).unwrap();
    };
    assert_eq!(landed.status(), StatusCode::SEE_OTHER);
    let location = landed.headers()["location"].to_str().unwrap().to_string();
    let code = location
        .split("code=")
        .nth(1)
        .unwrap_or_else(|| panic!("no handoff in {location}"));
    let resp = client
        .post(format!("{}/api/v1/auth/sso/exchange", server.base_url))
        .header("cookie", &cookie)
        .json(&json!({ "code": code }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    resp.json().await.unwrap()
}

async fn cargo_publish(cargo: &str, work: &Path, server: &TestServer, token: &str) {
    let home = work.join("cargo-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "[registry]\nglobal-credential-providers = [\"cargo:token\"]\n",
    )
    .unwrap();
    std::fs::write(
        home.join("credentials.toml"),
        format!("[registries.sso]\ntoken = \"Bearer {token}\"\n"),
    )
    .unwrap();
    let dir = work.join("ssocrate");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join(".cargo")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"ssocrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\ndescription = \"x\"\nlicense = \"MIT\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub const X: u8 = 1;\n").unwrap();
    std::fs::write(
        dir.join(".cargo/config.toml"),
        format!(
            "[registries.sso]\nindex = \"sparse+{}/cargo-sso/index/\"\n",
            server.base_url
        ),
    )
    .unwrap();
    let target = work.join("target");
    let env: [(&str, &OsStr); 3] = [
        ("CARGO_HOME", home.as_os_str()),
        ("CARGO_TARGET_DIR", target.as_os_str()),
        ("CARGO_TERM_COLOR", OsStr::new("never")),
    ];
    let (ok, stdout, stderr) = run_cmd(
        cargo,
        &[
            "publish",
            "--registry",
            "sso",
            "--no-verify",
            "--allow-dirty",
        ],
        &dir,
        &env,
    )
    .await;
    assert!(ok, "cargo publish failed:\n{stdout}\n{stderr}");
}

fn rootfs_tar(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("rootfs.tar");
    let mut builder = tar::Builder::new(std::fs::File::create(&path).unwrap());
    let content = b"hello from sso";
    let mut header = tar::Header::new_gnu();
    header.set_path("hello.txt").unwrap();
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append(&header, content.as_slice()).unwrap();
    builder.finish().unwrap();
    path
}

async fn docker_round_trip(
    docker: &str,
    work: &Path,
    server: &TestServer,
    user: &str,
    token: &str,
) {
    let config_dir = work.join("docker-config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let env = [("DOCKER_CONFIG", config_dir.as_os_str())];
    let run = |args: Vec<String>| {
        async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let (ok, stdout, stderr) = run_cmd(docker, &args, work, &env).await;
            assert!(ok, "docker {} failed:\n{stdout}\n{stderr}", args.join(" "));
        }
    };
    let registry = format!("127.0.0.1:{}", server.port);
    let image = format!("{registry}/oci-sso/team/app:1.0");
    let tar = rootfs_tar(work);
    run(vec![
        "import".into(),
        tar.display().to_string(),
        image.clone(),
    ])
    .await;
    run(vec![
        "login".into(),
        registry.clone(),
        "-u".into(),
        user.into(),
        "-p".into(),
        token.into(),
    ])
    .await;
    run(vec!["push".into(), image.clone()]).await;
    run(vec!["rmi".into(), image.clone()]).await;
    run(vec!["pull".into(), image.clone()]).await;
    run(vec!["rmi".into(), image]).await;
    run(vec!["logout".into(), registry]).await;
}

#[tokio::test]
async fn dex_login_then_cargo_publish_and_docker_push_pull() {
    let Some(docker) = docker_bin() else {
        return;
    };
    let Some(cargo) = client_bin("CARGO_BIN") else {
        return;
    };
    let result = tokio::time::timeout(TIMEOUT, run(&docker, &cargo)).await;
    assert!(result.is_ok(), "test timed out after {TIMEOUT:?}");
}

async fn run(docker: &str, cargo: &str) {
    let work = tempfile::TempDir::new().unwrap();
    let dex_port = free_port();
    let issuer = format!("http://127.0.0.1:{dex_port}/dex");
    let server = spawn_server(SpawnOpts {
        anonymous_read: false,
        repositories: vec![
            hosted("cargo-sso", RepositoryFormat::Cargo, Visibility::Private),
            hosted("oci-sso", RepositoryFormat::Oci, Visibility::Private),
        ],
        sso: SsoConfig {
            dev_insecure_http: true,
            providers: vec![SsoProviderConfig {
                name: "dex".into(),
                kind: "generic".into(),
                issuer: issuer.clone(),
                client_id: "opencargo".into(),
                client_secret: "s3cret".into(),
                scopes: vec!["groups".into()],
                groups_claim: Some("groups".into()),
                open: Some(false),
                required_groups: vec!["authors".into()],
                default_role: Some("publisher".into()),
                ..Default::default()
            }],
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    let redirect = format!("{}/api/v1/auth/sso/dex/callback", server.base_url);
    let _dex = start_dex(docker, work.path(), dex_port, &redirect).await;

    let session = browser_login(&server).await;
    let token = session["token"].as_str().unwrap().to_string();
    let user = session["username"].as_str().unwrap().to_string();
    assert_eq!(user, "kilgore");

    cargo_publish(cargo, work.path(), &server, &token).await;
    let index = Client::new()
        .get(format!(
            "{}/cargo-sso/index/ss/oc/ssocrate",
            server.base_url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(index.status(), StatusCode::OK);

    docker_round_trip(docker, work.path(), &server, &user, &token).await;
}
