//! NuGet groups: the union of their members, an anonymous caller asked for
//! credentials whenever a member is private to it, an authenticated caller
//! answered as if the members it cannot read were absent, and a member down
//! never turned into a 404.

mod common;

use reqwest::StatusCode;
use serde_json::{json, Value};

use common::fake_upstream::nuget::{self as fake, FakeNuget};
use common::faults::Outage;
use common::nuget::{nupkg, push};
use common::{
    add_token, create_user, group, hosted, proxy_with, spawn_server, ProxyOpts, SpawnOpts,
    TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

const N: RepositoryFormat = RepositoryFormat::Nuget;

async fn setup(up: &FakeNuget) -> TestServer {
    setup_with(up, None).await
}

async fn setup_with(up: &FakeNuget, outage: Option<Outage>) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![
            hosted("local", N, Visibility::Public),
            hosted("secret", N, Visibility::Private),
            proxy_with(
                "remote",
                N,
                &up.service_index(),
                ProxyOpts {
                    dl_allow_private: true,
                    ..Default::default()
                },
            ),
            group("all", N, &["local", "remote"]),
            group("private-first", N, &["secret", "local"]),
            group("private-last", N, &["local", "secret"]),
            group("without-secret", N, &["local"]),
        ],
        outage,
        ..Default::default()
    })
    .await
}

async fn get(url: String, token: Option<&str>) -> reqwest::Response {
    let req = reqwest::Client::new().get(url);
    let req = match token {
        Some(t) => req.bearer_auth(t),
        None => req,
    };
    req.send().await.unwrap()
}

