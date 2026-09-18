//! NuGet packages and pushes, as `dotnet pack` and `dotnet nuget push`
//! would produce them.

use std::io::Write as _;

use reqwest::StatusCode;

pub fn nuspec(id: &str, version: &str, deps: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>{id}</id>
    <version>{version}</version>
    <authors>tester</authors>
    <description>{id} for tests</description>
    <tags>test nuget</tags>
    <dependencies><group targetFramework="net8.0">{deps}</group></dependencies>
  </metadata>
</package>"#
    )
}

pub fn nupkg(id: &str, version: &str, extra: &[&str]) -> Vec<u8> {
    nupkg_with(id, version, "", extra)
}

pub fn nupkg_with(id: &str, version: &str, deps: &str, extra: &[&str]) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(&mut out);
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file(format!("{id}.nuspec"), options).unwrap();
    zip.write_all(nuspec(id, version, deps).as_bytes()).unwrap();
    zip.start_file(format!("lib/net8.0/{id}.dll"), options).unwrap();
    zip.write_all(version.as_bytes()).unwrap();
    for path in extra {
        zip.start_file(*path, options).unwrap();
        zip.write_all(b"x").unwrap();
    }
    zip.finish().unwrap();
    out.into_inner()
}

pub fn form(bytes: Vec<u8>) -> reqwest::multipart::Form {
    reqwest::multipart::Form::new().part(
        "package",
        reqwest::multipart::Part::bytes(bytes).file_name("package.nupkg"),
    )
}

pub async fn push_as(
    client: &reqwest::Client,
    base_url: &str,
    repo: &str,
    key: &str,
    bytes: Vec<u8>,
) -> StatusCode {
    client
        .put(format!("{base_url}/{repo}/v3/package"))
        .header("X-NuGet-ApiKey", key)
        .multipart(form(bytes))
        .send()
        .await
        .expect("push request failed")
        .status()
}

pub async fn push(client: &reqwest::Client, base_url: &str, repo: &str, bytes: Vec<u8>) {
    let status = push_as(client, base_url, repo, super::STATIC_TOKEN, bytes).await;
    assert_eq!(status, StatusCode::CREATED);
}
