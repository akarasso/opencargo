//! Real-time WebSocket endpoint.
//!
//! `GET /api/v1/events/ws` — upgraded outside the auth middleware because
//! browsers cannot set an `Authorization` header on WebSocket handshakes.
//! The client authenticates with its first frame instead:
//!
//! ```json
//! { "type": "auth", "token": "trg_..." }   // or {"type":"auth"} for anonymous
//! ```
//!
//! Server replies `{"type":"hello", ...}` then streams events filtered by the
//! connection's audience (see [`crate::domain::Audience`]): anonymous ⇒
//! Public, logged-in ⇒ Authenticated, admin ⇒ Admin. Which audience an event
//! has is decided where the repository is in hand, never here; this module
//! applies the `<=` and encodes the JSON.
//!
//! Keepalive: the client may send `{"type":"ping"}` and gets `{"type":"pong"}`;
//! the server also sends protocol-level Ping frames every 30s. Tokens are
//! re-validated every ~5 minutes so a revoked token drops the connection.

use std::time::Duration;

use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use serde::Serialize;
use serde_json::json;

use crate::auth::middleware::{authenticate_bearer, AuthUser};
use crate::domain::{Audience, DomainEvent};
use crate::ports::events::{Emitted, Received};
use crate::server::AppState;

/// How long the client has to send its auth frame.
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
/// Server-side keepalive ping interval.
const HEARTBEAT: Duration = Duration::from_secs(30);
/// Re-validate the bearer token every N heartbeats (~5 minutes).
const REVALIDATE_EVERY: u32 = 10;

// Application close codes (4000-4999 range is app-defined).
const CLOSE_UNAUTHORIZED: u16 = 4401;
const CLOSE_FORBIDDEN: u16 = 4403;
// Standard "Try Again Later" close code — used for transient DB failures
// during auth, which must not look like a token rejection to the client.
const CLOSE_TRY_AGAIN: u16 = 1013;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| client_loop(socket, state))
}

/// Identity attached to one WebSocket connection.
struct WsIdentity {
    level: Audience,
    username: String,
    role: String,
    /// Raw bearer token (empty for anonymous) — kept for re-validation.
    token: Option<String>,
}

