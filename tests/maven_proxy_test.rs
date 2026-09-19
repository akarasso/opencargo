mod common;

use std::collections::HashMap;

use reqwest::StatusCode;
use sha1::Digest as _;

use common::fake_osv::{self, cvss_record};
use common::fake_upstream::maven::{self as fake, FakeMaven};
use common::{
    expire_entries, group, hosted, policy_verdicts, proxy, sentinel, spawn_server, verdict_of, wait_for_policy_rows,
    SpawnOpts, TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility, VulnScanConfig};
use opencargo::policy::rules::PolicyConfig;
use opencargo::telemetry::vulns::severity::Severity;

const DIR: &str = "org/example/lib";
const V3_CRITICAL: &str = "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H";

fn sha1_hex(bytes: &[u8]) -> String {
    format!("{:x}", sha1::Sha1::digest(bytes))
}

fn artifact_doc(versions: &[&str]) -> String {
    let list: String = versions.iter().map(|v| format!("<version>{v}</version>")).collect();
    format!(
        "<?xml version=\"1.0\"?><metadata><groupId>org.example</groupId><artifactId>lib</artifactId>\
         <versioning><versions>{list}</versions><lastUpdated>20260101000000</lastUpdated></versioning></metadata>"
    )
}

async fn spawn(central: &FakeMaven) -> TestServer {
    spawn_server(SpawnOpts {
        repositories: vec![
            hosted("releases", RepositoryFormat::Maven, Visibility::Public),
            proxy("central", RepositoryFormat::Maven, &central.base_url),
            group("public", RepositoryFormat::Maven, &["releases", "central"]),
        ],
        ..Default::default()
    })
    .await
}

async fn get(s: &TestServer, repo: &str, path: &str) -> reqwest::Response {
    reqwest::get(format!("{}/maven/{repo}/{path}", s.base_url)).await.unwrap()
}

async fn text(s: &TestServer, repo: &str, path: &str) -> String {
    let resp = get(s, repo, path).await;
    assert_eq!(resp.status(), StatusCode::OK, "GET {repo}/{path}");
    resp.text().await.unwrap()
}

async fn put(s: &TestServer, path: &str, body: &str) {
    let resp = reqwest::Client::new()
        .put(format!("{}/maven/releases/{path}", s.base_url))
        .bearer_auth(STATIC_TOKEN)
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "PUT {path}: {}", resp.status());
}

#[tokio::test]
async fn a_proxied_file_is_verified_cached_and_summed_on_the_served_body() {
    let central = fake::start().await;
    let s = spawn(&central).await;
    let jar = b"central jar".to_vec();
    let path = format!("{DIR}/1.0/lib-1.0.jar");
    central.put_with(&path, jar.clone(), vec![("x-checksum-sha1", sha1_hex(&jar))]);
    central.put(&format!("{path}.sha1"), sha1_hex(&jar));
    central.put(&format!("{path}.md5"), "not what we serve");

    let served = get(&s, "central", &path).await;
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(served.bytes().await.unwrap().as_ref(), jar.as_slice());
    assert_eq!(text(&s, "central", &format!("{path}.sha1")).await, sha1_hex(&jar));
    assert_eq!(
        text(&s, "central", &format!("{path}.md5")).await,
        format!("{:x}", md5::Md5::digest(&jar)),
        "computed on the cached body, never the upstream's sidecar"
    );
    assert_eq!(get(&s, "central", &path).await.status(), StatusCode::OK);
    assert_eq!(central.count(&path), 1, "an immutable file is fetched once");
}

