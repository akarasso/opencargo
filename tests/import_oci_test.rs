//! OCI copies between two spawned opencargo: one is the source, read over
//! the distribution protocol (no catalogue, repositories named), the other
//! the target, sometimes behind a tap that breaks chosen requests.

mod common;

use common::fake_source::github::{FakeGithub, Inner as GithubInner};
use common::fake_source::tap::{Fault, Rule, Tap};
use common::fake_source::verdaccio::{Config, FakeVerdaccio, Pkg};
use common::import::{files_in, Importer};
use common::{hosted, push_blob, sha256_digest, spawn_server, SpawnOpts, TestServer, STATIC_TOKEN};
use opencargo::config::{RepositoryFormat, Visibility};
use reqwest::StatusCode;
use serde_json::{json, Value};

const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX: &str = "application/vnd.oci.image.index.v1+json";

async fn registry(repos: &[&str]) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: repos.iter().map(|r| hosted(r, RepositoryFormat::Oci, Visibility::Private)).collect(),
        ..Default::default()
    })
    .await
}

async fn put_manifest(base: &str, image: &str, reference: &str, media: &str, body: &[u8]) -> String {
    let resp = reqwest::Client::new()
        .put(format!("{base}/v2/{image}/manifests/{reference}"))
        .bearer_auth(STATIC_TOKEN)
        .header("content-type", media)
        .body(body.to_vec())
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{:?}", resp.text().await);
    sha256_digest(body)
}

/// An image of one config and one layer of `layer` bytes, tagged `tag`.
async fn image(base: &str, image: &str, tag: &str, layer: &[u8], arch: &str) -> String {
    let client = reqwest::Client::new();
    let config = json!({ "architecture": arch, "os": "linux", "rootfs": { "type": "layers", "diff_ids": [] } }).to_string();
    let config_digest = push_blob(&client, base, image, config.as_bytes()).await;
    let layer_digest = push_blob(&client, base, image, layer).await;
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST,
        "config": { "mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest, "size": config.len() },
        "layers": [{ "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": layer_digest, "size": layer.len() }]
    })
    .to_string();
    put_manifest(base, image, tag, MANIFEST, manifest.as_bytes()).await
}

async fn head_digest(base: &str, image: &str, reference: &str) -> Option<String> {
    let resp = reqwest::Client::new()
        .head(format!("{base}/v2/{image}/manifests/{reference}"))
        .bearer_auth(STATIC_TOKEN)
        .header("accept", format!("{MANIFEST}, {INDEX}"))
        .send()
        .await
        .unwrap();
    resp.status().is_success().then(|| resp.headers()["docker-content-digest"].to_str().unwrap().to_string())
}

async fn blob(base: &str, image: &str, digest: &str) -> (StatusCode, Vec<u8>) {
    let resp = reqwest::Client::new()
        .get(format!("{base}/v2/{image}/blobs/{digest}"))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    (resp.status(), resp.bytes().await.unwrap().to_vec())
}

fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.max(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn importer() -> Importer {
    Importer::new().env("OPENCARGO_IMPORT_SOURCE_TOKEN", STATIC_TOKEN)
}

fn args<'a>(src: &'a str, dst: &'a str, repo: &'a str) -> Vec<&'a str> {
    vec!["--source", "distribution", "--from", src, "--to", dst, "--source-repo", repo, "--target-repo", "dst"]
}

