//! Real services in throwaway containers, for the import end-to-end cases.
//! They pull images and take minutes, so they run only when
//! `OPENCARGO_E2E_CONTAINERS=1` asks for them; once asked, a missing docker
//! or a service that never comes up skips, unless `OPENCARGO_E2E_REQUIRE=1`
//! makes it a failure.

use std::time::{Duration, Instant};

pub fn container_gate(case: &str) -> bool {
    if std::env::var("OPENCARGO_E2E_CONTAINERS").as_deref() == Ok("1") {
        return true;
    }
    println!("skipped: {case} runs real services in containers (set OPENCARGO_E2E_CONTAINERS=1)");
    false
}

fn unavailable(why: &str) -> Option<Container> {
    assert!(
        std::env::var("OPENCARGO_E2E_REQUIRE").as_deref() != Ok("1"),
        "required by OPENCARGO_E2E_REQUIRE=1: {why}"
    );
    println!("skipped: {why}");
    None
}

pub struct Container {
    pub id: String,
    pub url: String,
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker").args(["rm", "-f", &self.id]).output();
    }
}

impl Container {
    /// `docker run -d` publishing `port` on a random loopback port.
    pub fn start(image: &str, port: u16, extra: &[&str]) -> Option<Container> {
        let publish = format!("127.0.0.1::{port}");
        let mut args = vec!["run", "-d", "-p", publish.as_str()];
        args.extend_from_slice(extra);
        args.push(image);
        let out = match std::process::Command::new("docker").args(&args).output() {
            Ok(o) => o,
            Err(e) => return unavailable(&format!("docker is not usable: {e}")),
        };
        if !out.status.success() {
            return unavailable(&format!("docker run {image} failed: {}", String::from_utf8_lossy(&out.stderr)));
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let port_out = std::process::Command::new("docker").args(["port", &id, &port.to_string()]).output().ok()?;
        let mapped = String::from_utf8_lossy(&port_out.stdout).lines().next().unwrap_or_default().trim().to_string();
        Some(Container { id, url: format!("http://{mapped}") })
    }

    pub fn exec(&self, cmd: &[&str]) -> Option<String> {
        let mut args = vec!["exec", self.id.as_str()];
        args.extend_from_slice(cmd);
        let out = std::process::Command::new("docker").args(&args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Polls `path` until it answers 200, up to `within`.
    pub async fn ready(&self, path: &str, within: Duration) -> bool {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let deadline = Instant::now() + within;
        let client = reqwest::Client::new();
        while Instant::now() < deadline {
            if let Ok(r) = client.get(format!("{}{path}", self.url)).send().await {
                if r.status().is_success() {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        unavailable(&format!("{} never answered {path}", self.url));
        false
    }
}
