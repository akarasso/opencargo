//! Real `mvn` and `gradle` against spawned servers. Each test skips when
//! its client is absent (`MVN_BIN`, `GRADLE_BIN`), unless
//! `OPENCARGO_E2E_REQUIRE=1`. Maven plugins resolve from the user's
//! `~/.m2/repository` as a read-only tail when there is one, from Maven
//! Central otherwise; the artifacts under test only ever from opencargo.

mod common;

use std::ffi::OsStr;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use reqwest::StatusCode;
use tempfile::TempDir;

use common::upstream_tap::{self, Tap};
use common::{
    add_token, basic_auth_header, client_bin, create_user, expire_entries, group, hosted, proxy,
    run_cmd, spawn_server, SpawnOpts, TestServer, STATIC_TOKEN,
};
use opencargo::config::{RepositoryFormat, Visibility};

const LIMIT: Duration = Duration::from_secs(600);

struct User {
    name: &'static str,
    token: String,
}

async fn publisher(server: &TestServer, name: &'static str) -> User {
    let client = reqwest::Client::new();
    create_user(&client, &server.base_url, STATIC_TOKEN, name, "publisher").await;
    User {
        name,
        token: add_token(&client, &server.base_url, name, "maven").await,
    }
}

fn repo_url(server: &TestServer, repo: &str) -> String {
    format!("{}/maven/{repo}", server.base_url)
}

/// A local repository of its own per build, and the user's as a tail.
struct Mvn {
    bin: String,
    work: TempDir,
}

impl Mvn {
    fn new(bin: String) -> Self {
        Self {
            bin,
            work: TempDir::new().unwrap(),
        }
    }

    fn dir(&self, name: &str) -> PathBuf {
        let dir = self.work.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn settings(&self, users: &[(&str, &User)]) -> PathBuf {
        let servers: String = users
            .iter()
            .map(|(id, u)| {
                format!(
                    "<server><id>{id}</id><username>{}</username><password>{}</password></server>",
                    u.name, u.token
                )
            })
            .collect();
        let names: Vec<&str> = users.iter().map(|(_, u)| u.name).collect();
        let path = self.work.path().join(format!("settings-{}.xml", names.join("-")));
        std::fs::write(&path, format!("<settings><servers>{servers}</servers></settings>")).unwrap();
        path
    }

    async fn run(&self, project: &Path, settings: &Path, m2: &Path, args: &[&str]) -> (bool, String) {
        let mut all: Vec<String> = vec![
            "-B".into(),
            "-e".into(),
            "-s".into(),
            settings.display().to_string(),
            format!("-Dmaven.repo.local={}", m2.display()),
        ];
        if let Some(tail) = user_repository() {
            all.push(format!("-Dmaven.repo.local.tail={}", tail.display()));
        }
        all.extend(args.iter().map(|a| a.to_string()));
        let refs: Vec<&str> = all.iter().map(String::as_str).collect();
        let (ok, stdout, stderr) = run_cmd(&self.bin, &refs, project, &[]).await;
        (ok, format!("{stdout}\n{stderr}"))
    }
}

fn user_repository() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let repo = Path::new(&home).join(".m2/repository");
    repo.is_dir().then_some(repo)
}

/// A library whose jar and POM both carry `marker`, so a build that mixed
/// the jar of one deploy with the POM of another is caught.
fn write_lib(dir: &Path, version: &str, marker: &str, release_url: &str, snapshot_url: &str) {
    let resources = dir.join("src/main/resources");
    std::fs::create_dir_all(&resources).unwrap();
    std::fs::write(resources.join("marker.txt"), marker).unwrap();
    std::fs::write(
        dir.join("pom.xml"),
        format!(
            r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>lib</artifactId>
  <version>{version}</version>
  <description>{marker}</description>
  <properties><project.build.sourceEncoding>UTF-8</project.build.sourceEncoding></properties>
  <distributionManagement>
    <repository><id>oc</id><url>{release_url}</url></repository>
    <snapshotRepository><id>oc</id><url>{snapshot_url}</url></snapshotRepository>
  </distributionManagement>
</project>"#
        ),
    )
    .unwrap();
}

