mod common;

use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::{
    add_scoped_token, create_user, hosted, push_blob, repo_scope, sha256_digest, spawn_server,
    SpawnOpts, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

/// Start a test server on a random port with an OCI hosted repository.
async fn setup() -> (String, tokio::task::JoinHandle<()>, TempDir) {
    let server = spawn_server(SpawnOpts {
        repositories: vec![hosted("oci-private", RepositoryFormat::Oci, Visibility::Public)],
        ..Default::default()
    })
    .await;
    (server.base_url, server.handle, server.tmp)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_oci_chunked_upload() {
    // Covers upload_chunk (PATCH) + complete_upload assembling from chunks.
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let chunk1 = b"first-half-of-the-blob-";
    let chunk2 = b"second-half-of-the-blob";
    let mut full = Vec::new();
    full.extend_from_slice(chunk1);
    full.extend_from_slice(chunk2);
    let digest = sha256_digest(&full);

    let resp = client
        .post(format!("{}/v2/oci-private/myapp/blobs/uploads/", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("start upload failed");
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let location = resp
        .headers()
        .get("location")
        .expect("missing Location")
        .to_str()
        .unwrap()
        .to_string();

    for chunk in [chunk1.as_slice(), chunk2.as_slice()] {
        let resp = client
            .patch(format!("{}{}", base_url, location))
            .bearer_auth("test-token")
            .body(chunk.to_vec())
            .send()
            .await
            .expect("patch chunk failed");
        assert_eq!(resp.status(), StatusCode::ACCEPTED, "patch chunk failed");
    }

    let resp = client
        .put(format!("{}{}?digest={}", base_url, location, digest))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("complete upload failed");
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "complete failed: {:?}",
        resp.text().await
    );

    let resp = client
        .get(format!("{}/v2/oci-private/myapp/blobs/{}", base_url, digest))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("get blob failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.bytes().await.expect("read blob failed");
    assert_eq!(body.as_ref(), full.as_slice(), "reassembled blob must match");
}

#[tokio::test]
async fn test_oci_blob_shared_across_images_in_repo() {
    // R2-2: blobs are content-addressed and deduplicated per repository, so a
    // blob pushed under one image must be downloadable under another image in
    // the same repo. Before the storage path was scoped to the repo, the blob
    // lived under the first image's path and this cross-image read 404'd —
    // exactly what breaks a docker pull of a second image sharing a layer.
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let blob_data = b"shared layer content";
    let digest = push_blob(&client, &base_url, "oci-private/myapp", blob_data).await; // pushed via image "myapp"

    let resp = client
        .get(format!(
            "{}/v2/oci-private/otherapp/blobs/{}",
            base_url, digest
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("cross-image blob get failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a blob pushed under one image must be readable under another image in the same repo"
    );
    let body = resp.bytes().await.expect("read blob failed");
    assert_eq!(
        body.as_ref(),
        blob_data,
        "cross-image blob content must match"
    );
}

#[tokio::test]
async fn test_oci_put_manifest_by_wrong_digest_rejected() {
    // R2-4: pushing a manifest by digest must verify that the reference equals
    // the real content digest (OCI spec); a mismatch must be rejected, not
    // silently stored under the real digest.
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","size":0,"digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000"},"layers":[]}"#;
    let wrong_digest =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    let resp = client
        .put(format!(
            "{}/v2/oci-private/myapp/manifests/{}",
            base_url, wrong_digest
        ))
        .bearer_auth("test-token")
        .header(
            "content-type",
            "application/vnd.oci.image.manifest.v1+json",
        )
        .body(manifest.to_vec())
        .send()
        .await
        .expect("put manifest request failed");

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a manifest pushed by a mismatched digest must be rejected"
    );
}

#[tokio::test]
async fn test_oci_blob_refcount_and_gc() {
    // B8: a blob referenced by a manifest can't be deleted (409); deleting the
    // manifest GCs its now-orphaned blobs.
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let config_data = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}";
    let layer_data = b"refcount-test-layer";
    let config_digest = push_blob(&client, &base_url, "oci-private/myapp", config_data).await;
    let layer_digest = push_blob(&client, &base_url, "oci-private/myapp", layer_data).await;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_data.len()
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": layer_digest,
            "size": layer_data.len()
        }]
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let resp = client
        .put(format!("{}/v2/oci-private/myapp/manifests/rc", base_url))
        .bearer_auth("test-token")
        .header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
        .body(manifest_bytes)
        .send()
        .await
        .expect("put manifest failed");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Deleting a still-referenced blob is refused.
    let resp = client
        .delete(format!("{}/v2/oci-private/myapp/blobs/{}", base_url, layer_digest))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("delete blob request failed");
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "a blob referenced by a manifest must not be deletable"
    );

    // Deleting the manifest GCs its now-orphaned blobs.
    let resp = client
        .delete(format!("{}/v2/oci-private/myapp/manifests/rc", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("delete manifest request failed");
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // The layer blob is gone (GC'd) -> HEAD now 404s.
    let resp = client
        .head(format!("{}/v2/oci-private/myapp/blobs/{}", base_url, layer_digest))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("head blob request failed");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "an orphaned blob should be garbage-collected on manifest deletion"
    );
}

/// A blob is content of the repository, so removing one is the delete rung:
/// a scope that only writes pushes it and does not remove it.
#[tokio::test]
async fn deleting_a_blob_asks_for_the_delete_rung() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();
    create_user(&client, &base_url, STATIC_TOKEN, "ci", "publisher").await;
    let writer =
        add_scoped_token(&client, &base_url, "ci", "writer", repo_scope("oci-*", &["read", "write"])).await;
    let remover = add_scoped_token(
        &client,
        &base_url,
        "ci",
        "remover",
        repo_scope("oci-*", &["read", "write", "delete"]),
    )
    .await;
    let digest = push_blob(&client, &base_url, "oci-private/myapp", b"orphan-layer").await;
    let url = format!("{base_url}/v2/oci-private/myapp/blobs/{digest}");

    let refused = client.delete(&url).bearer_auth(&writer).send().await.unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let body: Value = refused.json().await.unwrap();
    assert_eq!(body["code"], "insufficient_scope");

    let removed = client.delete(&url).bearer_auth(&remover).send().await.unwrap();
    assert_eq!(removed.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_oci_version_check() {
    let (base_url, _handle, _tmp) = setup().await;

    // Without credentials the ping is a Bearer challenge, so classic Docker
    // clients fetch a token before their first push; with a token it is 200.
    let resp = reqwest::get(format!("{}/v2/", base_url))
        .await
        .expect("request failed");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers()["www-authenticate"],
        format!("Bearer realm=\"{base_url}/v2/token\",service=\"opencargo\"")
    );
    assert_eq!(resp.headers()["docker-distribution-api-version"], "registry/2.0");

    let resp = reqwest::Client::new()
        .get(format!("{}/v2/", base_url))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["docker-distribution-api-version"], "registry/2.0");
    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(body, json!({}));
}

#[tokio::test]
async fn test_oci_push_and_pull() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create test data: a config blob and a layer blob
    let config_data = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}";
    let layer_data = b"fake-layer-data-for-testing-purposes";

    // Push the config blob
    let config_digest = push_blob(&client, &base_url, "oci-private/myapp", config_data).await;

    // Push the layer blob
    let layer_digest = push_blob(&client, &base_url, "oci-private/myapp", layer_data).await;

    // Build a manifest
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_data.len()
        },
        "layers": [
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": layer_digest,
                "size": layer_data.len()
            }
        ]
    });

    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_digest = sha256_digest(&manifest_bytes);

    // Push the manifest with tag "latest"
    let resp = client
        .put(format!(
            "{}/v2/oci-private/myapp/manifests/latest",
            base_url
        ))
        .bearer_auth("test-token")
        .header(
            "Content-Type",
            "application/vnd.oci.image.manifest.v1+json",
        )
        .body(manifest_bytes.clone())
        .send()
        .await
        .expect("put manifest request failed");
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "put manifest failed: {:?}",
        resp.text().await
    );

    // Pull the manifest by tag
    let resp = client
        .get(format!(
            "{}/v2/oci-private/myapp/manifests/latest",
            base_url
        ))
        .send()
        .await
        .expect("get manifest request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let pulled_digest = resp
        .headers()
        .get("docker-content-digest")
        .expect("missing Docker-Content-Digest header")
        .to_str()
        .expect("invalid digest header")
        .to_string();
    assert_eq!(pulled_digest, manifest_digest);

    let content_type = resp
        .headers()
        .get("content-type")
        .expect("missing content-type")
        .to_str()
        .expect("invalid content-type")
        .to_string();
    assert_eq!(
        content_type,
        "application/vnd.oci.image.manifest.v1+json"
    );

    let pulled_manifest: Value = resp.json().await.expect("invalid json");
    assert_eq!(pulled_manifest, manifest);

    // Pull the layer blob
    let resp = client
        .get(format!(
            "{}/v2/oci-private/myapp/blobs/{}",
            base_url, layer_digest
        ))
        .send()
        .await
        .expect("get blob request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let pulled_blob = resp.bytes().await.expect("failed to read blob");
    assert_eq!(pulled_blob.as_ref(), layer_data);
}

#[tokio::test]
async fn test_oci_list_tags() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Push some blobs and a manifest with multiple tags
    let config_data = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}";
    let layer_data = b"layer-data-for-tagging-test";

    let config_digest = push_blob(&client, &base_url, "oci-private/myapp", config_data).await;
    let layer_digest = push_blob(&client, &base_url, "oci-private/myapp", layer_data).await;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_data.len()
        },
        "layers": [
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": layer_digest,
                "size": layer_data.len()
            }
        ]
    });

    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();

    // Push with tag "v1.0"
    let resp = client
        .put(format!(
            "{}/v2/oci-private/myapp/manifests/v1.0",
            base_url
        ))
        .bearer_auth("test-token")
        .header(
            "Content-Type",
            "application/vnd.oci.image.manifest.v1+json",
        )
        .body(manifest_bytes.clone())
        .send()
        .await
        .expect("put manifest request failed");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Push with tag "latest"
    let resp = client
        .put(format!(
            "{}/v2/oci-private/myapp/manifests/latest",
            base_url
        ))
        .bearer_auth("test-token")
        .header(
            "Content-Type",
            "application/vnd.oci.image.manifest.v1+json",
        )
        .body(manifest_bytes.clone())
        .send()
        .await
        .expect("put manifest request failed");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // List tags
    let resp = client
        .get(format!(
            "{}/v2/oci-private/myapp/tags/list",
            base_url
        ))
        .send()
        .await
        .expect("list tags request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.expect("invalid json");
    let tags = body["tags"]
        .as_array()
        .expect("tags should be an array");

    assert_eq!(tags.len(), 2, "expected 2 tags, got {:?}", tags);
    let tag_names: Vec<&str> = tags.iter().filter_map(|t| t.as_str()).collect();
    assert!(
        tag_names.contains(&"v1.0"),
        "tags should contain v1.0: {:?}",
        tag_names
    );
    assert!(
        tag_names.contains(&"latest"),
        "tags should contain latest: {:?}",
        tag_names
    );
    assert!(
        resp_link(&client, &base_url, "/v2/oci-private/myapp/tags/list").await.1.is_none(),
        "a complete listing carries no Link"
    );

    // A partial page carries the cursor to the next one.
    let (body, link) = resp_link(&client, &base_url, "/v2/oci-private/myapp/tags/list?n=1").await;
    assert_eq!(body["tags"], json!(["latest"]));
    assert_eq!(
        link.as_deref(),
        Some("</v2/oci-private/myapp/tags/list?n=1&last=latest>; rel=\"next\"")
    );
    let (body, link) = resp_link(&client, &base_url, "/v2/oci-private/myapp/tags/list?n=1&last=latest").await;
    assert_eq!(body["tags"], json!(["v1.0"]));
    assert!(link.is_none(), "the last page has no next");
}

