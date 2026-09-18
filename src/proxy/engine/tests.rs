use std::time::Duration;

use axum::http::{header, Method};
use tokio::io::AsyncReadExt;

use super::*;
use crate::testing::fixture::*;
use crate::domain::{Repository, Visibility};
use crate::proxy::strategy::{CacheKey, Transfer, UrlSource};

fn found(o: AppResult<Outcome<Cached>>) -> Cached {
    match o {
        Ok(Outcome::Found(c)) => c,
        other => panic!("expected Found, got {other:?}"),
    }
}

#[test]
fn prefix_keys_do_not_collide() {
    let repo = Repository {
        id: 1,
        name: "p".into(),
        repo_type: "proxy".into(),
        format: "go".into(),
        visibility: Visibility::Public,
        upstream_url: None,
        config: None,
        created_at: chrono::DateTime::UNIX_EPOCH,
        updated_at: chrono::DateTime::UNIX_EPOCH,
    };
    let short = cache_path(
        CacheRepo(&repo),
        &CacheKey {
            kind: "go-list",
            key: "github.com/org/repo".into(),
        },
    );
    let long = cache_path(
        CacheRepo(&repo),
        &CacheKey {
            kind: "go-list",
            key: "github.com/org/repo/v2".into(),
        },
    );
    assert!(!long.starts_with(&short) && !short.starts_with(&long));
    let segs: Vec<&str> = short.split('/').collect();
    assert_eq!(&segs[..3], &["_proxy_cache", "p", "go-list"]);
    assert_eq!((segs[3].len(), segs[4].len()), (2, 64));
    assert!(segs[4].starts_with(segs[3]));
    assert!(
        !short.contains("github.com"),
        "the human key never reaches the path"
    );
}

#[tokio::test]
async fn singleflight_one_upstream_hit() {
    let fx = Fx::new().await;
    fx.set(|s| s.delay = Duration::from_millis(300));
    let engine = fx.engine(timeouts());
    let (strat, art) = (Strat::default(), "art/x".to_string());
    let (a, b) = tokio::join!(
        engine.fetch(&strat, &fx.up, fx.member(), &art),
        engine.fetch(&strat, &fx.up, fx.member(), &art),
    );
    assert_eq!(found(a).entry.id, found(b).entry.id);
    assert_eq!(fx.hits().len(), 1);
}

#[tokio::test]
async fn singleflight_wait_timeout_proceeds_unlocked() {
    let fx = Fx::new().await;
    fx.set(|s| s.delay = Duration::from_millis(400));
    let engine = fx.engine(Timeouts {
        singleflight_wait: Duration::from_millis(50),
        ..timeouts()
    });
    let (strat, art) = (Strat::default(), "art/x".to_string());
    let (a, b) = tokio::join!(
        engine.fetch(&strat, &fx.up, fx.member(), &art),
        engine.fetch(&strat, &fx.up, fx.member(), &art),
    );
    assert_eq!(
        found(a).entry.id,
        found(b).entry.id,
        "last writer wins on the same row"
    );
    assert_eq!(fx.hits().len(), 2, "the waiter duplicated the download");
}

#[tokio::test]
async fn part_file_unlinked_on_drop_and_cap() {
    let fx = Fx::new().await;
    let mut part = PartFile::new(fx.storage.as_ref(), "_proxy_cache/p/x.part-1".into())
        .await
        .unwrap();
    part.write_chunk(b"abc").await.unwrap();
    let resolved = fx.storage.resolve("_proxy_cache/p/x.part-1").unwrap();
    assert!(std::fs::metadata(&resolved).is_ok());
    drop(part);
    assert!(
        std::fs::metadata(&resolved).is_err(),
        "unlinked synchronously on drop"
    );

    let engine = fx.engine(timeouts());
    for transfer in [Transfer::Buffered, Transfer::Streamed] {
        let strat = Strat {
            max: 5,
            transfer,
            ..Default::default()
        };
        let res = engine
            .fetch(&strat, &fx.up, fx.member(), &"art/big".to_string())
            .await;
        assert!(
            matches!(res, Err(AppError::BadGateway(_))),
            "{transfer:?}: {res:?}"
        );
    }
    assert!(
        fx.row("t-item", "art/big").await.is_none(),
        "nothing recorded"
    );
    assert!(fx.files().is_empty(), "nothing written: {:?}", fx.files());
}

