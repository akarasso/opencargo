//! Building and uploading PyPI distributions the way twine sends them.

use std::io::Write;

use base64::Engine;
use sha2::Digest;

/// `METADATA` / `PKG-INFO` for `name` at `version`.
pub fn core_metadata(name: &str, version: &str, requires_python: Option<&str>) -> String {
    core_metadata_with(name, version, requires_python, &[])
}

pub fn core_metadata_with(
    name: &str,
    version: &str,
    requires_python: Option<&str>,
    requires_dist: &[&str],
) -> String {
    let mut out = format!("Metadata-Version: 2.1\nName: {name}\nVersion: {version}\nSummary: {name} for tests\n");
    if let Some(rp) = requires_python {
        out.push_str(&format!("Requires-Python: {rp}\n"));
    }
    for dep in requires_dist {
        out.push_str(&format!("Requires-Dist: {dep}\n"));
    }
    out.push('\n');
    out
}

/// A wheel holding one module and its `.dist-info`.
pub fn wheel(name: &str, version: &str, requires_python: Option<&str>) -> Vec<u8> {
    wheel_with(name, version, requires_python, &[])
}

/// A wheel an installer accepts: every member listed in `RECORD` with its
/// digest and size.
pub fn wheel_with(
    name: &str,
    version: &str,
    requires_python: Option<&str>,
    requires_dist: &[&str],
) -> Vec<u8> {
    let escaped = name.replace(['-', '.'], "_").to_lowercase();
    let dist_info = format!("{escaped}-{version}.dist-info");
    let members = [
        (format!("{escaped}/__init__.py"), format!("VERSION = \"{version}\"\n")),
        (format!("{dist_info}/METADATA"), core_metadata_with(name, version, requires_python, requires_dist)),
        (
            format!("{dist_info}/WHEEL"),
            "Wheel-Version: 1.0\nGenerator: tests\nRoot-Is-Purelib: true\nTag: py3-none-any\n".to_string(),
        ),
    ];
    let mut record = String::new();
    for (path, body) in &members {
        let digest = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(body.as_bytes()));
        record.push_str(&format!("{path},sha256={digest},{}\n", body.len()));
    }
    record.push_str(&format!("{dist_info}/RECORD,,\n"));
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut out);
        let opts = zip::write::SimpleFileOptions::default();
        for (path, body) in members.iter().chain(std::iter::once(&(format!("{dist_info}/RECORD"), record))) {
            zip.start_file(path.as_str(), opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
    out.into_inner()
}

pub fn wheel_name(name: &str, version: &str) -> String {
    format!("{}-{version}-py3-none-any.whl", name.replace(['-', '.'], "_").to_lowercase())
}

/// An sdist whose `PKG-INFO` names `name` at `version`.
pub fn sdist(name: &str, version: &str) -> Vec<u8> {
    sdist_with(name, version, &core_metadata(name, version, None))
}

/// An sdist carrying `pkg_info` as its `PKG-INFO`.
pub fn sdist_with(name: &str, version: &str, pkg_info: &str) -> Vec<u8> {
    let root = format!("{}-{version}", name.replace(['-', '.'], "_").to_lowercase());
    let mut out = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let pyproject = format!("[project]\nname = \"{name}\"\nversion = \"{version}\"\n");
        for (member, body) in [("PKG-INFO", pkg_info.to_string()), ("pyproject.toml", pyproject)] {
            let mut header = tar::Header::new_gnu();
            header.set_path(format!("{root}/{member}")).unwrap();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, body.as_bytes()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }
    out
}

pub fn sdist_name(name: &str, version: &str) -> String {
    format!("{}-{version}.tar.gz", name.replace(['-', '.'], "_").to_lowercase())
}

pub fn basic(username: &str, password: &str) -> String {
    let raw = format!("{username}:{password}");
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw))
}

/// The legacy upload form twine posts.
pub fn form(filename: &str, content: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----opencargo-test-boundary";
    let sha = format!("{:x}", sha2::Sha256::digest(content));
    let mut body = Vec::new();
    for (name, value) in [
        (":action", "file_upload"),
        ("protocol_version", "1"),
        ("metadata_version", "2.1"),
        ("name", "ignored-by-the-server"),
        ("version", "0"),
        ("filetype", if filename.ends_with(".whl") { "bdist_wheel" } else { "sdist" }),
        ("sha256_digest", sha.as_str()),
    ] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

pub async fn upload(
    client: &reqwest::Client,
    base_url: &str,
    repo: &str,
    authorization: &str,
    filename: &str,
    content: &[u8],
) -> reqwest::Response {
    let (content_type, body) = form(filename, content);
    client
        .post(format!("{base_url}/{repo}/legacy/"))
        .header("Authorization", authorization)
        .header("Content-Type", content_type)
        .body(body)
        .send()
        .await
        .unwrap()
}
