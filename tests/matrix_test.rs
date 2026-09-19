//! The coverage matrix, checked against the server rather than read.
//!
//! `Format::coverage` is a declaration; these tests are what stops it from
//! becoming a comfortable fiction. Each one takes a column, loops
//! `Format::ALL`, and holds the declared cell against what the running
//! server does with a real publish.

mod common;

use reqwest::StatusCode;

use common::{publish, spawn_server, SpawnOpts, STATIC_TOKEN};
use opencargo::domain::Format;

async fn server() -> common::TestServer {
    spawn_server(SpawnOpts {
        repositories: publish::repositories(),
        ..Default::default()
    })
    .await
}

/// Nine formats, nine publishes, one server: the loop the eight defects of
/// wave nine walked past, because every list that had to learn `raw` and
/// `mcp` was written out by hand.
#[tokio::test]
async fn every_format_publishes_into_its_own_repository() {
    let s = server().await;
    let client = reqwest::Client::new();
    for format in Format::ALL {
        let response = publish::publish(&client, &s.base_url, format, 1).await;
        let status = response.status();
        assert!(
            status.is_success(),
            "{format:?} publish answered {status}: {:?}",
            response.text().await
        );
    }
}

/// `emptiness_probe`: deleting a repository that holds something is a 409,
/// never a 204 that leaves rows behind. The probe is five hand-written counts
/// over five tables; this is what tells a format it was left out of them.
#[tokio::test]
async fn a_repository_holding_a_publish_refuses_to_be_deleted() {
    let s = server().await;
    let client = reqwest::Client::new();
    for format in Format::ALL {
        let repo = publish::repo_of(format);
        let published = publish::publish(&client, &s.base_url, format, 1).await;
        assert!(published.status().is_success(), "{format:?} publish failed");

        let delete = client
            .delete(format!("{}/api/v1/repositories/{repo}", s.base_url))
            .bearer_auth(STATIC_TOKEN)
            .send()
            .await
            .unwrap();
        let declared = format.coverage().emptiness_probe;
        assert!(declared.yes(), "{format:?} declares no emptiness probe: {declared:?}");
        assert_eq!(
            delete.status(),
            StatusCode::CONFLICT,
            "{repo} held a {format:?} publish and answered {}: {:?}",
            delete.status(),
            delete.text().await
        );
    }
}

/// `metered_publish`: a format whose row says `Yes` is refused by its own
/// entry at the limit. The section is built from the matrix, so a tenth
/// format is metered here the day it declares it.
#[tokio::test]
async fn the_meter_refuses_every_format_that_declares_one() {
    let metered: Vec<Format> = Format::ALL
        .into_iter()
        .filter(|f| f.coverage().metered_publish.yes())
        .collect();
    let mut section = String::from("[limits.publish.format]\n");
    for format in &metered {
        section.push_str(&format!("{} = 1\n", format.as_str()));
    }
    let s = spawn_server(SpawnOpts {
        repositories: publish::repositories(),
        limits: common::limits(&section),
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();

    for format in metered {
        let first = publish::publish(&client, &s.base_url, format, 1).await;
        assert!(first.status().is_success(), "{format:?} first publish failed");
        let second = publish::publish(&client, &s.base_url, format, 2).await;
        assert_eq!(
            second.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "{format:?} declares a metered publish and the second one answered {}",
            second.status()
        );
        assert!(
            second.headers().contains_key(reqwest::header::RETRY_AFTER),
            "{format:?} refused without Retry-After"
        );
    }
}

/// The other half: a format that declares it cannot be metered publishes as
/// often as it likes, even under the fallback that meters everything else.
/// That asking for a limit on it is refused at load, with the reason its cell
/// carries, is settled where the section is parsed (`config::tests`).
#[tokio::test]
async fn an_unmetered_format_publishes_past_the_fallback() {
    let unmetered: Vec<Format> = Format::ALL
        .into_iter()
        .filter(|f| !f.coverage().metered_publish.yes())
        .collect();
    assert!(!unmetered.is_empty(), "the column would be vacuous");

    let s = spawn_server(SpawnOpts {
        repositories: publish::repositories(),
        limits: common::limits("[limits.publish]\nper_window = 1\n"),
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    for format in unmetered {
        for n in 1..=2 {
            let response = publish::publish(&client, &s.base_url, format, n).await;
            assert!(
                response.status().is_success(),
                "{format:?} publish {n} was refused {} though nothing meters it",
                response.status()
            );
        }
    }
}

/// `reclaim_referenced`: every byte a publish leaves behind is claimed by a
/// committed row. A format missing from the union is not a cosmetic gap --
/// the sweeper deletes live artifacts, and nothing fails until a client asks
/// for one.
#[tokio::test]
async fn every_published_byte_is_claimed_by_the_reference_union() {
    use futures_util::TryStreamExt;

    let s = server().await;
    let client = reqwest::Client::new();
    for format in Format::ALL {
        let published = publish::publish(&client, &s.base_url, format, 1).await;
        assert!(published.status().is_success(), "{format:?} publish failed");
        assert!(
            format.coverage().reclaim_referenced.yes(),
            "{format:?} declares its keys unreferenced, which the sweeper reads as garbage"
        );
    }

    use opencargo::ports::referenced::Referenced;
    let stores = opencargo::server::open_stores(&s.tmp.path().join("opencargo.db")).await.unwrap();
    let referenced: Vec<Referenced> = stores
        .referenced()
        .referenced(std::time::Duration::from_secs(0), chrono::Utc::now())
        .try_collect()
        .await
        .unwrap();

    let claims = |key: &str| {
        referenced.iter().any(|r| {
            r.key == key || (r.prefix && key.starts_with(&format!("{}/", r.key)))
        })
    };
    let stored = common::stored_keys(&s).await;
    assert!(
        stored.len() >= Format::ALL.len(),
        "{} keys for {} publishes: the check would be vacuous",
        stored.len(),
        Format::ALL.len()
    );
    let orphans: Vec<String> = stored
        .into_iter()
        .filter(|key| !claims(key))
        .collect();
    assert!(
        orphans.is_empty(),
        "the sweeper would delete these live bytes: {orphans:#?}"
    );
}