fn write_consumer(dir: &Path, requirement: &str, url: &str) {
    std::fs::write(
        dir.join("pom.xml"),
        format!(
            r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>consumer</artifactId>
  <version>1.0</version>
  <properties><project.build.sourceEncoding>UTF-8</project.build.sourceEncoding></properties>
  <repositories>
    <repository><id>oc</id><url>{url}</url>
      <releases><enabled>true</enabled></releases>
      <snapshots><enabled>true</enabled><updatePolicy>always</updatePolicy></snapshots>
    </repository>
  </repositories>
  <dependencies>
    <dependency><groupId>org.example</groupId><artifactId>lib</artifactId><version>{requirement}</version></dependency>
  </dependencies>
</project>"#
        ),
    )
    .unwrap();
}

/// The marker inside the resolved jar and the description of the resolved
/// POM, as the local repository holds them.
fn resolved(m2: &Path, version: &str) -> (String, String) {
    let dir = m2.join("org/example/lib").join(version);
    let jar = std::fs::File::open(dir.join(format!("lib-{version}.jar"))).expect("the jar was resolved");
    let mut archive = zip::ZipArchive::new(jar).unwrap();
    let mut marker = String::new();
    archive.by_name("marker.txt").unwrap().read_to_string(&mut marker).unwrap();
    let pom = std::fs::read_to_string(dir.join(format!("lib-{version}.pom"))).expect("the POM was resolved");
    let description = pom
        .split("<description>")
        .nth(1)
        .and_then(|rest| rest.split("</description>").next())
        .unwrap_or_default()
        .to_string();
    (marker, description)
}

async fn deploy(mvn: &Mvn, project: &Path, user: &User, marker: &str, version: &str, url: &str) -> (bool, String) {
    write_lib(project, version, marker, url, url);
    let settings = mvn.settings(&[("oc", user)]);
    mvn.run(project, &settings, &mvn.dir(&format!("m2-deploy-{}", user.name)), &["deploy"]).await
}

async fn consume(mvn: &Mvn, name: &str, requirement: &str, url: &str, anyone: &User) -> (bool, String, PathBuf) {
    let project = mvn.dir(name);
    write_consumer(&project, requirement, url);
    let m2 = mvn.dir(&format!("m2-{name}"));
    let settings = mvn.settings(&[("oc", anyone)]);
    let (ok, log) = mvn.run(&project, &settings, &m2, &["-U", "compile"]).await;
    (ok, log, m2)
}

async fn within<F: std::future::Future<Output = ()>>(test: F) {
    assert!(tokio::time::timeout(LIMIT, test).await.is_ok(), "the test outran {LIMIT:?}");
}

/// B hosts the snapshots, A proxies B behind a tap and groups the proxy
/// after a hosted repository of its own.
async fn chain() -> (TestServer, TestServer, Tap) {
    let b = spawn_server(SpawnOpts {
        repositories: vec![hosted("upstream", RepositoryFormat::Maven, Visibility::Public)],
        ..Default::default()
    })
    .await;
    let tap = upstream_tap::start(&b.base_url).await;
    let a = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("local", RepositoryFormat::Maven, Visibility::Public),
            proxy("central", RepositoryFormat::Maven, &format!("{}/maven/upstream", tap.base_url)),
            group("all", RepositoryFormat::Maven, &["local", "central"]),
        ],
        ..Default::default()
    })
    .await;
    (a, b, tap)
}

