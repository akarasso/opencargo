//! PyPI proxy and group repositories: pages merged across members, files
//! verified against the sha256 their page announced, the member that lists a
//! filename owning it, and upstream credentials kept on the index's host.

mod common;

use reqwest::{Client, StatusCode};
use serde_json::Value;
use sha2::Digest;

use common::fake_upstream::pypi::{self as fake, Answer};
use common::pypi::{basic, sdist, sdist_name, upload, wheel, wheel_name};
use common::upstream_tap::{self, Tap};
use common::{
    expire_entries, group, hosted, proxy_with, spawn_server, ProxyOpts, SpawnOpts, TestServer,
    STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};
use opencargo::proxy::UpstreamAuth;

const JSON_V1: &str = "application/vnd.pypi.simple.v1+json";

fn token() -> String {
    basic("__token__", STATIC_TOKEN)
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn opts() -> ProxyOpts {
    ProxyOpts {
        dl_allow_private: true,
        ..Default::default()
    }
}

async fn json_page(client: &Client, s: &TestServer, repo: &str, project: &str) -> Value {
    let resp = client
        .get(format!("{}/{repo}/simple/{project}/", s.base_url))
        .header("Accept", JSON_V1)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{repo}/{project}");
    resp.json().await.unwrap()
}

fn filenames(page: &Value) -> Vec<String> {
    let mut names: Vec<String> = page["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["filename"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

async fn get(client: &Client, s: &TestServer, path: &str) -> reqwest::Response {
    client.get(format!("{}{path}", s.base_url)).send().await.unwrap()
}

/// A second opencargo holding `demo` 1.0, behind a recording tap.
async fn opencargo_upstream() -> (TestServer, Tap, Vec<u8>) {
    let up = spawn_server(SpawnOpts {
        repositories: vec![hosted("py-up", RepositoryFormat::Pypi, Visibility::Public)],
        ..Default::default()
    })
    .await;
    let client = Client::new();
    let w = wheel("demo", "1.0", None);
    for (name, body) in [(wheel_name("demo", "1.0"), w.clone()), (sdist_name("demo", "1.0"), sdist("demo", "1.0"))] {
        let resp = upload(&client, &up.base_url, "py-up", &token(), &name, &body).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let tap = upstream_tap::start(&up.base_url).await;
    (up, tap, w)
}

fn page_html(files: &[(&str, String, Option<String>)]) -> Vec<u8> {
    let mut out = String::from("<!DOCTYPE html><html><body>\n");
    for (name, href, digest) in files {
        let fragment = digest.as_ref().map(|d| format!("#sha256={d}")).unwrap_or_default();
        out.push_str(&format!("<a href=\"{href}{fragment}\">{name}</a><br/>\n"));
    }
    out.push_str("</body></html>\n");
    out.into_bytes()
}

#[tokio::test]
async fn a_proxy_serves_an_upstream_through_its_json_pages_and_caches_files() {
    let (_up, tap, w) = opencargo_upstream().await;
    let s = spawn_server(SpawnOpts {
        repositories: vec![proxy_with(
            "pypi-proxy",
            RepositoryFormat::Pypi,
            &format!("{}/py-up/simple", tap.base_url),
            opts(),
        )],
        ..Default::default()
    })
    .await;
    let client = Client::new();
    let page = json_page(&client, &s, "pypi-proxy", "demo").await;
    assert_eq!(filenames(&page), ["demo-1.0-py3-none-any.whl", "demo-1.0.tar.gz"]);
    let whl = page["files"].as_array().unwrap().iter().find(|f| f["filename"] == "demo-1.0-py3-none-any.whl").unwrap();
    assert_eq!(whl["url"], "../../files/demo/demo-1.0-py3-none-any.whl", "served from here, relative");
    assert_eq!(whl["hashes"]["sha256"], sha(&w).as_str());

    for _ in 0..2 {
        let resp = get(&client, &s, "/pypi-proxy/files/demo/demo-1.0-py3-none-any.whl").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.bytes().await.unwrap().as_ref(), w.as_slice());
    }
    assert_eq!(tap.count("/py-up/files/demo/demo-1.0-py3-none-any.whl"), 1, "the second read is a cache hit");

    let metadata = get(&client, &s, "/pypi-proxy/files/demo/demo-1.0-py3-none-any.whl.metadata").await;
    assert_eq!(metadata.status(), StatusCode::OK);
    let body = metadata.bytes().await.unwrap();
    assert_eq!(Some(sha(&body).as_str()), whl["core-metadata"]["sha256"].as_str());
    let none = get(&client, &s, "/pypi-proxy/files/demo/demo-1.0.tar.gz.metadata").await;
    assert_eq!(none.status(), StatusCode::NOT_FOUND);
    let unknown = get(&client, &s, "/pypi-proxy/simple/nope/").await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_group_merges_hosted_and_proxy_pages() {
    let (_up, tap, w) = opencargo_upstream().await;
    let s = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("py-local", RepositoryFormat::Pypi, Visibility::Public),
            proxy_with("pypi-proxy", RepositoryFormat::Pypi, &format!("{}/py-up/simple", tap.base_url), opts()),
            group("pypi-all", RepositoryFormat::Pypi, &["py-local", "pypi-proxy"]),
        ],
        ..Default::default()
    })
    .await;
    let client = Client::new();
    let local = upload(&client, &s.base_url, "py-local", &token(), &sdist_name("demo", "2.0"), &sdist("demo", "2.0")).await;
    assert_eq!(local.status(), StatusCode::OK);

    let page = json_page(&client, &s, "pypi-all", "demo").await;
    assert_eq!(
        filenames(&page),
        ["demo-1.0-py3-none-any.whl", "demo-1.0.tar.gz", "demo-2.0.tar.gz"]
    );
    let versions: Vec<&str> = page["versions"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(versions, ["1.0", "2"]);
    let from_proxy = get(&client, &s, "/pypi-all/files/demo/demo-1.0-py3-none-any.whl").await;
    assert_eq!(from_proxy.bytes().await.unwrap().as_ref(), w.as_slice());
    let from_local = get(&client, &s, "/pypi-all/files/demo/demo-2.0.tar.gz").await;
    assert_eq!(from_local.status(), StatusCode::OK);
    let index = get(&client, &s, "/pypi-all/simple/").await.text().await.unwrap();
    assert!(index.contains(">demo<"), "hosted members list their projects");
}

#[tokio::test]
async fn a_member_that_lists_a_filename_owns_every_outcome() {
    let upstream = fake::start().await;
    let name = sdist_name("demo", "1.0");
    let local_bytes = sdist("demo", "1.0");
    upstream.set(
        "/simple/demo/",
        Answer::Body("text/html", page_html(&[(&name, format!("/files/{name}"), Some(sha(b"upstream")))])),
    );
    upstream.set(&format!("/files/{name}"), Answer::Status(404));
    let s = spawn_server(SpawnOpts {
        repositories: vec![
            proxy_with("pypi-proxy", RepositoryFormat::Pypi, &format!("{}/simple", upstream.base_url), opts()),
            hosted("py-local", RepositoryFormat::Pypi, Visibility::Public),
            group("pypi-all", RepositoryFormat::Pypi, &["pypi-proxy", "py-local"]),
        ],
        ..Default::default()
    })
    .await;
    let client = Client::new();
    let resp = upload(&client, &s.base_url, "py-local", &token(), &name, &local_bytes).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let owned = get(&client, &s, &format!("/pypi-all/files/demo/{name}")).await;
    assert_eq!(owned.status(), StatusCode::NOT_FOUND, "the proxy lists it, so its miss is the answer");

    upstream.set("/simple/other/", Answer::Status(500));
    let other = sdist_name("other", "1.0");
    let resp = upload(&client, &s.base_url, "py-local", &token(), &other, &sdist("other", "1.0")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let fallen = get(&client, &s, &format!("/pypi-all/files/other/{other}")).await;
    assert_eq!(fallen.status(), StatusCode::OK, "a failed page passes to the next member");

    upstream.set("/simple/demo/", Answer::Body("text/html", page_html(&[])));
    expire_entries(&s).await;
    let unlisted = get(&client, &s, &format!("/pypi-all/files/demo/{name}")).await;
    assert_eq!(unlisted.status(), StatusCode::OK, "a name the page does not list passes on");
    assert_eq!(unlisted.bytes().await.unwrap().as_ref(), local_bytes.as_slice());
}

#[tokio::test]
async fn files_off_the_allowed_hosts_are_dropped_and_credentials_stay_on_the_index() {
    let index = fake::start().await;
    let other = fake::start_on("127.0.0.2").await;
    let body = b"file bytes".to_vec();
    let digest = Some(sha(&body));
    let on_index = sdist_name("demo", "1.0");
    let off_index = sdist_name("demo", "1.1");
    let bounced = sdist_name("demo", "1.2");
    index.set(
        "/simple/demo/",
        Answer::Body(
            "text/html",
            page_html(&[
                (&on_index, format!("/files/{on_index}"), digest.clone()),
                (&off_index, format!("{}/files/{off_index}", other.base_url), digest.clone()),
                (&bounced, format!("/files/{bounced}"), digest.clone()),
            ]),
        ),
    );
    index.set(&format!("/files/{on_index}"), Answer::Body("application/octet-stream", body.clone()));
    index.set(&format!("/files/{bounced}"), Answer::Redirect(format!("{}/files/{bounced}", other.base_url)));
    other.set(&format!("/files/{off_index}"), Answer::Body("application/octet-stream", body.clone()));
    other.set(&format!("/files/{bounced}"), Answer::Body("application/octet-stream", body.clone()));
    let auth = Some(UpstreamAuth::Basic {
        username: "mirror".into(),
        password: "secret".into(),
    });
    let index_host = index.base_url.trim_start_matches("http://").to_string();
    let other_host = other.base_url.trim_start_matches("http://").to_string();
    let s = spawn_server(SpawnOpts {
        repositories: vec![
            proxy_with(
                "default-hosts",
                RepositoryFormat::Pypi,
                &format!("{}/simple", index.base_url),
                ProxyOpts {
                    upstream_auth: auth.clone(),
                    ..opts()
                },
            ),
            proxy_with(
                "wider-hosts",
                RepositoryFormat::Pypi,
                &format!("{}/simple", index.base_url),
                ProxyOpts {
                    upstream_auth: auth,
                    file_hosts: vec![index_host, other_host],
                    ..opts()
                },
            ),
        ],
        ..Default::default()
    })
    .await;
    let client = Client::new();
    let narrow = json_page(&client, &s, "default-hosts", "demo").await;
    assert_eq!(filenames(&narrow), [on_index.clone(), bounced.clone()], "the other host is off the list");
    let refused = get(&client, &s, &format!("/default-hosts/files/demo/{off_index}")).await;
    assert_eq!(refused.status(), StatusCode::NOT_FOUND);

    let wide = json_page(&client, &s, "wider-hosts", "demo").await;
    assert_eq!(filenames(&wide).len(), 3);
    for name in [&on_index, &off_index] {
        let resp = get(&client, &s, &format!("/wider-hosts/files/demo/{name}")).await;
        assert_eq!(resp.status(), StatusCode::OK, "{name}");
    }
    let sent = index.hits(&format!("/files/{on_index}"));
    assert!(sent.iter().all(|a| a.as_deref().is_some_and(|a| a.starts_with("Basic "))), "the index host sees them");
    assert_eq!(other.hits(&format!("/files/{off_index}")), vec![None], "never another host, allowed or not");

    let hop = get(&client, &s, &format!("/wider-hosts/files/demo/{bounced}")).await;
    assert_eq!(hop.status(), StatusCode::BAD_GATEWAY, "a credentialed request never changes host");
    assert!(other.hits(&format!("/files/{bounced}")).iter().all(Option::is_none));
}

/// The page is the only authority on a file's bytes: a body off its
/// announced digest is refused and never stored; a new digest refetches.
#[tokio::test]
async fn a_body_off_its_announced_digest_is_refused_until_the_page_announces_it() {
    let upstream = fake::start().await;
    let name = sdist_name("demo", "1.0");
    let served = b"what the host serves".to_vec();
    upstream.set(
        "/simple/demo/",
        Answer::Body("text/html", page_html(&[(&name, format!("/files/{name}"), Some(sha(b"announced")))])),
    );
    upstream.set(&format!("/files/{name}"), Answer::Body("application/octet-stream", served.clone()));
    let s = spawn_server(SpawnOpts {
        repositories: vec![proxy_with("pypi-proxy", RepositoryFormat::Pypi, &format!("{}/simple", upstream.base_url), opts())],
        ..Default::default()
    })
    .await;
    let client = Client::new();
    for _ in 0..2 {
        let resp = get(&client, &s, &format!("/pypi-proxy/files/demo/{name}")).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }
    assert_eq!(upstream.hits(&format!("/files/{name}")).len(), 2, "nothing refused was kept");

    upstream.set(
        "/simple/demo/",
        Answer::Body("text/html", page_html(&[(&name, format!("/files/{name}"), Some(sha(&served)))])),
    );
    expire_entries(&s).await;
    let resp = get(&client, &s, &format!("/pypi-proxy/files/demo/{name}")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), served.as_slice());
}

#[tokio::test]
async fn warm_entry_with_stale_digest_is_refetched() {
    let upstream = fake::start().await;
    let name = wheel_name("demo", "1.0");
    let first = b"first build".to_vec();
    let second = b"second build".to_vec();
    let script = |body: &[u8]| {
        upstream.set(
            "/simple/demo/",
            Answer::Body("text/html", page_html(&[(&name, format!("/files/{name}"), Some(sha(body)))])),
        );
        upstream.set(&format!("/files/{name}"), Answer::Body("application/octet-stream", body.to_vec()));
    };
    script(&first);
    let s = spawn_server(SpawnOpts {
        repositories: vec![proxy_with("pypi-proxy", RepositoryFormat::Pypi, &format!("{}/simple", upstream.base_url), opts())],
        ..Default::default()
    })
    .await;
    let client = Client::new();
    let path = format!("/pypi-proxy/files/demo/{name}");
    assert_eq!(get(&client, &s, &path).await.bytes().await.unwrap().as_ref(), first.as_slice());
    assert_eq!(get(&client, &s, &path).await.bytes().await.unwrap().as_ref(), first.as_slice());
    assert_eq!(upstream.hits(&format!("/files/{name}")).len(), 1, "warm");

    script(&second);
    expire_entries(&s).await;
    let resp = get(&client, &s, &path).await;
    assert_eq!(resp.bytes().await.unwrap().as_ref(), second.as_slice(), "a warm body under an old digest is a miss");
    assert_eq!(upstream.hits(&format!("/files/{name}")).len(), 2);
}

#[tokio::test]
async fn both_metadata_names_are_read_from_an_html_upstream() {
    let upstream = fake::start().await;
    let a = wheel_name("demo", "1.0");
    let b = wheel_name("demo", "2.0");
    let html = format!(
        "<html><body><a href=\"/f/{a}#sha256=aa\" data-dist-info-metadata=\"sha256=bb\" data-requires-python=\"&gt;=3.9\">{a}</a>\n<a href=\"/f/{b}\" data-core-metadata=\"true\" data-yanked=\"\">{b}</a></body></html>"
    );
    upstream.set("/simple/demo/", Answer::Body("text/html", html.into_bytes()));
    let s = spawn_server(SpawnOpts {
        repositories: vec![proxy_with("pypi-proxy", RepositoryFormat::Pypi, &format!("{}/simple", upstream.base_url), opts())],
        ..Default::default()
    })
    .await;
    let client = Client::new();
    let page = json_page(&client, &s, "pypi-proxy", "demo").await;
    let files = page["files"].as_array().unwrap();
    assert_eq!(files[0]["core-metadata"], serde_json::json!({"sha256": "bb"}));
    assert_eq!(files[0]["dist-info-metadata"], files[0]["core-metadata"]);
    assert_eq!(files[0]["requires-python"], ">=3.9");
    assert_eq!(files[1]["core-metadata"], true);
    assert_eq!(files[1]["yanked"], true);
    let html = get(&client, &s, "/pypi-proxy/simple/demo/").await.text().await.unwrap();
    assert!(html.contains("data-core-metadata=\"sha256=bb\""));
    assert!(html.contains("data-dist-info-metadata=\"sha256=bb\""));
}
