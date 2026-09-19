//! What a warm npm proxy holds. A packument is the biggest document this
//! server serves, and a client asks for one per dependency: the cost of a
//! warm read must be the cost of streaming a file, not of the document.
//!
//! The bound is on live heap, not on RSS and not on a clock: an allocator
//! that counts what this process has asked for and not yet given back is
//! the same number on an idle machine and on a loaded one.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{json, Value};

use common::fake_upstream::npm as fake_npm;
use common::{proxy, spawn_server, SpawnOpts};
use opencargo::config::RepositoryFormat;

/// The fixture: 3000 versions carrying the prose a real packument carries,
/// which is 8.1 MiB of document and 1.3 MiB once rendered for install-v1.
const VERSIONS: usize = 3000;
/// The warm readers, at once: a per-request document costs this many times
/// over, a streamed file does not.
const READERS: usize = 4;
/// The peak of a round is the lowest of this many, because the counter is
/// the whole process's: anything else running here only ever adds to it,
/// so the smallest round is the closest reading of the warm path. A cost
/// paid per request is in every round, and the lowest one still fails.
const ROUNDS: usize = 3;

/// The live heap one round of warm reads may add, over the whole process.
///
/// Measured here: 0.35 to 0.51 MiB, the highest single round 1.2 MiB, most
/// of it the test client's own HTTP read buffers. The same rounds against
/// the read path this replaced, which parsed the document into a
/// `serde_json::Value` per request, added 93 MiB. One reader that parsed
/// and rendered the document again would add 9.5 MiB on its own, and four
/// readers that merely buffered the rendered answer 5.2 MiB: the bound is
/// below what a single regressed reader costs, not a margin around a
/// measurement.
const MAX_LIVE_BYTES: usize = 2 * 1024 * 1024;

/// A packument must be worth the assertion: a shrunk fixture would pass
/// the bound whatever the server did with it.
const MIN_PACKUMENT_BYTES: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------

struct Counted;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static ALLOC: Counted = Counted;

unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(p, layout);
    }
}

/// The live heap this process reached while `f` ran, above what it held
/// when `f` started. The default `realloc` and `alloc_zeroed` go through
/// `alloc` and `dealloc`, so every byte is counted once.
async fn peak_live<F, T>(f: F) -> (usize, T)
where
    F: std::future::Future<Output = T>,
{
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f.await;
    (
        PEAK.load(Ordering::Relaxed).saturating_sub(base),
        out,
    )
}

fn packument(name: &str) -> Value {
    let mut versions = serde_json::Map::new();
    let mut times = serde_json::Map::new();
    for n in 0..VERSIONS {
        let version = format!("1.{}.{}", n / 100, n % 100);
        versions.insert(
            version.clone(),
            json!({
                "name": name,
                "version": version,
                "description": "a package with a description as long as packuments really carry, \
                                repeated so the document weighs what a popular one weighs",
                "readme": "#".repeat(2048),
                "scripts": {"build": "tsc -p .", "test": "vitest run", "lint": "eslint ."},
                "dependencies": {"left-pad": "^1.3.0", "semver": "^7.6.2", "chalk": "^4.1.2"},
                "devDependencies": {"typescript": "^5.4.0", "vitest": "^1.6.0"},
                "maintainers": [{"name": "someone", "email": "someone@example.invalid"}],
                "_npmOperationalInternal": {"host": "s3://npm-registry-packages"},
                "dist": {
                    "tarball": format!("https://upstream.invalid/{name}/-/{name}-{version}.tgz"),
                    "integrity": "sha512-".to_string() + &"a".repeat(88),
                    "shasum": "b".repeat(40)
                }
            }),
        );
        times.insert(version, json!("2026-01-01T00:00:00.000Z"));
    }
    json!({
        "_id": name,
        "name": name,
        "description": "a heavy packument",
        "dist-tags": {"latest": format!("1.{}.{}", (VERSIONS - 1) / 100, (VERSIONS - 1) % 100)},
        "versions": Value::Object(versions),
        "time": Value::Object(times)
    })
}

/// One warm read, streamed and dropped chunk by chunk: what the client
/// holds is never what the assertion is about.
async fn read(client: &reqwest::Client, url: &str) -> usize {
    let mut resp = client
        .get(url)
        .header(
            reqwest::header::ACCEPT,
            "application/vnd.npm.install-v1+json",
        )
        .send()
        .await
        .expect("packument read failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let mut seen = 0;
    while let Some(chunk) = resp.chunk().await.expect("packument body failed") {
        seen += chunk.len();
    }
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_warm_packument_read_holds_no_document() {
    let name = "heavy";
    let doc = packument(name);
    let raw = serde_json::to_vec(&doc).expect("packument");
    assert!(
        raw.len() >= MIN_PACKUMENT_BYTES,
        "the fixture packument is {} bytes, under the {MIN_PACKUMENT_BYTES} this asserts over",
        raw.len()
    );
    drop(raw);

    let upstream = fake_npm::start(doc, "\"v1\"").await;
    let server = spawn_server(SpawnOpts {
        repositories: vec![proxy(
            "npm-proxy",
            RepositoryFormat::Npm,
            &upstream.base_url,
        )],
        ..Default::default()
    })
    .await;
    let url = format!("{}/npm-proxy/{name}", server.base_url);
    let client = reqwest::Client::new();

    let served = read(&client, &url).await;
    assert!(served > 0, "the warming read served nothing");

    let mut peaks = Vec::new();
    let mut sizes = Vec::new();
    for _ in 0..ROUNDS {
        let (peak, round) = peak_live(async {
            let mut readers = Vec::new();
            for _ in 0..READERS {
                readers.push(read(&client, &url));
            }
            futures_util::future::join_all(readers).await
        })
        .await;
        peaks.push(peak);
        sizes.extend(round);
    }
    let peak = peaks.iter().copied().min().expect("a round was measured");

    assert!(
        sizes.iter().all(|&n| n == served),
        "the warm reads did not all serve the same document: {sizes:?} against {served}"
    );
    assert_eq!(
        upstream.packument_hits().len(),
        1,
        "a warm read must not go upstream"
    );
    assert!(
        peak <= MAX_LIVE_BYTES,
        "{READERS} warm readers of a {served}-byte packument added {peak} bytes of live heap \
         (rounds: {peaks:?}), over the {MAX_LIVE_BYTES} this server is allowed: a packument is \
         being held, not streamed"
    );
}
