//! Real PyPI clients against a server: `twine` uploads, `pip`, `uv` and
//! `poetry` install through a group of a hosted member and a proxy of a
//! second opencargo. Every client is resolved from PATH or `*_BIN`, and an
//! absent one skips its test (a failure under `OPENCARGO_E2E_REQUIRE=1`).

mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{Client, StatusCode};
use sha2::Digest;
use tempfile::TempDir;

use common::pypi::{basic, sdist, sdist_name, upload, wheel_name, wheel_with};
use common::upstream_tap::{self, Tap};
use common::{client_bin, group, hosted, proxy_with, spawn_server, ProxyOpts, SpawnOpts, TestServer, STATIC_TOKEN};
use opencargo::config::{RepositoryFormat, Visibility};

const DEADLINE: Duration = Duration::from_secs(300);
const JSON_ACCEPT: &str = "application/vnd.pypi.simple.v1+json";

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Every tool's configuration, cache and keyring confined to one home.
struct Sandbox {
    tmp: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        for dir in ["home", "work", "cache"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        Self { tmp }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    fn env(&self, extra: &[(&str, String)]) -> Vec<(String, OsString)> {
        let home = self.path("home");
        let cache = self.path("cache");
        let mut env: Vec<(String, OsString)> = vec![
            ("HOME".into(), home.clone().into()),
            ("XDG_CONFIG_HOME".into(), home.join(".config").into()),
            ("XDG_CACHE_HOME".into(), cache.clone().into()),
            ("XDG_DATA_HOME".into(), home.join(".local/share").into()),
            ("PIP_CONFIG_FILE".into(), "/dev/null".into()),
            ("PIP_DISABLE_PIP_VERSION_CHECK".into(), "1".into()),
            ("PIP_NO_INPUT".into(), "1".into()),
            ("PIP_CACHE_DIR".into(), cache.join("pip").into()),
            ("UV_CACHE_DIR".into(), cache.join("uv").into()),
            ("UV_NO_CONFIG".into(), "1".into()),
            ("UV_PYTHON_DOWNLOADS".into(), "never".into()),
            ("POETRY_CACHE_DIR".into(), cache.join("poetry").into()),
            ("POETRY_CONFIG_DIR".into(), home.join(".config/pypoetry").into()),
            ("POETRY_VIRTUALENVS_IN_PROJECT".into(), "true".into()),
            ("POETRY_NO_INTERACTION".into(), "1".into()),
            ("PYTHON_KEYRING_BACKEND".into(), "keyring.backends.null.Keyring".into()),
            ("TWINE_NON_INTERACTIVE".into(), "1".into()),
        ];
        env.extend(extra.iter().map(|(k, v)| (k.to_string(), OsString::from(v))));
        env
    }

    async fn run(&self, program: &str, args: &[&str], cwd: &Path, extra: &[(&str, String)]) -> (bool, String) {
        let out = tokio::process::Command::new(program)
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .envs(self.env(extra))
            .output()
            .await
            .unwrap_or_else(|e| panic!("{program} did not start: {e}"));
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    async fn ok(&self, program: &str, args: &[&str], cwd: &Path, extra: &[(&str, String)]) -> String {
        let (ok, text) = self.run(program, args, cwd, extra).await;
        assert!(ok, "{program} {args:?} failed:\n{text}");
        text
    }

    fn write(&self, rel: &str, body: &[u8]) -> PathBuf {
        let path = self.path(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        path
    }
}

/// A server that reads nothing anonymously: `py-local` (hosted), a proxy of a
/// second opencargo that holds `dep-pkg`, and `pypi-all` over both; the
/// clients reach it through a tap.
struct Stack {
    server: TestServer,
    _upstream: TestServer,
    tap: Tap,
    dep_wheel: Vec<u8>,
}

impl Stack {
    async fn new() -> Self {
        let upstream = spawn_server(SpawnOpts {
            repositories: vec![hosted("py-up", RepositoryFormat::Pypi, Visibility::Public)],
            ..Default::default()
        })
        .await;
        let dep_wheel = wheel_with("dep-pkg", "1.0", None, &[]);
        let resp = upload(
            &Client::new(),
            &upstream.base_url,
            "py-up",
            &basic("__token__", STATIC_TOKEN),
            &wheel_name("dep-pkg", "1.0"),
            &dep_wheel,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let server = spawn_server(SpawnOpts {
            anonymous_read: false,
            repositories: vec![
                hosted("py-local", RepositoryFormat::Pypi, Visibility::Private),
                proxy_with(
                    "pypi-proxy",
                    RepositoryFormat::Pypi,
                    &format!("{}/py-up/simple", upstream.base_url),
                    ProxyOpts {
                        dl_allow_private: true,
                        ..Default::default()
                    },
                ),
                group("pypi-all", RepositoryFormat::Pypi, &["py-local", "pypi-proxy"]),
            ],
            ..Default::default()
        })
        .await;
        let tap = upstream_tap::start(&server.base_url).await;
        Self {
            server,
            _upstream: upstream,
            tap,
            dep_wheel,
        }
    }

    fn authority(&self) -> String {
        self.tap.base_url.trim_start_matches("http://").to_string()
    }

    /// The group's index with `__token__` credentials in the URL, as pip and
    /// uv take them.
    fn index(&self) -> String {
        format!("http://__token__:{STATIC_TOKEN}@{}/pypi-all/simple/", self.authority())
    }

    fn legacy(&self) -> String {
        format!("{}/py-local/legacy/", self.tap.base_url)
    }

    async fn publish(&self, sandbox: &Sandbox, twine: &str, files: &[(String, Vec<u8>)], skip_existing: bool) -> (bool, String) {
        let mut args = vec!["upload".to_string(), "--repository-url".to_string(), self.legacy()];
        if skip_existing {
            args.push("--skip-existing".into());
        }
        for (name, body) in files {
            args.push(sandbox.write(&format!("dist/{name}"), body).display().to_string());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        sandbox
            .run(
                twine,
                &args,
                &sandbox.path("work"),
                &[("TWINE_USERNAME", "__token__".into()), ("TWINE_PASSWORD", STATIC_TOKEN.into())],
            )
            .await
    }

    fn requested(&self, suffix: &str) -> usize {
        self.tap.hits.lock().unwrap().iter().filter(|(_, p)| p.ends_with(suffix)).count()
    }
}

fn demo_files(version: &str, requires_python: Option<&str>) -> Vec<(String, Vec<u8>)> {
    vec![
        (
            wheel_name("demo-pkg", version),
            wheel_with("demo-pkg", version, requires_python, &["dep-pkg>=1.0"]),
        ),
        (sdist_name("demo-pkg", version), sdist("demo-pkg", version)),
    ]
}

fn installed(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".dist-info"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

async fn within<F: std::future::Future<Output = ()>>(f: F) {
    assert!(tokio::time::timeout(DEADLINE, f).await.is_ok(), "timed out after {DEADLINE:?}");
}

#[tokio::test]
async fn twine_upload_then_pip_uv_and_poetry_install_through_a_group() {
    let (Some(twine), Some(pip), Some(uv), Some(poetry)) = (
        client_bin("TWINE_BIN"),
        client_bin("PIP_BIN"),
        client_bin("UV_BIN"),
        client_bin("POETRY_BIN"),
    ) else {
        return;
    };
    within(async {
        let stack = Stack::new().await;
        let sandbox = Sandbox::new();
        let (ok, out) = stack.publish(&sandbox, &twine, &demo_files("1.0", None), false).await;
        assert!(ok, "twine upload failed:\n{out}");
        let work = sandbox.path("work");
        let want = ["demo_pkg-1.0.dist-info", "dep_pkg-1.0.dist-info"];

        let target = sandbox.path("pip-target");
        let target_arg = target.display().to_string();
        sandbox
            .ok(&pip, &["install", "--index-url", &stack.index(), "--target", &target_arg, "demo-pkg"], &work, &[])
            .await;
        assert_eq!(installed(&target), want, "pip, with __token__ in the index URL");

        let venv = sandbox.path("uv-venv");
        let venv_arg = venv.display().to_string();
        sandbox.ok(&uv, &["venv", &venv_arg], &work, &[]).await;
        let python = venv.join("bin/python").display().to_string();
        sandbox
            .ok(&uv, &["pip", "install", "--python", &python, "--index-url", &stack.index(), "demo-pkg"], &work, &[])
            .await;
        let site = std::fs::read_dir(venv.join("lib"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("site-packages");
        let uv_installed = installed(&site);
        assert!(want.iter().all(|w| uv_installed.contains(&w.to_string())), "uv: {uv_installed:?}");

        let project = sandbox.path("poetry-project");
        std::fs::create_dir_all(&project).unwrap();
        let pyproject = format!(
            "[project]\nname = \"consumer\"\nversion = \"0.1.0\"\nrequires-python = \">=3.8\"\ndependencies = [\"demo-pkg==1.0\"]\n\n[tool.poetry]\npackage-mode = false\n\n[[tool.poetry.source]]\nname = \"oc\"\nurl = \"http://{}/pypi-all/simple/\"\npriority = \"primary\"\n",
            stack.authority()
        );
        std::fs::write(project.join("pyproject.toml"), pyproject).unwrap();
        let creds = [
            ("POETRY_HTTP_BASIC_OC_USERNAME", "__token__".to_string()),
            ("POETRY_HTTP_BASIC_OC_PASSWORD", STATIC_TOKEN.to_string()),
        ];
        sandbox.ok(&poetry, &["install"], &project, &creds).await;
        let poetry_site = std::fs::read_dir(project.join(".venv/lib"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("site-packages");
        let poetry_installed = installed(&poetry_site);
        assert!(want.iter().all(|w| poetry_installed.contains(&w.to_string())), "poetry: {poetry_installed:?}");
        let lock = std::fs::read_to_string(project.join("poetry.lock")).unwrap();
        assert!(lock.contains(&sha(&stack.dep_wheel)), "poetry locked the proxied wheel's digest");
    })
    .await;
}

#[tokio::test]
async fn twine_reupload_different_bytes_is_rejected() {
    let Some(twine) = client_bin("TWINE_BIN") else {
        return;
    };
    within(async {
        let stack = Stack::new().await;
        let sandbox = Sandbox::new();
        let files = demo_files("1.0", None);
        let (ok, out) = stack.publish(&sandbox, &twine, &files, false).await;
        assert!(ok, "{out}");
        let (ok, out) = stack.publish(&sandbox, &twine, &files, false).await;
        assert!(ok, "the same bytes again are accepted:\n{out}");
        let (ok, out) = stack.publish(&sandbox, &twine, &files, true).await;
        // twine 6.1 and later refuse --skip-existing for any index but PyPI's.
        let skip_supported = !out.contains("UnsupportedConfiguration");
        if skip_supported {
            assert!(ok, "--skip-existing over the same files is idempotent:\n{out}");
        } else {
            println!("skipped: this twine refuses --skip-existing off PyPI; CI pins one that does not");
        }

        let other = vec![(
            wheel_name("demo-pkg", "1.0"),
            wheel_with("demo-pkg", "1.0", Some(">=3.8"), &["dep-pkg>=1.0"]),
        )];
        let (ok, out) = stack.publish(&sandbox, &twine, &other, false).await;
        assert!(!ok, "other bytes under a published name must fail:\n{out}");
        assert!(out.contains("409") || out.to_lowercase().contains("conflict"), "{out}");
        if skip_supported {
            let (ok, out) = stack.publish(&sandbox, &twine, &other, true).await;
            assert!(ok && out.to_lowercase().contains("skipping"), "twine reads the refusal as a conflict:\n{out}");
        }
    })
    .await;
}

#[tokio::test]
async fn pip_require_hashes_through_group() {
    let (Some(twine), Some(pip)) = (client_bin("TWINE_BIN"), client_bin("PIP_BIN")) else {
        return;
    };
    within(async {
        let stack = Stack::new().await;
        let sandbox = Sandbox::new();
        let files = demo_files("1.0", None);
        let (ok, out) = stack.publish(&sandbox, &twine, &files, false).await;
        assert!(ok, "{out}");
        let work = sandbox.path("work");
        let good = format!(
            "demo-pkg==1.0 --hash=sha256:{}\ndep-pkg==1.0 --hash=sha256:{}\n",
            sha(&files[0].1),
            sha(&stack.dep_wheel)
        );
        let req = sandbox.write("work/requirements.txt", good.as_bytes()).display().to_string();
        let target = sandbox.path("hashed");
        let target_arg = target.display().to_string();
        sandbox
            .ok(
                &pip,
                &["install", "--require-hashes", "--only-binary", ":all:", "--index-url", &stack.index(), "--target", &target_arg, "-r", &req],
                &work,
                &[],
            )
            .await;
        assert_eq!(installed(&target), ["demo_pkg-1.0.dist-info", "dep_pkg-1.0.dist-info"]);

        let bad = format!("demo-pkg==1.0 --hash=sha256:{}\ndep-pkg==1.0 --hash=sha256:{}\n", sha(b"x"), sha(&stack.dep_wheel));
        let req = sandbox.write("work/bad.txt", bad.as_bytes()).display().to_string();
        let refused = sandbox.path("refused").display().to_string();
        let (ok, out) = sandbox
            .run(
                &pip,
                &["install", "--require-hashes", "--only-binary", ":all:", "--index-url", &stack.index(), "--target", &refused, "-r", &req],
                &work,
                &[],
            )
            .await;
        assert!(!ok && out.contains("HASHES"), "a wrong hash is refused:\n{out}");
    })
    .await;
}

#[tokio::test]
async fn requires_python_and_yanked_are_respected() {
    let (Some(twine), Some(pip), Some(uv)) = (client_bin("TWINE_BIN"), client_bin("PIP_BIN"), client_bin("UV_BIN")) else {
        return;
    };
    within(async {
        let stack = Stack::new().await;
        let sandbox = Sandbox::new();
        let mut files = demo_files("1.0", None);
        files.extend(demo_files("1.5", None));
        files.extend(demo_files("2.0", Some(">=4")));
        let (ok, out) = stack.publish(&sandbox, &twine, &files, false).await;
        assert!(ok, "{out}");
        let yank = Client::new()
            .post(format!("{}/py-local/pypi/demo-pkg/1.5/yank", stack.server.base_url))
            .header("Authorization", basic("__token__", STATIC_TOKEN))
            .body(r#"{"reason":"broken build"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(yank.status(), StatusCode::OK);
        let work = sandbox.path("work");

        let target = sandbox.path("pip");
        let target_arg = target.display().to_string();
        sandbox
            .ok(&pip, &["install", "--only-binary", ":all:", "--index-url", &stack.index(), "--target", &target_arg, "demo-pkg"], &work, &[])
            .await;
        assert!(installed(&target).contains(&"demo_pkg-1.0.dist-info".to_string()), "{:?}", installed(&target));

        let venv = sandbox.path("uv-venv");
        let venv_arg = venv.display().to_string();
        sandbox.ok(&uv, &["venv", &venv_arg], &work, &[]).await;
        let python = venv.join("bin/python").display().to_string();
        let out = sandbox
            .ok(&uv, &["pip", "install", "--python", &python, "--index-url", &stack.index(), "demo-pkg"], &work, &[])
            .await;
        assert!(out.contains("demo-pkg==1.0"), "uv skips the yanked 1.5 and the >=4 2.0:\n{out}");
    })
    .await;
}

#[tokio::test]
async fn pip_and_uv_read_json_and_html_pages() {
    let (Some(twine), Some(pip), Some(uv)) = (client_bin("TWINE_BIN"), client_bin("PIP_BIN"), client_bin("UV_BIN")) else {
        return;
    };
    within(async {
        let stack = Stack::new().await;
        let sandbox = Sandbox::new();
        let (ok, out) = stack.publish(&sandbox, &twine, &demo_files("1.0", None), false).await;
        assert!(ok, "{out}");
        let work = sandbox.path("work");
        for (flavor, forced) in [("json", None), ("html", Some("text/html"))] {
            *stack.tap.accept.lock().unwrap() = forced.map(str::to_string);
            let target = sandbox.path(&format!("pip-{flavor}"));
            let target_arg = target.display().to_string();
            sandbox
                .ok(&pip, &["install", "--index-url", &stack.index(), "--target", &target_arg, "demo-pkg"], &work, &[])
                .await;
            assert_eq!(installed(&target).len(), 2, "pip over {flavor}");
            let venv = sandbox.path(&format!("uv-{flavor}"));
            let venv_arg = venv.display().to_string();
            sandbox.ok(&uv, &["venv", &venv_arg], &work, &[]).await;
            let python = venv.join("bin/python").display().to_string();
            sandbox
                .ok(&uv, &["pip", "install", "--no-cache", "--python", &python, "--index-url", &stack.index(), "demo-pkg"], &work, &[])
                .await;
        }
        let asked: Vec<Option<String>> = stack
            .tap
            .accepted
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p.contains("/simple/demo-pkg/"))
            .map(|(_, a)| a.clone())
            .collect();
        assert!(
            asked.iter().all(|a| a.as_deref().is_some_and(|a| a.contains(JSON_ACCEPT))),
            "both clients ask for PEP 691 JSON first: {asked:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn uv_reads_core_metadata_and_falls_back_without_it() {
    let (Some(twine), Some(uv)) = (client_bin("TWINE_BIN"), client_bin("UV_BIN")) else {
        return;
    };
    within(async {
        let stack = Stack::new().await;
        let sandbox = Sandbox::new();
        let (ok, out) = stack.publish(&sandbox, &twine, &demo_files("1.0", None), false).await;
        assert!(ok, "{out}");
        let work = sandbox.path("work");
        let req = sandbox.write("work/in.txt", b"demo-pkg\n").display().to_string();
        let out = sandbox
            .ok(&uv, &["pip", "compile", "--no-cache", "--python-version", "3.12", "--index-url", &stack.index(), &req], &work, &[])
            .await;
        assert!(out.contains("demo-pkg==1.0") && out.contains("dep-pkg==1.0"), "{out}");
        let wheel = wheel_name("demo-pkg", "1.0");
        assert!(stack.requested(&format!("{wheel}.metadata")) >= 1, "uv read the PEP 714 core-metadata");
        assert_eq!(stack.requested(&wheel), 0, "resolution never downloaded the wheel");

        let before = stack.requested(&wheel_name("dep-pkg", "1.0"));
        let dep_metadata = stack.requested(&format!("{}.metadata", wheel_name("dep-pkg", "1.0")));
        assert!(dep_metadata >= 1, "the proxied page announces the upstream's metadata too");
        let bare = fallback_upstream(&stack).await;
        let req = sandbox.write("work/bare.txt", b"bare-pkg\n").display().to_string();
        let index = format!("http://__token__:{STATIC_TOKEN}@{}/bare-proxy/simple/", bare.tap_authority);
        let out = sandbox
            .ok(&uv, &["pip", "compile", "--no-cache", "--python-version", "3.12", "--index-url", &index, &req], &work, &[])
            .await;
        assert!(out.contains("bare-pkg==1.0"), "{out}");
        let bare_wheel = wheel_name("bare-pkg", "1.0");
        let hits = bare.tap.hits.lock().unwrap().clone();
        assert!(
            hits.iter().any(|(_, p)| p.ends_with(&bare_wheel)),
            "without PEP 658 uv reads the wheel itself: {hits:?}"
        );
        assert!(!hits.iter().any(|(_, p)| p.ends_with(".metadata")), "{hits:?}");
        assert_eq!(stack.requested(&wheel_name("dep-pkg", "1.0")), before);
    })
    .await;
}

/// A proxy over a scripted index that announces no metadata: the only way
/// to a wheel's requirements is the wheel.
struct Bare {
    _server: TestServer,
    tap: Tap,
    tap_authority: String,
    _index: common::fake_upstream::pypi::FakePypi,
}

async fn fallback_upstream(_stack: &Stack) -> Bare {
    use common::fake_upstream::pypi::{self as fake, Answer};
    let index = fake::start().await;
    let name = wheel_name("bare-pkg", "1.0");
    let body = wheel_with("bare-pkg", "1.0", None, &[]);
    let page = format!(
        "<html><body><a href=\"/files/{name}#sha256={}\">{name}</a></body></html>",
        sha(&body)
    );
    index.set("/simple/bare-pkg/", Answer::Body("text/html", page.into_bytes()));
    index.set(&format!("/files/{name}"), Answer::Body("application/octet-stream", body));
    let server = spawn_server(SpawnOpts {
        anonymous_read: false,
        repositories: vec![proxy_with(
            "bare-proxy",
            RepositoryFormat::Pypi,
            &format!("{}/simple", index.base_url),
            ProxyOpts {
                dl_allow_private: true,
                ..Default::default()
            },
        )],
        ..Default::default()
    })
    .await;
    let tap = upstream_tap::start(&server.base_url).await;
    let tap_authority = tap.base_url.trim_start_matches("http://").to_string();
    Bare {
        _server: server,
        tap,
        tap_authority,
        _index: index,
    }
}