#[tokio::test]
async fn part_file_commit_keeps_file() {
    let fx = Fx::new().await;
    let mut part = PartFile::new(fx.storage.as_ref(), "_proxy_cache/p/k/x.part-1".into())
        .await
        .unwrap();
    part.write_chunk(b"abc").await.unwrap();
    part.write_chunk(b"def").await.unwrap();
    part.commit(fx.storage.as_ref(), "_proxy_cache/p/k/x")
        .await
        .unwrap();
    assert!(!fx
        .storage
        .exists("_proxy_cache/p/k/x.part-1")
        .await
        .unwrap());
    assert_eq!(
        fx.storage.get("_proxy_cache/p/k/x").await.unwrap().as_ref(),
        b"abcdef"
    );
}

/// The ttl is a fact about time, not about a row: nothing is rewritten, no
/// test sleeps, and no entry is stored with a zero ttl, which would take a
/// different path through the engine than the one the server takes.
#[tokio::test]
async fn a_row_goes_stale_when_now_passes_its_expiry() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    let (strat, art) = (Strat::default(), "art/x".to_string());
    found(engine.fetch(&strat, &fx.up, fx.member(), &art).await);
    let fresh = fx.row("t-item", "art/x").await.unwrap();
    assert!(fresh.fresh, "a row just written is fresh");
    assert_eq!(fx.hits().len(), 1);

    // Well inside the fixture's 3600s ttl: still a hit. The margin is wide on
    // purpose: the shift adds to a real clock that moves between the two reads.
    fx.advance(Duration::from_secs(3000));
    assert!(fx.row("t-item", "art/x").await.unwrap().fresh);
    found(engine.fetch(&strat, &fx.up, fx.member(), &art).await);
    assert_eq!(fx.hits().len(), 1, "a fresh row asks nobody");

    fx.advance(Duration::from_secs(700));
    let expired = fx.row("t-item", "art/x").await.unwrap();
    assert!(!expired.fresh, "stale once the expiry has passed");
    assert_eq!(expired.id, fresh.id);
    let served = found(engine.fetch(&strat, &fx.up, fx.member(), &art).await);
    assert_eq!(fx.hits().len(), 2, "an expired row is revalidated");
    assert!(!served.stale, "and serves fresh again afterwards");
    assert!(fx.row("t-item", "art/x").await.unwrap().fresh);
}

#[tokio::test]
async fn buffered_refresh_never_truncates_reader() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    fx.set(|s| s.body = b"short".to_vec());
    let first = found(
        engine
            .fetch(&Strat::default(), &fx.up, fx.member(), &"art/x".to_string())
            .await,
    );
    let path = first.entry.storage_path.clone().unwrap();
    let (len, mut reader) = fx.storage.read_stream(&path).await.unwrap();
    assert_eq!(len, 5);

    fx.expire();
    fx.set(|s| s.body = b"a much longer body than before".to_vec());
    let second = found(
        engine
            .fetch(&Strat::default(), &fx.up, fx.member(), &"art/x".to_string())
            .await,
    );
    assert_eq!(second.entry.storage_path.as_deref(), Some(path.as_str()));
    assert_eq!(second.entry.size, 30);

    let mut held = Vec::new();
    reader.read_to_end(&mut held).await.unwrap();
    assert_eq!(held, b"short");
    assert_eq!(
        engine.bytes(&second).await.unwrap().as_ref(),
        b"a much longer body than before"
    );
    assert_eq!(fx.hits().len(), 2);
}

#[tokio::test]
async fn pointer_row_follows_store_key() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    let body = found(
        engine
            .fetch(
                &pointer_strat(),
                &fx.up,
                fx.member(),
                &"art/tag".to_string(),
            )
            .await,
    );
    let sha = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(b"hello upstream")
    );
    assert_eq!(body.entry.kind, "t-body");
    assert_eq!(body.entry.cache_key, format!("sha256/{sha}"));
    assert_eq!(body.entry.expires_at, None, "the body is immutable");
    assert!(body.entry.storage_path.is_some());

    let pointer = fx.row("t-item", "art/tag").await.unwrap();
    assert_eq!(
        (pointer.storage_path, pointer.digest.as_deref()),
        (None, Some(sha.as_str()))
    );
    assert!(
        pointer.expires_at.is_some(),
        "the pointer carries the policy ttl"
    );

    let again = found(
        engine
            .fetch(
                &pointer_strat(),
                &fx.up,
                fx.member(),
                &"art/tag".to_string(),
            )
            .await,
    );
    assert_eq!(again.entry.id, body.entry.id);
    assert_eq!(fx.hits().len(), 1);
}

