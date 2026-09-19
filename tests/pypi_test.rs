//! A hosted PyPI repository over HTTP: the legacy upload, the Simple API in
//! both flavors, files and their `.metadata`, yank and delete, and the Basic
//! challenge only PyPI routes carry.

mod common;

use reqwest::{Client, StatusCode};
use serde_json::Value;
use sha2::Digest;

use common::pypi::{basic, sdist, sdist_name, upload, wheel, wheel_name};
use common::{
    add_scoped_token, create_user, hosted, repo_scope, spawn_server, SpawnOpts, TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

const JSON_V1: &str = "application/vnd.pypi.simple.v1+json";

fn token() -> String {
    basic("__token__", STATIC_TOKEN)
}

async fn server() -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![
            hosted("py", RepositoryFormat::Pypi, Visibility::Public),
            hosted("py-private", RepositoryFormat::Pypi, Visibility::Private),
            hosted("npm", RepositoryFormat::Npm, Visibility::Private),
        ],
        ..Default::default()
    })
    .await
}

async fn publish_demo(s: &TestServer, client: &Client) -> (Vec<u8>, Vec<u8>) {
    let w = wheel("Demo.Pkg", "1.0", Some(">=3.8"));
    let sd = sdist("Demo.Pkg", "1.0");
    for (name, body) in [(wheel_name("Demo.Pkg", "1.0"), &w), (sdist_name("Demo.Pkg", "1.0"), &sd)] {
        let resp = upload(client, &s.base_url, "py", &token(), &name, body).await;
        assert_eq!(resp.status(), StatusCode::OK, "{name}: {}", resp.text().await.unwrap());
    }
    (w, sd)
}