#[tokio::test]
async fn mvn_snapshot_is_fresh_through_proxy_and_group_and_never_mixed() {
    let Some(bin) = client_bin("MVN_BIN") else { return };
    within(async {
        let (a, b, _tap) = chain().await;
        let mvn = Mvn::new(bin);
        let alice = publisher(&b, "alice").await;
        let reader = publisher(&a, "reader").await;
        let lib = mvn.dir("lib");
        let (ok, log) = deploy(&mvn, &lib, &alice, "one", "1.0-SNAPSHOT", &repo_url(&b, "upstream")).await;
        assert!(ok, "first deploy:\n{log}");
        let (ok, log, m2) = consume(&mvn, "first", "1.0-SNAPSHOT", &repo_url(&a, "all"), &reader).await;
        assert!(ok, "first resolution:\n{log}");
        assert_eq!(resolved(&m2, "1.0-SNAPSHOT"), ("one".to_string(), "one".to_string()));

        let (ok, log) = deploy(&mvn, &lib, &alice, "two", "1.0-SNAPSHOT", &repo_url(&b, "upstream")).await;
        assert!(ok, "second deploy:\n{log}");
        expire_entries(&a).await;
        let (ok, log, m2) = consume(&mvn, "second", "1.0-SNAPSHOT", &repo_url(&a, "all"), &reader).await;
        assert!(ok, "second resolution:\n{log}");
        assert_eq!(resolved(&m2, "1.0-SNAPSHOT"), ("two".to_string(), "two".to_string()), "jar and POM of one build");
    })
    .await;
}

#[tokio::test]
async fn mvn_resolves_a_range_across_two_members_of_a_group() {
    let Some(bin) = client_bin("MVN_BIN") else { return };
    within(async {
        let (a, b, _tap) = chain().await;
        let mvn = Mvn::new(bin);
        let on_b = publisher(&b, "alice").await;
        let on_a = publisher(&a, "alice").await;
        let (ok, log) = deploy(&mvn, &mvn.dir("lib12"), &on_b, "1.2", "1.2", &repo_url(&b, "upstream")).await;
        assert!(ok, "deploy 1.2 upstream:\n{log}");
        let (ok, log) = deploy(&mvn, &mvn.dir("lib11"), &on_a, "1.1", "1.1", &repo_url(&a, "local")).await;
        assert!(ok, "deploy 1.1 locally:\n{log}");
        let (ok, log, m2) = consume(&mvn, "range", "[1.0,2.0)", &repo_url(&a, "all"), &on_a).await;
        assert!(ok, "range resolution:\n{log}");
        assert_eq!(resolved(&m2, "1.2").0, "1.2", "the newest version of either member");
    })
    .await;
}

