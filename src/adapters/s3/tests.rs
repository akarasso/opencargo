use std::sync::Mutex;

use chrono::TimeDelta;
use tokio::io::AsyncReadExt;

use super::*;
use crate::testing::fake_s3::{FakeS3, Running};
use crate::testing::ledger::MemLedger;

pub(crate) fn settings(endpoint: &str, prefix: &str) -> S3Settings {
    S3Settings {
        endpoint: Some(endpoint.to_string()),
        region: "us-east-1".to_string(),
        bucket: "test".to_string(),
        prefix: prefix.to_string(),
        allow_http: true,
        virtual_hosted_style: false,
        access_key_id: "key".to_string(),
        secret_access_key: "secret".to_string(),
        session_token: None,
        request_timeout: Duration::from_secs(5),
        completion_timeout: Duration::from_secs(5),
        part_size: 8,
        max_multipart_uploads: 2,
        exists_cache_entries: 16,
        max_retries: Some(0),
    }
}

struct Fx {
    running: Running,
    ledger: Arc<MemLedger>,
    storage: Arc<S3Storage>,
}

impl Fx {
    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    async fn with(tweak: impl FnOnce(&mut S3Settings)) -> Self {
        let running = FakeS3::start().await;
        let mut s = settings(&running.endpoint, "inst/a");
        tweak(&mut s);
        let ledger = Arc::new(MemLedger::default());
        let storage = Arc::new(
            S3Storage::build(
                &s,
                StoreIdentity("artifacts".to_string()),
                ledger.clone(),
                Arc::new(crate::adapters::system::SystemClock),
            )
            .unwrap(),
        );
        Self {
            running,
            ledger,
            storage,
        }
    }

    fn fake(&self) -> &FakeS3 {
        &self.running.fake
    }

    fn dyn_storage(&self) -> Arc<dyn StorageBackend> {
        self.storage.clone()
    }
}

mod contract {
    crate::storage_contract!(async {
        let fx = super::Fx::new().await;
        let s = fx.dyn_storage();
        (fx, s)
    });
}

#[tokio::test]
async fn keys_live_under_the_prefix_and_list_as_logical_keys() {
    let fx = Fx::new().await;
    fx.storage.put("a/b", Bytes::from_static(b"x")).await.unwrap();
    assert_eq!(fx.fake().keys(), vec!["inst/a/a/b".to_string()]);
    let listed: Vec<_> = fx.storage.list("").try_collect().await.unwrap();
    assert_eq!(listed[0].key, "a/b");
    let long = "k".repeat(S3_MAX_KEY_BYTES - "inst/a/".len() + 1);
    assert!(
        matches!(fx.storage.put(&long, Bytes::new()).await, Err(StorageError::InvalidPath(_))),
        "the prefix counts against the key budget"
    );
    assert_eq!(fx.storage.key_budget().key, S3_MAX_KEY_BYTES - "inst/a/".len());
}

/// A body over one part is a multipart upload recorded in the ledger while
/// open; nothing is listed before the completion, and the row goes with it.
#[tokio::test]
async fn uncommitted_bytes_of_an_open_multipart_are_never_listed() {
    let fx = Fx::new().await;
    let mut w = fx.storage.writer("big").await.unwrap();
    w.reserve(20).await.unwrap();
    w.write(Bytes::from_static(b"0123456789abcdefghij")).await.unwrap();
    assert_eq!(fx.fake().open_uploads(), 1);
    assert_eq!(fx.ledger.in_flight().await.unwrap(), 1);
    let listed: Vec<_> = fx.storage.list("").try_collect().await.unwrap();
    assert!(listed.is_empty());
    assert_eq!(w.commit().await.unwrap(), 20);
    assert_eq!(fx.storage.get("big").await.unwrap().as_ref(), b"0123456789abcdefghij");
    assert_eq!(fx.ledger.in_flight().await.unwrap(), 0);
}

#[tokio::test]
async fn a_dropped_multipart_writer_aborts_its_upload() {
    let fx = Fx::new().await;
    let mut w = fx.storage.writer("big").await.unwrap();
    w.write(Bytes::from_static(b"0123456789abcdefghij")).await.unwrap();
    drop(w);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while fx.fake().open_uploads() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(fx.fake().open_uploads(), 0);
    assert!(fx.storage.stat("big").await.unwrap().is_none());
}

