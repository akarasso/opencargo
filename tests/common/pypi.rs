//! Building and uploading PyPI distributions the way twine sends them.

use std::io::Write;

use base64::Engine;
use sha2::Digest;

/// `METADATA` / `PKG-INFO` for `name` at `version`.
pub fn core_metadata(name: &str, version: &str, requires_python: Option<&str>) -> String {
    let mut out = format!("Metadata-Version: 2.1\nName: {name}\nVersion: {version}\nSummary: {name} for tests\n");
    if let Some(rp) = requires_python {
        out.push_str(&format!("Requires-Python: {rp}\n"));
    }
    out.push('\n');
    out
}

/// A wheel holding one module and its `.dist-info`.
pub fn wheel(name: &str, version: &str, requires_python: Option<&str>) -> Vec<u8> {
    let escaped = name.replace(['-', '.'], "_").to_lowercase();
    let dist_info = format!("{escaped}-{version}.dist-info");
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut out);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file(format!("{escaped}/__init__.py"), opts).unwrap();
        zip.write_all(format!("VERSION = \"{version}\"\n").as_bytes()).unwrap();
        zip.start_file(format!("{dist_info}/METADATA"), opts).unwrap();
        zip.write_all(core_metadata(name, version, requires_python).as_bytes()).unwrap();
        zip.start_file(format!("{dist_info}/WHEEL"), opts).unwrap();
        zip.write_all(b"Wheel-Version: 1.0\nGenerator: tests\nRoot-Is-Purelib: true\nTag: py3-none-any\n").unwrap();
        zip.start_file(format!("{dist_info}/RECORD"), opts).unwrap();
        zip.finish().unwrap();
    }
    out.into_inner()
}

pub fn wheel_name(name: &str, version: &str) -> String {
    format!("{}-{version}-py3-none-any.whl", name.replace(['-', '.'], "_").to_lowercase())
}

/// An sdist whose `PKG-INFO` names `name` at `version`.
pub fn sdist(name: &str, version: &str) -> Vec<u8> {
    let root = format!("{}-{version}", name.replace(['-', '.'], "_").to_lowercase());
    let mut out = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let info = core_metadata(name, version, None);
        let mut header = tar::Header::new_gnu();
        header.set_path(format!("{root}/PKG-INFO")).unwrap();
        header.set_size(info.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, info.as_bytes()).unwrap();
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
