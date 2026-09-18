mod common;

use reqwest::StatusCode;
use serde_json::json;

use common::{hosted, seed_error, spawn_server, SpawnOpts, STATIC_TOKEN};
use opencargo::config::{RepositoryFormat, Visibility};

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