#[tokio::test]
async fn pointer_row_touched_with_target() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    found(
        engine
            .fetch(
                &pointer_strat(),
                &fx.up,
                fx.member(),
                &"art/tag".to_string(),
            )
            .await,
    );
    // Far enough that a touch writes a later second, well short of the ttl.
    fx.advance(Duration::from_secs(60));
    let before = fx.row("t-item", "art/tag").await.unwrap().last_used_at;
    found(
        engine
            .fetch(
                &pointer_strat(),
                &fx.up,
                fx.member(),
                &"art/tag".to_string(),
            )
            .await,
    );
    let pointer = fx.row("t-item", "art/tag").await.unwrap();
    let target = fx
        .row(
            "t-body",
            &format!("sha256/{}", pointer.digest.as_deref().unwrap()),
        )
        .await
        .unwrap();
    assert!(pointer.last_used_at > before);
    assert!(target.last_used_at > before);
    assert_eq!(fx.hits().len(), 1);
}

#[tokio::test]
async fn stale_pointer_served_on_upstream_error() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    let fresh = found(
        engine
            .fetch(
                &pointer_strat(),
                &fx.up,
                fx.member(),
                &"art/tag".to_string(),
            )
            .await,
    );
    assert!(!fresh.stale);
    fx.expire();
    fx.set(|s| s.fail = true);
    let stale = found(
        engine
            .fetch(
                &pointer_strat(),
                &fx.up,
                fx.member(),
                &"art/tag".to_string(),
            )
            .await,
    );
    assert!(stale.stale);
    assert_eq!(stale.entry.id, fresh.entry.id);
    assert_eq!(
        engine.bytes(&stale).await.unwrap().as_ref(),
        b"hello upstream"
    );
    let payload = stale.into_payload();
    let resp = engine.stream_response(&payload, Vec::new()).await.unwrap();
    assert_eq!(
        resp.headers()
            .get(header::WARNING)
            .unwrap()
            .to_str()
            .unwrap(),
        "110 - \"Response is Stale\""
    );
    assert_eq!(resp.headers().get(header::CONTENT_LENGTH).unwrap(), "14");
    assert_eq!(fx.hits().len(), 2);
    assert!(
        fx.row("t-item", "art/tag")
            .await
            .unwrap()
            .storage_path
            .is_none(),
        "failures are never cached"
    );
}

#[tokio::test]
async fn stale_pointer_without_target_regets_without_if_none_match() {
    let fx = Fx::new().await;
    fx.set(|s| s.etag = Some("\"v1\"".into()));
    let engine = fx.engine(timeouts());
    let first = found(
        engine
            .fetch(
                &pointer_strat(),
                &fx.up,
                fx.member(),
                &"art/tag".to_string(),
            )
            .await,
    );

    fx.expire();
    let revalidated = found(
        engine
            .fetch(
                &pointer_strat(),
                &fx.up,
                fx.member(),
                &"art/tag".to_string(),
            )
            .await,
    );
    assert_eq!(revalidated.entry.id, first.entry.id);
    assert!(!revalidated.stale, "a 304 refreshes the pointer");
    assert_eq!(fx.hits()[1].2.get(header::IF_NONE_MATCH).unwrap(), "\"v1\"");

    fx.expire();
    fx.storage
        .delete(first.entry.storage_path.as_deref().unwrap())
        .await
        .unwrap();
    let regot = found(
        engine
            .fetch(
                &pointer_strat(),
                &fx.up,
                fx.member(),
                &"art/tag".to_string(),
            )
            .await,
    );
    assert!(!regot.stale);
    assert!(fx
        .storage
        .exists(regot.entry.storage_path.as_deref().unwrap())
        .await
        .unwrap());
    let hits = fx.hits();
    assert_eq!(hits.len(), 3);
    assert!(
        hits[2].2.get(header::IF_NONE_MATCH).is_none(),
        "an evicted target must never be revalidated"
    );
}