#[tokio::test]
async fn opencargo_to_opencargo_oci_end_to_end() {
    let src = registry(&["src"]).await;
    let dst = registry(&["dst"]).await;
    let amd = image(&src.base_url, "src/team/app", "1.0-amd64", &noise(4096, 1), "amd64").await;
    let arm = image(&src.base_url, "src/team/app", "1.0-arm64", &noise(4096, 2), "arm64").await;
    let index = json!({
        "schemaVersion": 2,
        "mediaType": INDEX,
        "manifests": [
            { "mediaType": MANIFEST, "digest": amd, "size": 0, "platform": { "architecture": "amd64", "os": "linux" } },
            { "mediaType": MANIFEST, "digest": arm, "size": 0, "platform": { "architecture": "arm64", "os": "linux" } }
        ]
    })
    .to_string();
    let index_digest = put_manifest(&src.base_url, "src/team/app", "1.0", INDEX, index.as_bytes()).await;
    let from = format!("{}/", src.base_url);
    let run = importer().run(&args(&from, &dst.base_url, "src/team/app")).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 3);
    assert!(run.kinds().is_empty(), "the operator named the repositories: nothing is incomplete {:?}", run.gaps());
    assert_eq!(head_digest(&dst.base_url, "dst/src-team/app", "1.0").await.as_deref(), Some(index_digest.as_str()));
    assert_eq!(head_digest(&dst.base_url, "dst/src-team/app", &amd).await.as_deref(), Some(amd.as_str()));
    assert_eq!(head_digest(&dst.base_url, "dst/src-team/app", "1.0-arm64").await.as_deref(), Some(arm.as_str()));

    let flat = importer().run(&[args(&from, &dst.base_url, "src/team/app"), vec!["--flatten-names"]].concat()).await;
    assert_eq!(flat.code, 0, "{}", flat.stdout);
    assert!(head_digest(&dst.base_url, "dst/app", "1.0").await.is_some());
}

#[tokio::test]
async fn oci_multi_arch_index_copies_children_first() {
    let src = registry(&["src"]).await;
    let dst = registry(&["dst"]).await;
    let amd = image(&src.base_url, "src/app", "amd", &noise(512, 3), "amd64").await;
    let arm = image(&src.base_url, "src/app", "arm", &noise(512, 4), "arm64").await;
    let index = json!({ "schemaVersion": 2, "mediaType": INDEX, "manifests": [
        { "mediaType": MANIFEST, "digest": amd, "size": 1, "platform": { "architecture": "amd64", "os": "linux" } },
        { "mediaType": MANIFEST, "digest": arm, "size": 1, "platform": { "architecture": "arm64", "os": "linux" } } ] })
    .to_string();
    put_manifest(&src.base_url, "src/app", "multi", INDEX, index.as_bytes()).await;
    let tap = Tap::start(&dst.base_url, Vec::new()).await;
    let run = importer()
        .run(&[args(&format!("{}/", src.base_url), &tap.url, "src/app"), vec!["--include", "*/app", "--concurrency", "1"]].concat())
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    let puts: Vec<String> = tap
        .inner
        .log
        .hits()
        .into_iter()
        .filter(|h| h.method == "PUT" && h.path.contains("/manifests/"))
        .map(|h| h.path.rsplit('/').next().unwrap().to_string())
        .collect();
    let multi = puts.iter().position(|p| p == "multi").unwrap();
    for child in [&amd, &arm] {
        let at = puts.iter().position(|p| p == child).unwrap_or_else(|| panic!("{child} never put: {puts:?}"));
        assert!(at < multi, "{child} after the index: {puts:?}");
    }
}

#[tokio::test]
async fn two_tags_on_one_digest_both_land() {
    let src = registry(&["src"]).await;
    let dst = registry(&["dst"]).await;
    let d = image(&src.base_url, "src/app", "1.2.3", &noise(2048, 5), "amd64").await;
    let body = reqwest::Client::new()
        .get(format!("{}/v2/src/app/manifests/1.2.3", src.base_url))
        .bearer_auth(STATIC_TOKEN)
        .header("accept", MANIFEST)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    put_manifest(&src.base_url, "src/app", "stable", MANIFEST, &body).await;
    let tap = Tap::start(&dst.base_url, Vec::new()).await;
    let run = importer().run(&[args(&format!("{}/", src.base_url), &tap.url, "src/app"), vec!["--flatten-names"]].concat()).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 2);
    for tag in ["1.2.3", "stable"] {
        assert_eq!(head_digest(&dst.base_url, "dst/app", tag).await.as_deref(), Some(d.as_str()), "{tag}");
    }
    assert_eq!(tap.count("POST", "/blobs/uploads/"), 2, "one config and one layer, uploaded once");
}

