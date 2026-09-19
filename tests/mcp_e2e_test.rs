//! A real `npx` runs the command a generated `.mcp.json` carries, against
//! an opencargo npm repository, and the stdio server it installs answers
//! `tools/list`. Skipped without npx unless OPENCARGO_E2E_REQUIRE=1.

mod common;

use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use common::mcp::*;
use common::{client_bin, spawn_server, SpawnOpts, STATIC_TOKEN};
use opencargo::config::{RepositoryFormat, Visibility};

const SERVER_JS: &str = r#"#!/usr/bin/env node
const rl = require('readline').createInterface({ input: process.stdin });
rl.on('line', (line) => {
  const msg = JSON.parse(line);
  if (msg.id === undefined) return;
  const result = msg.method === 'initialize'
    ? { protocolVersion: '2025-06-18', capabilities: { tools: {} }, serverInfo: { name: 'echo', version: '1.0.0' } }
    : { tools: [{ name: 'echo', description: 'Echoes its input.', inputSchema: { type: 'object' } }] };
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: msg.id, result }) + '\n');
});
"#;

fn tarball(package_json: &str) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        for (path, body, mode) in [("package/package.json", package_json, 0o644), ("package/index.js", SERVER_JS, 0o755)] {
            let mut header = tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_size(body.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            tar.append(&header, body.as_bytes()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }
    out
}

#[tokio::test]
async fn npx_runs_the_generated_mcp_json_entry_from_the_local_npm_repository() {
    let Some(npx) = client_bin("NPX_BIN") else {
        return;
    };
    let server = spawn_server(SpawnOpts {
        repositories: vec![
            common::hosted("npm-platform", RepositoryFormat::Npm, Visibility::Public),
            hosted("internal", Visibility::Public),
        ],
        ..Default::default()
    })
    .await;
    let client = reqwest::Client::new();
    let meta = json!({"name": "@acme/echo-mcp", "version": "1.0.0", "bin": {"echo-mcp": "index.js"}});
    let tgz = tarball(&meta.to_string());
    let publish = json!({
        "name": "@acme/echo-mcp",
        "dist-tags": {"latest": "1.0.0"},
        "versions": {"1.0.0": meta},
        "_attachments": {"echo-mcp-1.0.0.tgz": {
            "content_type": "application/octet-stream",
            "data": base64::engine::general_purpose::STANDARD.encode(&tgz),
            "length": tgz.len()}},
    });
    let resp = client
        .put(format!("{}/npm-platform/@acme%2fecho-mcp", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&publish)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());

    let record = json!({"name": "io.github.acme/echo", "description": "Echo", "version": "1.0.0",
        "packages": [{"registryType": "npm", "identifier": "@acme/echo-mcp", "version": "1.0.0", "transport": {"type": "stdio"}}]});
    let resp = client
        .post(format!("{}/internal/v0.1/publish", server.base_url))
        .bearer_auth(STATIC_TOKEN)
        .json(&record)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    approve(&server, "internal", "io.github.acme/echo", "1.0.0").await;
    let (status, cfg) = get(&server, "/internal/clients/claude-code/config.json?npm=npm-platform").await;
    assert_eq!(status, 200, "{cfg}");
    let entry = &cfg["mcpServers"]["echo"];
    assert_eq!(entry["command"], "npx");
    let args: Vec<String> = entry["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect();

    let home = tempfile::TempDir::new().unwrap();
    let mut child = tokio::process::Command::new(&npx)
        .args(&args)
        .env("npm_config_cache", home.path().join("cache"))
        .env("npm_config_userconfig", home.path().join("npmrc"))
        .env("npm_config_update_notifier", "false")
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for line in [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ] {
        stdin.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    }
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let answer = tokio::time::timeout(Duration::from_secs(180), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let msg: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            if msg["id"] == 2 {
                return msg;
            }
        }
        Value::Null
    })
    .await
    .expect("npx never answered tools/list");
    assert_eq!(answer["result"]["tools"][0]["name"], "echo", "{answer}");
}