#[tokio::test]
async fn buffered_bodies_land_on_disk_as_they_arrive() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    let art = "art/drip".to_string();
    let strat = Strat::default();
    let fetch = engine.fetch(&strat, &fx.up, fx.member(), &art);
    tokio::pin!(fetch);
    let mid_transfer = tokio::time::timeout(Duration::from_millis(500), &mut fetch).await;
    assert!(mid_transfer.is_err(), "the drip is still going");
    let parts: Vec<_> = fx
        .files()
        .into_iter()
        .filter(|p| p.to_string_lossy().contains(".part-"))
        .collect();
    assert_eq!(parts.len(), 1, "a buffered body is on disk, not in memory");
    let done = found(fetch.await);
    assert_eq!(done.entry.size, 20);
    assert!(fx
        .files()
        .iter()
        .all(|p| !p.to_string_lossy().contains(".part-")));
}

#[tokio::test]
async fn body_shorter_than_content_length_is_never_recorded() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    for transfer in [Transfer::Buffered, Transfer::Streamed] {
        let strat = Strat {
            transfer,
            ..Default::default()
        };
        let res = engine
            .fetch(&strat, &fx.up, fx.member(), &"art/lying".to_string())
            .await;
        assert!(
            matches!(res, Err(AppError::BadGateway(_))),
            "{transfer:?}: {res:?}"
        );
    }
    assert!(fx.row("t-item", "art/lying").await.is_none());
    assert!(fx.files().is_empty(), "{:?}", fx.files());
}

#[tokio::test]
async fn buffered_total_timeout_is_502() {
    let fx = Fx::new().await;
    let engine = fx.engine(Timeouts {
        buffered_total: Duration::from_millis(300),
        ..timeouts()
    });
    let res = engine
        .fetch(
            &Strat::default(),
            &fx.up,
            fx.member(),
            &"art/drip".to_string(),
        )
        .await;
    assert!(matches!(res, Err(AppError::BadGateway(_))), "{res:?}");
    assert!(fx.row("t-item", "art/drip").await.is_none());

    let streamed = Strat {
        transfer: Transfer::Streamed,
        ..Default::default()
    };
    let done = found(
        engine
            .fetch(&streamed, &fx.up, fx.member(), &"art/drip".to_string())
            .await,
    );
    assert_eq!(
        done.entry.size, 20,
        "a streamed transfer is bounded by idleness only"
    );
}

#[tokio::test]
async fn head_via_get_shares_singleflight_key() {
    let fx = Fx::new().await;
    fx.set(|s| s.delay = Duration::from_millis(300));
    let engine = fx.engine(timeouts());
    let (strat, art) = (Strat::default(), "art/x".to_string());
    let (h, f) = tokio::join!(
        engine.head(&strat, &fx.up, fx.member(), &art),
        engine.fetch(&strat, &fx.up, fx.member(), &art),
    );
    let head = match h {
        Ok(Outcome::Found(p)) => p,
        other => panic!("{other:?}"),
    };
    assert_eq!(head.size, 14);
    assert_eq!(found(f).entry.size, 14);
    let hits = fx.hits();
    assert_eq!(
        hits.len(),
        1,
        "HEAD and GET on one key share the leader's download"
    );
    assert_eq!(hits[0].0, Method::GET);
}

#[tokio::test]
async fn head_hit_answers_from_row_without_request() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    found(
        engine
            .fetch(&Strat::default(), &fx.up, fx.member(), &"art/x".to_string())
            .await,
    );
    fx.set(|s| s.fail = true);
    let head = match engine
        .head(&Strat::default(), &fx.up, fx.member(), &"art/x".to_string())
        .await
    {
        Ok(Outcome::Found(p)) => p,
        other => panic!("{other:?}"),
    };
    assert!(matches!(head.src, Src::HeadOnly));
    assert_eq!(head.size, 14);
    assert_eq!(
        head.content_type.as_deref(),
        Some("application/octet-stream")
    );
    let resp = engine.stream_response(&head, Vec::new()).await.unwrap();
    assert_eq!(resp.headers().get(header::CONTENT_LENGTH).unwrap(), "14");
    let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
    assert!(body.is_empty());
    assert_eq!(fx.hits().len(), 1);
}