#[tokio::test]
async fn oci_existing_blob_is_not_reuploaded() {
    let src = registry(&["src"]).await;
    let dst = registry(&["dst"]).await;
    let layer = noise(1024, 6);
    image(&src.base_url, "src/app", "one", &layer, "amd64").await;
    let from = format!("{}/", src.base_url);
    assert_eq!(importer().run(&[args(&from, &dst.base_url, "src/app"), vec!["--flatten-names"]].concat()).await.code, 0);
    image(&src.base_url, "src/app", "two", &layer, "arm64").await;
    let tap = Tap::start(&dst.base_url, Vec::new()).await;
    let run = importer().run(&[args(&from, &tap.url, "src/app"), vec!["--flatten-names"]].concat()).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("copied"), 1);
    assert_eq!(run.count("skipped"), 1);
    assert_eq!(tap.count("POST", "/blobs/uploads/"), 1, "only the new config moves; the shared layer is there");
}

#[tokio::test]
async fn oci_moved_tag_is_repointed_not_failed() {
    let src = registry(&["src"]).await;
    let dst = registry(&["dst"]).await;
    let first = image(&src.base_url, "src/app", "latest", &noise(700, 7), "amd64").await;
    let from = format!("{}/", src.base_url);
    let imp = importer();
    let base = [args(&from, &dst.base_url, "src/app"), vec!["--flatten-names"]].concat();
    assert_eq!(imp.run(&base).await.code, 0);
    let second = image(&src.base_url, "src/app", "latest", &noise(700, 8), "amd64").await;
    let strict = importer().run(&[base.clone(), vec!["--no-retag"]].concat()).await;
    assert_eq!(strict.code, 2, "{}", strict.stdout);
    assert_eq!(head_digest(&dst.base_url, "dst/app", "latest").await.as_deref(), Some(first.as_str()));
    let run = imp.run(&base).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(head_digest(&dst.base_url, "dst/app", "latest").await.as_deref(), Some(second.as_str()));
    let retag = format!("Retagged {first} -> {second}");
    assert!(run.stdout.contains(&retag), "{}", run.stdout);
    assert_eq!(files_in(&imp.spool()), 0);
    let offline = imp.sub("report", &[]).await;
    assert_eq!(offline.code, 0);
    assert!(offline.stdout.contains(&retag), "the state file keeps the note after the run: {}", offline.stdout);
}

#[tokio::test]
async fn oci_blobs_stream_without_spooling_whole_image() {
    let src = registry(&["src"]).await;
    let dst = registry(&["dst"]).await;
    let layer = noise(3 * 1024 * 1024 + 17, 9);
    image(&src.base_url, "src/app", "big", &layer, "amd64").await;
    let tap = Tap::start(&dst.base_url, Vec::new()).await;
    let imp = importer();
    let run = imp
        .run(&[args(&format!("{}/", src.base_url), &tap.url, "src/app"), vec!["--flatten-names", "--oci-chunk", "1MiB"]].concat())
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(tap.count("PATCH", "/blobs/uploads/"), 5, "four chunks of the layer, one of the config");
    assert_eq!(files_in(&imp.spool()), 0, "nothing of an image is ever spooled");
    let manifest: Value = reqwest::Client::new()
        .get(format!("{}/v2/dst/app/manifests/big", dst.base_url))
        .bearer_auth(STATIC_TOKEN)
        .header("accept", MANIFEST)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let d = manifest["layers"][0]["digest"].as_str().unwrap();
    assert_eq!(blob(&dst.base_url, "dst/app", d).await, (StatusCode::OK, layer));
}

