//! `opencargo import` end to end: from a second spawned opencargo with a
//! real npm client on the target side, and, when asked for, from real
//! Verdaccio, Nexus and registry:2 services in containers.

mod common;

use std::time::Duration;

use base64::Engine as _;
use common::containers::{container_gate, Container};
use common::import::Importer;
use common::{hosted, spawn_server, SpawnOpts, TestServer, STATIC_TOKEN};
use opencargo::config::{RepositoryFormat, Visibility};
use reqwest::StatusCode;
use serde_json::{json, Value};

const VERDACCIO: &str = "verdaccio/verdaccio@sha256:7b067a47ae51fb9dff3dcdce60ec0a2cbd7650c208cb4b9f6d37cb1b09b39d43";
const NEXUS: &str = "sonatype/nexus3@sha256:390b8ec7e712c680ed8b449b766695a7c5ebbf41e489317618e63ca199f2cc09";
const REGISTRY: &str = "registry@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373";

/// An npm publish body for `name@version`, as `npm publish` sends it.
fn publish_body(registry: &str, name: &str, version: &str, deps: Value, tag: &str) -> Value {
    let tgz = common::fake_source::verdaccio::tarball(name, version, &deps, Some(&format!("# {name}\n")));
    let short = name.split_once('/').map_or(name, |(_, n)| n);
    json!({
        "_id": name, "name": name, "description": format!("{name}, imported"),
        "dist-tags": { tag: version },
        "versions": { version: { "name": name, "version": version, "dependencies": deps, "main": "index.js",
            "dist": { "shasum": common::fake_source::sha1_hex(&tgz), "tarball": format!("{registry}/{name}/-/{short}-{version}.tgz") } } },
        "_attachments": { format!("{short}-{version}.tgz"): { "content_type": "application/octet-stream",
            "data": base64::engine::general_purpose::STANDARD.encode(&tgz), "length": tgz.len() } }
    })
}

async fn npm_publish(registry: &str, auth: &str, name: &str, version: &str, deps: Value, tag: &str) {
    let resp = reqwest::Client::new()
        .put(format!("{registry}/{}", name.replacen('/', "%2f", 1)))
        .header("authorization", auth)
        .json(&publish_body(registry, name, version, deps, tag))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    assert!(status.is_success(), "publish {name}@{version}: {status} {:?}", resp.text().await);
}

async fn packument(t: &TestServer, repo: &str, name: &str) -> (StatusCode, Value) {
    let resp = reqwest::Client::new().get(format!("{}/{repo}/{name}", t.base_url)).bearer_auth(STATIC_TOKEN).send().await.unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn npm_target() -> TestServer {
    spawn_server(SpawnOpts { repositories: vec![hosted("npm", RepositoryFormat::Npm, Visibility::Public)], ..Default::default() }).await
}

#[tokio::test]
async fn opencargo_to_opencargo_npm_end_to_end() {
    let src = spawn_server(SpawnOpts { repositories: vec![hosted("src", RepositoryFormat::Npm, Visibility::Private)], ..Default::default() }).await;
    let auth = format!("Bearer {STATIC_TOKEN}");
    let registry = format!("{}/src", src.base_url);
    npm_publish(&registry, &auth, "left-pad", "1.0.0", json!({}), "latest").await;
    npm_publish(&registry, &auth, "left-pad", "1.1.0", json!({}), "latest").await;
    npm_publish(&registry, &auth, "left-pad", "2.0.0-beta.1", json!({}), "beta").await;
    npm_publish(&registry, &auth, "@acme/app", "0.1.0", json!({ "left-pad": "^1.0.0" }), "latest").await;
    let t = npm_target().await;
    let imp = Importer::new().env("OPENCARGO_IMPORT_SOURCE_TOKEN", STATIC_TOKEN);
    let run = imp.run(&["--source", "verdaccio", "--from", &format!("{registry}/"), "--to", &t.base_url, "--target-repo", "npm"]).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 4);
    let (_, doc) = packument(&t, "npm", "left-pad").await;
    assert_eq!(doc["dist-tags"], json!({ "latest": "1.1.0", "beta": "2.0.0-beta.1" }));

    let Some(npm) = common::client_bin("NPM_BIN") else { return };
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("package.json"), r#"{"name":"consumer","version":"1.0.0","private":true}"#).unwrap();
    let npmrc = format!("registry={}/npm/\n", t.base_url);
    std::fs::write(project.path().join(".npmrc"), npmrc).unwrap();
    let cache = project.path().join(".cache");
    let (ok, stdout, stderr) = common::run_cmd(
        &npm,
        &["install", "@acme/app@0.1.0", "--no-audit", "--no-fund", "--cache", cache.to_str().unwrap()],
        project.path(),
        &[],
    )
    .await;
    assert!(ok, "npm install from the target failed: {stdout}\n{stderr}");
    let installed: Value =
        serde_json::from_str(&std::fs::read_to_string(project.path().join("node_modules/left-pad/package.json")).unwrap()).unwrap();
    assert_eq!(installed["version"], "1.1.0", "the imported dependency resolved through the imported latest");
}