#[tokio::test]
async fn head_on_negative_row_is_404() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    let res = engine
        .fetch(
            &Strat::default(),
            &fx.up,
            fx.member(),
            &"art/missing".to_string(),
        )
        .await;
    assert!(matches!(res, Ok(Outcome::NotFound)));
    let negative = fx.row("t-item", "art/missing").await.unwrap();
    assert_eq!((negative.status, negative.storage_path), (404, None));
    assert!(
        negative.expires_at.is_some(),
        "negative rows carry negative_secs"
    );

    for via_get in [true, false] {
        let strat = Strat {
            via_get,
            ..Default::default()
        };
        let res = engine
            .head(&strat, &fx.up, fx.member(), &"art/missing".to_string())
            .await;
        assert!(matches!(res, Ok(Outcome::NotFound)), "{res:?}");
    }
    assert_eq!(
        fx.hits().len(),
        1,
        "a fresh negative row answers HEAD without upstream"
    );
}

#[tokio::test]
async fn content_chosen_url_on_private_host_is_refused_before_any_request() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    let art = "art/x".to_string();
    for (source, via_get) in [
        (
            UrlSource::Content {
                allow_private: false,
            },
            true,
        ),
        (
            UrlSource::Content {
                allow_private: false,
            },
            false,
        ),
    ] {
        let strat = Strat {
            source,
            via_get,
            ..Default::default()
        };
        let fetched = engine.fetch(&strat, &fx.up, fx.member(), &art).await;
        assert!(
            matches!(fetched, Err(AppError::BadGateway(ref m)) if m.contains("private address")),
            "{fetched:?}"
        );
        let headed = engine.head(&strat, &fx.up, fx.member(), &art).await;
        assert!(matches!(headed, Err(AppError::BadGateway(_))), "{headed:?}");
    }
    assert!(
        fx.hits().is_empty(),
        "the loopback fake was never contacted"
    );
    assert!(fx.row("t-item", "art/x").await.is_none());

    let allowed = Strat {
        source: UrlSource::Content {
            allow_private: true,
        },
        ..Default::default()
    };
    found(engine.fetch(&allowed, &fx.up, fx.member(), &art).await);
    assert_eq!(fx.hits().len(), 1);
}

#[tokio::test]
async fn content_length_describes_the_streamed_file_not_the_row() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    let cached = found(
        engine
            .fetch(&Strat::default(), &fx.up, fx.member(), &"art/x".to_string())
            .await,
    );
    let path = cached.entry.storage_path.clone().unwrap();
    assert_eq!(cached.entry.size, 14);
    fx.storage
        .put(&path, bytes::Bytes::from_static(b"short"))
        .await
        .unwrap();

    let resp = engine
        .stream_response(&cached.into_payload(), Vec::new())
        .await
        .unwrap();
    assert_eq!(resp.headers().get(header::CONTENT_LENGTH).unwrap(), "5");
    let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
    assert_eq!(body.as_ref(), b"short");
}

#[tokio::test]
async fn missing_stored_file_is_404_not_502() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    let payload = Payload::file("npm/hosted/gone-1.0.0.tgz".into(), 3);
    let res = engine.stream_response(&payload, Vec::new()).await;
    assert!(matches!(res, Err(AppError::NotFound(_))), "{res:?}");
}

#[tokio::test]
async fn revalidated_pointer_keeps_its_target_immutable() {
    let fx = Fx::new().await;
    fx.set(|s| s.etag = Some("\"v1\"".into()));
    let engine = fx.engine(timeouts());
    let art = "art/tag".to_string();
    let first = found(
        engine
            .fetch(&pointer_strat(), &fx.up, fx.member(), &art)
            .await,
    );
    assert_eq!(first.entry.expires_at, None);

    fx.expire();
    let revalidated = found(
        engine
            .fetch(&pointer_strat(), &fx.up, fx.member(), &art)
            .await,
    );
    assert_eq!(revalidated.entry.id, first.entry.id);
    let target = fx.row("t-body", &first.entry.cache_key).await.unwrap();
    assert_eq!(
        target.expires_at, None,
        "a 304 extends the pointer, never the body"
    );
    let pointer = fx.row("t-item", "art/tag").await.unwrap();
    assert!(pointer.expires_at.is_some());
    assert_eq!(fx.hits().len(), 2);
}

