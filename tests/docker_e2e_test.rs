mod common;

use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::{basic_auth_header, create_user, hosted, sha256_digest, spawn_server, SpawnOpts};
use opencargo::config::{RepositoryFormat, Visibility};

// ---------------------------------------------------------------------------
// Setup helpers
// ---------------------------------------------------------------------------

/// Start a test server with a private OCI repository.
/// `anonymous_read` controls whether unauthenticated GET/HEAD is allowed.
async fn setup_with_anon(anonymous_read: bool) -> (String, tokio::task::JoinHandle<()>, TempDir) {
    let server = spawn_server(SpawnOpts {
        anonymous_read,
        repositories: vec![hosted("oci-private", RepositoryFormat::Oci, Visibility::Private)],
        ..Default::default()
    })
    .await;
    (server.base_url, server.handle, server.tmp)
}

/// Start a test server with anonymous_read = true.
async fn setup() -> (String, tokio::task::JoinHandle<()>, TempDir) {
    setup_with_anon(true).await
}

/// Change a user's password via admin API.
async fn change_password_as_admin(
    client: &reqwest::Client,
    base_url: &str,
    admin_token: &str,
    username: &str,
    new_password: &str,
) {
    let resp = client
        .put(format!("{}/api/v1/users/{}/password", base_url, username))
        .bearer_auth(admin_token)
        .json(&json!({
            "new_password": new_password
        }))
        .send()
        .await
        .expect("change password request failed");

    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .expect("invalid json from change password");
    assert_eq!(
        status,
        StatusCode::OK,
        "change password failed: {:?}",
        body
    );
    assert_eq!(body["ok"], true);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full Docker push/pull flow using Basic Auth.
#[tokio::test]
async fn test_docker_push_pull_with_basic_auth() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create a user "docker-user" with role "publisher"
    create_user(&client, &base_url, "test-token", "docker-user", "publisher").await;

    // Change the password to something known (also clears must_change_password)
    let password = "docker-pass-123";
    change_password_as_admin(&client, &base_url, "test-token", "docker-user", password).await;

    let auth_value = basic_auth_header("docker-user", password);

    // Step 1: GET /v2/ with Basic Auth -> 200
    let resp = client
        .get(format!("{}/v2/", base_url))
        .header("Authorization", &auth_value)
        .send()
        .await
        .expect("v2 check request failed");
    assert_eq!(resp.status(), StatusCode::OK, "GET /v2/ should return 200");

    // Step 2: POST /v2/oci-private/myapp/blobs/uploads/ with Basic Auth -> 202
    let resp = client
        .post(format!("{}/v2/oci-private/myapp/blobs/uploads/", base_url))
        .header("Authorization", &auth_value)
        .send()
        .await
        .expect("start upload request failed");
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "start upload should return 202"
    );

    let location = resp
        .headers()
        .get("location")
        .expect("missing Location header")
        .to_str()
        .expect("invalid location header")
        .to_string();

    // Step 3: PUT the upload URL with blob data + digest -> 201
    let blob_data = b"fake-layer-data-for-docker-e2e-test";
    let blob_digest = sha256_digest(blob_data);
    let upload_url = format!("{}{}?digest={}", base_url, location, blob_digest);

    let resp = client
        .put(&upload_url)
        .header("Authorization", &auth_value)
        .body(blob_data.to_vec())
        .send()
        .await
        .expect("complete upload request failed");
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "complete upload should return 201: {:?}",
        resp.text().await
    );

    // Push a config blob too
    let config_data = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}";
    let config_digest = sha256_digest(config_data);

    let resp = client
        .post(format!("{}/v2/oci-private/myapp/blobs/uploads/", base_url))
        .header("Authorization", &auth_value)
        .send()
        .await
        .expect("start config upload request failed");
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let config_location = resp
        .headers()
        .get("location")
        .expect("missing Location header")
        .to_str()
        .expect("invalid location header")
        .to_string();

    let config_upload_url = format!("{}{}?digest={}", base_url, config_location, config_digest);
    let resp = client
        .put(&config_upload_url)
        .header("Authorization", &auth_value)
        .body(config_data.to_vec())
        .send()
        .await
        .expect("complete config upload request failed");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Step 4: PUT manifest with tag "v1.0"
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
                "digest": blob_digest,
                "size": blob_data.len()
            }
        ]
    });

    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();

    let resp = client
        .put(format!(
            "{}/v2/oci-private/myapp/manifests/v1.0",
            base_url
        ))
        .header("Authorization", &auth_value)
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
        "put manifest should return 201: {:?}",
        resp.text().await
    );

    // ---- Pull flow with Basic Auth ----

    // Step 5a: GET manifest by tag -> 200
    let resp = client
        .get(format!(
            "{}/v2/oci-private/myapp/manifests/v1.0",
            base_url
        ))
        .header("Authorization", &auth_value)
        .send()
        .await
        .expect("get manifest request failed");
    assert_eq!(resp.status(), StatusCode::OK, "get manifest should return 200");

    let pulled_manifest: Value = resp.json().await.expect("invalid json");
    assert_eq!(pulled_manifest, manifest);

    // Step 5b: GET blob -> 200, correct data
    let resp = client
        .get(format!(
            "{}/v2/oci-private/myapp/blobs/{}",
            base_url, blob_digest
        ))
        .header("Authorization", &auth_value)
        .send()
        .await
        .expect("get blob request failed");
    assert_eq!(resp.status(), StatusCode::OK, "get blob should return 200");

    let pulled_blob = resp.bytes().await.expect("failed to read blob");
    assert_eq!(
        pulled_blob.as_ref(),
        blob_data,
        "pulled blob data should match"
    );

    // Step 5c: GET tags list -> includes "v1.0"
    let resp = client
        .get(format!(
            "{}/v2/oci-private/myapp/tags/list",
            base_url
        ))
        .header("Authorization", &auth_value)
        .send()
        .await
        .expect("list tags request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.expect("invalid json");
    let tags = body["tags"]
        .as_array()
        .expect("tags should be an array");
    let tag_names: Vec<&str> = tags.iter().filter_map(|t| t.as_str()).collect();
    assert!(
        tag_names.contains(&"v1.0"),
        "tags should contain v1.0: {:?}",
        tag_names
    );
}