/// The policy pipeline sees a proxied Maven artifact like any other format's:
/// one row per served build under its `groupId:artifactId`, the OSV rule
/// fires on the Maven ecosystem, and the three rules that cannot mean
/// anything here say `not_applicable` instead of never appearing.
#[tokio::test]
async fn a_proxied_artifact_records_a_policy_row_on_which_osv_fires() {
    let osv = fake_osv::start().await;
    osv.affect("Maven", "org.example:lib", "1.0", &["GHSA-mvn"]);
    osv.record(cvss_record("GHSA-mvn", "CVSS_V3", V3_CRITICAL));
    let central = fake::start().await;
    let jar = b"central jar".to_vec();
    let path = format!("{DIR}/1.0/lib-1.0.jar");
    central.put(&path, jar.clone());
    central.put(&format!("{DIR}/1.0/lib-1.0.pom"), "<project/>");
    central.put(&format!("{DIR}/1.1/lib-1.1.jar"), b"sentinel".to_vec());
    let s = spawn_server(SpawnOpts {
        repositories: vec![proxy("central", RepositoryFormat::Maven, &central.base_url)],
        vuln: VulnScanConfig {
            enabled: true,
            osv_base_url: osv.base_url.clone(),
            ..Default::default()
        },
        policy: HashMap::from([(
            "central".to_string(),
            PolicyConfig {
                min_release_age: Some("48h".parse().unwrap()),
                osv_severity: Some(Severity::High),
                install_scripts: true,
                typosquat: true,
                ..Default::default()
            },
        )]),
        ..Default::default()
    })
    .await;

    assert_eq!(get(&s, "central", &format!("{DIR}/1.0/lib-1.0.pom")).await.status(), StatusCode::OK);
    assert_eq!(get(&s, "central", &path).await.status(), StatusCode::OK);
    let rows = wait_for_policy_rows(&s, 1).await;
    let row = &rows[0];
    assert_eq!(
        (row.format.as_str(), row.name.as_str(), row.version.as_deref()),
        ("maven", "org.example:lib", Some("1.0"))
    );
    assert_eq!(row.member_repo, "central");
    assert_eq!(
        row.digest.as_deref(),
        Some(format!("{:x}", sha2::Sha256::digest(&jar)).as_str())
    );
    assert_eq!(row.date_source, "none");
    let verdicts = policy_verdicts(&s).await;
    assert_eq!(
        verdict_of(&verdicts, row.id, "osv_severity"),
        ("would_block", "GHSA-mvn critical >= high")
    );
    assert!(row.would_block);
    for (rule, reason) in [
        ("min_release_age", "maven: upstream carries no per-version publication date"),
        ("install_scripts", "maven: install scripts are an npm and NuGet notion"),
        ("typosquat", "maven: no name list"),
    ] {
        assert_eq!(verdict_of(&verdicts, row.id, rule), ("not_applicable", reason), "{rule}");
    }

    let rows = sentinel(&s, &format!("{}/maven/central/{DIR}/1.1/lib-1.1.jar", s.base_url), 2).await;
    assert_eq!(rows[1].version.as_deref(), Some("1.1"), "the POM served first recorded nothing");
}

#[tokio::test]
async fn a_body_contradicting_its_sidecar_or_header_is_a_502_and_never_cached() {
    let central = fake::start().await;
    let s = spawn(&central).await;
    let sidecar = format!("{DIR}/1.0/lib-1.0.jar");
    central.put(&sidecar, "tampered");
    central.put(&format!("{sidecar}.sha1"), sha1_hex(b"original"));
    assert_eq!(get(&s, "central", &sidecar).await.status(), StatusCode::BAD_GATEWAY);

    let header = format!("{DIR}/1.1/lib-1.1.jar");
    central.put_with(&header, "tampered", vec![("x-checksum-sha1", sha1_hex(b"original"))]);
    assert_eq!(get(&s, "central", &header).await.status(), StatusCode::BAD_GATEWAY);

    central.put(&sidecar, "original");
    let healed = get(&s, "central", &sidecar).await;
    assert_eq!(healed.status(), StatusCode::OK, "nothing of the refused body was kept");
    assert_eq!(healed.text().await.unwrap(), "original");
}

#[tokio::test]
async fn stale_metadata_serves_matching_checksum() {
    let central = fake::start().await;
    let s = spawn(&central).await;
    let path = format!("{DIR}/maven-metadata.xml");
    central.put(&path, artifact_doc(&["1.0"]));
    let fresh = text(&s, "central", &path).await;

    expire_entries(&s).await;
    central.set_down(true);
    let stale = get(&s, "central", &path).await;
    assert_eq!(stale.status(), StatusCode::OK, "Central down: the expired copy is served");
    assert!(stale.headers().contains_key("warning"));
    let body = stale.text().await.unwrap();
    assert_eq!(body, fresh);
    assert_eq!(text(&s, "central", &format!("{path}.sha1")).await, sha1_hex(body.as_bytes()));
}