/// The budget blocks a writer only once its bytes would need a multipart
/// upload; a body under one part never waits.
#[tokio::test]
async fn multipart_budget_blocks_the_extra_writer() {
    let fx = Fx::with(|s| s.max_multipart_uploads = 1).await;
    let mut first = fx.storage.writer("one").await.unwrap();
    first.reserve(20).await.unwrap();
    let mut second = fx.storage.writer("two").await.unwrap();
    let blocked = tokio::time::timeout(Duration::from_secs(1), second.reserve(20)).await;
    assert!(blocked.is_err(), "the second multipart waits for a slot");
    fx.storage.put("small", Bytes::from_static(b"tiny")).await.unwrap();
    first.write(Bytes::from_static(b"0123456789abcdefghij")).await.unwrap();
    first.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), second.reserve(20)).await.unwrap().unwrap();
}

/// A completion that lands but answers past the bound is decided by `stat`.
#[tokio::test]
async fn completion_past_the_bound_is_decided_by_stat() {
    let fx = Fx::with(|s| s.completion_timeout = Duration::from_secs(1)).await;
    fx.fake().delay_next("complete", Duration::from_secs(3));
    let mut w = fx.storage.writer("late").await.unwrap();
    w.write(Bytes::from_static(b"0123456789abcdefghij")).await.unwrap();
    assert_eq!(w.commit().await.unwrap(), 20);

    fx.fake().delay_next("copy", Duration::from_secs(3));
    fx.storage.copy_object("late", "copied").await.unwrap();
    assert_eq!(fx.storage.stat("copied").await.unwrap().unwrap().size, 20);
}

#[tokio::test]
async fn an_embedded_copy_error_is_unavailable() {
    let fx = Fx::new().await;
    fx.storage.put("src", Bytes::from_static(b"x")).await.unwrap();
    fx.fake().embed_copy_error();
    assert!(matches!(
        fx.storage.copy_object("src", "dst").await,
        Err(StorageError::Unavailable)
    ));
}

#[tokio::test]
async fn a_partial_batch_delete_is_an_error() {
    let fx = Fx::new().await;
    for k in ["d/1", "d/2"] {
        fx.storage.put(k, Bytes::from_static(b"x")).await.unwrap();
    }
    fx.fake().fail_delete_of("inst/a/d/2");
    let keys = vec!["d/1".to_string(), "d/2".to_string()];
    assert!(matches!(fx.storage.delete_batch(&keys).await, Err(StorageError::Unavailable)));
}

#[tokio::test]
async fn no_checksum_header_by_default() {
    let fx = Fx::new().await;
    fx.storage.put("small", Bytes::from_static(b"x")).await.unwrap();
    let mut w = fx.storage.writer("big").await.unwrap();
    w.write(Bytes::from_static(b"0123456789abcdefghij")).await.unwrap();
    w.commit().await.unwrap();
    assert!(fx.fake().checksum_headers().is_empty(), "{:?}", fx.fake().checksum_headers());
}

/// `head` fills and reads the cache, `stat` neither; a delete of this
/// process empties it.
#[tokio::test]
async fn exists_cache_fill_and_invalidation_rules() {
    let fx = Fx::new().await;
    fx.storage.put("k", Bytes::from_static(b"x")).await.unwrap();
    assert!(fx.storage.head("k").await.unwrap().is_some());
    fx.fake().remove("inst/a/k");
    assert!(fx.storage.head("k").await.unwrap().is_some(), "a stale positive, healed by the read path");
    assert!(fx.storage.stat("k").await.unwrap().is_none(), "stat is never cached");
    fx.storage.delete("k").await.unwrap();
    assert!(fx.storage.head("k").await.unwrap().is_none());
    assert_eq!(fx.storage.cached_entries(), 0);
}

#[tokio::test]
async fn probe_re_puts_its_health_object() {
    let fx = Fx::new().await;
    fx.storage.probe().await.unwrap();
    assert!(fx.fake().keys().contains(&"inst/a/_backend/health".to_string()));
    fx.fake().remove("inst/a/_backend/health");
    fx.storage.probe().await.unwrap();
    assert!(fx.fake().keys().contains(&"inst/a/_backend/health".to_string()));
}