/// `(body, Link header)` of a tag listing.
async fn resp_link(client: &reqwest::Client, base_url: &str, path: &str) -> (Value, Option<String>) {
    let resp = client
        .get(format!("{base_url}{path}"))
        .send()
        .await
        .expect("list tags request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    let link = resp
        .headers()
        .get("link")
        .map(|v| v.to_str().unwrap().to_string());
    (resp.json().await.expect("invalid json"), link)
}

#[tokio::test]
async fn test_oci_head_blob() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let blob_data = b"test-blob-content-for-head-check";
    let digest = push_blob(&client, &base_url, "oci-private/myapp", blob_data).await;

    // HEAD the blob (anonymous read includes HEAD)
    let resp = client
        .head(format!(
            "{}/v2/oci-private/myapp/blobs/{}",
            base_url, digest
        ))
        .send()
        .await
        .expect("head blob request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    // Verify headers
    let content_digest = resp
        .headers()
        .get("docker-content-digest")
        .expect("missing Docker-Content-Digest header")
        .to_str()
        .expect("invalid digest header");
    assert_eq!(content_digest, digest);

    let content_length = resp
        .headers()
        .get("content-length")
        .expect("missing Content-Length header")
        .to_str()
        .expect("invalid content-length");
    assert_eq!(
        content_length,
        blob_data.len().to_string(),
        "content-length mismatch"
    );

    // HEAD a non-existent blob should return 404
    let resp = client
        .head(format!(
            "{}/v2/oci-private/myapp/blobs/sha256:0000000000000000000000000000000000000000000000000000000000000000",
            base_url
        ))
        .send()
        .await
        .expect("head blob request failed");

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_oci_manifest_by_digest() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let config_data = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}";
    let layer_data = b"layer-data-for-digest-test";

    let config_digest = push_blob(&client, &base_url, "oci-private/myapp", config_data).await;
    let layer_digest = push_blob(&client, &base_url, "oci-private/myapp", layer_data).await;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_data.len()
        },
        "layers": [
            {
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": layer_digest,
                "size": layer_data.len()
            }
        ]
    });

    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_digest = sha256_digest(&manifest_bytes);

    // Push manifest with tag "v2.0"
    let resp = client
        .put(format!(
            "{}/v2/oci-private/myapp/manifests/v2.0",
            base_url
        ))
        .bearer_auth("test-token")
        .header(
            "Content-Type",
            "application/vnd.oci.image.manifest.v1+json",
        )
        .body(manifest_bytes.clone())
        .send()
        .await
        .expect("put manifest request failed");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Retrieve the manifest by its digest (not by tag)
    let resp = client
        .get(format!(
            "{}/v2/oci-private/myapp/manifests/{}",
            base_url, manifest_digest
        ))
        .send()
        .await
        .expect("get manifest by digest request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let pulled_digest = resp
        .headers()
        .get("docker-content-digest")
        .expect("missing Docker-Content-Digest header")
        .to_str()
        .expect("invalid digest header")
        .to_string();
    assert_eq!(pulled_digest, manifest_digest);

    let pulled_manifest: Value = resp.json().await.expect("invalid json");
    assert_eq!(pulled_manifest, manifest);
}

