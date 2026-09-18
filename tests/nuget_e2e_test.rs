//! A real `dotnet` against opencargo: pack, push with `-k`, restore through
//! a group with `packageSourceCredentials`, `list package --outdated`,
//! delete as unlist then restore of the unlisted version, search, and an
//! anonymous restore of a private group answered 401 rather than NU1101.
//! `nuget.exe` (`NUGET_BIN`) is driven the same way when present.
//!
//! Skipped unless `DOTNET_BIN` (or `dotnet` on PATH) runs;
//! `scripts/dotnet-in-docker` is a pinned SDK image that can stand in.

mod common;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;

use common::{client_bin, create_user, add_token, group, hosted, run_cmd, spawn_server, SpawnOpts, STATIC_TOKEN};
use opencargo::config::{RepositoryFormat, Visibility};

struct Env {
    bin: String,
    home: PathBuf,
    packages: PathBuf,
}

impl Env {
    async fn run(&self, args: &[&str], cwd: &Path) -> (bool, String) {
        let env: [(&str, &OsStr); 2] = [
            ("HOME", self.home.as_os_str()),
            ("NUGET_PACKAGES", self.packages.as_os_str()),
        ];
        let (ok, out, err) = run_cmd(&self.bin, args, cwd, &env).await;
        (ok, format!("{out}\n{err}"))
    }
}