/// When anonymous_read is false, GET /v2/ without auth should return 401
/// with a Www-Authenticate header.
#[tokio::test]
async fn test_docker_auth_required() {
    let (base_url, _handle, _tmp) = setup_with_anon(false).await;
    let client = reqwest::Client::new();

    // GET /v2/ without auth -> 401
    let resp = client
        .get(format!("{}/v2/", base_url))
        .send()
        .await
        .expect("request failed");

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "GET /v2/ without auth should return 401"
    );

    // Check for Www-Authenticate header
    let www_auth = resp
        .headers()
        .get("www-authenticate")
        .expect("missing Www-Authenticate header on 401 response");
    let www_auth_str = www_auth.to_str().expect("invalid Www-Authenticate header");
    assert_eq!(
        www_auth_str,
        format!("Bearer realm=\"{base_url}/v2/token\",service=\"opencargo\""),
        "Www-Authenticate should point at the token endpoint"
    );
}

/// Basic Auth with wrong password should return 401.
#[tokio::test]
async fn test_docker_basic_auth_wrong_password() {
    let (base_url, _handle, _tmp) = setup_with_anon(false).await;
    let client = reqwest::Client::new();

    // Create a user and set a known password
    create_user(&client, &base_url, "test-token", "docker-user", "publisher").await;
    change_password_as_admin(&client, &base_url, "test-token", "docker-user", "correct-password")
        .await;

    // Try with wrong password
    let bad_auth = basic_auth_header("docker-user", "wrong-password");
    let resp = client
        .get(format!("{}/v2/", base_url))
        .header("Authorization", &bad_auth)
        .send()
        .await
        .expect("request failed");

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "Basic Auth with wrong password should return 401"
    );
}

/// A reader user should not be able to push (start upload returns 403).
#[tokio::test]
async fn test_docker_basic_auth_reader_cannot_push() {
    let (base_url, _handle, _tmp) = setup().await;
    let client = reqwest::Client::new();

    // Create a reader user
    create_user(&client, &base_url, "test-token", "reader-user", "reader").await;
    change_password_as_admin(&client, &base_url, "test-token", "reader-user", "reader-pass").await;

    let reader_auth = basic_auth_header("reader-user", "reader-pass");

    // Reader can authenticate to /v2/
    let resp = client
        .get(format!("{}/v2/", base_url))
        .header("Authorization", &reader_auth)
        .send()
        .await
        .expect("request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "reader should be able to access /v2/"
    );

    // Reader should NOT be able to start a push (POST upload)
    let resp = client
        .post(format!(
            "{}/v2/oci-private/myapp/blobs/uploads/",
            base_url
        ))
        .header("Authorization", &reader_auth)
        .send()
        .await
        .expect("start upload request failed");

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "reader should not be able to push: got {}",
        resp.status()
    );
}