/// Hostile image names and tags must be rejected with 400 at manifest push:
/// the image name lands in DB rows and storage paths, the tag in oci_tags.
#[tokio::test]
async fn test_oci_push_rejects_invalid_name_and_tag() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","size":0,"digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000"},"layers":[]}"#;

    // Uppercase image name violates the distribution-spec name rule.
    let resp = client
        .put(format!(
            "{}/v2/oci-private/MyApp/manifests/latest",
            base_url
        ))
        .bearer_auth("test-token")
        .header("content-type", "application/vnd.oci.image.manifest.v1+json")
        .body(manifest.to_vec())
        .send()
        .await
        .expect("put manifest request failed");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "an uppercase OCI image name must be rejected with 400"
    );

    // Leading-dot tag violates the distribution-spec tag rule.
    let resp = client
        .put(format!(
            "{}/v2/oci-private/myapp/manifests/.badtag",
            base_url
        ))
        .bearer_auth("test-token")
        .header("content-type", "application/vnd.oci.image.manifest.v1+json")
        .body(manifest.to_vec())
        .send()
        .await
        .expect("put manifest request failed");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a leading-dot OCI tag must be rejected with 400"
    );

    // Starting an upload with a hostile image name fails as early as possible.
    let resp = client
        .post(format!(
            "{}/v2/oci-private/Evil..Image/blobs/uploads/",
            base_url
        ))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("start upload request failed");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a hostile image name must be rejected at upload start"
    );
}

