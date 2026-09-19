//! `Format::coverage` held against a running server, one test per column.

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

/// The probe is five hand-written counts over five tables.
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

/// That a limit on such a format is refused at load is settled in `config::tests`.
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

/// A format missing from the union has its live artifacts swept, and nothing
/// fails until a client asks for one.
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

/// `download_signal`: the count on the dashboard is the assertion. Nine
/// publishes, nine downloads, and the total is the number of formats whose
/// row says the read is recorded against the version it served.
#[tokio::test]
async fn the_download_count_is_exactly_the_formats_that_declare_the_signal() {
    let s = server().await;
    let client = reqwest::Client::new();
    for format in Format::ALL {
        let published = publish::publish(&client, &s.base_url, format, 1).await;
        assert!(published.status().is_success(), "{format:?} publish failed");
        let fetched = publish::fetch(&client, &s.base_url, format, 1).await;
        assert!(
            fetched.status().is_success(),
            "{format:?} download answered {}: {:?}",
            fetched.status(),
            fetched.text().await
        );
    }

    let declared = Format::ALL
        .into_iter()
        .filter(|f| f.coverage().download_signal.yes())
        .count();
    let dashboard: serde_json::Value = client
        .get(format!("{}/api/v1/dashboard", s.base_url))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        dashboard["total_downloads"].as_i64(),
        Some(declared as i64),
        "{declared} formats declare a download signal; the dashboard counted {}",
        dashboard["total_downloads"]
    );
}
