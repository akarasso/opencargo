use reqwest::StatusCode;
use serde_json::Value;
use tempfile::TempDir;

use opencargo::config::{
    AuthConfig, Config, DatabaseConfig, RepositoryConfig, RepositoryFormat, RepositoryType,
    ServerConfig, TlsConfig, Visibility,
};
use opencargo::server;

// ---------------------------------------------------------------------------
// TLS test
// ---------------------------------------------------------------------------

/// Generate a self-signed cert and start the server with TLS enabled,
/// then make a request using reqwest configured to accept self-signed certs.
#[tokio::test]
async fn test_tls_server() {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let storage_path = tmp.path().join("storage");
    let db_path = tmp.path().join("test.db");

    // Generate self-signed certificate
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("failed to generate self-signed cert");

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    let cert_path = tmp.path().join("cert.pem");
    let key_path = tmp.path().join("key.pem");
    std::fs::write(&cert_path, &cert_pem).expect("failed to write cert");
    std::fs::write(&key_path, &key_pem).expect("failed to write key");

    let db_url = format!(
        "sqlite:{}?mode=rwc",
        db_path.to_str().expect("non-utf8 temp path")
    );

    // Find an available port
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind");
    let addr = listener.local_addr().expect("no local addr");
    drop(listener);

    let base_url = format!("https://localhost:{}", addr.port());

    let config = Config {
        server: ServerConfig {
            bind: addr.to_string(),
            base_url: base_url.clone(),
            storage_path: storage_path
                .to_str()
                .expect("non-utf8 temp path")
                .to_string(),
            tls: TlsConfig {
                enabled: true,
                cert_path: cert_path.to_str().unwrap().to_string(),
                key_path: key_path.to_str().unwrap().to_string(),
            },
        },
        database: DatabaseConfig { url: db_url },
        auth: AuthConfig {
            anonymous_read: true,
            static_tokens: vec!["test-token".to_string()],
            ..Default::default()
        },
        repositories: vec![RepositoryConfig {
            name: "test-npm".to_string(),
            repo_type: RepositoryType::Hosted,
            format: RepositoryFormat::Npm,
            visibility: Visibility::Public,
            upstream: None,
            members: None,
        }],
        ..Default::default()
    };

    let app_state = server::build_state(&config)
        .await
        .expect("failed to build app state");
    let router = server::build_router(app_state);

    let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
        &cert_path,
        &key_path,
    )
    .await
    .expect("failed to create rustls config");

    let bind_addr: std::net::SocketAddr = addr;

    let _handle = tokio::spawn(async move {
        axum_server::bind_rustls(bind_addr, tls_config)
            .serve(router.into_make_service())
            .await
            .ok();
    });

    // Build a reqwest client that accepts self-signed certs
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("failed to build reqwest client");

    // Wait for the server to be ready
    for _ in 0..100 {
        match client.get(format!("{}/health/live", &base_url)).send().await {
            Ok(resp) if resp.status().is_success() => break,
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }

    // Make a request
    let resp = client
        .get(format!("{}/health/live", base_url))
        .send()
        .await
        .expect("TLS request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.expect("invalid json");
    assert_eq!(body["status"], "ok", "health check should return ok over TLS");
}