#[tokio::test]
async fn verdaccio_container_import_end_to_end() {
    if !container_gate("verdaccio_container_import_end_to_end") {
        return;
    }
    let Some(v) = Container::start(VERDACCIO, 4873, &[]) else { return };
    if !v.ready("/-/ping", Duration::from_secs(90)).await {
        return;
    }
    let user: Value = reqwest::Client::new()
        .put(format!("{}/-/user/org.couchdb.user:importer", v.url))
        .json(&json!({ "name": "importer", "password": "import-secret" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = user["token"].as_str().expect("verdaccio issues a token").to_string();
    let auth = format!("Bearer {token}");
    npm_publish(&v.url, &auth, "vd-left", "1.0.0", json!({}), "latest").await;
    npm_publish(&v.url, &auth, "vd-left", "1.1.0", json!({}), "latest").await;
    npm_publish(&v.url, &auth, "@vd/app", "0.1.0", json!({ "vd-left": "^1.0.0" }), "latest").await;
    let t = npm_target().await;
    let imp = Importer::new().env("OPENCARGO_IMPORT_SOURCE_TOKEN", &token);
    let run = imp.run(&["--source", "verdaccio", "--from", &format!("{}/", v.url), "--to", &t.base_url, "--target-repo", "npm"]).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 3, "{}", run.stdout);
    let (status, doc) = packument(&t, "npm", "@vd/app").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["versions"]["0.1.0"]["dependencies"], json!({ "vd-left": "^1.0.0" }));
    assert_eq!(packument(&t, "npm", "vd-left").await.1["dist-tags"]["latest"], "1.1.0");
    let again = imp.run(&["--source", "verdaccio", "--from", &format!("{}/", v.url), "--to", &t.base_url, "--target-repo", "npm"]).await;
    assert_eq!(again.code, 0, "{}", again.stdout);
    assert_eq!(again.count("skipped"), 3);
}

#[tokio::test]
async fn nexus_container_import_end_to_end() {
    if !container_gate("nexus_container_import_end_to_end") {
        return;
    }
    let Some(n) = Container::start(NEXUS, 8081, &[]) else { return };
    if !n.ready("/service/rest/v1/status", Duration::from_secs(300)).await {
        return;
    }
    let password = n.exec(&["cat", "/nexus-data/admin.password"]).expect("the admin password").trim().to_string();
    let client = reqwest::Client::new();
    let create = |format: &str, name: &str| {
        let body = json!({ "name": name, "online": true,
            "storage": { "blobStoreName": "default", "strictContentTypeValidation": true, "writePolicy": "allow_once" } });
        client.post(format!("{}/service/rest/v1/repositories/{format}/hosted", n.url)).basic_auth("admin", Some(&password)).json(&body).send()
    };
    assert!(create("npm", "npm-internal").await.unwrap().status().is_success());
    assert!(create("raw", "files").await.unwrap().status().is_success());
    let auth = common::basic_auth_header("admin", &password);
    let registry = format!("{}/repository/npm-internal", n.url);
    npm_publish(&registry, &auth, "nx-lib", "1.0.0", json!({}), "latest").await;
    npm_publish(&registry, &auth, "@nx/app", "2.0.0", json!({ "nx-lib": "^1.0.0" }), "latest").await;
    let raw = client
        .put(format!("{}/repository/files/docs/readme.txt", n.url))
        .basic_auth("admin", Some(&password))
        .body("hello")
        .send()
        .await
        .unwrap();
    assert!(raw.status().is_success());
    let t = npm_target().await;
    let imp = Importer::new().env("OPENCARGO_IMPORT_SOURCE_USER", "admin").env("OPENCARGO_IMPORT_SOURCE_PASSWORD", &password);
    let args = [
        "--source", "nexus", "--from", &format!("{}/", n.url), "--to", &t.base_url, "--source-repo", "npm-internal",
        "--source-repo", "files", "--map", "npm-internal=npm",
    ];
    let run = imp.run(&args).await;
    assert_eq!(run.code, 4, "a raw repository is named, not copied: {}", run.stdout);
    assert!(run.stdout.contains("nexus 3.76.1"), "{}", run.stdout);
    assert_eq!(run.count("copied"), 2, "{}", run.stdout);
    assert!(run.gaps().iter().any(|g| g.0 == "UnsupportedFormat" && g.1 == "files"), "{:?}", run.gaps());
    let (status, doc) = packument(&t, "npm", "@nx/app").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["versions"]["2.0.0"]["dependencies"], json!({ "nx-lib": "^1.0.0" }));
    let proposed = imp.sub("permissions", &[]).await;
    assert_eq!(proposed.code, 0, "{}", proposed.stdout);
}