/// `sources` are `(key, url)`; every one gets the credentials when given.
fn config(sources: &[(&str, &str)], credentials: Option<(&str, &str)>) -> String {
    let adds: String = sources
        .iter()
        .map(|(key, url)| format!(r#"<add key="{key}" value="{url}" allowInsecureConnections="true" />"#))
        .collect();
    let creds: String = credentials
        .map(|(user, token)| {
            sources
                .iter()
                .map(|(key, _)| {
                    format!(
                        r#"<{key}><add key="Username" value="{user}" /><add key="ClearTextPassword" value="{token}" /></{key}>"#
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<configuration>
  <packageSources><clear />{adds}</packageSources>
  <packageSourceCredentials>{creds}</packageSourceCredentials>
</configuration>"#
    )
}

fn consumer(dir: &Path, version: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("App.csproj"),
        format!(
            r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net8.0</TargetFramework></PropertyGroup>
  <ItemGroup><PackageReference Include="Greeter" Version="[{version}]" /></ItemGroup>
</Project>"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join("Program.cs"), "System.Console.WriteLine(1);\n").unwrap();
}

#[tokio::test]
async fn dotnet_push_restore_outdated_unlist_and_anonymous_401() {
    let Some(bin) = client_bin("DOTNET_BIN") else {
        return;
    };
    tokio::time::timeout(Duration::from_secs(900), roundtrip(bin))
        .await
        .expect("the dotnet roundtrip timed out");
}

async fn roundtrip(bin: String) {
    let server = spawn_server(SpawnOpts {
        repositories: vec![
            hosted("nuget-local", RepositoryFormat::Nuget, Visibility::Private),
            group("nuget", RepositoryFormat::Nuget, &["nuget-local"]),
        ],
        ..Default::default()
    })
    .await;
    let base = &server.base_url;
    let c = reqwest::Client::new();
    create_user(&c, base, STATIC_TOKEN, "dev", "publisher").await;
    let token = add_token(&c, base, "dev", "ci").await;

    let tmp = TempDir::new().unwrap();
    let work = tmp.path();
    let env = Env {
        bin,
        home: work.join("home"),
        packages: work.join("packages"),
    };
    std::fs::create_dir_all(&env.home).unwrap();
    let hosted_src = format!("{base}/nuget-local/v3/index.json");
    let group_src = format!("{base}/nuget/v3/index.json");
    std::fs::write(
        work.join("nuget.config"),
        config(&[("oc", &group_src), ("local", &hosted_src)], Some(("dev", &token))),
    )
    .unwrap();

    let (ok, out) = env.run(&["new", "classlib", "-n", "Greeter", "-o", "greeter"], work).await;
    assert!(ok, "{out}");
    for v in ["1.0.0", "1.1.0"] {
        let prop = format!("-p:PackageVersion={v}");
        let (ok, out) = env.run(&["pack", "greeter", "-c", "Release", "-o", "out", &prop], work).await;
        assert!(ok, "pack {v}: {out}");
        let file = format!("out/Greeter.{v}.nupkg");
        let (ok, out) = env
            .run(&["nuget", "push", &file, "-s", "local", "-k", &token], work)
            .await;
        assert!(ok, "push {v}: {out}");
    }
    let (ok, out) = env
        .run(&["nuget", "push", "out/Greeter.1.0.0.nupkg", "-s", "local", "-k", &token], work)
        .await;
    assert!(!ok && out.contains("409"), "a second push is a conflict: {out}");
    let (ok, out) = env
        .run(&["nuget", "push", "out/Greeter.1.1.0.nupkg", "-s", "local", "-k", "az"], work)
        .await;
    println!("NUGET-R3-3: dotnet nuget push -k az with Basic credentials configured: ok={ok}\n{out}");
    assert!(!ok, "a placeholder key with Basic credentials is refused under A1 C7");

    let (ok, out) = env
        .run(&["pack", "greeter", "-c", "Release", "-o", "out", "-p:PackageVersion=1.2.0-rc"], work)
        .await;
    assert!(ok, "{out}");
    let (ok, out) = env
        .run(&["nuget", "push", "out/Greeter.1.2.0-rc.nupkg", "-s", "local"], work)
        .await;
    println!("NUGET-R3-3: dotnet nuget push without -k, Basic credentials configured: ok={ok}\n{out}");
    assert!(ok, "Basic credentials alone push: {out}");

    consumer(&work.join("app"), "1.0.0");
    let (ok, out) = env.run(&["restore", "app"], work).await;
    assert!(ok, "restore through the group: {out}");
    let (ok, out) = env.run(&["list", "app", "package", "--outdated"], work).await;
    assert!(ok && out.contains("1.1.0"), "outdated: {out}");

    let (ok, out) = env
        .run(&["nuget", "delete", "Greeter", "1.0.0", "-s", "local", "-k", &token, "--non-interactive"], work)
        .await;
    assert!(ok, "delete is an unlist: {out}");
    std::fs::remove_dir_all(&env.packages).ok();
    let (ok, out) = env.run(&["restore", "app", "--force"], work).await;
    assert!(ok, "the unlisted version still restores by exact version: {out}");
    let (ok, out) = env
        .run(&["package", "search", "Greeter", "--source", "oc"], work)
        .await;
    assert!(ok && out.contains("1.1.0"), "{out}");
    assert!(!out.contains("1.0.0"), "search does not show the unlisted version: {out}");

    let outside = TempDir::new().unwrap();
    let anon = outside.path().join("anon");
    std::fs::create_dir_all(&anon).unwrap();
    std::fs::write(anon.join("nuget.config"), config(&[("oc", &group_src)], None)).unwrap();
    consumer(&anon.join("app"), "1.1.0");
    std::fs::remove_dir_all(&env.packages).ok();
    let (ok, out) = env.run(&["restore", "app", "--no-cache"], &anon).await;
    assert!(!ok, "an anonymous restore of a private group fails: {out}");
    assert!(out.contains("401"), "it is asked for credentials: {out}");
    assert!(!out.contains("NU1101"), "not reported as a missing package: {out}");
}

#[tokio::test]
async fn nuget_exe_push_install_delete() {
    let Some(bin) = client_bin("NUGET_BIN") else {
        return;
    };
    let server = spawn_server(SpawnOpts {
        repositories: vec![hosted("nuget", RepositoryFormat::Nuget, Visibility::Public)],
        ..Default::default()
    })
    .await;
    let tmp = TempDir::new().unwrap();
    let work = tmp.path();
    let pkg = work.join("Greeter.1.0.0.nupkg");
    std::fs::write(&pkg, common::nuget::nupkg("Greeter", "1.0.0", &[])).unwrap();
    let src = format!("{}/nuget/v3/index.json", server.base_url);
    let pkg = pkg.to_string_lossy().into_owned();
    let (ok, out, err) = run_cmd(&bin, &["push", &pkg, "-Source", &src, "-ApiKey", STATIC_TOKEN], work, &[]).await;
    assert!(ok, "{out}{err}");
    let out_dir = work.join("installed").to_string_lossy().into_owned();
    let (ok, out, err) = run_cmd(&bin, &["install", "Greeter", "-Version", "1.0.0", "-Source", &src, "-OutputDirectory", &out_dir], work, &[]).await;
    assert!(ok, "{out}{err}");
    let (ok, out, err) = run_cmd(&bin, &["delete", "Greeter", "1.0.0", "-Source", &src, "-ApiKey", STATIC_TOKEN, "-NonInteractive"], work, &[]).await;
    assert!(ok, "{out}{err}");
}