async fn client_loop(mut socket: WebSocket, state: AppState) {
    let identity = match authenticate(&mut socket, &state).await {
        Some(id) => id,
        None => return, // close frame already sent
    };

    let hello = json!({
        "type": "hello",
        "username": identity.username,
        "role": identity.role,
        "anonymous": identity.token.is_none(),
    });
    if socket.send(Message::text(hello.to_string())).await.is_err() {
        return;
    }

    let mut rx = state.events.subscribe();
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await; // first tick fires immediately — consume it
    let mut ticks: u32 = 0;

    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Received::Event(ev) => {
                    if ev.audience <= identity.level {
                        let Some(text) = frame(&ev) else { continue };
                        if socket.send(Message::text(text)).await.is_err() {
                            break;
                        }
                    }
                }
                // Subscriber fell behind and missed events: tell the client to
                // refetch what it displays instead of trusting the stream.
                Received::Lagged => {
                    if socket
                        .send(Message::text(r#"{"type":"resync"}"#.to_string()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Received::Closed => break,
            },

            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        if v.get("type").and_then(|t| t.as_str()) == Some("ping")
                            && socket
                                .send(Message::text(r#"{"type":"pong"}"#.to_string()))
                                .await
                                .is_err()
                        {
                            break;
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {} // Ping/Pong frames handled by the stack; ignore binary
                Some(Err(_)) => break,
            },

            _ = heartbeat.tick() => {
                if socket.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
                ticks += 1;
                // Re-validate mid-stream: a revoked/expired token, a forced
                // password rotation, or a role change must not keep streaming
                // at the visibility level captured at connect time (an
                // ex-admin would otherwise keep the admin event feed until
                // they closed the tab). Closing forces the client to
                // reconnect and re-authenticate at its current level.
                if ticks.is_multiple_of(REVALIDATE_EVERY) {
                    if let Some(ref token) = identity.token {
                        let violation = match authenticate_bearer(&state.auth, token).await {
                            Ok(None) => Some((CLOSE_UNAUTHORIZED, "token no longer valid")),
                            // Transient DB failure: keep the previously
                            // validated identity instead of dropping the
                            // connection; the next revalidation tick retries.
                            Err(e) => {
                                tracing::warn!(error = %e, "ws token revalidation skipped: database error");
                                None
                            }
                            Ok(Some(user)) if user.must_change_password => {
                                Some((CLOSE_FORBIDDEN, "password change required"))
                            }
                            Ok(Some(user)) => {
                                let fresh_level = if user.role == "admin" {
                                    Audience::Admin
                                } else {
                                    Audience::Authenticated
                                };
                                if fresh_level != identity.level {
                                    Some((CLOSE_UNAUTHORIZED, "access level changed — reconnect"))
                                } else {
                                    None
                                }
                            }
                        };
                        if let Some((code, reason)) = violation {
                            close(&mut socket, code, reason).await;
                            break;
                        }
                    }
                }
            },
        }
    }
}

/// Wait for the auth frame and resolve the connection's identity.
/// Returns `None` after sending an appropriate close frame on failure.
async fn authenticate(socket: &mut WebSocket, state: &AppState) -> Option<WsIdentity> {
    let frame = tokio::time::timeout(AUTH_TIMEOUT, socket.recv()).await;

    let text = match frame {
        Ok(Some(Ok(Message::Text(t)))) => t,
        _ => {
            close(socket, CLOSE_UNAUTHORIZED, "expected auth frame").await;
            return None;
        }
    };

    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            close(socket, CLOSE_UNAUTHORIZED, "invalid auth frame").await;
            return None;
        }
    };
    if parsed.get("type").and_then(|t| t.as_str()) != Some("auth") {
        close(socket, CLOSE_UNAUTHORIZED, "expected auth frame").await;
        return None;
    }

    let token = parsed
        .get("token")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string());

    match token {
        Some(t) => match authenticate_bearer(&state.auth, &t).await {
            Ok(Some(AuthUser {
                must_change_password: true,
                ..
            })) => {
                close(socket, CLOSE_FORBIDDEN, "password change required").await;
                None
            }
            Ok(Some(user)) => {
                let level = if user.role == "admin" {
                    Audience::Admin
                } else {
                    Audience::Authenticated
                };
                Some(WsIdentity {
                    level,
                    username: user.username,
                    role: user.role,
                    token: Some(t),
                })
            }
            Ok(None) => {
                close(socket, CLOSE_UNAUTHORIZED, "invalid token").await;
                None
            }
            // A DB failure says nothing about the token: tell the client to
            // retry rather than treat it as rejected.
            Err(e) => {
                tracing::warn!(error = %e, "database error during ws authentication");
                close(socket, CLOSE_TRY_AGAIN, "authentication temporarily unavailable").await;
                None
            }
        },
        None => {
            if state.auth.anonymous_read {
                Some(WsIdentity {
                    level: Audience::Public,
                    username: "anonymous".to_string(),
                    role: "anonymous".to_string(),
                    token: None,
                })
            } else {
                close(socket, CLOSE_UNAUTHORIZED, "authentication required").await;
                None
            }
        }
    }
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_string().into(),
        })))
        .await;
}

// ---------------------------------------------------------------------------
// Wire encoding
// ---------------------------------------------------------------------------

/// The envelope every event has always gone out in: the dotted name, the
/// payload, and the emission timestamp in RFC 3339 with milliseconds.
#[derive(Serialize)]
struct Frame<'a> {
    #[serde(rename = "type")]
    event_type: &'a str,
    data: serde_json::Value,
    ts: String,
}