async fn start(client: &reqwest::Client, base_url: &str, image: &str) -> String {
    let resp = client
        .post(format!("{base_url}/v2/{image}/blobs/uploads/"))
        .bearer_auth("test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert!(resp.headers().contains_key("oci-chunk-min-length"));
    resp.headers()["location"].to_str().unwrap().to_string()
}

/// A chunk that does not start where the upload stands is a 416 carrying
/// the range held so far; the status route reports it; an unknown id is a
/// 404 in the distribution's error shape.
#[tokio::test]
async fn upload_ranges_status_and_unknown_ids() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();
    let location = start(&client, &base_url, "oci-private/myapp").await;

    for want in [StatusCode::ACCEPTED, StatusCode::RANGE_NOT_SATISFIABLE] {
        let resp = client
            .patch(format!("{base_url}{location}"))
            .bearer_auth("test-token")
            .header("content-range", "0-3")
            .body(b"abcd".to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), want);
        assert_eq!(resp.headers()["range"], "0-3");
        if want == StatusCode::RANGE_NOT_SATISFIABLE {
            let body: Value = resp.json().await.unwrap();
            assert_eq!(body["errors"][0]["code"], "BLOB_UPLOAD_INVALID");
        }
    }

    let resp = client
        .get(format!("{base_url}{location}"))
        .bearer_auth("test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(resp.headers()["range"], "0-3");

    let resp = client
        .patch(format!("{base_url}/v2/oci-private/myapp/blobs/uploads/no-such-id"))
        .bearer_auth("test-token")
        .body(b"x".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["errors"][0]["code"], "BLOB_UPLOAD_UNKNOWN");

    let digest = sha256_digest(b"abcdef");
    let resp = client
        .put(format!("{base_url}{location}?digest={digest}"))
        .bearer_auth("test-token")
        .body(b"ef".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "{:?}", resp.text().await);
    let resp = client
        .get(format!("{base_url}/v2/oci-private/myapp/blobs/{digest}"))
        .bearer_auth("test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"abcdef");
}

/// A manifest may only list blobs the repository holds.
#[tokio::test]
async fn a_manifest_with_an_unknown_blob_is_refused() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": sha256_digest(b"never"),
            "size": 5
        },
        "layers": []
    });
    let resp = client
        .put(format!("{base_url}/v2/oci-private/myapp/manifests/v1"))
        .bearer_auth("test-token")
        .header("content-type", "application/vnd.oci.image.manifest.v1+json")
        .body(serde_json::to_vec(&manifest).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["errors"][0]["code"], "MANIFEST_BLOB_UNKNOWN");
}
