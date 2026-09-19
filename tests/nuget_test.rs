//! NuGet v3 hosted repositories over HTTP: push, flat container,
//! registration, unlist and relist, and the credentials a push accepts.

mod common;

use reqwest::StatusCode;
use serde_json::{json, Value};

use common::nuget::{nupkg, push, push_as};
use common::{
    add_scoped_token, add_token, basic_auth_header, create_user, hosted, repo_scope, spawn_server,
    SpawnOpts, TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

async fn setup() -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![
            hosted("nuget", RepositoryFormat::Nuget, Visibility::Public),
            hosted("npm", RepositoryFormat::Npm, Visibility::Public),
        ],
        ..Default::default()
    })
    .await
}

async fn get_json(client: &reqwest::Client, url: &str) -> (StatusCode, Value) {
    let resp = client.get(url).send().await.unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_pushed_package_is_served_by_every_v3_resource() {
    let server = setup().await;
    let c = reqwest::Client::new();
    let base = &server.base_url;
    let bytes = nupkg("My.Lib", "1.0.0-Beta", &[]);
    push(&c, base, "nuget", bytes.clone()).await;

    let (status, index) = get_json(&c, &format!("{base}/nuget/v3/index.json")).await;
    assert_eq!(status, StatusCode::OK);
    let types: Vec<&str> = index["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["@type"].as_str().unwrap())
        .collect();
    for wanted in ["PackageBaseAddress/3.0.0", "RegistrationsBaseUrl/3.6.0", "SearchQueryService", "PackagePublish/2.0.0"] {
        assert!(types.contains(&wanted), "{wanted}");
    }

    let (_, flat) = get_json(&c, &format!("{base}/nuget/v3/flatcontainer/my.lib/index.json")).await;
    assert_eq!(flat, json!({"versions": ["1.0.0-beta"]}));
    let resp = c
        .get(format!("{base}/nuget/v3/flatcontainer/My.Lib/1.0.0-BETA/my.lib.1.0.0-beta.nupkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), bytes.as_slice(), "stored byte for byte");
    let nuspec = c
        .get(format!("{base}/nuget/v3/flatcontainer/my.lib/1.0.0-beta/my.lib.nuspec"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(nuspec.contains("<id>My.Lib</id>"));

    let (status, reg) = get_json(&c, &format!("{base}/nuget/v3/registration/my.lib/index.json")).await;
    assert_eq!(status, StatusCode::OK);
    let leaf = &reg["items"][0]["items"][0];
    assert_eq!(leaf["catalogEntry"]["id"], "My.Lib", "the nuspec's case is data");
    assert_eq!(leaf["catalogEntry"]["version"], "1.0.0-Beta");
    assert_eq!(leaf["catalogEntry"]["listed"], true);
    assert!(leaf["packageContent"].as_str().unwrap().starts_with(base.as_str()));
    let (status, doc) = get_json(&c, &format!("{base}/nuget/v3/registration/my.lib/1.0.0-beta.json")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["listed"], true);

    let (status, _) = get_json(&c, &format!("{base}/nuget/v3/flatcontainer/nope/index.json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_equivalent_spelling_of_a_version_is_a_conflict() {
    let server = setup().await;
    let c = reqwest::Client::new();
    push(&c, &server.base_url, "nuget", nupkg("a", "1.0", &[])).await;
    for spelling in ["1.0.0.0", "1.0.0+build", "01.0.0"] {
        let status = push_as(&c, &server.base_url, "nuget", STATIC_TOKEN, nupkg("A", spelling, &[])).await;
        assert_eq!(status, StatusCode::CONFLICT, "{spelling}");
    }
    let (_, flat) = get_json(&c, &format!("{}/nuget/v3/flatcontainer/a/index.json", server.base_url)).await;
    assert_eq!(flat, json!({"versions": ["1.0.0"]}));
}

#[tokio::test]
async fn an_invalid_package_is_a_bad_request() {
    let server = setup().await;
    let c = reqwest::Client::new();
    let status = push_as(&c, &server.base_url, "nuget", STATIC_TOKEN, b"not a zip".to_vec()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let status = push_as(&c, &server.base_url, "npm", STATIC_TOKEN, nupkg("a", "1.0.0", &[])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a nupkg into an npm repository");
}

/// An unlist hides a version without destroying it and relist restores it,
/// so both are the write rung: the key that pushes a package unlists it.
#[tokio::test]
async fn unlist_and_relist_are_the_write_rung() {
    let server = setup().await;
    let c = reqwest::Client::new();
    let base = &server.base_url;
    create_user(&c, base, STATIC_TOKEN, "ci", "publisher").await;
    let writer = add_scoped_token(&c, base, "ci", "writer", repo_scope("nuget", &["read", "write"])).await;
    assert_eq!(push_as(&c, base, "nuget", &writer, nupkg("lib", "1.0.0", &[])).await, StatusCode::CREATED);

    let version = format!("{base}/nuget/v3/package/lib/1.0.0");
    let unlisted = c.delete(&version).header("X-NuGet-ApiKey", &writer).send().await.unwrap();
    assert_eq!(unlisted.status(), StatusCode::NO_CONTENT);
    let relisted = c.post(&version).header("X-NuGet-ApiKey", &writer).send().await.unwrap();
    assert_eq!(relisted.status(), StatusCode::OK);
}

#[tokio::test]
async fn unlist_hides_nothing_an_exact_version_asks_for() {
    let server = setup().await;
    let c = reqwest::Client::new();
    let base = &server.base_url;
    push(&c, base, "nuget", nupkg("lib", "1.0.0", &[])).await;
    push(&c, base, "nuget", nupkg("lib", "2.0.0", &[])).await;

    let resp = c
        .delete(format!("{base}/nuget/v3/package/lib/1.0.0"))
        .header("X-NuGet-ApiKey", STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let (_, flat) = get_json(&c, &format!("{base}/nuget/v3/flatcontainer/lib/index.json")).await;
    assert_eq!(flat, json!({"versions": ["1.0.0", "2.0.0"]}));
    let resp = c
        .get(format!("{base}/nuget/v3/flatcontainer/lib/1.0.0/lib.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "restorable by exact version");
    let (_, reg) = get_json(&c, &format!("{base}/nuget/v3/registration/lib/index.json")).await;
    let items = reg["items"][0]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "the index lists unlisted versions");
    assert_eq!(items[0]["catalogEntry"]["listed"], false);
    let (_, leaf) = get_json(&c, &format!("{base}/nuget/v3/registration/lib/1.0.0.json")).await;
    assert_eq!(leaf["listed"], false);

    let resp = c
        .post(format!("{base}/nuget/v3/package/lib/1.0.0"))
        .header("X-NuGet-ApiKey", STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let (_, leaf) = get_json(&c, &format!("{base}/nuget/v3/registration/lib/1.0.0.json")).await;
    assert_eq!(leaf["listed"], true);
}

#[tokio::test]
async fn every_version_is_indexed_beyond_the_inline_bound() {
    let server = setup().await;
    let c = reqwest::Client::new();
    let base = &server.base_url;
    for i in 0..130 {
        push(&c, base, "nuget", nupkg("big", &format!("1.0.{i}"), &[])).await;
    }
    let (_, reg) = get_json(&c, &format!("{base}/nuget/v3/registration/big/index.json")).await;
    let pages = reg["items"].as_array().unwrap();
    let total: u64 = pages.iter().map(|p| p["count"].as_u64().unwrap()).sum();
    assert_eq!(total, 130);
    assert!(pages[0].get("items").is_none(), "not inlined past the bound");
    let page_url = pages[1]["@id"].as_str().unwrap();
    let (status, page) = get_json(&c, page_url).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"].as_array().unwrap().len(), page["count"].as_u64().unwrap() as usize);
}

#[tokio::test]
async fn identical_concurrent_pushes_land_once() {
    let server = setup().await;
    let c = reqwest::Client::new();
    let base = &server.base_url;
    let bytes = nupkg("race", "1.0.0", &[]);
    let (a, b) = tokio::join!(
        push_as(&c, base, "nuget", STATIC_TOKEN, bytes.clone()),
        push_as(&c, base, "nuget", STATIC_TOKEN, bytes.clone())
    );
    let mut statuses = [a, b];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::CREATED, StatusCode::CONFLICT]);
    let resp = c
        .get(format!("{base}/nuget/v3/flatcontainer/race/1.0.0/race.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.bytes().await.unwrap().as_ref(), bytes.as_slice());
}

#[tokio::test]
async fn a_push_takes_its_api_key_as_primary_and_refuses_any_invalid_credential() {
    let server = setup().await;
    let c = reqwest::Client::new();
    let base = &server.base_url;
    create_user(&c, base, STATIC_TOKEN, "reader", "reader").await;
    let reader = add_token(&c, base, "reader", "r").await;
    create_user(&c, base, STATIC_TOKEN, "pusher", "publisher").await;
    let pusher = add_token(&c, base, "pusher", "p").await;

    let anonymous = c
        .put(format!("{base}/nuget/v3/package"))
        .multipart(common::nuget::form(nupkg("x", "1.0.0", &[])))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        anonymous.headers()["www-authenticate"],
        "Basic realm=\"opencargo\""
    );

    let both = |key: &str, basic: String| {
        c.put(format!("{base}/nuget/v3/package"))
            .header("X-NuGet-ApiKey", key.to_string())
            .header("Authorization", basic)
            .multipart(common::nuget::form(nupkg("x", "1.0.0", &[])))
    };
    let invalid = both("trg_notatoken_notatoken", basic_auth_header("pusher", &pusher)).send().await.unwrap();
    assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED, "one invalid credential refuses");
    let resp = both(&pusher, basic_auth_header("reader", &reader)).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "the API key is primary on a push");
    let resp = both(&reader, basic_auth_header("pusher", &pusher)).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "the reader's key wins and cannot write");

    let status = push_as(&c, base, "nuget", "trg_notatoken_notatoken", nupkg("y", "1.0.0", &[])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_placeholder_key_beside_valid_basic_credentials_is_ignored() {
    let server = setup().await;
    let c = reqwest::Client::new();
    let base = &server.base_url;
    create_user(&c, base, STATIC_TOKEN, "pusher", "publisher").await;
    let pusher = add_token(&c, base, "pusher", "p").await;
    let push = |key: &str, version: &str| {
        c.put(format!("{base}/nuget/v3/package"))
            .header("X-NuGet-ApiKey", key.to_string())
            .header("Authorization", basic_auth_header("pusher", &pusher))
            .multipart(common::nuget::form(nupkg("az.placeholder", version, &[])))
    };
    let resp = push("az", "1.0.0").send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "a dummy key is not a credential");
    let resp = push("trg_notatoken_notatoken", "2.0.0").send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "a token-shaped key is verified");
    let status = push_as(&c, base, "nuget", "az", nupkg("az.alone", "1.0.0", &[])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn npm_gains_no_basic_challenge() {
    let server = setup().await;
    let resp = reqwest::Client::new()
        .put(format!("{}/npm/some-package", server.base_url))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get("www-authenticate").is_none());
}

#[tokio::test]
async fn search_filters_before_the_window_and_never_shows_an_unlisted_version() {
    let server = setup().await;
    let c = reqwest::Client::new();
    let base = &server.base_url;
    push(&c, base, "nuget", nupkg("Alpha.Lib", "1.0.0", &[])).await;
    push(&c, base, "nuget", nupkg("Alpha.Lib", "2.0.0-rc.1", &[])).await;
    push(&c, base, "nuget", nupkg("Beta.Lib", "1.0.0-beta", &[])).await;
    push(&c, base, "nuget", nupkg("Gamma.Lib", "1.0.0", &[])).await;
    push(&c, base, "nuget", nupkg("Gamma.Lib", "1.1.0", &[])).await;
    c.delete(format!("{base}/nuget/v3/package/gamma.lib/1.1.0"))
        .header("X-NuGet-ApiKey", STATIC_TOKEN)
        .send()
        .await
        .unwrap();

    let (_, stable) = get_json(&c, &format!("{base}/nuget/v3/search?q=lib&take=1")).await;
    assert_eq!(stable["totalHits"], 2, "Beta.Lib has no stable version");
    assert_eq!(stable["data"].as_array().unwrap().len(), 1);
    assert_eq!(stable["data"][0]["id"], "Alpha.Lib");
    assert_eq!(stable["data"][0]["version"], "1.0.0", "semver2 prereleases filtered");

    let (_, second) = get_json(&c, &format!("{base}/nuget/v3/search?q=lib&skip=1&take=1")).await;
    assert_eq!(second["data"][0]["id"], "Gamma.Lib");
    let versions: Vec<&str> = second["data"][0]["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["version"].as_str().unwrap())
        .collect();
    assert_eq!(versions, ["1.0.0"], "the unlisted 1.1.0 is not searchable");

    let (_, all) = get_json(
        &c,
        &format!("{base}/nuget/v3/search?q=lib&prerelease=true&semVerLevel=2.0.0"),
    )
    .await;
    assert_eq!(all["totalHits"], 3);
    assert_eq!(all["data"][0]["version"], "2.0.0-rc.1");
}

fn files_under(dir: &std::path::Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path.to_string_lossy().into_owned());
        }
    }
}

/// NuGet 2.5 as implemented: the hosted `.nuspec` is read out of the stored
/// `.nupkg` at push and kept in the version row, so the `.nupkg` is the one
/// shared key a push places; a second key would escape port 23's clause.
#[tokio::test]
async fn a_hosted_push_places_the_nupkg_and_no_other_key() {
    if common::storage_is_s3() {
        eprintln!("skipped under S3: the test lists the stored keys on disk");
        return;
    }
    let server = setup().await;
    let c = reqwest::Client::new();
    let base = &server.base_url;
    push(&c, base, "nuget", nupkg("Only.Lib", "1.0.0", &[])).await;
    let mut files = Vec::new();
    files_under(&server.tmp.path().join("storage"), &mut files);
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].contains("only.lib.1.0.0.nupkg"), "{files:?}");
    let nuspec = c
        .get(format!("{base}/nuget/v3/flatcontainer/only.lib/1.0.0/only.lib.nuspec"))
        .send()
        .await
        .unwrap();
    assert_eq!(nuspec.status(), StatusCode::OK);
    assert!(nuspec.text().await.unwrap().contains("<id>Only.Lib</id>"));
}

/// NuGet 3.2: a field past the cap is refused before anything is placed,
/// and leaves neither a part, a key, nor a version behind.
#[tokio::test]
async fn spool_field_over_cap_is_413_and_removes_part() {
    if common::storage_is_s3() {
        eprintln!("skipped under S3: the test lists the stored keys on disk");
        return;
    }
    let server = setup().await;
    let c = reqwest::Client::new();
    let base = &server.base_url;
    let over = vec![0u8; opencargo::registry::nuget::publish::MAX_NUPKG_BYTES + 1];
    let status = push_as(&c, base, "nuget", STATIC_TOKEN, over).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let mut files = Vec::new();
    files_under(&server.tmp.path().join("storage"), &mut files);
    assert!(files.is_empty(), "{files:?}");
    push(&c, base, "nuget", nupkg("After.Lib", "1.0.0", &[])).await;
    let mut files = Vec::new();
    files_under(&server.tmp.path().join("storage"), &mut files);
    assert_eq!(files.len(), 1, "the refused push held no budget and left no key: {files:?}");
}