/// Another store's rows in the shared ledger are not this store's to sweep.
#[tokio::test]
async fn sweep_aborts_only_this_stores_idle_uploads() {
    let fx = Fx::new().await;
    let mut w = fx.storage.writer("big").await.unwrap();
    w.write(Bytes::from_static(b"0123456789abcdefghij")).await.unwrap();
    std::mem::forget(w);
    fx.ledger.opened("foreign", "backup\u{1f}other/key", Utc::now()).await.unwrap();
    let later = Utc::now() + TimeDelta::hours(2);
    let swept = fx.storage.sweep_abandoned(Duration::from_secs(3600), later).await.unwrap();
    assert_eq!(swept, 1);
    assert_eq!(fx.fake().open_uploads(), 0);
    assert_eq!(fx.ledger.in_flight().await.unwrap(), 1, "the backup store's row stays");
}

#[tokio::test]
async fn tls_roots_are_the_compiled_in_set() {
    let fx = Fx::new().await;
    assert_eq!(fx.storage.trusted_roots(), webpki_root_certs::TLS_SERVER_ROOT_CERTS.len());
    assert!(fx.storage.trusted_roots() > 100);
}

#[tokio::test]
async fn a_fault_is_textless_and_a_missing_object_is_not_a_fault() {
    let fx = Fx::new().await;
    assert!(matches!(fx.storage.get("absent").await, Err(StorageError::NotFound)));
    fx.fake().fail_next(1);
    let err = fx.storage.get("absent").await.unwrap_err();
    assert!(matches!(err, StorageError::Unavailable));
    assert_eq!(err.to_string(), "the storage backend is unavailable, try again");
}

#[tokio::test]
async fn read_stream_serves_the_whole_object() {
    let fx = Fx::new().await;
    fx.storage.put("r", Bytes::from_static(b"stream me")).await.unwrap();
    let mut read = fx.storage.read_stream("r").await.unwrap();
    let mut body = Vec::new();
    read.body.read_to_end(&mut body).await.unwrap();
    assert_eq!((read.total, body.as_slice()), (9, b"stream me".as_slice()));
}

#[test]
fn env_allowlist_is_the_only_environment_read() {
    let cfg = crate::config::S3Config {
        bucket: "from-config".to_string(),
        ..Default::default()
    };
    let asked = Mutex::new(Vec::new());
    let env = |name: &str| {
        asked.lock().unwrap().push(name.to_string());
        match name {
            "AWS_ACCESS_KEY_ID" => Some("id".to_string()),
            "OPENCARGO_S3_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            "AWS_PROFILE" | "AWS_ENDPOINT_URL" => Some("never read".to_string()),
            _ => None,
        }
    };
    let resolved = S3Settings::resolve(&cfg, &env).unwrap();
    assert_eq!(resolved.bucket, "from-config");
    assert_eq!((resolved.access_key_id.as_str(), resolved.secret_access_key.as_str()), ("id", "secret"));
    for name in asked.lock().unwrap().iter() {
        assert!(settings::ENV_ALLOWLIST.contains(&name.as_str()), "{name} is not allowlisted");
    }
    assert!(S3Settings::resolve(&cfg, &|_| None).is_err(), "credentials are required");
    assert_eq!(format!("{resolved:?}"), "S3Settings(..)");
}

/// The ledger is keyed on the declared identity: the same store reached
/// under another endpoint spelling sweeps what the first one left.
#[tokio::test]
async fn ledger_survives_an_endpoint_rename() {
    let fx = Fx::new().await;
    let mut w = fx.storage.writer("big").await.unwrap();
    w.write(Bytes::from_static(b"0123456789abcdefghij")).await.unwrap();
    std::mem::forget(w);

    let renamed = fx.running.endpoint.replace("127.0.0.1", "localhost");
    let again = S3Storage::build(
        &settings(&renamed, "inst/a"),
        StoreIdentity("artifacts".to_string()),
        fx.ledger.clone(),
        Arc::new(crate::adapters::system::SystemClock),
    )
    .unwrap();
    let later = Utc::now() + TimeDelta::hours(2);
    assert_eq!(again.sweep_abandoned(Duration::from_secs(3600), later).await.unwrap(), 1);
    assert_eq!(fx.fake().open_uploads(), 0);
}

#[tokio::test]
async fn identity_renders_no_location() {
    let fx = Fx::new().await;
    let shown = format!("{:?}", fx.storage.identity());
    for secret in ["test", "127.0.0.1", "inst/a", "key"] {
        assert!(!shown.contains(secret), "{shown} names {secret}");
    }
}
