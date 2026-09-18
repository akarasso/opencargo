//! NuGet proxy repositories against a nuget.org-shaped fake: served
//! documents carry no upstream URL but the catalog leaf, packages are
//! verified against the hash the catalog leaf declares, and every redirect
//! stays on the origin it was asked of.

mod common;

use reqwest::StatusCode;
use serde_json::Value;

use common::fake_upstream::nuget::{self as fake, FakeNuget};
use common::nuget::nupkg;
use common::{proxy_with, spawn_server, ProxyOpts, SpawnOpts, TestServer};
use opencargo::config::RepositoryFormat;
use opencargo::proxy::UpstreamAuth;

async fn setup() -> (FakeNuget, TestServer) {
    let up = fake::start().await;
    let server = spawn_server(SpawnOpts {
        repositories: vec![proxy_with(
            "nuget-proxy",
            RepositoryFormat::Nuget,
            &up.service_index(),
            ProxyOpts {
                dl_allow_private: true,
                upstream_auth: Some(UpstreamAuth::Bearer {
                    token: "upstream-secret".into(),
                }),
                ..Default::default()
            },
        )],
        ..Default::default()
    })
    .await;
    (up, server)
}

async fn get(url: &str) -> reqwest::Response {
    reqwest::Client::new().get(url).send().await.unwrap()
}

fn nupkg_url(server: &TestServer, id: &str, v: &str) -> String {
    format!("{}/nuget-proxy/v3/flatcontainer/{id}/{v}/{id}.{v}.nupkg", server.base_url)
}

#[tokio::test]
async fn documents_are_ours_and_packages_are_verified_by_the_catalog_leaf() {
    let (up, server) = setup().await;
    let bytes = nupkg("Up.Lib", "1.2.3", &[]);
    up.add("Up.Lib", "1.2.3", bytes.clone());

    let resp = get(&format!("{}/nuget-proxy/v3/index.json", server.base_url)).await;
    let index: Value = resp.json().await.unwrap();
    assert!(!index.to_string().contains(&up.base_url));
    assert!(!index.to_string().contains("PackagePublish"), "a proxy takes no push");

    let reg: Value = get(&format!("{}/nuget-proxy/v3/registration/up.lib/index.json", server.base_url))
        .await
        .json()
        .await
        .unwrap();
    let text = reg.to_string();
    let entry = &reg["items"][0]["items"][0]["catalogEntry"];
    assert_eq!(entry["id"], "Up.Lib");
    let catalog = entry["@id"].as_str().unwrap().to_string();
    assert!(catalog.starts_with(&up.base_url), "the catalog leaf is the one upstream URL");
    assert_eq!(
        text.matches(&up.base_url).count(),
        1,
        "no other upstream URL is served: {text}"
    );

    let flat: Value = get(&format!("{}/nuget-proxy/v3/flatcontainer/up.lib/index.json", server.base_url))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(flat["versions"][0], "1.2.3");

    let resp = get(&nupkg_url(&server, "up.lib", "1.2.3")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), bytes.as_slice());
    assert_eq!(up.count("/catalog/up.lib.1.2.3."), 1, "the hash came from the catalog leaf");

    let resp = get(&nupkg_url(&server, "up.lib", "1.2.3")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(up.count("/up.lib.1.2.3.nupkg"), 1, "served from the verified cache");
}

#[tokio::test]
async fn an_unknown_package_is_a_not_found() {
    let (_up, server) = setup().await;
    let resp = get(&format!("{}/nuget-proxy/v3/registration/nope/index.json", server.base_url)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = get(&nupkg_url(&server, "nope", "1.0.0")).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_digest_mismatch_is_a_bad_gateway_and_nothing_is_served() {
    let (up, server) = setup().await;
    up.add("Bad", "1.0.0", nupkg("Bad", "1.0.0", &[]));
    up.switch(|s| s.wrong_hash = true);
    for _ in 0..2 {
        let resp = get(&nupkg_url(&server, "bad", "1.0.0")).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }
    assert_eq!(up.count("/bad.1.0.0.nupkg"), 1, "quarantined on its announced digest, not fetched again (C6)");
}

#[tokio::test]
async fn a_registration_hash_is_used_without_the_catalog_leaf() {
    let (up, server) = setup().await;
    up.add("Reg", "1.0.0", nupkg("Reg", "1.0.0", &[]));
    up.switch(|s| {
        s.hash_in_registration = true;
        s.wrong_hash = true;
    });
    let resp = get(&nupkg_url(&server, "reg", "1.0.0")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(up.count("/catalog/reg.1.0.0."), 0);
}

#[tokio::test]
async fn a_redirect_off_the_declared_origin_is_refused() {
    let (up, server) = setup().await;
    up.add("Moved", "1.0.0", nupkg("Moved", "1.0.0", &[]));
    up.switch(|s| s.redirect_nupkg = true);
    let resp = get(&nupkg_url(&server, "moved", "1.0.0")).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    let (up2, server2) = setup().await;
    up2.switch(|s| s.redirect_index = true);
    let resp = get(&format!("{}/nuget-proxy/v3/registration/x/index.json", server2.base_url)).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY, "the service index follows no redirect off origin");
}

#[tokio::test]
async fn search_off_origin_carries_no_credentials() {
    let (up, server) = setup().await;
    up.add("Found.Lib", "1.0.0", nupkg("Found.Lib", "1.0.0", &[]));
    let resp: Value = get(&format!("{}/nuget-proxy/v3/search?q=found", server.base_url))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(resp["totalHits"], 1);
    assert_eq!(resp["data"][0]["id"], "Found.Lib");
    assert!(!resp.to_string().contains(&up.other_url));
    assert!(up.count("/query") >= 1);
    assert!(!up.credentials_sent("/query"), "the search lives on another origin");
    assert!(up.credentials_sent("/v3/index.json"), "the declared origin gets them");
}

#[tokio::test]
async fn a_gzipped_registration_is_decoded() {
    let (up, server) = setup().await;
    up.add("Zipped", "2.0.0", nupkg("Zipped", "2.0.0", &[]));
    up.switch(|s| s.gzip_registration = true);
    let flat: Value = get(&format!("{}/nuget-proxy/v3/flatcontainer/zipped/index.json", server.base_url))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(flat["versions"][0], "2.0.0");
}

#[tokio::test]
async fn warm_entry_with_stale_digest_is_refetched() {
    let (up, server) = setup().await;
    up.add("Warm", "1.0.0", nupkg("Warm", "1.0.0", &[]));
    assert_eq!(get(&nupkg_url(&server, "warm", "1.0.0")).await.status(), StatusCode::OK);
    let republished = nupkg("Warm", "1.0.0", &["extra.txt"]);
    up.add("Warm", "1.0.0", republished.clone());
    common::expire_entries(&server).await;
    let resp = get(&nupkg_url(&server, "warm", "1.0.0")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), republished.as_slice());
    assert_eq!(up.count("/warm.1.0.0.nupkg"), 2, "the new announced sha512 is a miss");
}
