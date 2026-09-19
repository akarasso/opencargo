//! One upstream per format, and one way to bring an artifact in from it.
//!
//! The policy report only ever sees what an instance pulls from outside, so a
//! column about it cannot be checked against a hosted publish. Exhaustive on
//! `Format`, like the publisher: a tenth format says how it reaches outside,
//! or it does not compile.

use reqwest::{Client, Response};
use serde_json::json;

use opencargo::config::{RepositoryConfig, RepositoryFormat};
use opencargo::domain::Format;

use super::fake_upstream::{cargo as fake_cargo, go as fake_go, maven as fake_maven, mcp as fake_mcp, npm as fake_npm, nuget as fake_nuget, oci as fake_oci, pypi as fake_pypi};
use super::{proxy_with, ProxyOpts, STATIC_TOKEN};

pub const PACKAGE: &str = "widget";
pub const VERSION: &str = "1.0.0";

pub struct Upstreams {
    pub npm: fake_npm::FakeNpm,
    pub cargo: fake_cargo::FakeIndex,
    pub go: fake_go::FakeGoProxy,
    pub pypi: fake_pypi::FakePypi,
    pub maven: fake_maven::FakeMaven,
    pub nuget: fake_nuget::FakeNuget,
    pub oci: fake_oci::FakeRegistry,
    pub mcp: fake_mcp::FakeRegistry,
    pub raw: fake_maven::FakeMaven,
}

pub fn repo_of(format: Format) -> String {
    format!("{}-up", format.as_str())
}

pub async fn start() -> Upstreams {
    let packument = json!({
        "name": PACKAGE,
        "dist-tags": {"latest": VERSION},
        "versions": {VERSION: {"name": PACKAGE, "version": VERSION, "dist": {"tarball": "http://replaced/widget-1.0.0.tgz"}}},
    });
    let up = Upstreams {
        npm: fake_npm::start(packument, "\"etag\"").await,
        cargo: fake_cargo::start().await,
        go: fake_go::start().await,
        pypi: fake_pypi::start().await,
        maven: fake_maven::start().await,
        nuget: fake_nuget::start().await,
        oci: fake_oci::start(fake_oci::Options::default()).await,
        mcp: fake_mcp::start().await,
        raw: fake_maven::start().await,
    };
    up.cargo.add_crate(PACKAGE, VERSION, b"crate bytes");
    up.cargo.set_dl("");
    up.npm.add_tarball(&format!("{PACKAGE}-{VERSION}.tgz"), b"tarball bytes");
    up.pypi.set(
        &format!("/files/{PACKAGE}-{VERSION}-py3-none-any.whl"),
        fake_pypi::Answer::Body("application/octet-stream", b"wheel bytes".to_vec()),
    );
    up.pypi.set(
        &format!("/simple/{PACKAGE}/"),
        fake_pypi::Answer::Body(
            "text/html",
            format!(
                "<html><body><a href=\"../../files/{PACKAGE}-{VERSION}-py3-none-any.whl\">{PACKAGE}-{VERSION}-py3-none-any.whl</a></body></html>"
            )
            .into_bytes(),
        ),
    );
    up.nuget.add(PACKAGE, VERSION, super::nuget::nupkg(PACKAGE, VERSION, &[]));
    up.maven
        .put(&format!("org/example/{PACKAGE}/{VERSION}/{PACKAGE}-{VERSION}.jar"), b"jar bytes".to_vec());
    up.maven
        .put(&format!("org/example/{PACKAGE}/{VERSION}/{PACKAGE}-{VERSION}.pom"), "<project/>");
    up.raw.put("dist/file.bin", b"upstream bytes".to_vec());
    let config = fake_oci::Blob::Bytes(b"{}".to_vec());
    let config_digest = up.oci.add_blob(PACKAGE, config);
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest, "size": 2},
        "layers": [],
    }))
    .unwrap();
    up.oci.add_manifest(
        PACKAGE,
        Some(VERSION),
        &manifest,
        "application/vnd.oci.image.manifest.v1+json",
    );
    up.mcp.put(
        fake_mcp::server(&format!("io.github.acme/{PACKAGE}"), VERSION),
        "active",
        chrono::Utc::now(),
    );
    up
}