#[tokio::test]
async fn a_rejected_refresh_is_never_served_stale() {
    let central = fake::start().await;
    let s = spawn(&central).await;
    let path = format!("{DIR}/maven-metadata.xml");
    central.put(&path, artifact_doc(&["1.0"]));
    text(&s, "central", &path).await;
    expire_entries(&s).await;
    central.put_with(&path, artifact_doc(&["1.0", "2.0"]), vec![("x-checksum-sha1", sha1_hex(b"else"))]);
    assert_eq!(get(&s, "central", &path).await.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn group_metadata_merges_members() {
    let central = fake::start().await;
    let s = spawn(&central).await;
    for v in ["1.0", "1.2"] {
        put(&s, &format!("{DIR}/{v}/lib-{v}.pom"), "<project/>").await;
    }
    central.put(&format!("{DIR}/maven-metadata.xml"), artifact_doc(&["1.10", "2.0-SNAPSHOT", "1.2"]));

    let doc = text(&s, "public", &format!("{DIR}/maven-metadata.xml")).await;
    let versions: Vec<&str> = doc
        .split("<version>")
        .skip(1)
        .map(|chunk| chunk.split("</version>").next().unwrap())
        .collect();
    assert_eq!(versions, ["1.0", "1.2", "1.10", "2.0-SNAPSHOT"], "{doc}");
    assert!(doc.contains("<latest>2.0-SNAPSHOT</latest>") && doc.contains("<release>1.10</release>"), "{doc}");

    put(&s, &format!("{DIR}/3.0/lib-3.0.pom"), "<project/>").await;
    let again = text(&s, "public", &format!("{DIR}/maven-metadata.xml")).await;
    assert!(again.contains("<release>3.0</release>"), "a member's new version is in the next render");
}

#[tokio::test]
async fn group_metadata_checksum_matches_rendered_body() {
    let central = fake::start().await;
    let s = spawn(&central).await;
    put(&s, &format!("{DIR}/1.0/lib-1.0.pom"), "<project/>").await;
    central.put(&format!("{DIR}/maven-metadata.xml"), artifact_doc(&["2.0"]));
    central.put(&format!("{DIR}/maven-metadata.xml.sha1"), "0".repeat(40));
    let body = text(&s, "public", &format!("{DIR}/maven-metadata.xml")).await;
    for (ext, want) in [
        ("sha1", sha1_hex(body.as_bytes())),
        ("md5", format!("{:x}", md5::Md5::digest(body.as_bytes()))),
        ("sha256", format!("{:x}", sha2::Sha256::digest(body.as_bytes()))),
    ] {
        assert_eq!(text(&s, "public", &format!("{DIR}/maven-metadata.xml.{ext}")).await, want, "{ext}");
    }
}

#[tokio::test]
async fn a_group_file_and_its_sum_come_from_the_same_member() {
    let central = fake::start().await;
    let s = spawn(&central).await;
    let path = format!("{DIR}/1.0/lib-1.0.pom");
    put(&s, &path, "<project>hosted</project>").await;
    central.put(&path, "<project>central</project>");
    assert_eq!(text(&s, "public", &path).await, "<project>hosted</project>");
    assert_eq!(text(&s, "public", &format!("{path}.sha1")).await, sha1_hex(b"<project>hosted</project>"));

    let only_upstream = format!("{DIR}/2.0/lib-2.0.pom");
    central.put(&only_upstream, "<project>central</project>");
    assert_eq!(text(&s, "public", &format!("{only_upstream}.sha1")).await, sha1_hex(b"<project>central</project>"));
    assert_eq!(get(&s, "public", &format!("{DIR}/9.9/lib-9.9.jar")).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_snapshot_is_fresh_through_the_proxy_once_its_metadata_expires() {
    let central = fake::start().await;
    let s = spawn(&central).await;
    let v = "1.0-SNAPSHOT";
    let snapshot = |build: u32, ts: &str| {
        format!(
            "<metadata><groupId>org.example</groupId><artifactId>lib</artifactId><version>{v}</version>\
             <versioning><snapshot><timestamp>{ts}</timestamp><buildNumber>{build}</buildNumber></snapshot>\
             <lastUpdated>{}</lastUpdated><snapshotVersions><snapshotVersion><extension>jar</extension>\
             <value>1.0-{ts}-{build}</value><updated>{}</updated></snapshotVersion></snapshotVersions>\
             </versioning></metadata>",
            ts.replace('.', ""),
            ts.replace('.', "")
        )
    };
    let meta = format!("{DIR}/{v}/maven-metadata.xml");
    central.put(&meta, snapshot(1, "20260918.120000"));
    assert!(text(&s, "public", &meta).await.contains("<buildNumber>1</buildNumber>"));
    central.put(&meta, snapshot(2, "20260918.130000"));
    assert!(text(&s, "public", &meta).await.contains("<buildNumber>1</buildNumber>"), "cached until it expires");
    expire_entries(&s).await;
    assert!(text(&s, "public", &meta).await.contains("<buildNumber>2</buildNumber>"));
}

fn stored_files(dir: &std::path::Path, name: &str, found: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            stored_files(&path, name, found);
        } else if path.to_string_lossy().contains(name) && !path.to_string_lossy().contains("_scratch") {
            found.push(path);
        }
    }
}

#[tokio::test]
async fn a_hosted_member_storage_fault_is_a_503_not_a_fall_through() {
    if common::storage_is_s3() {
        eprintln!("skipped under S3: the fault is a file permission on disk");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let central = fake::start().await;
    let s = spawn(&central).await;
    let path = format!("{DIR}/1.0/lib-1.0.pom");
    put(&s, &path, "<project>hosted</project>").await;
    central.put(&path, "<project>central</project>");
    let mut files = Vec::new();
    stored_files(&s.tmp.path().join("storage"), "lib-1.0.pom", &mut files);
    assert_eq!(files.len(), 1, "{files:?}");
    std::fs::set_permissions(&files[0], std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&files[0]).is_ok() {
        println!("skipped: permissions do not bind this user");
        return;
    }
    let resp = get(&s, "public", &path).await;
    std::fs::set_permissions(&files[0], std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "never the other member's copy");
    assert_eq!(central.count(&path), 0);
    assert_eq!(text(&s, "public", &path).await, "<project>hosted</project>");
}
