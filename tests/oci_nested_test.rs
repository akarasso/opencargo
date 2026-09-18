mod common;

use reqwest::StatusCode;
use serde_json::{json, Value};

use common::{hosted, push_blob, sha256_digest, spawn_server, SpawnOpts, TestServer, STATIC_TOKEN};
use opencargo::config::{RepositoryFormat, Visibility};

const REPO: &str = "oci-hosted";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

async fn setup() -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![hosted(REPO, RepositoryFormat::Oci, Visibility::Public)],
        ..Default::default()
    })
    .await
}

fn manifest_for(config: &[u8], layer: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST_TYPE,
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": sha256_digest(config),
            "size": config.len()
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": sha256_digest(layer),
            "size": layer.len()
        }]
    }))
    .unwrap()
}

/// Push config + layer + a tagged manifest under `image`; returns the manifest
/// bytes and the PUT response.
async fn push_image(
    client: &reqwest::Client,
    base_url: &str,
    image: &str,
    tag: &str,
) -> (Vec<u8>, reqwest::Response) {
    let config = format!("{{\"image\":\"{image}\"}}").into_bytes();
    let layer = format!("layer-of-{image}").into_bytes();
    push_blob(client, base_url, image, &config).await;
    push_blob(client, base_url, image, &layer).await;
    let manifest = manifest_for(&config, &layer);
    let resp = client
        .put(format!("{base_url}/v2/{image}/manifests/{tag}"))
        .bearer_auth(STATIC_TOKEN)
        .header("content-type", MANIFEST_TYPE)
        .body(manifest.clone())
        .send()
        .await
        .expect("put manifest request failed");
    (manifest, resp)
}

fn header(resp: &reqwest::Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing {name} header"))
        .to_str()
        .expect("non-ascii header")
        .to_string()
}

async fn every_route_roundtrip(client: &reqwest::Client, base_url: &str, name: &str) {
    let image = format!("{REPO}/{name}");
    let (manifest, resp) = push_image(client, base_url, &image, "1.0").await;
    assert_eq!(resp.status(), StatusCode::CREATED, "{name}: manifest push");
    let digest = sha256_digest(&manifest);
    assert_eq!(header(&resp, "docker-content-digest"), digest);

    let layer_digest = sha256_digest(format!("layer-of-{image}").as_bytes());
    let blob_url = format!("{base_url}/v2/{image}/blobs/{layer_digest}");
    let resp = client.head(&blob_url).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{name}: HEAD blob");
    assert_eq!(header(&resp, "docker-content-digest"), layer_digest);
    let resp = client.get(&blob_url).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{name}: GET blob");
    assert_eq!(
        resp.bytes().await.unwrap(),
        format!("layer-of-{image}").as_bytes()
    );

    for reference in ["1.0", digest.as_str()] {
        let url = format!("{base_url}/v2/{image}/manifests/{reference}");
        let resp = client.head(&url).send().await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "{name}: HEAD manifest {reference}"
        );
        assert_eq!(header(&resp, "docker-content-digest"), digest);
        assert_eq!(header(&resp, "content-length"), manifest.len().to_string());
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "{name}: GET manifest {reference}"
        );
        assert_eq!(header(&resp, "content-type"), MANIFEST_TYPE);
        assert_eq!(resp.bytes().await.unwrap(), manifest.as_slice());
    }

    let tags: Value = client
        .get(format!("{base_url}/v2/{image}/tags/list"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        tags,
        json!({ "name": image, "tags": ["1.0"] }),
        "{name}: tags"
    );

    let unreferenced = push_blob(client, base_url, &image, b"unreferenced").await;
    let resp = client
        .delete(format!("{base_url}/v2/{image}/blobs/{unreferenced}"))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "{name}: DELETE blob");
    let resp = client
        .delete(format!("{base_url}/v2/{image}/manifests/1.0"))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "{name}: DELETE manifest"
    );
    let resp = client
        .get(format!("{base_url}/v2/{image}/manifests/1.0"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "{name}: deleted manifest"
    );
}

#[tokio::test]
async fn push_pull_team_app_and_org_team_app_on_every_route() {
    let server = setup().await;
    let client = reqwest::Client::new();
    every_route_roundtrip(&client, &server.base_url, "team/app").await;
    every_route_roundtrip(&client, &server.base_url, "org/team/app").await;
}

fn files_under(dir: &std::path::Path, out: &mut Vec<String>, root: &std::path::Path) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_under(&path, out, root);
        } else {
            out.push(path.strip_prefix(root).unwrap().to_string_lossy().into_owned());
        }
    }
}

/// Manifest keys carry the image, blob keys only the digest, and both lie
/// under the repository's incarnation rather than its name.
#[tokio::test]
async fn manifest_keys_carry_the_image_and_blob_keys_do_not() {
    let server = setup().await;
    let client = reqwest::Client::new();
    let storage = server.tmp.path().join("storage");

    for name in ["myapp", "team/app"] {
        let image = format!("{REPO}/{name}");
        let (manifest, resp) = push_image(&client, &server.base_url, &image, "v1").await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let hex = sha256_digest(&manifest).trim_start_matches("sha256:").to_string();
        let layer_hex = sha256_digest(format!("layer-of-{image}").as_bytes())
            .trim_start_matches("sha256:")
            .to_string();

        let mut files = Vec::new();
        files_under(&storage, &mut files, &storage);
        let manifest_file = files
            .iter()
            .find(|f| f.contains(&format!("/{name}/{hex}/manifest~")))
            .unwrap_or_else(|| panic!("{name}: no manifest key in {files:?}"));
        assert!(manifest_file.starts_with("r/"), "{manifest_file}");
        assert_eq!(std::fs::read(storage.join(manifest_file)).unwrap(), manifest);
        let blob_file = files
            .iter()
            .find(|f| f.contains(&format!("/_blobs/{layer_hex}/blob~")))
            .unwrap_or_else(|| panic!("{name}: no blob key in {files:?}"));
        assert!(!blob_file.contains(name), "blobs never carry the image name");
        assert!(!files.iter().any(|f| f.contains(REPO)), "no key names the repository");
    }
}

#[tokio::test]
async fn location_headers_carry_nested_name() {
    let server = setup().await;
    let client = reqwest::Client::new();
    let image = format!("{REPO}/team/app");

    let resp = client
        .post(format!("{}/v2/{image}/blobs/uploads/", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let location = header(&resp, "location");
    let uuid = header(&resp, "docker-upload-uuid");
    assert_eq!(location, format!("/v2/{image}/blobs/uploads/{uuid}"));

    let resp = client
        .patch(format!("{}{location}", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .body(b"chunk".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(header(&resp, "location"), location);

    let digest = sha256_digest(b"chunk");
    let resp = client
        .put(format!("{}{location}?digest={digest}", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        header(&resp, "location"),
        format!("/v2/{image}/blobs/{digest}")
    );

    let (manifest, resp) = push_image(&client, &server.base_url, &image, "latest").await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        header(&resp, "location"),
        format!("/v2/{image}/manifests/{}", sha256_digest(&manifest))
    );
}

#[tokio::test]
async fn name_segment_called_blobs_or_manifests_routes() {
    let server = setup().await;
    let client = reqwest::Client::new();
    for name in [
        "team/blobs",
        "team/manifests",
        "uploads/tags",
        "blobs/uploads/app",
    ] {
        every_route_roundtrip(&client, &server.base_url, name).await;
    }
}