#[tokio::test]
async fn distribution_registry_import_end_to_end() {
    if !container_gate("distribution_registry_import_end_to_end") {
        return;
    }
    let Some(r) = Container::start(REGISTRY, 5000, &[]) else { return };
    if !r.ready("/v2/", Duration::from_secs(60)).await {
        return;
    }
    let client = reqwest::Client::new();
    let push = |data: Vec<u8>| {
        let (client, url) = (client.clone(), r.url.clone());
        async move {
            let digest = common::sha256_digest(&data);
            let resp = client.post(format!("{url}/v2/team/app/blobs/uploads/")).send().await.unwrap();
            let location = resp.headers()["location"].to_str().unwrap().to_string();
            let sep = if location.contains('?') { '&' } else { '?' };
            let target = if location.starts_with("http") { location } else { format!("{url}{location}") };
            let done = client.put(format!("{target}{sep}digest={digest}")).body(data).send().await.unwrap();
            assert_eq!(done.status(), StatusCode::CREATED);
            digest
        }
    };
    let config = br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#.to_vec();
    let layer: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
    let (cd, ld) = (push(config.clone()).await, push(layer.clone()).await);
    let manifest = json!({ "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": { "mediaType": "application/vnd.oci.image.config.v1+json", "digest": cd, "size": config.len() },
        "layers": [{ "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": ld, "size": layer.len() }] });
    let put = client
        .put(format!("{}/v2/team/app/manifests/1.0", r.url))
        .header("content-type", "application/vnd.oci.image.manifest.v1+json")
        .body(manifest.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::CREATED);
    let t = spawn_server(SpawnOpts { repositories: vec![hosted("images", RepositoryFormat::Oci, Visibility::Private)], ..Default::default() }).await;
    let run = Importer::new()
        .run(&["--source", "distribution", "--from", &format!("{}/", r.url), "--to", &t.base_url, "--source-repo", "team/app", "--target-repo", "images"])
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 1);
    let got = client
        .get(format!("{}/v2/images/team/app/blobs/{ld}", t.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), StatusCode::OK);
    assert_eq!(got.bytes().await.unwrap().to_vec(), layer);
}