#[tokio::test]
async fn negative_refresh_unlinks_the_body_it_replaces() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    let art = "art/x".to_string();
    let cached = found(
        engine
            .fetch(&Strat::default(), &fx.up, fx.member(), &art)
            .await,
    );
    let path = cached.entry.storage_path.unwrap();
    assert!(fx.storage.exists(&path).await.unwrap());

    fx.expire();
    fx.set(|s| s.gone = true);
    let res = engine
        .fetch(&Strat::default(), &fx.up, fx.member(), &art)
        .await;
    assert!(matches!(res, Ok(Outcome::NotFound)), "{res:?}");
    let row = fx.row("t-item", "art/x").await.unwrap();
    assert_eq!((row.status, row.storage_path), (404, None));
    assert!(
        !fx.storage.exists(&path).await.unwrap(),
        "no row references the old body any more"
    );
    assert!(fx.files().is_empty(), "{:?}", fx.files());

    fx.set(|s| s.gone = false);
    let shared = found(
        engine
            .fetch(
                &pointer_strat(),
                &fx.up,
                fx.member(),
                &"art/tag".to_string(),
            )
            .await,
    );
    let body_path = shared.entry.storage_path.unwrap();
    fx.expire();
    fx.set(|s| s.gone = true);
    let res = engine
        .fetch(
            &pointer_strat(),
            &fx.up,
            fx.member(),
            &"art/tag".to_string(),
        )
        .await;
    assert!(matches!(res, Ok(Outcome::NotFound)), "{res:?}");
    assert!(
        fx.storage.exists(&body_path).await.unwrap(),
        "a digest-addressed body may be shared and stays for the sweep"
    );
}

#[tokio::test]
async fn peek_never_hits_upstream() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    let (strat, art) = (Strat::default(), "art/x".to_string());
    assert!(engine
        .peek(&strat, fx.member(), &art)
        .await
        .unwrap()
        .is_none());
    assert!(fx.hits().is_empty(), "a cold peek asks nobody");

    let first = found(engine.fetch(&strat, &fx.up, fx.member(), &art).await);
    // Far enough that a touch writes a later second, well short of the ttl.
    fx.advance(Duration::from_secs(60));
    let before = fx.row("t-item", "art/x").await.unwrap();
    let peeked = engine
        .peek(&strat, fx.member(), &art)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peeked.entry.id, first.entry.id);
    assert!(!peeked.stale);
    let after = fx.row("t-item", "art/x").await.unwrap();
    assert_eq!(after.last_used_at, before.last_used_at, "no row touched");

    fx.expire();
    let stale = engine
        .peek(&strat, fx.member(), &art)
        .await
        .unwrap()
        .unwrap();
    assert!(stale.stale, "a stale body is still a body");
    assert_eq!(fx.hits().len(), 1);

    fx.set(|s| s.gone = true);
    let missing = "art/missing".to_string();
    assert!(matches!(
        engine.fetch(&strat, &fx.up, fx.member(), &missing).await,
        Ok(Outcome::NotFound)
    ));
    assert!(
        engine
            .peek(&strat, fx.member(), &missing)
            .await
            .unwrap()
            .is_none(),
        "a negative row is no body"
    );
    fx.set(|s| s.gone = false);
    let tag = "art/tag".to_string();
    let body = found(
        engine
            .fetch(&pointer_strat(), &fx.up, fx.member(), &tag)
            .await,
    );
    let via_pointer = engine
        .peek(&pointer_strat(), fx.member(), &tag)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        via_pointer.entry.id, body.entry.id,
        "a pointer peeks its target"
    );
    assert_eq!(fx.hits().len(), 3);
}