#[tokio::test]
async fn concurrent_mvn_deploys_leave_one_winner_and_coherent_metadata() {
    let Some(bin) = client_bin("MVN_BIN") else { return };
    within(async {
        let a = spawn_server(SpawnOpts {
            repositories: vec![hosted("local", RepositoryFormat::Maven, Visibility::Public)],
            ..Default::default()
        })
        .await;
        let mvn = Mvn::new(bin);
        let alice = publisher(&a, "alice").await;
        let bob = publisher(&a, "bob").await;
        let url = repo_url(&a, "local");
        let (lib_a, lib_b) = (mvn.dir("lib-alice"), mvn.dir("lib-bob"));
        let ((ok_a, log_a), (ok_b, log_b)) = tokio::join!(
            deploy(&mvn, &lib_a, &alice, "alice", "1.0", &url),
            deploy(&mvn, &lib_b, &bob, "bob", "1.0", &url),
        );
        assert!(ok_a ^ ok_b, "exactly one winner:\n--- alice\n{log_a}\n--- bob\n{log_b}");
        let winner = if ok_a { "alice" } else { "bob" };
        let loser_log = if ok_a { &log_b } else { &log_a };
        assert!(loser_log.contains("409"), "the loser was refused with a conflict:\n{loser_log}");

        let metadata = reqwest::get(format!("{url}/org/example/lib/maven-metadata.xml"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(metadata.matches("<version>1.0</version>").count(), 1, "{metadata}");
        let (ok, log, m2) = consume(&mvn, "after-race", "1.0", &url, &alice).await;
        assert!(ok, "resolution after the race:\n{log}");
        assert_eq!(resolved(&m2, "1.0"), (winner.to_string(), winner.to_string()));
    })
    .await;
}

#[tokio::test]
async fn mvn_deploy_with_a_wrong_checksum_is_refused_and_nothing_is_visible() {
    let Some(bin) = client_bin("MVN_BIN") else { return };
    within(async {
        let a = spawn_server(SpawnOpts {
            repositories: vec![hosted("local", RepositoryFormat::Maven, Visibility::Public)],
            ..Default::default()
        })
        .await;
        let mvn = Mvn::new(bin);
        let alice = publisher(&a, "alice").await;
        let url = repo_url(&a, "local");
        let declared = reqwest::Client::new()
            .put(format!("{url}/org/example/lib/1.0/lib-1.0.jar.sha1"))
            .header("authorization", basic_auth_header(alice.name, &alice.token))
            .body("0".repeat(40))
            .send()
            .await
            .unwrap();
        assert_eq!(declared.status(), StatusCode::CREATED);
        let (ok, log) = deploy(&mvn, &mvn.dir("lib"), &alice, "one", "1.0", &url).await;
        assert!(!ok, "the deploy must fail:\n{log}");
        assert!(log.contains("400"), "{log}");
        for path in ["1.0/lib-1.0.jar", "1.0/lib-1.0.pom", "maven-metadata.xml"] {
            let status = reqwest::get(format!("{url}/org/example/lib/{path}")).await.unwrap().status();
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
    })
    .await;
}

#[tokio::test]
async fn mvn_resolves_from_the_proxy_cache_while_central_is_down() {
    let Some(bin) = client_bin("MVN_BIN") else { return };
    within(async {
        let (a, b, tap) = chain().await;
        let mvn = Mvn::new(bin);
        let alice = publisher(&b, "alice").await;
        let reader = publisher(&a, "reader").await;
        let (ok, log) = deploy(&mvn, &mvn.dir("lib"), &alice, "cached", "1.0", &repo_url(&b, "upstream")).await;
        assert!(ok, "deploy upstream:\n{log}");
        let (ok, log, _) = consume(&mvn, "warm", "1.0", &repo_url(&a, "central"), &reader).await;
        assert!(ok, "warming the cache:\n{log}");

        tap.fail.store(true, Ordering::SeqCst);
        let (ok, log, m2) = consume(&mvn, "cold", "1.0", &repo_url(&a, "central"), &reader).await;
        assert!(ok, "resolution with the upstream down:\n{log}");
        assert_eq!(resolved(&m2, "1.0").0, "cached");
    })
    .await;
}

#[tokio::test]
async fn mvn_deploy_succeeds_again_once_storage_is_back() {
    if common::storage_is_s3() {
        eprintln!("skipped under S3: the fault is a directory permission on disk");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let Some(bin) = client_bin("MVN_BIN") else { return };
    within(async {
        let a = spawn_server(SpawnOpts {
            repositories: vec![hosted("local", RepositoryFormat::Maven, Visibility::Public)],
            ..Default::default()
        })
        .await;
        let mvn = Mvn::new(bin);
        let alice = publisher(&a, "alice").await;
        let url = repo_url(&a, "local");
        let scratch = a.tmp.path().join("storage/_scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::write(scratch.join("probe"), b"x").is_ok() {
            println!("skipped: permissions do not bind this user");
            return;
        }
        let lib = mvn.dir("lib");
        let (ok, log) = deploy(&mvn, &lib, &alice, "late", "1.0", &url).await;
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!ok, "storage down: the deploy fails\n{log}");
        assert!(log.contains("503"), "{log}");
        let (ok, log) = deploy(&mvn, &lib, &alice, "late", "1.0", &url).await;
        assert!(ok, "the retry once storage is back:\n{log}");
        let (ok, log, m2) = consume(&mvn, "after", "1.0", &url, &alice).await;
        assert!(ok, "{log}");
        assert_eq!(resolved(&m2, "1.0"), ("late".to_string(), "late".to_string()));
    })
    .await;
}

/// `GRADLE_BIN` or `gradle` on the path, of a release that runs on this
/// JDK: an old one answers `--version` and then fails every build.
fn gradle_bin() -> Option<String> {
    let bin = std::env::var("GRADLE_BIN").unwrap_or_else(|_| "gradle".to_string());
    let major = std::process::Command::new(&bin)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let out = String::from_utf8_lossy(&o.stdout).to_string();
            let line = out.lines().find(|l| l.starts_with("Gradle "))?.to_string();
            line["Gradle ".len()..].split('.').next()?.trim().parse::<u32>().ok()
        });
    if major.is_some_and(|m| m >= 8) {
        return Some(bin);
    }
    assert!(
        std::env::var("OPENCARGO_E2E_REQUIRE").as_deref() != Ok("1"),
        "a Gradle 8 or later is required by OPENCARGO_E2E_REQUIRE=1 (set GRADLE_BIN)"
    );
    println!("skipped: no usable gradle (set GRADLE_BIN to a Gradle 8 or later)");
    None
}

async fn gradle(bin: &str, project: &Path, home: &Path, args: &[&str]) -> (bool, String) {
    let mut all = vec!["--no-daemon", "--console=plain", "--stacktrace"];
    all.extend_from_slice(args);
    let env: [(&str, &OsStr); 1] = [("GRADLE_USER_HOME", home.as_os_str())];
    let (ok, stdout, stderr) = run_cmd(bin, &all, project, &env).await;
    (ok, format!("{stdout}\n{stderr}"))
}

fn write_gradle_lib(dir: &Path, version: &str, url: &str, user: &User) {
    std::fs::create_dir_all(dir.join("src/main/resources")).unwrap();
    std::fs::write(dir.join("src/main/resources/marker.txt"), version).unwrap();
    std::fs::write(dir.join("settings.gradle"), "rootProject.name = 'lib'\n").unwrap();
    std::fs::write(
        dir.join("build.gradle"),
        format!(
            r#"plugins {{ id 'java-library'; id 'maven-publish' }}
group = 'org.example'
version = '{version}'
publishing {{
  publications {{ lib(MavenPublication) {{ from components.java }} }}
  repositories {{
    maven {{
      url = '{url}'
      allowInsecureProtocol = true
      credentials {{ username = '{}'; password = '{}' }}
    }}
  }}
}}
"#,
            user.name, user.token
        ),
    )
    .unwrap();
}

#[tokio::test]
async fn gradle_publishes_and_resolves_a_dynamic_version_through_the_group() {
    let Some(bin) = gradle_bin() else { return };
    within(async {
        let (a, b, _tap) = chain().await;
        let work = TempDir::new().unwrap();
        let home = work.path().join("gradle-home");
        let on_b = publisher(&b, "alice").await;
        let on_a = publisher(&a, "alice").await;
        for (version, url, user) in [("1.2", repo_url(&b, "upstream"), &on_b), ("1.1", repo_url(&a, "local"), &on_a)] {
            let dir = work.path().join(format!("lib-{version}"));
            write_gradle_lib(&dir, version, &url, user);
            let (ok, log) = gradle(&bin, &dir, &home, &["publish"]).await;
            assert!(ok, "gradle publish {version}:\n{log}");
        }
        for path in ["1.1/lib-1.1.module", "1.1/lib-1.1.jar.sha512"] {
            let status = reqwest::get(format!("{}/org/example/lib/{path}", repo_url(&a, "local"))).await.unwrap().status();
            assert_eq!(status, StatusCode::OK, "{path}");
        }

        let consumer = work.path().join("consumer");
        std::fs::create_dir_all(&consumer).unwrap();
        std::fs::write(consumer.join("settings.gradle"), "rootProject.name = 'consumer'\n").unwrap();
        std::fs::write(
            consumer.join("build.gradle"),
            format!(
                r#"plugins {{ id 'java' }}
repositories {{ maven {{ url = '{}'; allowInsecureProtocol = true }} }}
dependencies {{ implementation 'org.example:lib:1.+' }}
"#,
                repo_url(&a, "all")
            ),
        )
        .unwrap();
        let (ok, log) = gradle(&bin, &consumer, &home, &["dependencies", "--configuration", "runtimeClasspath"]).await;
        assert!(ok, "gradle resolution:\n{log}");
        assert!(log.contains("org.example:lib:1.+ -> 1.2"), "the newest across both members:\n{log}");
    })
    .await;
}
