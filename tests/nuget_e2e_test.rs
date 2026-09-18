//! A real `dotnet` against opencargo behind a TLS reverse proxy (nginx,
//! pinned by digest): pack, push with `-k`, restore through a group whose
//! members are a hosted feed and a proxy of a nuget.org-shaped upstream
//! (the package verified against its catalog leaf's sha512, a lying leaf
//! refused), `list package --outdated`, delete as unlist then restore of
//! the unlisted version, search, and an anonymous restore of a private
//! group answered 401 rather than NU1101. `nuget.exe` (`NUGET_BIN`) is
//! driven through the same proxy when present.
//!
//! Skipped unless `DOTNET_BIN` (or `dotnet` on PATH) runs, a failure under
//! `OPENCARGO_E2E_REQUIRE=1`; `scripts/dotnet-in-docker` is a pinned SDK
//! image that can stand in, `scripts/nuget-in-docker` a pinned Mono image
//! running the `nuget.exe` that `NUGET_EXE` names. The proxy needs `docker`.

mod common;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;

use common::fake_upstream::nuget as fake;
use common::{
    add_token, client_bin, create_user, group, hosted, proxy_with, run_cmd, spawn_server, ProxyOpts,
    SpawnOpts, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

const NGINX: &str =
    "nginx:1.27-alpine@sha256:65645c7bb6a0661892a8b03b89d0743208a18dd2f3f17a54ef4b76fb8e2f2a10";

/// nginx terminating TLS on `url` for an opencargo on `upstream`, with a CA
/// clients trust through `SSL_CERT_FILE`.
struct TlsProxy {
    name: String,
    url: String,
    ca: PathBuf,
    _dir: TempDir,
}

impl Drop for TlsProxy {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker").args(["rm", "-f", &self.name]).output();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

impl TlsProxy {
    async fn start(upstream_port: u16, port: u16) -> Self {
        let dir = TempDir::new().unwrap();
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "opencargo e2e CA");
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .unwrap()
            .signed_by(&key, &ca, &ca_key)
            .unwrap();
        std::fs::write(dir.path().join("cert.pem"), leaf.pem()).unwrap();
        std::fs::write(dir.path().join("key.pem"), key.serialize_pem()).unwrap();
        let ca_path = dir.path().join("ca.pem");
        std::fs::write(&ca_path, ca.pem()).unwrap();
        std::fs::write(
            dir.path().join("nginx.conf"),
            format!(
                r#"events {{}}
http {{
  server {{
    listen 127.0.0.1:{port} ssl;
    ssl_certificate /certs/cert.pem;
    ssl_certificate_key /certs/key.pem;
    client_max_body_size 300m;
    proxy_request_buffering off;
    proxy_buffering off;
    location / {{
      proxy_pass http://127.0.0.1:{upstream_port};
      proxy_set_header Host $http_host;
      proxy_set_header X-Forwarded-Proto https;
      proxy_set_header X-Forwarded-For $remote_addr;
    }}
  }}
}}
"#
            ),
        )
        .unwrap();
        let name = format!("opencargo-nuget-e2e-{port}");
        let certs = format!("{}:/certs:ro", dir.path().display());
        let conf = format!("{}:/etc/nginx/nginx.conf:ro", dir.path().join("nginx.conf").display());
        let out = std::process::Command::new("docker")
            .args(["run", "-d", "--rm", "--name", &name, "--network", "host", "-v", &certs, "-v", &conf, NGINX])
            .output()
            .expect("docker is required by the NuGet E2E");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let url = format!("https://localhost:{port}");
        let proxy = Self {
            name,
            url,
            ca: ca_path,
            _dir: dir,
        };
        proxy.wait_ready().await;
        proxy
    }

    async fn wait_ready(&self) {
        let ca = reqwest::Certificate::from_pem(&std::fs::read(&self.ca).unwrap()).unwrap();
        let client = reqwest::Client::builder().add_root_certificate(ca).build().unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            if let Ok(resp) = client.get(format!("{}/health/live", self.url)).send().await {
                if resp.status().is_success() {
                    return;
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "the TLS proxy never answered");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

struct Env {
    bin: String,
    home: PathBuf,
    packages: PathBuf,
    ca: PathBuf,
}

impl Env {
    async fn run(&self, args: &[&str], cwd: &Path) -> (bool, String) {
        let env: [(&str, &OsStr); 3] = [
            ("HOME", self.home.as_os_str()),
            ("NUGET_PACKAGES", self.packages.as_os_str()),
            ("SSL_CERT_FILE", self.ca.as_os_str()),
        ];
        let (ok, out, err) = run_cmd(&self.bin, args, cwd, &env).await;
        (ok, format!("{out}\n{err}"))
    }
}

/// `sources` are `(key, url)`; every one gets the credentials when given.
fn config(sources: &[(&str, &str)], credentials: Option<(&str, &str)>) -> String {
    let adds: String = sources
        .iter()
        .map(|(key, url)| format!(r#"<add key="{key}" value="{url}" />"#))
        .collect();
    let creds: String = credentials
        .map(|(user, token)| {
            sources
                .iter()
                .map(|(key, _)| {
                    format!(
                        r#"<{key}><add key="Username" value="{user}" /><add key="ClearTextPassword" value="{token}" /></{key}>"#
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<configuration>
  <packageSources><clear />{adds}</packageSources>
  <packageSourceCredentials>{creds}</packageSourceCredentials>
</configuration>"#
    )
}

fn sdk_framework(csproj: &Path) -> String {
    let text = std::fs::read_to_string(csproj).unwrap();
    let start = text.find("<TargetFramework>").unwrap() + "<TargetFramework>".len();
    let end = text[start..].find('<').unwrap();
    text[start..start + end].to_string()
}

fn consumer(dir: &Path, tfm: &str, references: &[(&str, &str)]) {
    std::fs::create_dir_all(dir).unwrap();
    let items: String = references
        .iter()
        .map(|(id, version)| format!(r#"<PackageReference Include="{id}" Version="[{version}]" />"#))
        .collect();
    std::fs::write(
        dir.join("App.csproj"),
        format!(
            r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup><OutputType>Exe</OutputType><TargetFramework>{tfm}</TargetFramework></PropertyGroup>
  <ItemGroup>{items}</ItemGroup>
</Project>"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join("Program.cs"), "System.Console.WriteLine(1);\n").unwrap();
}

#[tokio::test]
async fn dotnet_push_restore_outdated_unlist_and_anonymous_401() {
    let Some(bin) = client_bin("DOTNET_BIN") else {
        return;
    };
    tokio::time::timeout(Duration::from_secs(900), roundtrip(bin))
        .await
        .expect("the dotnet roundtrip timed out");
}

async fn roundtrip(bin: String) {
    let upstream = fake::start().await;
    upstream.add("Upstream.Dep", "1.0.0", common::nuget::nupkg("Upstream.Dep", "1.0.0", &[]));
    upstream.add("Tampered.Dep", "1.0.0", common::nuget::nupkg("Tampered.Dep", "1.0.0", &[]));
    let port = free_port();
    let server = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("nuget-local", RepositoryFormat::Nuget, Visibility::Private),
            proxy_with(
                "nuget-org",
                RepositoryFormat::Nuget,
                &upstream.service_index(),
                ProxyOpts {
                    dl_allow_private: true,
                    ..Default::default()
                },
            ),
            group("nuget", RepositoryFormat::Nuget, &["nuget-local", "nuget-org"]),
        ],
        public_url: Some(format!("https://localhost:{port}")),
        ..Default::default()
    })
    .await;
    let tls = TlsProxy::start(server.port, port).await;
    let direct = &server.base_url;
    let c = reqwest::Client::new();
    create_user(&c, direct, STATIC_TOKEN, "dev", "publisher").await;
    let token = add_token(&c, direct, "dev", "ci").await;

    let tmp = TempDir::new().unwrap();
    let work = tmp.path();
    let env = Env {
        bin,
        home: work.join("home"),
        packages: work.join("packages"),
        ca: tls.ca.clone(),
    };
    std::fs::create_dir_all(&env.home).unwrap();
    let hosted_src = format!("{}/nuget-local/v3/index.json", tls.url);
    let group_src = format!("{}/nuget/v3/index.json", tls.url);
    std::fs::write(
        work.join("nuget.config"),
        config(&[("oc", &group_src), ("local", &hosted_src)], Some(("dev", &token))),
    )
    .unwrap();

    let index: serde_json::Value = c
        .get(format!("{direct}/nuget-local/v3/index.json"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for resource in index["resources"].as_array().unwrap() {
        let id = resource["@id"].as_str().unwrap();
        assert!(id.starts_with(&tls.url), "announced behind the proxy's https origin: {id}");
    }

    let (ok, out) = env.run(&["new", "classlib", "-n", "Greeter", "-o", "greeter"], work).await;
    assert!(ok, "{out}");
    let tfm = sdk_framework(&work.join("greeter/Greeter.csproj"));
    for v in ["1.0.0", "1.1.0"] {
        let prop = format!("-p:PackageVersion={v}");
        let (ok, out) = env.run(&["pack", "greeter", "-c", "Release", "-o", "out", &prop], work).await;
        assert!(ok, "pack {v}: {out}");
    }
    let (ok, out) = env
        .run(&["nuget", "push", "out/Greeter.1.0.0.nupkg", "-s", "local", "-k", &token], work)
        .await;
    assert!(ok, "push 1.0.0: {out}");
    let (ok, out) = env
        .run(&["nuget", "push", "out/Greeter.1.0.0.nupkg", "-s", "local", "-k", &token], work)
        .await;
    assert!(!ok && out.contains("409"), "a second push is a conflict: {out}");
    let (ok, out) = env
        .run(&["nuget", "push", "out/Greeter.1.1.0.nupkg", "-s", "local", "-k", "az"], work)
        .await;
    println!("NUGET-R3-3: dotnet nuget push -k az with Basic credentials configured: ok={ok}\n{out}");
    assert!(ok, "a placeholder key is no credential, Basic alone pushes: {out}");

    let (ok, out) = env
        .run(&["pack", "greeter", "-c", "Release", "-o", "out", "-p:PackageVersion=1.2.0-rc"], work)
        .await;
    assert!(ok, "{out}");
    let (ok, out) = env
        .run(&["nuget", "push", "out/Greeter.1.2.0-rc.nupkg", "-s", "local"], work)
        .await;
    println!("NUGET-R3-3: dotnet nuget push without -k, Basic credentials configured: ok={ok}\n{out}");
    assert!(ok, "Basic credentials alone push: {out}");

    consumer(&work.join("app"), &tfm, &[("Greeter", "1.0.0"), ("Upstream.Dep", "1.0.0")]);
    let (ok, out) = env.run(&["restore", "app"], work).await;
    assert!(ok, "restore through the group, a hosted and a proxied package: {out}");
    assert!(upstream.count("/catalog/") > 0, "the proxied package was verified against its catalog leaf");
    let (ok, out) = env.run(&["list", "app", "package", "--outdated"], work).await;
    assert!(ok && out.contains("1.1.0"), "outdated: {out}");

    upstream.switch(|s| s.wrong_hash = true);
    consumer(&work.join("tampered"), &tfm, &[("Tampered.Dep", "1.0.0")]);
    let (ok, out) = env.run(&["restore", "tampered"], work).await;
    assert!(!ok, "a package whose catalog leaf disagrees is refused: {out}");
    upstream.switch(|s| s.wrong_hash = false);

    let (ok, out) = env
        .run(&["nuget", "delete", "Greeter", "1.0.0", "-s", "local", "-k", &token, "--non-interactive"], work)
        .await;
    assert!(ok, "delete is an unlist: {out}");
    std::fs::remove_dir_all(&env.packages).ok();
    let (ok, out) = env.run(&["restore", "app", "--force"], work).await;
    assert!(ok, "the unlisted version still restores by exact version: {out}");
    let (ok, out) = env
        .run(&["package", "search", "Greeter", "--source", "oc"], work)
        .await;
    let greeter: Vec<&str> = out.lines().filter(|l| l.contains("Greeter")).collect();
    assert!(ok && greeter.iter().any(|l| l.contains("1.1.0")), "{out}");
    assert!(
        greeter.iter().all(|l| !l.contains("1.0.0")),
        "search does not show the unlisted version: {out}"
    );

    let outside = TempDir::new().unwrap();
    let anon = outside.path().join("anon");
    std::fs::create_dir_all(&anon).unwrap();
    std::fs::write(anon.join("nuget.config"), config(&[("oc", &group_src)], None)).unwrap();
    consumer(&anon.join("app"), &tfm, &[("Greeter", "1.1.0")]);
    std::fs::remove_dir_all(&env.packages).ok();
    let (ok, out) = env.run(&["restore", "app", "--no-cache"], &anon).await;
    assert!(!ok, "an anonymous restore of a private group fails: {out}");
    assert!(out.contains("401"), "it is asked for credentials: {out}");
    assert!(!out.contains("NU1101"), "not reported as a missing package: {out}");
}

#[tokio::test]
async fn nuget_exe_push_install_delete() {
    let Some(bin) = client_bin("NUGET_BIN") else {
        return;
    };
    let port = free_port();
    let server = spawn_server(SpawnOpts {
        repositories: vec![hosted("nuget", RepositoryFormat::Nuget, Visibility::Public)],
        public_url: Some(format!("https://localhost:{port}")),
        ..Default::default()
    })
    .await;
    let tls = TlsProxy::start(server.port, port).await;
    let tmp = TempDir::new().unwrap();
    let work = tmp.path();
    let env: [(&str, &OsStr); 1] = [("SSL_CERT_FILE", tls.ca.as_os_str())];
    let pkg = work.join("Greeter.1.0.0.nupkg");
    std::fs::write(&pkg, common::nuget::nupkg("Greeter", "1.0.0", &[])).unwrap();
    let src = format!("{}/nuget/v3/index.json", tls.url);
    let pkg = pkg.to_string_lossy().into_owned();
    let (ok, out, err) = run_cmd(&bin, &["push", &pkg, "-Source", &src, "-ApiKey", STATIC_TOKEN], work, &env).await;
    assert!(ok, "{out}{err}");
    let out_dir = work.join("installed").to_string_lossy().into_owned();
    let (ok, out, err) = run_cmd(&bin, &["install", "Greeter", "-Version", "1.0.0", "-Source", &src, "-OutputDirectory", &out_dir], work, &env).await;
    assert!(ok, "{out}{err}");
    let (ok, out, err) = run_cmd(&bin, &["delete", "Greeter", "1.0.0", "-Source", &src, "-ApiKey", STATIC_TOKEN, "-NonInteractive"], work, &env).await;
    assert!(ok, "{out}{err}");
}