#[tokio::test]
async fn refresh_never_blocks_a_fresh_hit() {
    let fx = Fx::new().await;
    fx.set(|s| s.etag = Some("\"v1\"".into()));
    let engine = fx.engine(timeouts());
    let (strat, art) = (Strat::default(), "art/x".to_string());
    let first = found(engine.fetch(&strat, &fx.up, fx.member(), &art).await);
    fx.set(|s| {
        s.etag = Some("\"v2\"".into());
        s.delay = Duration::from_secs(2);
    });
    let refresh = {
        let (engine, up, repo) = (engine.clone(), fx.up.clone(), fx.repo.clone());
        tokio::spawn(async move {
            engine
                .refresh(
                    &Strat::default(),
                    &up,
                    CacheRepo(&repo),
                    &"art/x".to_string(),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fx.hits().len(), 2, "the refresh is in flight");
    let started = std::time::Instant::now();
    let hit = found(engine.fetch(&strat, &fx.up, fx.member(), &art).await);
    assert_eq!(hit.entry.id, first.entry.id);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a fresh hit waited {:?} behind the refresh",
        started.elapsed()
    );
    assert_eq!(fx.hits().len(), 2, "the hit made no request");
    found(refresh.await.unwrap());
    assert_eq!(fx.hits().len(), 2);
}

#[tokio::test]
async fn refresh_never_records_a_miss() {
    let fx = Fx::new().await;
    fx.set(|s| s.etag = Some("\"v1\"".into()));
    let engine = fx.engine(timeouts());
    let (strat, art) = (Strat::default(), "art/x".to_string());
    let first = found(engine.fetch(&strat, &fx.up, fx.member(), &art).await);
    let path = first.entry.storage_path.clone().unwrap();

    let same = found(engine.refresh(&strat, &fx.up, fx.member(), &art).await);
    assert_eq!(same.entry.id, first.entry.id);
    let hits = fx.hits();
    assert_eq!(
        hits.len(),
        2,
        "a fresh row is refreshed conditionally, never a hit"
    );
    assert_eq!(
        hits[1].2.get(header::IF_NONE_MATCH).unwrap(),
        "\"v1\"",
        "the request carried the row's ETag"
    );

    fx.set(|s| s.gone = true);
    let res = engine.refresh(&strat, &fx.up, fx.member(), &art).await;
    assert!(matches!(res, Ok(Outcome::NotFound)), "{res:?}");
    fx.set(|s| {
        s.gone = false;
        s.fail = true;
    });
    let res = engine.refresh(&strat, &fx.up, fx.member(), &art).await;
    assert!(matches!(res, Ok(Outcome::NotFound)), "{res:?}");
    let row = fx.row("t-item", "art/x").await.unwrap();
    assert_eq!(row.status, 200, "no negative row");
    assert!(fx.storage.exists(&path).await.unwrap(), "no file deleted");
    fx.set(|s| s.fail = false);
    let served = found(engine.fetch(&strat, &fx.up, fx.member(), &art).await);
    assert_eq!(served.entry.id, first.entry.id);
    assert_eq!(
        fx.hits().len(),
        4,
        "the fresh row still serves without a request"
    );

    fx.set(|s| s.gone = true);
    fx.expire();
    let res = engine.fetch(&strat, &fx.up, fx.member(), &art).await;
    assert!(matches!(res, Ok(Outcome::NotFound)), "{res:?}");
    let row = fx.row("t-item", "art/x").await.unwrap();
    assert_eq!(
        row.status, 404,
        "the same route under fetch writes the 404 row"
    );
}

fn sha256_hex(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(body))
}

#[tokio::test]
async fn warm_entry_with_stale_digest_is_refetched() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    let art = "art/pinned".to_string();
    let first = Strat {
        policy: CachePolicy::Immutable,
        known: Some(sha256_hex(b"hello upstream")),
        ..Default::default()
    };
    found(engine.fetch(&first, &fx.up, fx.member(), &art).await);
    found(engine.fetch(&first, &fx.up, fx.member(), &art).await);
    assert_eq!(fx.hits().len(), 1, "a matching warm entry is served");

    fx.set(|s| s.body = b"republished".to_vec());
    let republished = Strat {
        policy: CachePolicy::Immutable,
        known: Some(sha256_hex(b"republished")),
        ..Default::default()
    };
    let got = found(engine.fetch(&republished, &fx.up, fx.member(), &art).await);
    assert_eq!(fx.hits().len(), 2, "a stale digest is a miss");
    assert_eq!(
        got.entry.digest.as_deref(),
        Some(sha256_hex(b"republished").as_str())
    );
    assert_eq!(engine.bytes(&got).await.unwrap().as_ref(), b"republished");
}

#[tokio::test]
async fn a_body_contradicting_its_known_digest_is_refused() {
    let fx = Fx::new().await;
    let engine = fx.engine(timeouts());
    let strat = Strat {
        known: Some(sha256_hex(b"something else")),
        ..Default::default()
    };
    let res = engine
        .fetch(&strat, &fx.up, fx.member(), &"art/x".to_string())
        .await;
    assert!(matches!(res, Err(AppError::BadGateway(_))), "{res:?}");
    assert!(fx.row("t-item", "art/x").await.is_none(), "nothing recorded");
}
