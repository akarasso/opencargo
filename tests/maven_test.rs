mod common;

use reqwest::StatusCode;
use serde_json::json;
use sha1::Digest as _;

use common::{
    basic_auth_header, hosted, seed_error, spawn_server, SpawnOpts, TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

const DIR: &str = "org/example/lib";

fn sha1_hex(bytes: &[u8]) -> String {
    format!("{:x}", sha1::Sha1::digest(bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

async fn server(visibility: Visibility) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![hosted("releases", RepositoryFormat::Maven, visibility)],
        ..Default::default()
    })
    .await
}

fn url(s: &TestServer, path: &str) -> String {
    format!("{}/maven/releases/{path}", s.base_url)
}

async fn put(s: &TestServer, path: &str, body: impl Into<reqwest::Body>) -> reqwest::Response {
    reqwest::Client::new()
        .put(url(s, path))
        .bearer_auth(STATIC_TOKEN)
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn get(s: &TestServer, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(url(s, path))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap()
}

async fn text(s: &TestServer, path: &str) -> String {
    let resp = get(s, path).await;
    assert_eq!(resp.status(), StatusCode::OK, "GET {path}");
    resp.text().await.unwrap()
}

const POM: &str = r#"<project><groupId>org.example</groupId><artifactId>lib</artifactId><version>1.0</version></project>"#;

#[tokio::test]
async fn a_repository_named_maven_is_refused_at_the_seed_and_by_the_api() {
    let err = seed_error(vec![hosted("maven", RepositoryFormat::Npm, Visibility::Public)]).await;
    assert!(err.contains("reserved"), "{err}");

    let server = spawn_server(SpawnOpts::default()).await;
    let client = reqwest::Client::new();
    let create = |name: &'static str| {
        client
            .post(format!("{}/api/v1/repositories", server.base_url))
            .bearer_auth(STATIC_TOKEN)
            .json(&json!({"name": name, "type": "hosted", "format": "maven"}))
            .send()
    };
    let refused = create("maven").await.unwrap();
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let created = create("maven-releases").await.unwrap();
    assert_eq!(created.status(), StatusCode::CREATED, "{:?}", created.text().await);
}

#[tokio::test]
async fn a_release_is_deployed_and_resolved_with_computed_checksums() {
    let s = server(Visibility::Public).await;
    let jar = b"jar bytes".to_vec();
    assert_eq!(put(&s, &format!("{DIR}/1.0/lib-1.0.jar"), jar.clone()).await.status(), StatusCode::CREATED);
    assert_eq!(get(&s, &format!("{DIR}/1.0/lib-1.0.jar")).await.status(), StatusCode::NOT_FOUND, "no POM yet");
    assert_eq!(put(&s, &format!("{DIR}/1.0/lib-1.0.jar.sha1"), sha1_hex(&jar)).await.status(), StatusCode::CREATED);
    assert_eq!(put(&s, &format!("{DIR}/1.0/lib-1.0.pom"), POM).await.status(), StatusCode::CREATED);
    assert_eq!(put(&s, &format!("{DIR}/1.0/lib-1.0.jar"), jar.clone()).await.status(), StatusCode::OK, "same bytes");

    let client_doc = "<metadata><groupId>org.example</groupId><artifactId>lib</artifactId>\
        <versioning><release>1.0</release><versions><version>1.0</version><version>9.9</version></versions></versioning></metadata>";
    assert_eq!(put(&s, &format!("{DIR}/maven-metadata.xml"), client_doc).await.status(), StatusCode::CREATED);
    assert_eq!(
        put(&s, &format!("{DIR}/maven-metadata.xml.sha1"), sha1_hex(client_doc.as_bytes())).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        put(&s, &format!("{DIR}/maven-metadata.xml.sha1"), "0".repeat(40)).await.status(),
        StatusCode::BAD_REQUEST
    );

    let served = get(&s, &format!("{DIR}/1.0/lib-1.0.jar")).await;
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(served.bytes().await.unwrap().as_ref(), jar.as_slice());
    assert_eq!(text(&s, &format!("{DIR}/1.0/lib-1.0.jar.sha256")).await, sha256_hex(&jar));

    let metadata = get(&s, &format!("{DIR}/maven-metadata.xml")).await;
    let etag = metadata.headers()["etag"].to_str().unwrap().to_string();
    let body = metadata.text().await.unwrap();
    assert!(body.contains("<version>1.0</version>") && !body.contains("9.9"), "files decide: {body}");
    assert!(body.contains("<release>1.0</release>"));
    assert_eq!(text(&s, &format!("{DIR}/maven-metadata.xml.sha1")).await, sha1_hex(body.as_bytes()));
    let again = reqwest::Client::new()
        .get(url(&s, &format!("{DIR}/maven-metadata.xml")))
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::NOT_MODIFIED);

    let head = reqwest::Client::new().head(url(&s, &format!("{DIR}/1.0/lib-1.0.pom"))).send().await.unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()["content-length"], POM.len().to_string().as_str());
}

