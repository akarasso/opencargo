mod common;

use std::io::Write as _;
use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{json, Value};

use common::fake_osv::{self, cvss_record, labelled_record, FakeOsv};
use common::{
    build_npm_publish_body, build_tarball, hosted, spawn_server, SpawnOpts, TestServer,
    STATIC_TOKEN,
};
use opencargo::config::{Config, RepositoryFormat, Visibility, VulnScanConfig};

const NPM_REPO: &str = "test-npm";
const GO_REPO: &str = "test-go";
const PKG: &str = "@test/vuln-pkg";
const VERSION: &str = "1.0.0";
const DEP: &str = "lodash";
const DEP_VERSION: &str = "4.17.20";

const V3_CRITICAL: &str = "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H";
const V3_MEDIUM: &str = "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:L/I:N/A:N";
const V4_HIGH: &str = "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:N/VA:N/SC:N/SI:N/SA:N";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct Lab {
    osv: FakeOsv,
    server: TestServer,
    client: reqwest::Client,
}

fn scan(block_on_critical: bool, fail_closed: bool) -> VulnScanConfig {
    VulnScanConfig {
        enabled: true,
        block_on_critical,
        fail_closed,
        ..Default::default()
    }
}

/// A fake OSV plus an opencargo pointed at it, with an npm and a go repository.
async fn lab(vuln: VulnScanConfig) -> Lab {
    let osv = fake_osv::start().await;
    let vuln = VulnScanConfig {
        osv_base_url: osv.base_url.clone(),
        ..vuln
    };
    spawn_lab(osv, vuln).await
}

async fn spawn_lab(osv: FakeOsv, vuln: VulnScanConfig) -> Lab {
    let server = spawn_server(SpawnOpts {
        repositories: vec![
            hosted(NPM_REPO, RepositoryFormat::Npm, Visibility::Public),
            hosted(GO_REPO, RepositoryFormat::Go, Visibility::Public),
        ],
        vuln,
        ..Default::default()
    })
    .await;
    Lab {
        osv,
        server,
        client: reqwest::Client::new(),
    }
}

impl Lab {
    async fn db(&self) -> sqlx::SqlitePool {
        let db_path = self.server.tmp.path().join("opencargo.db");
        sqlx::SqlitePool::connect(&format!("sqlite:{}", db_path.display()))
            .await
            .expect("failed to open the server database")
    }

    /// The one `vulnerability_scans` row: `(status, scan_results_json)`.
    async fn scan_row(&self) -> (String, String) {
        let pool = self.db().await;
        let row = sqlx::query_as("SELECT status, scan_results_json FROM vulnerability_scans")
            .fetch_one(&pool)
            .await
            .expect("scan row");
        pool.close().await;
        row
    }

    /// Mark `lodash@4.17.20` as affected by `record`, ready for `publish`.
    fn advisory(&self, record: Value) {
        let id = record["id"].as_str().expect("record id").to_string();
        self.osv.affect("npm", DEP, DEP_VERSION, &[&id]);
        self.osv.record(record);
    }