impl Upstreams {
    /// One repository per format, each pointed at its own upstream.
    pub fn repositories(&self) -> Vec<RepositoryConfig> {
        Format::ALL
            .into_iter()
            .map(|format| {
                let name = repo_of(format);
                let fmt = RepositoryFormat::from(format);
                match format {
                    Format::Mcp => super::proxy(&name, fmt, &self.url(format)),
                    // Every fake binds a loopback address, which an upstream
                    // that follows a document's links refuses by default.
                    _ => proxy_with(
                        &name,
                        fmt,
                        &self.url(format),
                        ProxyOpts {
                            dl_allow_private: true,
                            file_hosts: vec!["127.0.0.1".to_string()],
                            ..Default::default()
                        },
                    ),
                }
            })
            .collect()
    }

    /// A policy entry per repository: the engine records only where one is
    /// configured, so a report of nine formats needs nine of them.
    pub fn policy(&self) -> std::collections::HashMap<String, opencargo::policy::rules::PolicyConfig> {
        Format::ALL
            .into_iter()
            .map(|format| {
                (
                    repo_of(format),
                    // The engine records where a rule is enabled, and one that
                    // cannot apply to a format answers not_applicable rather
                    // than refusing the configuration.
                    opencargo::policy::rules::PolicyConfig {
                        typosquat: true,
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    fn url(&self, format: Format) -> String {
        match format {
            Format::Npm => self.npm.base_url.clone(),
            Format::Cargo => self.cargo.index_url(),
            Format::Go => self.go.base_url.clone(),
            Format::Pypi => format!("{}/simple", self.pypi.base_url),
            Format::Maven => self.maven.base_url.clone(),
            Format::Nuget => self.nuget.service_index(),
            Format::Oci => self.oci.base_url.clone(),
            Format::Mcp => self.mcp.base_url.clone(),
            Format::Raw => self.raw.base_url.clone(),
        }
    }

    /// The one request that makes this format reach its upstream. For eight
    /// formats that is a read of the artifact -- the document that names it is
    /// not what a policy judges. An mcp mirror has no read path to the
    /// outside: it is a sync, and the record is written there.
    pub async fn bring_in(&self, client: &Client, base_url: &str, format: Format) -> Response {
        let repo = repo_of(format);
        if matches!(format, Format::Mcp) {
            let synced = client
                .post(format!("{base_url}/api/v1/mcp/{repo}/sync"))
                .bearer_auth(STATIC_TOKEN)
                .send()
                .await
                .unwrap();
            if !synced.status().is_success() {
                return synced;
            }
            return client
                .get(format!(
                    "{base_url}/{repo}/v0.1/servers/io.github.acme%2F{PACKAGE}/versions/{VERSION}"
                ))
                .bearer_auth(STATIC_TOKEN)
                .send()
                .await
                .unwrap();
        }
        let url = match format {
            Format::Npm => {
                format!("{base_url}/{repo}/{PACKAGE}/-/{PACKAGE}-{VERSION}.tgz")
            }
            Format::Cargo => {
                format!("{base_url}/{repo}/api/v1/crates/{PACKAGE}/{VERSION}/download")
            }
            Format::Go => format!("{base_url}/{repo}/{}/@v/v1.0.0.zip", fake_go::MODULE),
            Format::Pypi => format!(
                "{base_url}/{repo}/files/{PACKAGE}/{PACKAGE}-{VERSION}-py3-none-any.whl"
            ),
            Format::Maven => format!(
                "{base_url}/maven/{repo}/org/example/{PACKAGE}/{VERSION}/{PACKAGE}-{VERSION}.jar"
            ),
            Format::Nuget => format!(
                "{base_url}/{repo}/v3/flatcontainer/{PACKAGE}/{VERSION}/{PACKAGE}.{VERSION}.nupkg"
            ),
            Format::Oci => format!("{base_url}/v2/{repo}/{PACKAGE}/manifests/{VERSION}"),
            Format::Raw => format!("{base_url}/raw/{repo}/dist/file.bin"),
            Format::Mcp => unreachable!("answered above: a mirror syncs, it does not read"),
        };
        let request = client.get(url).bearer_auth(STATIC_TOKEN);
        let request = match format {
            Format::Oci => request.header("accept", "application/vnd.oci.image.manifest.v1+json"),
            _ => request,
        };
        request.send().await.unwrap()
    }
}
