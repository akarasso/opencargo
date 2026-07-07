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
//! connection's visibility level (see [`crate::events::Visibility`]):
//! anonymous ⇒ Public, logged-in ⇒ Authenticated, admin ⇒ Admin.
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
use serde_json::json;

use crate::auth::middleware::{authenticate_bearer, AuthUser};
use crate::events::Visibility;
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

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| client_loop(socket, state))
}

/// Identity attached to one WebSocket connection.
struct WsIdentity {
    level: Visibility,
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
                Ok(ev) => {
                    if ev.visibility <= identity.level {
                        let frame = match serde_json::to_string(&*ev) {
                            Ok(f) => f,
                            Err(_) => continue,
                        };
                        if socket.send(Message::text(frame)).await.is_err() {
                            break;
                        }
                    }
                }
                // Subscriber fell behind and missed events: tell the client to
                // refetch what it displays instead of trusting the stream.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    if socket
                        .send(Message::text(r#"{"type":"resync"}"#.to_string()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },

            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        if v.get("type").and_then(|t| t.as_str()) == Some("ping") {
                            if socket
                                .send(Message::text(r#"{"type":"pong"}"#.to_string()))
                                .await
                                .is_err()
                            {
                                break;
                            }
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
                if ticks % REVALIDATE_EVERY == 0 {
                    if let Some(ref token) = identity.token {
                        let violation = match authenticate_bearer(&state.auth, token).await {
                            None => Some((CLOSE_UNAUTHORIZED, "token no longer valid")),
                            Some(user) if user.must_change_password => {
                                Some((CLOSE_FORBIDDEN, "password change required"))
                            }
                            Some(user) => {
                                let fresh_level = if user.role == "admin" {
                                    Visibility::Admin
                                } else {
                                    Visibility::Authenticated
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
            Some(AuthUser {
                must_change_password: true,
                ..
            }) => {
                close(socket, CLOSE_FORBIDDEN, "password change required").await;
                None
            }
            Some(user) => {
                let level = if user.role == "admin" {
                    Visibility::Admin
                } else {
                    Visibility::Authenticated
                };
                Some(WsIdentity {
                    level,
                    username: user.username,
                    role: user.role,
                    token: Some(t),
                })
            }
            None => {
                close(socket, CLOSE_UNAUTHORIZED, "invalid token").await;
                None
            }
        },
        None => {
            if state.auth.anonymous_read {
                Some(WsIdentity {
                    level: Visibility::Public,
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