#[tokio::test]
async fn an_uploaded_release_is_listed_in_both_flavors_and_served() {
    let s = server().await;
    let client = Client::new();
    let (w, _) = publish_demo(&s, &client).await;

    let index = client.get(format!("{}/py/simple/", s.base_url)).send().await.unwrap();
    assert_eq!(index.status(), StatusCode::OK);
    assert!(index.text().await.unwrap().contains("<a href=\"demo-pkg/\">demo-pkg</a>"));

    let page = client.get(format!("{}/py/simple/demo-pkg/", s.base_url)).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    let html = page.text().await.unwrap();
    let sha = format!("{:x}", sha2::Sha256::digest(&w));
    assert!(html.contains(&format!("href=\"../../files/demo-pkg/demo_pkg-1.0-py3-none-any.whl#sha256={sha}\"")), "{html}");
    assert!(html.contains("data-requires-python=\"&gt;=3.8\""));
    assert!(html.contains("data-core-metadata=\"sha256="));
    assert!(html.contains("data-dist-info-metadata=\"sha256="));

    let json: Value = client
        .get(format!("{}/py/simple/demo-pkg/", s.base_url))
        .header("Accept", JSON_V1)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(json["meta"]["api-version"], "1.1");
    assert_eq!(json["name"], "demo-pkg");
    assert_eq!(json["versions"], serde_json::json!(["1"]));
    let files = json["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    let whl = files.iter().find(|f| f["filename"] == "demo_pkg-1.0-py3-none-any.whl").unwrap();
    assert_eq!(whl["hashes"]["sha256"], sha.as_str());
    assert_eq!(whl["core-metadata"], whl["dist-info-metadata"]);
    let sd = files.iter().find(|f| f["filename"] == "demo_pkg-1.0.tar.gz").unwrap();
    assert_eq!(sd["core-metadata"], false);

    let bytes = client
        .get(format!("{}/py/files/demo-pkg/demo_pkg-1.0-py3-none-any.whl", s.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(bytes.status(), StatusCode::OK);
    assert_eq!(bytes.bytes().await.unwrap().as_ref(), w.as_slice());
    let metadata = client
        .get(format!("{}/py/files/demo-pkg/demo_pkg-1.0-py3-none-any.whl.metadata", s.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(metadata.status(), StatusCode::OK);
    let body = metadata.bytes().await.unwrap();
    assert!(body.starts_with(b"Metadata-Version"));
    let announced = whl["core-metadata"]["sha256"].as_str().unwrap();
    assert_eq!(format!("{:x}", sha2::Sha256::digest(&body)), announced);
    let none = client
        .get(format!("{}/py/files/demo-pkg/demo_pkg-1.0.tar.gz.metadata", s.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(none.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn negotiation_redirects_and_validators() {
    let s = server().await;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    publish_demo(&s, &client).await;
    let url = format!("{}/py/simple/demo-pkg/", s.base_url);

    let refused = client.get(&url).header("Accept", "application/json").send().await.unwrap();
    assert_eq!(refused.status(), StatusCode::NOT_ACCEPTABLE);

    let first = client.get(&url).send().await.unwrap();
    let etag = first.headers()["etag"].to_str().unwrap().to_string();
    let again = client.get(&url).header("If-None-Match", &etag).send().await.unwrap();
    assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
    let json = client.get(&url).header("Accept", JSON_V1).header("If-None-Match", &etag).send().await.unwrap();
    assert_eq!(json.status(), StatusCode::OK, "the JSON body has its own validator");

    for (from, to) in [
        ("/py/simple/Demo_Pkg/", "/py/simple/demo-pkg/"),
        ("/py/simple/demo-pkg", "/py/simple/demo-pkg/"),
    ] {
        let resp = client.get(format!("{}{from}", s.base_url)).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT, "{from}");
        assert_eq!(resp.headers()["location"], to);
    }
    let missing = client.get(format!("{}/py/simple/nope/", s.base_url)).send().await.unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_reupload_is_idempotent_and_other_bytes_conflict() {
    let s = server().await;
    let client = Client::new();
    let (w, _) = publish_demo(&s, &client).await;
    let name = wheel_name("Demo.Pkg", "1.0");
    let again = upload(&client, &s.base_url, "py", &token(), &name, &w).await;
    assert_eq!(again.status(), StatusCode::OK, "the same bytes are accepted again");

    let other = wheel("Demo.Pkg", "1.0", None);
    let refused = upload(&client, &s.base_url, "py", &token(), &name, &other).await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert!(refused.text().await.unwrap().contains("already exists"));

    let equivalent = upload(&client, &s.base_url, "py", &token(), "Demo.Pkg-1.0-py3-none-ANY.whl", &wheel("Demo.Pkg", "1.0", None)).await;
    assert_eq!(equivalent.status(), StatusCode::CONFLICT, "an equivalent filename is the same file");
}

#[tokio::test]
async fn the_archive_is_the_only_source_of_name_and_version() {
    let s = server().await;
    let client = Client::new();
    let lying = upload(&client, &s.base_url, "py", &token(), &wheel_name("other", "1.0"), &wheel("demo", "1.0", None)).await;
    assert_eq!(lying.status(), StatusCode::BAD_REQUEST);
    let wrong = upload(&client, &s.base_url, "py", &token(), &wheel_name("demo", "2.0"), &wheel("demo", "1.0", None)).await;
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    let junk = upload(&client, &s.base_url, "py", &token(), "demo-1.0.tar.gz", b"not an archive").await;
    assert_eq!(junk.status(), StatusCode::BAD_REQUEST);
    let exe = upload(&client, &s.base_url, "py", &token(), "demo-1.0.exe", b"x").await;
    assert_eq!(exe.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pypi_routes_challenge_with_basic_and_others_do_not() {
    let s = server().await;
    let client = Client::new();
    let anonymous = upload(&client, &s.base_url, "py", "", &sdist_name("demo", "1.0"), &sdist("demo", "1.0")).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert!(anonymous.headers()["www-authenticate"].to_str().unwrap().starts_with("Basic"));

    let unknown = upload(&client, &s.base_url, "py", &basic("__token__", "trg_nope"), &sdist_name("demo", "1.0"), &sdist("demo", "1.0")).await;
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED, "an unknown token is a 401, never anonymous");
    assert!(unknown.headers()["www-authenticate"].to_str().unwrap().starts_with("Basic"));

    let private = client.get(format!("{}/py-private/simple/", s.base_url)).send().await.unwrap();
    assert_eq!(private.status(), StatusCode::UNAUTHORIZED);
    assert!(private.headers()["www-authenticate"].to_str().unwrap().starts_with("Basic"));
    let with_token = client
        .get(format!("{}/py-private/simple/", s.base_url))
        .header("Authorization", token())
        .send()
        .await
        .unwrap();
    assert_eq!(with_token.status(), StatusCode::OK);

    for path in ["/npm/left-pad", "/api/v1/users", "/v2/"] {
        let resp = client.get(format!("{}{path}", s.base_url)).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{path}");
        let challenge = resp.headers().get("www-authenticate").map(|v| v.to_str().unwrap().to_string());
        assert!(!challenge.unwrap_or_default().starts_with("Basic"), "{path} gains no Basic challenge");
    }
}

/// A yank hides a release without destroying it and unyank restores it, so
/// both are the write rung; removing the release is the delete rung.
#[tokio::test]
async fn yank_is_the_write_rung_and_deletion_is_not() {
    let s = server().await;
    let client = Client::new();
    create_user(&client, &s.base_url, STATIC_TOKEN, "ci", "publisher").await;
    let writer = add_scoped_token(&client, &s.base_url, "ci", "writer", repo_scope("py", &["read", "write"])).await;
    let auth = basic("__token__", &writer);
    let uploaded = upload(&client, &s.base_url, "py", &auth, &sdist_name("demo", "1.0"), &sdist("demo", "1.0")).await;
    assert_eq!(uploaded.status(), StatusCode::OK, "{}", uploaded.text().await.unwrap());

    let release = format!("{}/py/pypi/demo/1.0", s.base_url);
    let yanked = client.post(format!("{release}/yank")).header("Authorization", &auth).send().await.unwrap();
    assert_eq!(yanked.status(), StatusCode::OK);
    let unyanked = client.delete(format!("{release}/yank")).header("Authorization", &auth).send().await.unwrap();
    assert_eq!(unyanked.status(), StatusCode::OK);

    let refused = client.delete(&release).header("Authorization", &auth).send().await.unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(refused.json::<Value>().await.unwrap()["code"], "insufficient_scope");
}

#[tokio::test]
async fn yank_unyank_and_delete_act_on_the_release() {
    let s = server().await;
    let client = Client::new();
    publish_demo(&s, &client).await;
    let page = || async {
        client
            .get(format!("{}/py/simple/demo-pkg/", s.base_url))
            .header("Accept", JSON_V1)
            .send()
            .await
            .unwrap()
    };
    let yank = client
        .post(format!("{}/py/pypi/Demo.Pkg/1.0.0/yank", s.base_url))
        .header("Authorization", token())
        .body(r#"{"reason":"broken"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(yank.status(), StatusCode::OK);
    let json: Value = page().await.json().await.unwrap();
    assert!(json["files"].as_array().unwrap().iter().all(|f| f["yanked"] == "broken"));
    let unyank = client
        .delete(format!("{}/py/pypi/demo-pkg/1.0/yank", s.base_url))
        .header("Authorization", token())
        .send()
        .await
        .unwrap();
    assert_eq!(unyank.status(), StatusCode::OK);
    let json: Value = page().await.json().await.unwrap();
    assert!(json["files"].as_array().unwrap().iter().all(|f| f["yanked"] == false));

    let anonymous = client.delete(format!("{}/py/pypi/demo-pkg/1.0", s.base_url)).send().await.unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let deleted = client
        .delete(format!("{}/py/pypi/demo-pkg/1.0", s.base_url))
        .header("Authorization", token())
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::OK);
    assert_eq!(page().await.status(), StatusCode::NOT_FOUND);
    let gone = client
        .get(format!("{}/py/files/demo-pkg/demo_pkg-1.0.tar.gz", s.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
}
