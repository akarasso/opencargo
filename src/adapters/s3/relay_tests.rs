//! Real-wire faults: MinIO behind a relay that resets or stalls one
//! connection. Runs only when an S3 endpoint is configured the way
//! `scripts/test-s3.sh` configures it; skips otherwise.

use chrono::TimeDelta;
use tokio::io::AsyncReadExt;

use super::*;
use crate::testing::ledger::MemLedger;
use crate::testing::relay::{Fault, Relay, RelayHandle};

struct Wire {
    relay: RelayHandle,
    ledger: Arc<MemLedger>,
    storage: S3Storage,
}

const MIB: usize = 1024 * 1024;

async fn wire(tweak: impl FnOnce(&mut S3Settings)) -> Option<Wire> {
    let Ok(endpoint) = std::env::var("OPENCARGO_S3_ENDPOINT") else {
        eprintln!("skipped: OPENCARGO_S3_ENDPOINT is not set (scripts/test-s3.sh sets it)");
        return None;
    };
    let relay = Relay::start(&endpoint).await;
    let mut settings = S3Settings {
        endpoint: Some(relay.endpoint.clone()),
        region: "us-east-1".to_string(),
        bucket: std::env::var("OPENCARGO_TEST_S3_BUCKET").unwrap_or_else(|_| "opencargo-test".to_string()),
        prefix: format!("relay/{}", uuid::Uuid::new_v4().simple()),
        allow_http: true,
        virtual_hosted_style: false,
        access_key_id: std::env::var("OPENCARGO_S3_ACCESS_KEY_ID").unwrap_or_default(),
        secret_access_key: std::env::var("OPENCARGO_S3_SECRET_ACCESS_KEY").unwrap_or_default(),
        session_token: None,
        request_timeout: Duration::from_secs(2),
        completion_timeout: Duration::from_secs(30),
        part_size: 5 * MIB,
        max_multipart_uploads: 4,
        exists_cache_entries: 0,
        max_retries: None,
    };
    tweak(&mut settings);
    let ledger = Arc::new(MemLedger::default());
    let storage = S3Storage::build(
        &settings,
        StoreIdentity("artifacts".to_string()),
        ledger.clone(),
        Arc::new(crate::adapters::system::SystemClock),
    )
    .unwrap();
    Some(Wire {
        relay,
        ledger,
        storage,
    })
}

fn body(len: usize, seed: u8) -> Bytes {
    Bytes::from((0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect::<Vec<u8>>())
}

async fn write_multipart(storage: &S3Storage, key: &str, data: &Bytes) -> Result<u64, StorageError> {
    let mut w = storage.writer(key).await?;
    for chunk in data.chunks(MIB) {
        w.reserve(chunk.len()).await?;
        w.write(Bytes::copy_from_slice(chunk)).await?;
    }
    w.commit().await
}

async fn read_all(storage: &S3Storage, key: &str) -> Result<Vec<u8>, std::io::Error> {
    let mut read = storage.read_stream(key).await.map_err(std::io::Error::other)?;
    let mut out = Vec::new();
    read.body.read_to_end(&mut out).await?;
    Ok(out)
}

/// A part whose connection resets before its answer is retried; the
/// object is whole.
#[tokio::test]
async fn a_reset_part_is_retried() {
    let Some(w) = wire(|_| {}).await else { return };
    w.relay.relay.arm("partNumber=2", Fault::ResetBeforeAnswer);
    let data = body(12 * MIB, 1);
    assert_eq!(write_multipart(&w.storage, "retried", &data).await.unwrap(), data.len() as u64);
    assert_eq!(w.relay.relay.hits(), 1);
    assert_eq!(w.storage.get("retried").await.unwrap(), data);
    assert_eq!(w.ledger.in_flight().await.unwrap(), 0);
}

/// A dropped writer aborts its upload and closes its ledger row.
#[tokio::test]
async fn a_dropped_writer_aborts_on_the_wire() {
    let Some(w) = wire(|_| {}).await else { return };
    let mut writer = w.storage.writer("dropped").await.unwrap();
    writer.write(body(6 * MIB, 2)).await.unwrap();
    assert_eq!(w.ledger.in_flight().await.unwrap(), 1);
    drop(writer);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while w.ledger.in_flight().await.unwrap() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(w.ledger.in_flight().await.unwrap(), 0, "the abort closed the row");
    assert!(w.storage.stat("dropped").await.unwrap().is_none());
}

/// A writer lost with its process leaves a ledger row the sweep aborts.
#[tokio::test]
async fn a_crashed_writer_is_swept_through_the_ledger() {
    let Some(w) = wire(|_| {}).await else { return };
    let mut writer = w.storage.writer("crashed").await.unwrap();
    writer.write(body(6 * MIB, 3)).await.unwrap();
    std::mem::forget(writer);
    let later = Utc::now() + TimeDelta::hours(2);
    assert_eq!(w.storage.sweep_abandoned(Duration::from_secs(3600), later).await.unwrap(), 1);
    assert_eq!(w.ledger.in_flight().await.unwrap(), 0);
    assert!(w.storage.stat("crashed").await.unwrap().is_none());
}

/// A GET reset part-way errors; it never hands out a short or mixed body,
/// and an overwrite between the reset and the next read is read whole.
#[tokio::test]
async fn a_reset_read_errors_and_an_overwrite_never_mixes() {
    let Some(w) = wire(|s| s.max_retries = Some(0)).await else { return };
    let first = body(3 * MIB, 4);
    w.storage.put("read", first.clone()).await.unwrap();
    w.relay.relay.arm("GET /", Fault::ResetAfter(MIB));
    match read_all(&w.storage, "read").await {
        Err(_) => {}
        Ok(bytes) => assert_eq!(bytes, first.as_ref(), "a resumed read is exact"),
    }
    let second = body(3 * MIB, 5);
    w.storage.put("read", second.clone()).await.unwrap();
    assert_eq!(read_all(&w.storage, "read").await.unwrap(), second.as_ref());
}

/// A consumer slower than the read timeout still gets every byte: the
/// timeout is idleness of the wire, not of the reader.
#[tokio::test]
async fn a_slow_consumer_is_not_a_read_idle() {
    let Some(w) = wire(|_| {}).await else { return };
    let data = body(2 * MIB, 6);
    w.storage.put("slow", data.clone()).await.unwrap();
    let mut read = w.storage.read_stream("slow").await.unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = read.body.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() <= 512 * 1024 {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
    assert_eq!(out, data.as_ref());
}

/// A wire that stops answering past the read timeout fails the read.
#[tokio::test]
async fn a_stalled_wire_is_a_read_idle() {
    let Some(w) = wire(|s| s.max_retries = Some(0)).await else { return };
    let data = body(2 * MIB, 7);
    w.storage.put("stalled", data.clone()).await.unwrap();
    w.relay.relay.arm("GET /", Fault::StallAfter(MIB, Duration::from_secs(6)));
    assert!(read_all(&w.storage, "stalled").await.is_err());
}
