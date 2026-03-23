use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::SqlitePool;
use tracing::{info, warn};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct WebhookDispatcher {
    client: reqwest::Client,
    db: Option<SqlitePool>,
}

impl WebhookDispatcher {
    pub fn new(db: SqlitePool) -> Self {
        Self {
            client: reqwest::Client::new(),
            db: Some(db),
        }
    }

    /// Create a dispatcher without a database (for testing or when DB is not
    /// available yet). It will not dispatch any webhooks.
    pub fn new_noop() -> Self {
        Self {
            client: reqwest::Client::new(),
            db: None,
        }
    }

    /// Dispatch a webhook event to all active webhooks that match the event.
    /// Loads webhooks from the database on each call.
    pub async fn dispatch(&self, event: &str, data: &serde_json::Value) {
        let db = match &self.db {
            Some(db) => db,
            None => return,
        };

        let webhooks = match crate::db::get_active_webhooks(db, event).await {
            Ok(whs) => whs,
            Err(e) => {
                warn!(error = %e, "Failed to load webhooks from DB");
                return;
            }
        };

        for webhook in webhooks {
            let payload = serde_json::json!({
                "event": event,
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "data": data,
            });

            let client = self.client.clone();
            let url = webhook.url.clone();
            let secret = webhook.secret.clone();
            let payload_clone = payload.clone();

            tokio::spawn(async move {
                send_webhook(&client, &url, secret.as_deref(), &payload_clone).await;
            });
        }
    }

    /// Dispatch a webhook to a specific URL (for test webhook endpoint).
    pub async fn dispatch_to_url(
        &self,
        url: &str,
        secret: Option<&str>,
        payload: &serde_json::Value,
    ) {
        let client = self.client.clone();
        let url = url.to_string();
        let secret = secret.map(|s| s.to_string());
        let payload = payload.clone();

        tokio::spawn(async move {
            send_webhook(&client, &url, secret.as_deref(), &payload).await;
        });
    }
}

/// Actually send a webhook request (shared by dispatch and dispatch_to_url).
async fn send_webhook(
    client: &reqwest::Client,
    url: &str,
    secret: Option<&str>,
    payload: &serde_json::Value,
) {
    let body = serde_json::to_vec(payload).unwrap_or_default();

    let mut request = client
        .post(url)
        .header("Content-Type", "application/json");

    if let Some(secret) = secret {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(&body);
        let result = mac.finalize();
        let signature = hex::encode(result.into_bytes());
        request = request.header(
            "X-Webhook-Signature",
            format!("sha256={}", signature),
        );
    }

    match request.body(body).send().await {
        Ok(resp) => {
            info!(
                url = %url,
                event = %payload["event"],
                status = %resp.status(),
                "Webhook delivered"
            );
        }
        Err(e) => {
            warn!(
                url = %url,
                event = %payload["event"],
                error = %e,
                "Webhook delivery failed"
            );
        }
    }
}

mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}