#[tokio::test]
async fn a_wrong_checksum_is_refused_and_nothing_it_carries_becomes_visible() {
    let s = server(Visibility::Public).await;
    let jar = b"jar bytes".to_vec();
    assert_eq!(put(&s, &format!("{DIR}/1.0/lib-1.0.jar.sha1"), sha1_hex(b"other")).await.status(), StatusCode::CREATED);
    assert_eq!(put(&s, &format!("{DIR}/1.0/lib-1.0.jar"), jar).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(put(&s, &format!("{DIR}/1.0/lib-1.0.jar.md5"), "zz").await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(put(&s, &format!("{DIR}/1.0/lib-1.0.pom"), POM).await.status(), StatusCode::CREATED);
    assert_eq!(get(&s, &format!("{DIR}/1.0/lib-1.0.jar")).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(get(&s, &format!("{DIR}/1.0/lib-1.0.pom")).await.status(), StatusCode::NOT_FOUND, "a declaration still waits");
}

#[tokio::test]
async fn a_private_repository_challenges_with_basic_and_a_miss_is_a_plain_404() {
    let s = server(Visibility::Private).await;
    let client = reqwest::Client::new();
    let anonymous = client.get(url(&s, &format!("{DIR}/maven-metadata.xml"))).send().await.unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert!(anonymous.headers()["www-authenticate"].to_str().unwrap().starts_with("Basic"));
    let unauthenticated_put = client.put(url(&s, &format!("{DIR}/1.0/lib-1.0.pom"))).body(POM).send().await.unwrap();
    assert_eq!(unauthenticated_put.status(), StatusCode::UNAUTHORIZED);
    assert!(unauthenticated_put.headers().contains_key("www-authenticate"));

    let miss = client
        .get(url(&s, &format!("{DIR}/maven-metadata.xml")))
        .header("authorization", basic_auth_header("deployer", STATIC_TOKEN))
        .send()
        .await
        .unwrap();
    assert_eq!(miss.status(), StatusCode::NOT_FOUND);
    assert!(!miss.headers().contains_key("www-authenticate"));
    let deployed = client
        .put(url(&s, &format!("{DIR}/1.0/lib-1.0.pom")))
        .header("authorization", basic_auth_header("deployer", STATIC_TOKEN))
        .body(POM)
        .send()
        .await
        .unwrap();
    assert_eq!(deployed.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn a_snapshot_build_is_announced_with_its_pom_and_never_mixed() {
    let s = server(Visibility::Public).await;
    let v = "1.0-SNAPSHOT";
    for (file, body) in [("lib-1.0-20260918.120000-1.jar", "jar-1"), ("lib-1.0-20260918.120000-1.pom", POM)] {
        assert_eq!(put(&s, &format!("{DIR}/{v}/{file}"), body).await.status(), StatusCode::CREATED);
    }
    assert_eq!(put(&s, &format!("{DIR}/{v}/lib-1.0-20260918.130000-2.jar"), "jar-2").await.status(), StatusCode::CREATED);
    let doc = text(&s, &format!("{DIR}/{v}/maven-metadata.xml")).await;
    assert!(doc.contains("<buildNumber>1</buildNumber>") && !doc.contains("130000-2"), "{doc}");
    assert_eq!(put(&s, &format!("{DIR}/{v}/lib-1.0-20260918.130000-2.pom"), POM).await.status(), StatusCode::CREATED);
    let doc = text(&s, &format!("{DIR}/{v}/maven-metadata.xml")).await;
    assert!(doc.contains("<buildNumber>2</buildNumber>") && !doc.contains("120000-1"), "{doc}");
    assert_eq!(text(&s, &format!("{DIR}/{v}/lib-1.0-20260918.130000-2.jar")).await, "jar-2");
    let artifact = text(&s, &format!("{DIR}/maven-metadata.xml")).await;
    assert!(artifact.contains("<version>1.0-SNAPSHOT</version>") && !artifact.contains("<release>"), "{artifact}");
}
