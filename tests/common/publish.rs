//! One publish per format, exhaustive on `Format`.
//!
//! No step asserts: a publish made of several requests returns the first that
//! is not a success, so a metered test reads the refusal instead of a panic.

use reqwest::{Client, Response, StatusCode};
use serde_json::json;
use sha1::Digest as _;

use opencargo::config::{RepositoryConfig, RepositoryFormat, Visibility};
use opencargo::domain::Format;

use super::{
    build_cargo_publish_body, build_go_module_zip, build_npm_publish_body, build_tarball, hosted,
    mcp, nuget, pypi, sha256_digest, STATIC_TOKEN,
};

pub fn repo_of(format: Format) -> String {
    format!("{}-repo", format.as_str())
}

pub fn repositories() -> Vec<RepositoryConfig> {
    Format::ALL
        .into_iter()
        .map(|format| {
            hosted(
                &repo_of(format),
                RepositoryFormat::from(format),
                Visibility::Public,
            )
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct Subject {
    pub name: String,
    pub version: String,
}

pub async fn publish(client: &Client, base_url: &str, format: Format, n: usize) -> Response {
    publish_named(client, base_url, format, n).await.1
}

pub async fn publish_named(
    client: &Client,
    base_url: &str,
    format: Format,
    n: usize,
) -> (Subject, Response) {
    let repo = repo_of(format);
    let subject = subject(format, n);
    let response = match format {
        Format::Npm => {
            let tarball = build_tarball(&format!(
                r#"{{"name":"{}","version":"{}"}}"#,
                subject.name, subject.version
            ));
            client
                .put(format!("{base_url}/{repo}/{}", subject.name))
                .bearer_auth(STATIC_TOKEN)
                .json(&build_npm_publish_body(
                    &subject.name,
                    &subject.version,
                    "matrix",
                    &tarball,
                ))
                .send()
                .await
                .unwrap()
        }
        Format::Cargo => {
            let meta = json!({"name": subject.name, "vers": subject.version}).to_string();
            client
                .put(format!("{base_url}/{repo}/api/v1/crates/new"))
                .bearer_auth(STATIC_TOKEN)
                .body(build_cargo_publish_body(&meta, b"crate bytes"))
                .send()
                .await
                .unwrap()
        }
        Format::Go => client
            .put(format!(
                "{base_url}/{repo}/{}/@v/{}",
                subject.name, subject.version
            ))
            .bearer_auth(STATIC_TOKEN)
            .header("content-type", "application/zip")
            .body(build_go_module_zip(&subject.name, &subject.version))
            .send()
            .await
            .unwrap(),
        Format::Pypi => {
            pypi::upload(
                client,
                base_url,
                &repo,
                &pypi::basic("__token__", STATIC_TOKEN),
                &pypi::wheel_name(&subject.name, &subject.version),
                &pypi::wheel(&subject.name, &subject.version, None),
            )
            .await
        }
        Format::Nuget => client
            .put(format!("{base_url}/{repo}/v3/package"))
            .header("X-NuGet-ApiKey", STATIC_TOKEN)
            .multipart(nuget::form(nuget::nupkg(&subject.name, &subject.version, &[])))
            .send()
            .await
            .unwrap(),
        Format::Maven => {
            let dir = format!("org/example/{}", subject.name);
            let jar = b"jar bytes".to_vec();
            let put = |path: String, body: Vec<u8>| {
                client
                    .put(format!("{base_url}/maven/{repo}/{path}"))
                    .bearer_auth(STATIC_TOKEN)
                    .body(body)
                    .send()
            };
            let jar_path = format!("{dir}/{}/{}-{}.jar", subject.version, subject.name, subject.version);
            let deployed = put(jar_path.clone(), jar.clone()).await.unwrap();
            if let Some(refused) = refusal(deployed) {
                return (subject, refused);
            }
            let sha = format!("{:x}", sha1::Sha1::digest(&jar));
            let checksum = put(format!("{jar_path}.sha1"), sha.into_bytes()).await.unwrap();
            if let Some(refused) = refusal(checksum) {
                return (subject, refused);
            }
            let pom = format!(
                "<project><groupId>org.example</groupId><artifactId>{}</artifactId>\
                 <version>{}</version></project>",
                subject.name, subject.version
            );
            put(
                format!("{dir}/{}/{}-{}.pom", subject.version, subject.name, subject.version),
                pom.into_bytes(),
            )
            .await
            .unwrap()
        }
        Format::Oci => {
            let image = format!("{repo}/{}", subject.name);
            let config = b"{\"architecture\":\"amd64\",\"os\":\"linux\"}".to_vec();
            let layer = b"layer bytes".to_vec();
            let mut digests = Vec::new();
            for blob in [&config, &layer] {
                match blob_of(client, base_url, &image, blob).await {
                    Ok(digest) => digests.push(digest),
                    Err(refused) => return (subject, refused),
                }
            }
            let manifest = serde_json::to_vec(&json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {
                    "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": digests[0],
                    "size": config.len(),
                },
                "layers": [{
                    "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": digests[1],
                    "size": layer.len(),
                }],
            }))
            .unwrap();
            client
                .put(format!(
                    "{base_url}/v2/{image}/manifests/{}",
                    subject.version
                ))
                .bearer_auth(STATIC_TOKEN)
                .header("content-type", "application/vnd.oci.image.manifest.v1+json")
                .body(manifest)
                .send()
                .await
                .unwrap()
        }
        Format::Mcp => client
            .post(format!("{base_url}/{repo}/v0.1/publish"))
            .bearer_auth(STATIC_TOKEN)
            .json(&mcp::record(&subject.name, &subject.version))
            .send()
            .await
            .unwrap(),
        Format::Raw => client
            .put(format!("{base_url}/raw/{repo}/{}", subject.name))
            .bearer_auth(STATIC_TOKEN)
            .body("raw bytes")
            .send()
            .await
            .unwrap(),
    };
    (subject, response)
}

pub fn subject(format: Format, n: usize) -> Subject {
    let version = match format {
        Format::Go => "v1.0.0".to_string(),
        Format::Oci => format!("v{n}"),
        _ => "1.0.0".to_string(),
    };
    let name = match format {
        Format::Npm => format!("pkg-{n}"),
        Format::Cargo => format!("krate-{n}"),
        Format::Go => format!("example.com/mod{n}"),
        Format::Pypi => format!("widget{n}"),
        Format::Nuget => format!("Widget{n}"),
        Format::Maven => format!("lib{n}"),
        Format::Oci => "app".to_string(),
        Format::Mcp => format!("io.github.acme/srv{n}"),
        Format::Raw => format!("dist/file-{n}.bin"),
    };
    Subject { name, version }
}

/// The dependency a `publish_with_dependency` declares, in the format's own
/// grammar. Exhaustive: a format claiming to be scannable has to say how its
/// document names what it depends on.
pub fn dependency(format: Format) -> Option<(&'static str, &'static str)> {
    match format {
        Format::Npm => Some(("lodash", "4.17.20")),
        Format::Cargo => Some(("serde", "1.0.100")),
        Format::Go => Some(("example.com/dep", "v1.2.3")),
        Format::Pypi => Some(("idna", "3.4")),
        Format::Maven => Some(("org.example:dep", "2.0.0")),
        Format::Nuget => Some(("Newtonsoft.Json", "12.0.1")),
        Format::Oci | Format::Mcp | Format::Raw => None,
    }
}

/// The same publish, with one dependency declared, for the formats whose row
/// says a scan can read it.
pub async fn publish_with_dependency(
    client: &Client,
    base_url: &str,
    format: Format,
    n: usize,
) -> (Subject, Response) {
    let Some((dep, dep_version)) = dependency(format) else {
        return publish_named(client, base_url, format, n).await;
    };
    let repo = repo_of(format);
    let s = subject(format, n);
    let response = match format {
        Format::Npm => {
            let manifest = format!(
                r#"{{"name":"{}","version":"{}","dependencies":{{"{dep}":"^{dep_version}"}}}}"#,
                s.name, s.version
            );
            let tarball = build_tarball(&manifest);
            let mut body = build_npm_publish_body(&s.name, &s.version, "matrix", &tarball);
            body["versions"][&s.version]["dependencies"] =
                json!({ dep: format!("^{dep_version}") });
            client
                .put(format!("{base_url}/{repo}/{}", s.name))
                .bearer_auth(STATIC_TOKEN)
                .json(&body)
                .send()
                .await
                .unwrap()
        }
        Format::Cargo => {
            let meta = json!({
                "name": s.name,
                "vers": s.version,
                "deps": [{
                    "name": dep,
                    "version_req": format!("^{dep_version}"),
                    "kind": "normal",
                }],
            })
            .to_string();
            client
                .put(format!("{base_url}/{repo}/api/v1/crates/new"))
                .bearer_auth(STATIC_TOKEN)
                .body(build_cargo_publish_body(&meta, b"crate bytes"))
                .send()
                .await
                .unwrap()
        }
        Format::Go => {
            let zip = go_zip_requiring(&s.name, &s.version, dep, dep_version);
            client
                .put(format!("{base_url}/{repo}/{}/@v/{}", s.name, s.version))
                .bearer_auth(STATIC_TOKEN)
                .header("content-type", "application/zip")
                .body(zip)
                .send()
                .await
                .unwrap()
        }
        Format::Pypi => {
            let requires = format!("{dep}>={dep_version}");
            pypi::upload(
                client,
                base_url,
                &repo,
                &pypi::basic("__token__", STATIC_TOKEN),
                &pypi::wheel_name(&s.name, &s.version),
                &pypi::wheel_with(&s.name, &s.version, None, &[requires.as_str()]),
            )
            .await
        }
        Format::Nuget => {
            let deps = format!(r#"<dependency id="{dep}" version="[{dep_version}, )" />"#);
            client
                .put(format!("{base_url}/{repo}/v3/package"))
                .header("X-NuGet-ApiKey", STATIC_TOKEN)
                .multipart(nuget::form(nuget::nupkg_with(
                    &s.name,
                    &s.version,
                    &deps,
                    &[],
                )))
                .send()
                .await
                .unwrap()
        }
        Format::Maven => {
            let dir = format!("org/example/{}", s.name);
            let jar = b"jar bytes".to_vec();
            let put = |path: String, body: Vec<u8>| {
                client
                    .put(format!("{base_url}/maven/{repo}/{path}"))
                    .bearer_auth(STATIC_TOKEN)
                    .body(body)
                    .send()
            };
            let jar_path = format!("{dir}/{0}/{1}-{0}.jar", s.version, s.name);
            let deployed = put(jar_path.clone(), jar.clone()).await.unwrap();
            if let Some(refused) = refusal(deployed) {
                return (s, refused);
            }
            let sha = format!("{:x}", sha1::Sha1::digest(&jar));
            let checksum = put(format!("{jar_path}.sha1"), sha.into_bytes()).await.unwrap();
            if let Some(refused) = refusal(checksum) {
                return (s, refused);
            }
            let (group, artifact) = dep.split_once(':').unwrap();
            let pom = format!(
                "<project><groupId>org.example</groupId><artifactId>{}</artifactId>\
                 <version>{}</version><dependencies><dependency>\
                 <groupId>{group}</groupId><artifactId>{artifact}</artifactId>\
                 <version>{dep_version}</version></dependency></dependencies></project>",
                s.name, s.version
            );
            put(
                format!("{dir}/{0}/{1}-{0}.pom", s.version, s.name),
                pom.into_bytes(),
            )
            .await
            .unwrap()
        }
        Format::Oci | Format::Mcp | Format::Raw => unreachable!("no dependency to declare"),
    };
    (s, response)
}

fn go_zip_requiring(module: &str, version: &str, dep: &str, dep_version: &str) -> Vec<u8> {
    use std::io::Write as _;
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .last_modified_time(zip::DateTime::default());
        zip.start_file(format!("{module}@{version}/go.mod"), options)
            .unwrap();
        zip.write_all(
            format!("module {module}\n\ngo 1.21\n\nrequire {dep} {dep_version}\n").as_bytes(),
        )
        .unwrap();
        zip.start_file(format!("{module}@{version}/main.go"), options)
            .unwrap();
        zip.write_all(b"package mod\n").unwrap();
        zip.finish().unwrap();
    }
    buf
}

/// The request a client makes to obtain what `publish` created. Exhaustive
/// too, so a format cannot answer a download column without a download.
pub async fn fetch(client: &Client, base_url: &str, format: Format, n: usize) -> Response {
    let repo = repo_of(format);
    let s = subject(format, n);
    let url = match format {
        Format::Npm => format!("{base_url}/{repo}/{0}/-/{0}-{1}.tgz", s.name, s.version),
        Format::Cargo => format!(
            "{base_url}/{repo}/api/v1/crates/{}/{}/download",
            s.name, s.version
        ),
        Format::Go => format!("{base_url}/{repo}/{}/@v/{}.zip", s.name, s.version),
        Format::Pypi => format!(
            "{base_url}/{repo}/files/{}/{}",
            s.name,
            pypi::wheel_name(&s.name, &s.version)
        ),
        Format::Nuget => format!(
            "{base_url}/{repo}/v3/flatcontainer/{0}/{1}/{0}.{1}.nupkg",
            s.name.to_ascii_lowercase(),
            s.version
        ),
        Format::Maven => format!(
            "{base_url}/maven/{repo}/org/example/{0}/{1}/{0}-{1}.jar",
            s.name, s.version
        ),
        Format::Oci => format!("{base_url}/v2/{repo}/{}/manifests/{}", s.name, s.version),
        Format::Mcp => format!(
            "{base_url}/{repo}/v0.1/servers/{}/versions/{}",
            s.name.replace('/', "%2F"),
            s.version
        ),
        Format::Raw => format!("{base_url}/raw/{repo}/{}", s.name),
    };
    let request = client.get(url).bearer_auth(STATIC_TOKEN);
    let request = match format {
        Format::Oci => request.header(
            "accept",
            "application/vnd.oci.image.manifest.v1+json",
        ),
        _ => request,
    };
    request.send().await.unwrap()
}

fn refusal(response: Response) -> Option<Response> {
    (!response.status().is_success()).then_some(response)
}

async fn blob_of(
    client: &Client,
    base_url: &str,
    image: &str,
    blob: &[u8],
) -> Result<String, Response> {
    let start = client
        .post(format!("{base_url}/v2/{image}/blobs/uploads/"))
        .bearer_auth(STATIC_TOKEN)
        .send()
        .await
        .unwrap();
    if start.status() != StatusCode::ACCEPTED {
        return Err(start);
    }
    let location = start.headers()["location"].to_str().unwrap().to_string();
    let digest = sha256_digest(blob);
    let done = client
        .put(format!("{base_url}{location}?digest={digest}"))
        .bearer_auth(STATIC_TOKEN)
        .body(blob.to_vec())
        .send()
        .await
        .unwrap();
    if !done.status().is_success() {
        return Err(done);
    }
    Ok(digest)
}