#[tokio::test]
async fn a_group_merges_hosted_and_proxy_versions() {
    let up = fake::start().await;
    let server = setup(&up).await;
    let c = reqwest::Client::new();
    let local = nupkg("Shared.Lib", "2.0.0", &[]);
    push(&c, &server.base_url, "local", local.clone()).await;
    up.add("Shared.Lib", "1.0.0", nupkg("Shared.Lib", "1.0.0", &[]));
    up.add("Shared.Lib", "2.0.0", nupkg("Shared.Lib", "2.0.0", &["upstream.txt"]));

    let flat: Value = get(format!("{}/all/v3/flatcontainer/shared.lib/index.json", server.base_url), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(flat, json!({"versions": ["1.0.0", "2.0.0"]}));
    let reg: Value = get(format!("{}/all/v3/registration/shared.lib/index.json", server.base_url), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(reg["items"][0]["count"], 2);
    let content = reg["items"][0]["items"][0]["packageContent"].as_str().unwrap();
    assert!(content.starts_with(&format!("{}/all/v3/", server.base_url)), "{content}");

    let resp = get(format!("{}/all/v3/flatcontainer/shared.lib/2.0.0/shared.lib.2.0.0.nupkg", server.base_url), None).await;
    assert_eq!(resp.bytes().await.unwrap().as_ref(), local.as_slice(), "the first member wins");
    let resp = get(format!("{}/all/v3/flatcontainer/shared.lib/1.0.0/shared.lib.1.0.0.nupkg", server.base_url), None).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn anonymous_on_a_group_with_a_private_member_is_asked_for_credentials() {
    let up = fake::start().await;
    let server = setup(&up).await;
    let c = reqwest::Client::new();
    push(&c, &server.base_url, "local", nupkg("Here", "1.0.0", &[])).await;
    for group in ["private-first", "private-last"] {
        for id in ["here", "nowhere"] {
            let resp = get(format!("{}/{group}/v3/flatcontainer/{id}/index.json", server.base_url), None).await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{group} {id}");
            assert_eq!(resp.headers()["www-authenticate"], "Basic realm=\"opencargo\"");
            let body = resp.text().await.unwrap();
            assert!(!body.contains("secret"), "the member is not named: {body}");
        }
        let resp = get(format!("{}/{group}/v3/index.json", server.base_url), None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{group}: not NU1101");
    }
    let resp = get(format!("{}/all/v3/flatcontainer/here/index.json", server.base_url), None).await;
    assert_eq!(resp.status(), StatusCode::OK, "a group of public members stays anonymous");
}

#[tokio::test]
async fn an_authenticated_caller_sees_the_group_without_the_member_it_cannot_read() {
    let up = fake::start().await;
    let server = setup(&up).await;
    let c = reqwest::Client::new();
    let base = &server.base_url;
    push(&c, base, "local", nupkg("Here", "1.0.0", &[])).await;
    push(&c, base, "secret", nupkg("Hidden", "1.0.0", &[])).await;
    push(&c, base, "secret", nupkg("Here", "2.0.0", &[])).await;
    create_user(&c, base, STATIC_TOKEN, "outsider", "reader").await;
    let token = add_token(&c, base, "outsider", "t").await;
    let resp = c
        .put(format!("{base}/api/v1/users/outsider/permissions/secret"))
        .bearer_auth(STATIC_TOKEN)
        .json(&json!({"can_read": false, "can_write": false, "can_delete": false, "can_admin": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    for group in ["private-first", "private-last"] {
        for path in ["flatcontainer/here/index.json", "flatcontainer/hidden/index.json", "search?q=h"] {
            let with = get(format!("{base}/{group}/v3/{path}"), Some(&token)).await;
            let without = get(format!("{base}/without-secret/v3/{path}"), Some(&token)).await;
            assert_eq!(with.status(), without.status(), "{group} {path}");
            let (with, without) = (with.text().await.unwrap(), without.text().await.unwrap());
            assert_eq!(with.replace(group, "without-secret"), without, "{group} {path}");
        }
    }
}

#[tokio::test]
async fn a_member_down_is_never_a_not_found() {
    let up = fake::start().await;
    let server = setup(&up).await;
    let c = reqwest::Client::new();
    let base = &server.base_url;
    push(&c, base, "local", nupkg("Local.Lib", "1.0.0", &[])).await;
    up.add("Cached.Lib", "1.0.0", nupkg("Cached.Lib", "1.0.0", &[]));
    up.add("Cold.Lib", "1.0.0", nupkg("Cold.Lib", "1.0.0", &[]));
    let url = |id: &str| format!("{base}/all/v3/flatcontainer/{id}/1.0.0/{id}.1.0.0.nupkg");
    assert_eq!(get(url("cached.lib"), None).await.status(), StatusCode::OK);

    up.switch(|s| s.down = true);
    let resp = get(format!("{base}/all/v3/flatcontainer/local.lib/index.json"), None).await;
    assert_eq!(resp.status(), StatusCode::OK, "the hosted member answers");
    assert_eq!(get(url("local.lib"), None).await.status(), StatusCode::OK);
    assert_eq!(get(url("cached.lib"), None).await.status(), StatusCode::OK, "the verified cache answers");
    let cold = get(url("cold.lib"), None).await;
    assert_eq!(cold.status(), StatusCode::BAD_GATEWAY, "a member down is not an absent package");
    let cold = get(format!("{base}/all/v3/registration/cold.lib/index.json"), None).await;
    assert_eq!(cold.status(), StatusCode::BAD_GATEWAY);
}

/// NuGet 2.6 and invariant 14: a store or storage outage is ours, a 503,
/// never the 404 `dotnet` would cache as NU1101 nor a member's 502.
#[tokio::test]
async fn an_unavailable_store_or_storage_is_503_never_404_nor_502() {
    let up = fake::start().await;
    let outage = Outage::default();
    let server = setup_with(&up, Some(outage.clone())).await;
    let c = reqwest::Client::new();
    let base = &server.base_url;
    push(&c, base, "local", nupkg("Local.Lib", "1.0.0", &[])).await;
    push(&c, base, "secret", nupkg("Local.Lib", "2.0.0", &[])).await;
    up.add("Remote.Lib", "1.0.0", nupkg("Remote.Lib", "1.0.0", &[]));
    create_user(&c, base, STATIC_TOKEN, "reader", "reader").await;
    let token = add_token(&c, base, "reader", "t").await;

    let paths = |id: &str| {
        [
            format!("flatcontainer/{id}/index.json"),
            format!("registration/{id}/index.json"),
            format!("flatcontainer/{id}/1.0.0/{id}.1.0.0.nupkg"),
        ]
    };
    for id in ["local.lib", "remote.lib"] {
        for path in paths(id) {
            let resp = get(format!("{base}/all/v3/{path}"), None).await;
            assert_eq!(resp.status(), StatusCode::OK, "warm {path}");
        }
    }

    up.add("Cold.Lib", "1.0.0", nupkg("Cold.Lib", "1.0.0", &[]));
    outage.set(true, false);
    for (who, t) in [("anonymous", None), ("reader", Some(token.as_str()))] {
        for id in ["local.lib", "remote.lib", "cold.lib"] {
            let [flat, registration, package] = paths(id);
            let resp = get(format!("{base}/all/v3/{package}"), t).await;
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "storage down, {who}: {package}");
            for path in [flat, registration] {
                let status = get(format!("{base}/all/v3/{path}"), t).await.status();
                if id == "cold.lib" {
                    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "storage down, {who}: {path}");
                } else {
                    assert!(
                        status == StatusCode::OK || status == StatusCode::SERVICE_UNAVAILABLE,
                        "a warm document is served from memory or refused as ours, {who}: {path} {status}"
                    );
                }
            }
        }
    }

    outage.set(false, true);
    for path in paths("local.lib") {
        let resp = get(format!("{base}/private-first/v3/{path}"), Some(&token)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "permissions down, reader: {path}");
        let resp = get(format!("{base}/private-first/v3/{path}"), None).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "an anonymous caller is asked for credentials, which reads no grant: {path}"
        );
    }

    outage.set(false, false);
    for path in paths("local.lib") {
        let resp = get(format!("{base}/private-first/v3/{path}"), Some(&token)).await;
        assert_eq!(resp.status(), StatusCode::OK, "back up: {path}");
    }
}

fn versions_of(reg: &Value) -> Vec<(String, bool)> {
    reg["items"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|page| page["items"].as_array().unwrap().iter())
        .map(|leaf| {
            (
                leaf["catalogEntry"]["version"].as_str().unwrap().to_string(),
                leaf["catalogEntry"]["listed"].as_bool().unwrap(),
            )
        })
        .collect()
}

/// NuGet decision 11 and invariant 7: a merged document is kept while the
/// hosted members' stamps hold, so a publish or an unlist shows at once,
/// whatever the upstream age cap.
#[tokio::test]
async fn the_registration_memo_is_invalidated_by_the_hosted_stamp() {
    let up = fake::start().await;
    let server = setup(&up).await;
    let c = reqwest::Client::new();
    let base = &server.base_url;
    push(&c, base, "local", nupkg("Shared.Lib", "2.0.0", &[])).await;
    up.add("Shared.Lib", "1.0.0", nupkg("Shared.Lib", "1.0.0", &[]));
    let registration = format!("{base}/all/v3/registration/shared.lib/index.json");
    let flat = format!("{base}/all/v3/flatcontainer/shared.lib/index.json");

    let first = get(registration.clone(), None).await;
    assert!(first.headers().get("warning").is_none());
    let first: Value = first.json().await.unwrap();
    assert_eq!(versions_of(&first), [("1.0.0".into(), true), ("2.0.0".into(), true)]);
    assert_eq!(get(flat.clone(), None).await.status(), StatusCode::OK);

    common::expire_entries(&server).await;
    let asked = up.count("");
    let kept: Value = get(registration.clone(), None).await.json().await.unwrap();
    assert_eq!(kept, first);
    assert_eq!(up.count(""), asked, "served from the memo: nothing revalidated upstream");

    push(&c, base, "local", nupkg("Shared.Lib", "3.0.0", &[])).await;
    let moved: Value = get(registration.clone(), None).await.json().await.unwrap();
    assert!(up.count("") > asked, "the stamp moved: recomputed through the members");
    assert!(versions_of(&moved).contains(&("3.0.0".into(), true)), "{moved}");
    let flat_now: Value = get(flat.clone(), None).await.json().await.unwrap();
    assert!(flat_now["versions"].as_array().unwrap().contains(&json!("3.0.0")), "{flat_now}");

    let resp = c
        .delete(format!("{base}/local/v3/package/shared.lib/3.0.0"))
        .header("X-NuGet-ApiKey", STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let unlisted: Value = get(registration.clone(), None).await.json().await.unwrap();
    assert_eq!(
        versions_of(&unlisted),
        [("1.0.0".into(), true), ("2.0.0".into(), true), ("3.0.0".into(), false)]
    );
}

/// Under load (NuGet checklist 4): many readers of the same and of distinct
/// packages through a group with a proxy member, while pushes land, take
/// no parse permit twice for one body and never answer saturated; every
/// pushed version is in the merged documents once the pushes are done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_registration_memo_holds_under_concurrent_reads_and_pushes() {
    let up = fake::start().await;
    let server = setup(&up).await;
    let c = reqwest::Client::new();
    let base = server.base_url.clone();
    let ids: Vec<String> = (0..8).map(|i| format!("Load.Lib{i}")).collect();
    for id in &ids {
        up.add(id, "1.0.0", nupkg(id, "1.0.0", &[]));
        push(&c, &base, "local", nupkg(id, "2.0.0", &[])).await;
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut readers = Vec::new();
    for n in 0..64 {
        let (base, id) = (base.clone(), ids[n % ids.len()].to_ascii_lowercase());
        readers.push(tokio::spawn(async move {
            let doc = if n % 2 == 0 { "registration" } else { "flatcontainer" };
            let resp = get(format!("{base}/all/v3/{doc}/{id}/index.json"), None).await;
            (resp.status(), id)
        }));
    }
    let mut pushes = Vec::new();
    for (n, id) in ids.iter().enumerate() {
        let (c, base, id) = (c.clone(), base.clone(), id.clone());
        pushes.push(tokio::spawn(async move {
            push(&c, &base, "local", nupkg(&id, &format!("3.0.{n}"), &[])).await;
        }));
    }
    for reader in readers {
        let (status, id) = tokio::time::timeout_at(deadline, reader).await.unwrap().unwrap();
        assert_eq!(status, StatusCode::OK, "{id}");
    }
    for p in pushes {
        tokio::time::timeout_at(deadline, p).await.unwrap().unwrap();
    }
    for (n, id) in ids.iter().enumerate() {
        let id = id.to_ascii_lowercase();
        let flat: Value = get(format!("{base}/all/v3/flatcontainer/{id}/index.json"), None)
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(flat["versions"], json!(["1.0.0", "2.0.0", format!("3.0.{n}")]), "{id}");
    }
}