/// One image copied, one chosen request of it broken. The layer is sized in
/// MiB and the chunk asked for is 1MiB because the sink raises its chunk to
/// the target's `OCI-Chunk-Min-Length`, which is 1MiB on S3 and nothing on a
/// filesystem: a smaller one splits the layer on one backend and not on the
/// other, so a rule counting PATCHes would fire on a different byte.
async fn faulted(rule: Rule) -> (TestServer, Tap, common::import::Run, Vec<u8>) {
    let src = registry(&["src"]).await;
    let dst = registry(&["dst"]).await;
    let layer = noise(2 * 1024 * 1024 + 17, 10);
    image(&src.base_url, "src/app", "t", &layer, "amd64").await;
    let tap = Tap::start(&dst.base_url, vec![rule]).await;
    let run = importer()
        .run(&[args(&format!("{}/", src.base_url), &tap.url, "src/app"), vec!["--flatten-names", "--oci-chunk", "1MiB"]].concat())
        .await;
    (dst, tap, run, layer)
}

async fn layer_landed(dst: &TestServer, layer: &[u8]) {
    let d = sha256_digest(layer);
    assert_eq!(blob(&dst.base_url, "dst/app", &d).await, (StatusCode::OK, layer.to_vec()));
}

#[tokio::test]
async fn oci_patch_dropped_without_a_status_resumes_from_the_target_offset() {
    let rule = Rule { method: "PATCH", path_contains: "/blobs/uploads/", skip: 1, times: 1, fault: Fault::Drop, range: None };
    let (dst, tap, run, layer) = faulted(rule).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    layer_landed(&dst, &layer).await;
    assert!(tap.count("GET", "/blobs/uploads/") >= 1, "the sink asked the target where the upload stands");
    assert_eq!(tap.count("POST", "/blobs/uploads/"), 2, "no restart: config and layer, one upload each");
}

#[tokio::test]
async fn oci_404_on_an_upload_restarts_from_post() {
    let rule = Rule {
        method: "PATCH",
        path_contains: "/blobs/uploads/",
        skip: 2,
        times: 1,
        fault: Fault::Instead(StatusCode::NOT_FOUND),
        range: None,
    };
    let (dst, tap, run, layer) = faulted(rule).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    layer_landed(&dst, &layer).await;
    assert_eq!(tap.count("POST", "/blobs/uploads/"), 3);
}

#[tokio::test]
async fn oci_zero_byte_range_resumes_from_zero_not_one() {
    let rule = Rule {
        method: "PATCH",
        path_contains: "/blobs/uploads/",
        skip: 1,
        times: 1,
        fault: Fault::Instead(StatusCode::RANGE_NOT_SATISFIABLE),
        range: Some("0-0"),
    };
    let (dst, tap, run, layer) = faulted(rule).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    layer_landed(&dst, &layer).await;
    assert_eq!(tap.count("POST", "/blobs/uploads/"), 2, "resent in place, not restarted");
}

#[tokio::test]
async fn oci_restart_reports_orphaned_target_bytes() {
    let rule = Rule {
        method: "PATCH",
        path_contains: "/blobs/uploads/",
        skip: 1,
        times: 1,
        fault: Fault::After(StatusCode::INTERNAL_SERVER_ERROR),
        range: None,
    };
    let (dst, _tap, run, layer) = faulted(rule).await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    layer_landed(&dst, &layer).await;
    assert!(run.stdout.contains("bytes left on abandoned target uploads"), "{}", run.stdout);
}

#[tokio::test]
async fn oci_restarts_are_capped() {
    let rule = Rule {
        method: "PATCH",
        path_contains: "/blobs/uploads/",
        skip: 0,
        times: 100,
        fault: Fault::Instead(StatusCode::NOT_FOUND),
        range: None,
    };
    let (_dst, tap, run, _) = faulted(rule).await;
    assert_eq!(run.code, 2, "{}", run.stdout);
    assert!(run.gaps().iter().any(|g| g.2.contains("restarts")), "{:?}", run.gaps());
    assert_eq!(tap.count("POST", "/blobs/uploads/"), 4);
}