    async fn publish(&self, deps: Value) -> reqwest::Response {
        let tarball = build_tarball(&format!(r#"{{"name":"{PKG}","version":"{VERSION}"}}"#));
        let mut body = build_npm_publish_body(PKG, VERSION, "vuln test", &tarball);
        body["versions"][VERSION]["dependencies"] = deps;
        self.client
            .put(format!("{}/{NPM_REPO}/{PKG}", self.server.base_url))
            .bearer_auth(STATIC_TOKEN)
            .json(&body)
            .send()
            .await
            .expect("publish request failed")
    }

    async fn publish_lodash(&self) -> reqwest::Response {
        self.publish(json!({ DEP: DEP_VERSION })).await
    }

    async fn vulns(&self) -> Value {
        let resp = self
            .client
            .get(format!(
                "{}/api/v1/vulns/{PKG}/{VERSION}",
                self.server.base_url
            ))
            .send()
            .await
            .expect("vulns request failed");
        assert_eq!(resp.status(), StatusCode::OK);
        resp.json().await.expect("invalid vulns json")
    }

    /// Poll the vulns endpoint until the background scan has landed.
    async fn wait_for_scan(&self) -> Value {
        for _ in 0..100 {
            let body = self.vulns().await;
            if body["scanned_at"].is_string() {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the scan never completed");
    }

    async fn rescan(&self) -> Value {
        let resp = self
            .client
            .post(format!(
                "{}/api/v1/vulns/{PKG}/{VERSION}/rescan",
                self.server.base_url
            ))
            .bearer_auth(STATIC_TOKEN)
            .send()
            .await
            .expect("rescan request failed");
        assert_eq!(resp.status(), StatusCode::OK);
        resp.json().await.expect("invalid rescan json")
    }

    async fn count(&self, sql: &str) -> i64 {
        let pool = self.db().await;
        let n: i64 = sqlx::query_scalar(sql)
            .fetch_one(&pool)
            .await
            .expect("count query failed");
        pool.close().await;
        n
    }

    async fn status_at(&self, path: &str) -> StatusCode {
        self.client
            .get(format!("{}{path}", self.server.base_url))
            .send()
            .await
            .expect("request failed")
            .status()
    }
}

fn only_detail(body: &Value) -> &Value {
    let details = body["details"].as_array().expect("details array");
    assert_eq!(details.len(), 1, "one finding expected: {body}");
    &details[0]
}

/// A Go module zip whose go.mod is `go_mod`.
fn go_zip(module: &str, version: &str, go_mod: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file(format!("{module}@{version}/go.mod"), options)
            .unwrap();
        zip.write_all(go_mod.as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf
}

/// These cases reach the live osv.dev; CI runs offline.
fn network_tests_enabled() -> bool {
    if std::env::var("OPENCARGO_NETWORK_TESTS").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("skipped: set OPENCARGO_NETWORK_TESTS=1 to run live osv.dev scan tests");
    false
}

// ---------------------------------------------------------------------------
// Severity classification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mal_id_is_critical() {
    let lab = lab(scan(false, false)).await;
    lab.advisory(json!({ "id": "MAL-2024-1", "summary": "malicious package" }));
    assert_eq!(lab.publish_lodash().await.status(), StatusCode::OK);

    let body = lab.wait_for_scan().await;
    assert_eq!(body["status"], "critical");
    let detail = only_detail(&body);
    assert_eq!(detail["vuln_id"], "MAL-2024-1");
    assert_eq!(detail["severity"], "critical");
    assert!(
        detail["score"].is_null(),
        "a MAL id carries no score: {detail}"
    );
    assert_eq!(detail["summary"], "malicious package");
}

#[tokio::test]
async fn database_specific_label_wins() {
    let lab = lab(scan(false, false)).await;
    lab.advisory(labelled_record("GHSA-label", "HIGH", Some(V3_CRITICAL)));
    assert_eq!(lab.publish_lodash().await.status(), StatusCode::OK);

    let body = lab.wait_for_scan().await;
    assert_eq!(body["status"], "warning");
    let detail = only_detail(&body);
    assert_eq!(detail["severity"], "high");
    assert_eq!(detail["score"], 9.8);
}

#[tokio::test]
async fn cvss_v3_9_8_is_critical() {
    let lab = lab(scan(false, false)).await;
    lab.advisory(cvss_record("GHSA-v3", "CVSS_V3", V3_CRITICAL));
    assert_eq!(lab.publish_lodash().await.status(), StatusCode::OK);

    let body = lab.wait_for_scan().await;
    assert_eq!(body["status"], "critical");
    let detail = only_detail(&body);
    assert_eq!(detail["severity"], "critical");
    assert_eq!(detail["score"], 9.8);
    assert_eq!(detail["dependency"], DEP);
    assert_eq!(detail["version"], DEP_VERSION);
}

#[tokio::test]
async fn cvss_v4_vector_is_scored() {
    let lab = lab(scan(false, false)).await;
    lab.advisory(cvss_record("GHSA-v4", "CVSS_V4", V4_HIGH));
    assert_eq!(lab.publish_lodash().await.status(), StatusCode::OK);

    let body = lab.wait_for_scan().await;
    let detail = only_detail(&body);
    assert_eq!(detail["severity"], "high");
    let score = detail["score"].as_f64().expect("a CVSS 4.0 score");
    assert!(
        (7.0..9.0).contains(&score),
        "high band expected, got {score}"
    );
}

#[tokio::test]
async fn cvss_5_3_is_medium_not_blocking() {
    let lab = lab(scan(true, false)).await;
    lab.advisory(cvss_record("GHSA-medium", "CVSS_V3", V3_MEDIUM));
    assert_eq!(lab.publish_lodash().await.status(), StatusCode::OK);

    let body = lab.wait_for_scan().await;
    assert_eq!(body["status"], "warning");
    let detail = only_detail(&body);
    assert_eq!(detail["severity"], "medium");
    assert_eq!(detail["score"], 5.3);
}

// ---------------------------------------------------------------------------
// Advisory fetching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn advisory_fetched_once_across_two_scans() {
    let lab = lab(scan(false, false)).await;
    lab.advisory(cvss_record("GHSA-once", "CVSS_V3", V3_MEDIUM));
    assert_eq!(lab.publish_lodash().await.status(), StatusCode::OK);
    lab.wait_for_scan().await;
    assert_eq!(lab.osv.hits("GHSA-once"), 1);

    let body = lab.rescan().await;
    assert_eq!(only_detail(&body)["severity"], "medium");
    assert_eq!(
        lab.osv.hits("GHSA-once"),
        1,
        "the second scan must hit the cache"
    );
    assert_eq!(
        lab.count("SELECT COUNT(*) FROM vulnerability_scans").await,
        1
    );
}

#[tokio::test]
async fn never_more_than_max_concurrency_in_flight() {
    let lab = lab(VulnScanConfig {
        max_concurrency: 2,
        ..scan(true, false)
    })
    .await;
    lab.osv.set_record_delay(Duration::from_millis(150));
    let mut deps = serde_json::Map::new();
    for i in 0..6 {
        let (name, id) = (format!("dep-{i}"), format!("GHSA-conc-{i}"));
        lab.osv.affect("npm", &name, "1.0.0", &[&id]);
        lab.osv.record(cvss_record(&id, "CVSS_V3", V3_MEDIUM));
        deps.insert(name, json!("1.0.0"));
    }
    // block_on_critical makes the gate await the whole assessment before answering.
    assert_eq!(
        lab.publish(Value::Object(deps)).await.status(),
        StatusCode::OK
    );

    let fetched: usize = (0..6)
        .map(|i| lab.osv.hits(&format!("GHSA-conc-{i}")))
        .sum();
    assert_eq!(fetched, 6);
    assert!(
        lab.osv.peak_inflight() <= 2,
        "peak in-flight {} exceeds max_concurrency 2",
        lab.osv.peak_inflight()
    );
    let body = lab.wait_for_scan().await;
    assert_eq!(body["vulnerable_deps"], 6);
    assert_eq!(body["details"].as_array().map(Vec::len), Some(6));
}

// ---------------------------------------------------------------------------
// Publish gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn blocked_publish_leaves_nothing_downloadable() {
    let lab = lab(scan(true, false)).await;
    lab.advisory(cvss_record("GHSA-block", "CVSS_V3", V3_CRITICAL));

    let resp = lab.publish_lodash().await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.expect("error json");
    assert!(
        body["error"].as_str().unwrap_or("").contains("critical"),
        "the refusal names the reason: {body}"
    );

    assert_eq!(
        lab.status_at(&format!("/{NPM_REPO}/{PKG}")).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        lab.status_at(&format!("/{NPM_REPO}/{PKG}/-/vuln-pkg-{VERSION}.tgz"))
            .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(lab.count("SELECT COUNT(*) FROM versions").await, 0);
    assert_eq!(lab.count("SELECT COUNT(*) FROM packages").await, 0);
    assert_eq!(
        lab.count("SELECT COUNT(*) FROM vulnerability_scans").await,
        0
    );
}

#[tokio::test]
async fn non_blocking_publish_persists_critical_row() {
    let lab = lab(scan(false, false)).await;
    lab.advisory(cvss_record("GHSA-persist", "CVSS_V3", V3_CRITICAL));
    assert_eq!(lab.publish_lodash().await.status(), StatusCode::OK);

    let body = lab.wait_for_scan().await;
    assert_eq!(body["status"], "critical");
    assert_eq!(body["vulnerable_deps"], 1);
    assert_eq!(
        lab.count("SELECT COUNT(*) FROM vulnerability_scans WHERE status = 'critical'")
            .await,
        1
    );
    assert_eq!(
        lab.status_at(&format!("/{NPM_REPO}/{PKG}")).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn osv_down_fail_open_publishes_with_warning() {
    let lab = lab(scan(true, false)).await;
    lab.osv.set_down(true);

    assert_eq!(lab.publish_lodash().await.status(), StatusCode::OK);
    assert_eq!(
        lab.status_at(&format!("/{NPM_REPO}/{PKG}")).await,
        StatusCode::OK
    );
    let body = lab.vulns().await;
    assert_eq!(
        body["status"], "not_scanned",
        "an unreachable OSV records nothing: {body}"
    );
    assert_eq!(
        lab.count("SELECT COUNT(*) FROM vulnerability_scans").await,
        0
    );
}

#[tokio::test]
async fn osv_down_fail_closed_is_503() {
    let lab = lab(scan(true, true)).await;
    lab.osv.set_down(true);

    let resp = lab.publish_lodash().await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        lab.status_at(&format!("/{NPM_REPO}/{PKG}")).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(lab.count("SELECT COUNT(*) FROM versions").await, 0);
}

// ---------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------

#[tokio::test]
async fn go_require_block_is_scanned() {
    let lab = lab(scan(false, false)).await;
    lab.osv
        .affect("Go", "github.com/vuln/dep", "v1.2.3", &["GHSA-go"]);
    lab.osv.record(cvss_record("GHSA-go", "CVSS_V3", V3_MEDIUM));
    let go_mod = "module vulnmod\n\ngo 1.22\n\nrequire (\n\tgithub.com/vuln/dep v1.2.3\n\tgolang.org/x/clean v0.1.0 // indirect\n)\n";

    let resp = lab
        .client
        .put(format!(
            "{}/{GO_REPO}/vulnmod/@v/v1.0.0",
            lab.server.base_url
        ))
        .bearer_auth(STATIC_TOKEN)
        .header("content-type", "application/zip")
        .body(go_zip("vulnmod", "v1.0.0", go_mod))
        .send()
        .await
        .expect("go publish failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // The module path has no API route of its own, so the scan row is read from the DB.
    let mut row = None;
    for _ in 0..100 {
        if lab.count("SELECT COUNT(*) FROM vulnerability_scans").await == 1 {
            row = Some(lab.scan_row().await);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (status, results) = row.expect("the go scan never landed");
    assert_eq!(status, "warning");
    let results: Value = serde_json::from_str(&results).expect("scan results json");
    assert_eq!(results["total_deps"], 2);
    assert_eq!(only_detail(&results)["vuln_id"], "GHSA-go");
    assert_eq!(only_detail(&results)["dependency"], "github.com/vuln/dep");
}

// ---------------------------------------------------------------------------
// Config and live osv.dev
// ---------------------------------------------------------------------------

/// A config without a `[vuln_scan]` table must still point at OSV and allow
/// concurrent advisory fetches; a derived `Default` gave "" and 0.
#[test]
fn default_config_has_osv_url_and_concurrency() {
    let cfg: Config = toml::from_str("").expect("empty config parses");
    assert_eq!(cfg.vuln_scan.osv_base_url, "https://api.osv.dev");
    assert_eq!(cfg.vuln_scan.max_concurrency, 8);
    assert!(!cfg.vuln_scan.enabled);
    assert!(!cfg.vuln_scan.block_on_critical);
    assert!(!cfg.vuln_scan.fail_closed);
}

/// An opencargo on the default (live) osv.dev; the idle fake keeps the helpers usable.
async fn live_lab() -> Lab {
    spawn_lab(fake_osv::start().await, scan(false, false)).await
}

/// Publishing a package with a dependency triggers a scan against the live
/// osv.dev whose results are available via the API.
#[tokio::test]
async fn test_vuln_scan_on_publish() {
    if !network_tests_enabled() {
        return;
    }
    let lab = live_lab().await;
    assert_eq!(lab.publish_lodash().await.status(), StatusCode::OK);

    let body = lab.wait_for_scan().await;
    assert_eq!(body["package"], PKG);
    assert_eq!(body["version"], VERSION);
    assert!(
        body["total_deps"].as_i64().unwrap_or(-1) >= 0,
        "total_deps should be returned"
    );
}

/// A package with no dependencies gets a clean scan.
#[tokio::test]
async fn test_vuln_scan_clean_package() {
    if !network_tests_enabled() {
        return;
    }
    let lab = live_lab().await;
    assert_eq!(lab.publish(json!({})).await.status(), StatusCode::OK);

    let body = lab.wait_for_scan().await;
    assert_eq!(body["package"], PKG);
    assert_eq!(body["version"], VERSION);
    assert_eq!(body["total_deps"], 0);
    assert_eq!(body["vulnerable_deps"], 0);
    assert_eq!(body["status"], "clean");
}
