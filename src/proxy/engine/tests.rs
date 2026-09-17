use std::time::Duration;

use axum::http::{header, Method};
use tokio::io::AsyncReadExt;

use super::fixture::*;
use super::*;
use crate::db::Repository;
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
        visibility: "public".into(),
        upstream_url: None,
        config_json: None,
        created_at: String::new(),
        updated_at: String::new(),
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
    let mut part = PartFile::new(&fx.storage, "_proxy_cache/p/x.part-1".into())
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
    let mut part = PartFile::new(&fx.storage, "_proxy_cache/p/k/x.part-1".into())
        .await
        .unwrap();
    part.write_chunk(b"abc").await.unwrap();
    part.write_chunk(b"def").await.unwrap();
    part.commit(&fx.storage, "_proxy_cache/p/k/x")
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

    fx.expire().await;
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
    sqlx::query("UPDATE proxy_cache_entries SET last_used_at = datetime('now', '-1 day')")
        .execute(&fx.pool)
        .await
        .unwrap();
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
    fx.expire().await;
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

    fx.expire().await;
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

    fx.expire().await;
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
        (UrlSource::Content { allow_private: false }, true),
        (UrlSource::Content { allow_private: false }, false),
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
    assert!(fx.hits().is_empty(), "the loopback fake was never contacted");
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