/// `None` for a payload that will not serialize, which is a frame skipped
/// rather than a connection dropped.
fn frame(emitted: &Emitted) -> Option<String> {
    serde_json::to_string(&Frame {
        event_type: emitted.event.kind(),
        data: payload(&emitted.event),
        ts: emitted
            .at
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
    .ok()
}

/// The `data` object of each event. This is the one place the registry's
/// vocabulary becomes a client's, so the shapes are written out rather than
/// derived: a renamed field here is a broken UI.
fn payload(event: &DomainEvent) -> serde_json::Value {
    match event {
        DomainEvent::PackagePublished(r) => json!({
            "package": r.package,
            "version": r.version,
            "repository": r.repository,
            "format": r.format.as_str(),
            "published_by": r.published_by,
        }),
        DomainEvent::PackagePromoted(p) => {
            let mut data = json!({
                "package": p.package,
                "version": p.version,
                "to": p.to,
                "repository": p.to,
                "promoted_by": p.promoted_by,
            });
            if let Some(from) = &p.from {
                data["from"] = json!(from);
            }
            data
        }
        DomainEvent::RegistryChanged { repository } => json!({ "repository": repository }),
        DomainEvent::RepositoriesChanged => json!({}),
        DomainEvent::PermissionsChanged { username } => json!({ "username": username }),
        DomainEvent::AuditEntry {
            username,
            action,
            target,
        } => json!({ "username": username, "action": action, "target": target }),
        DomainEvent::PolicyResolution(c) => json!({
            "repo": c.repo,
            "member": c.member,
            "count": c.count,
            "would_block": c.would_block,
            "unknown": c.unknown,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Format, PackagePromotion, PackageRelease, ResolutionCounts};

    fn at() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-09-18T09:00:00.123Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn sent(event: DomainEvent) -> String {
        frame(&Emitted {
            event,
            audience: Audience::Public,
            at: at(),
        })
        .expect("the envelope always serializes")
    }

    /// The envelope, field for field and byte for byte: a client parses this
    /// string, so the refactor that produced it may not reshape it.
    #[test]
    fn a_publish_goes_out_in_the_envelope_it_always_has() {
        assert_eq!(
            sent(DomainEvent::PackagePublished(PackageRelease {
                package: "@sec/hidden".to_string(),
                version: "1.0.0".to_string(),
                repository: "npm-secret".to_string(),
                format: Format::Cargo,
                published_by: "alice".to_string(),
            })),
            r#"{"type":"package.published","data":{"format":"cargo","package":"@sec/hidden","published_by":"alice","repository":"npm-secret","version":"1.0.0"},"ts":"2026-09-18T09:00:00.123Z"}"#
        );
    }

    /// `from` names a repository the receiver may not be able to read, so it
    /// is present only when the caller decided it was safe.
    #[test]
    fn a_promotion_carries_its_source_only_when_it_was_given_one() {
        let mut promotion = PackagePromotion {
            package: "left-pad".to_string(),
            version: "1.0.0".to_string(),
            to: "npm-prod".to_string(),
            promoted_by: "alice".to_string(),
            from: None,
        };
        let without = sent(DomainEvent::PackagePromoted(promotion.clone()));
        assert!(!without.contains("\"from\""), "{without}");

        promotion.from = Some("npm-staging".to_string());
        let with = sent(DomainEvent::PackagePromoted(promotion));
        assert!(with.contains(r#""from":"npm-staging""#), "{with}");
        assert!(with.contains(r#""repository":"npm-prod""#), "{with}");
    }

    #[test]
    fn every_other_event_keeps_its_dotted_name_and_payload() {
        for (event, expected) in [
            (
                DomainEvent::RegistryChanged {
                    repository: "npm-secret".to_string(),
                },
                r#"{"type":"registry.changed","data":{"repository":"npm-secret"},"ts":"2026-09-18T09:00:00.123Z"}"#,
            ),
            (
                DomainEvent::RepositoriesChanged,
                r#"{"type":"repositories.changed","data":{},"ts":"2026-09-18T09:00:00.123Z"}"#,
            ),
            (
                DomainEvent::PermissionsChanged {
                    username: "bob".to_string(),
                },
                r#"{"type":"permissions.changed","data":{"username":"bob"},"ts":"2026-09-18T09:00:00.123Z"}"#,
            ),
            (
                DomainEvent::AuditEntry {
                    username: "alice".to_string(),
                    action: "user.delete".to_string(),
                    target: None,
                },
                r#"{"type":"audit.entry","data":{"action":"user.delete","target":null,"username":"alice"},"ts":"2026-09-18T09:00:00.123Z"}"#,
            ),
            (
                DomainEvent::PolicyResolution(ResolutionCounts {
                    repo: "npm-all".to_string(),
                    member: "npm-proxy".to_string(),
                    count: 3,
                    would_block: 1,
                    unknown: 0,
                }),
                r#"{"type":"policy.resolution","data":{"count":3,"member":"npm-proxy","repo":"npm-all","unknown":0,"would_block":1},"ts":"2026-09-18T09:00:00.123Z"}"#,
            ),
        ] {
            assert_eq!(sent(event), expected);
        }
    }
}