#[tokio::test]
async fn github_container_and_npm_packages_are_imported() {
    let ghcr = registry(&["acme"]).await;
    let d = image(&ghcr.base_url, "acme/app", "v1", &noise(900, 11), "amd64").await;
    let npm = FakeVerdaccio::start(vec![Pkg::new("@acme/lib", &["1.0.0"])], Config::default()).await;
    let mut api = GithubInner::new("acme");
    api.npm = vec!["lib".into()];
    api.containers = vec![("app".into(), vec![(d.clone(), vec!["v1".into()]), ("sha256:untagged".into(), vec![])])];
    api.maven = 3;
    let gh = FakeGithub::start(api).await;
    let dst = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("images", RepositoryFormat::Oci, Visibility::Private),
            hosted("npm", RepositoryFormat::Npm, Visibility::Private),
        ],
        ..Default::default()
    })
    .await;
    let run = importer()
        .run(&[
            "--source", "github", "--from", &format!("{}/", gh.url), "--to", &dst.base_url, "--source-repo", "acme",
            "--map", "acme=images", "--github-npm-url", &format!("{}/", npm.url), "--github-registry-url", &format!("{}/", ghcr.base_url),
            "--map", "never=never",
        ])
        .await;
    assert_eq!(run.code, 1, "one target repository cannot take two formats: {}", run.stdout);
    assert!(run.stdout.contains("several formats"), "{}", run.stdout);

    let npm_only = importer()
        .run(&[
            "--source", "github", "--from", &format!("{}/", gh.url), "--to", &dst.base_url, "--source-repo", "acme",
            "--target-repo", "npm", "--github-npm-url", &format!("{}/", npm.url), "--github-registry-url", &format!("{}/", ghcr.base_url),
            "--exclude", "acme/app",
        ])
        .await;
    assert_eq!(npm_only.code, 4, "{}", npm_only.stdout);
    assert!(npm_only.gaps().iter().any(|g| g.0 == "UnsupportedFormat" && g.1 == "acme/maven"), "{:?}", npm_only.gaps());
    assert_eq!(npm_only.count("copied"), 1);
    assert!(npm_only.stdout.contains("as importer"), "{}", npm_only.stdout);

    let images = importer()
        .run(&[
            "--source", "github", "--from", &format!("{}/", gh.url), "--to", &dst.base_url, "--source-repo", "acme",
            "--target-repo", "images", "--github-npm-url", &format!("{}/", npm.url), "--github-registry-url", &format!("{}/", ghcr.base_url),
            "--include", "acme/app", "--allow-incomplete",
        ])
        .await;
    assert_eq!(images.code, 0, "{}", images.stdout);
    assert_eq!(head_digest(&dst.base_url, "images/acme/app", "v1").await.as_deref(), Some(d.as_str()));
}

#[tokio::test]
async fn github_per_page_times_page_over_10000_is_incomplete_gap() {
    let mut api = GithubInner::new("big");
    api.maven = 10_050;
    let gh = FakeGithub::start(api).await;
    let dst = registry(&["dst"]).await;
    let run = importer()
        .run(&[
            "--source", "github", "--from", &format!("{}/", gh.url), "--to", &dst.base_url, "--source-repo", "big",
            "--target-repo", "dst", "--dry-run", "--rate", "0",
        ])
        .await;
    assert_eq!(run.code, 4, "{}", run.stdout);
    assert!(run.gaps().iter().any(|g| g.0 == "ListingIncomplete" && g.1 == "big/maven" && g.2.contains("10000")), "{:?}", run.gaps());
    assert_eq!(gh.inner.log.count(|h| h.query.contains("package_type=maven")), 100);
}

#[tokio::test]
async fn distribution_tags_list_walks_the_rfc5988_link_and_never_asks_for_catalog() {
    let src = registry(&["src"]).await;
    let dst = registry(&["dst"]).await;
    for i in 0..3 {
        image(&src.base_url, "src/app", &format!("t{i}"), &noise(128, 20 + i), "amd64").await;
    }
    let tap = Tap::start(&src.base_url, Vec::new()).await;
    let run = importer()
        .run(&[args(&format!("{}/", tap.url), &dst.base_url, "src/app"), vec!["--flatten-names", "--dry-run"]].concat())
        .await;
    assert_eq!(run.code, 0, "{}", run.stdout);
    assert_eq!(run.count("pending"), 3);
    assert_eq!(tap.count("GET", "_catalog"), 0);
    assert!(tap.count("GET", "/tags/list") >= 1);
}
