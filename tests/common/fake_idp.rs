//! An OpenID Provider on a loopback port: discovery, JWKS, an authorization
//! endpoint that approves at once, a token endpoint that checks PKCE, and
//! knobs for every failure the adapter has to classify. Keys are P-256, the
//! tokens ES256.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine;
use serde_json::{json, Map, Value};
use sha2::Digest;

pub const CLIENT_ID: &str = "opencargo";
pub const CLIENT_SECRET: &str = "s3cret";

struct Key {
    kid: String,
    pem: String,
    x: String,
    y: String,
}

impl Key {
    fn generate(kid: &str) -> Self {
        let pair = rcgen::KeyPair::generate().expect("P-256 key");
        let raw = pair.public_key_raw();
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        Key {
            kid: kid.to_string(),
            pem: pair.serialize_pem(),
            x: b64(&raw[1..33]),
            y: b64(&raw[33..65]),
        }
    }
}

struct Grant {
    nonce: Option<String>,
    challenge: Option<String>,
    redirect_uri: String,
    claims: Map<String, Value>,
}

struct Inner {
    base: String,
    discovery_issuer: Mutex<String>,
    token_iss: Mutex<String>,
    keys: Mutex<Vec<Key>>,
    codes: Mutex<HashMap<String, Grant>>,
    claims: Mutex<Map<String, Value>>,
    iss_supported: AtomicBool,
    send_iss: AtomicBool,
    discovery_status: AtomicU16,
    jwks_status: AtomicU16,
    token_status: AtomicU16,
    discovery_hits: AtomicUsize,
    jwks_hits: AtomicUsize,
    token_hits: AtomicUsize,
    next_code: AtomicUsize,
    discovery_delay_ms: AtomicUsize,
}

#[derive(Clone)]
pub struct FakeIdp {
    pub issuer: String,
    inner: Arc<Inner>,
}

pub async fn start() -> FakeIdp {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the fake IdP");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let inner = Arc::new(Inner {
        base: base.clone(),
        discovery_issuer: Mutex::new(base.clone()),
        token_iss: Mutex::new(base.clone()),
        keys: Mutex::new(vec![Key::generate("k1")]),
        codes: Mutex::new(HashMap::new()),
        claims: Mutex::new(Map::new()),
        iss_supported: AtomicBool::new(true),
        send_iss: AtomicBool::new(true),
        discovery_status: AtomicU16::new(200),
        jwks_status: AtomicU16::new(200),
        token_status: AtomicU16::new(0),
        discovery_hits: AtomicUsize::new(0),
        jwks_hits: AtomicUsize::new(0),
        token_hits: AtomicUsize::new(0),
        next_code: AtomicUsize::new(0),
        discovery_delay_ms: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .route("/jwks", get(jwks))
        .route("/logout", get(|| async { "bye" }))
        .fallback(get(discovery))
        .with_state(inner.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    FakeIdp {
        issuer: base,
        inner,
    }
}

impl FakeIdp {
    /// The claims the next approved login carries, over `sub`/`email`
    /// defaults; `iss`, `aud`, `exp` and `nonce` are the IdP's own.
    pub fn login_as(&self, claims: Value) {
        let Value::Object(map) = claims else {
            panic!("claims are an object")
        };
        *self.inner.claims.lock().unwrap() = map;
    }

    pub fn set_discovery_issuer(&self, issuer: &str) {
        *self.inner.discovery_issuer.lock().unwrap() = issuer.to_string();
    }

    pub fn set_token_iss(&self, iss: &str) {
        *self.inner.token_iss.lock().unwrap() = iss.to_string();
    }

    /// Whether discovery announces RFC 9207, and whether the response
    /// carries `iss` at all.
    pub fn iss_parameter(&self, announced: bool, sent: bool) {
        self.inner.iss_supported.store(announced, Ordering::SeqCst);
        self.inner.send_iss.store(sent, Ordering::SeqCst);
    }

    pub fn discovery_status(&self, status: u16) {
        self.inner.discovery_status.store(status, Ordering::SeqCst);
    }

    pub fn jwks_status(&self, status: u16) {
        self.inner.jwks_status.store(status, Ordering::SeqCst);
    }

    /// Answer every code exchange with `status`; 0 restores normal service.
    pub fn token_status(&self, status: u16) {
        self.inner.token_status.store(status, Ordering::SeqCst);
    }

    pub fn discovery_delay(&self, ms: usize) {
        self.inner.discovery_delay_ms.store(ms, Ordering::SeqCst);
    }

    /// A new signing key; the old one stays published beside it.
    pub fn rotate(&self, kid: &str) {
        self.inner.keys.lock().unwrap().push(Key::generate(kid));
    }

    /// Drop every key but the newest, as an IdP that finished a rotation.
    pub fn retire_old_keys(&self) {
        let mut keys = self.inner.keys.lock().unwrap();
        let last = keys.pop().unwrap();
        *keys = vec![last];
    }

    pub fn discovery_hits(&self) -> usize {
        self.inner.discovery_hits.load(Ordering::SeqCst)
    }

    pub fn jwks_hits(&self) -> usize {
        self.inner.jwks_hits.load(Ordering::SeqCst)
    }

    pub fn token_hits(&self) -> usize {
        self.inner.token_hits.load(Ordering::SeqCst)
    }

    /// A code the token endpoint will honour, as the authorization endpoint
    /// would have issued it.
    pub fn issue_code(&self, nonce: &str, challenge: &str, redirect_uri: &str) -> String {
        issue(&self.inner, Some(nonce), Some(challenge), redirect_uri)
    }

    /// An ID Token signed with the current key over `claims` exactly.
    pub fn sign(&self, claims: &Value) -> String {
        sign(&self.inner, claims)
    }
}

fn issue(
    inner: &Inner,
    nonce: Option<&str>,
    challenge: Option<&str>,
    redirect_uri: &str,
) -> String {
    let code = format!("code-{}", inner.next_code.fetch_add(1, Ordering::SeqCst));
    inner.codes.lock().unwrap().insert(
        code.clone(),
        Grant {
            nonce: nonce.map(str::to_string),
            challenge: challenge.map(str::to_string),
            redirect_uri: redirect_uri.to_string(),
            claims: inner.claims.lock().unwrap().clone(),
        },
    );
    code
}

fn sign(inner: &Inner, claims: &Value) -> String {
    let keys = inner.keys.lock().unwrap();
    let key = keys.last().unwrap();
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    header.kid = Some(key.kid.clone());
    let encoding = jsonwebtoken::EncodingKey::from_ec_pem(key.pem.as_bytes()).unwrap();
    jsonwebtoken::encode(&header, claims, &encoding).unwrap()
}

fn status(code: u16) -> Option<Response> {
    (code != 200 && code != 0).then(|| StatusCode::from_u16(code).unwrap().into_response())
}

async fn discovery(State(inner): State<Arc<Inner>>, uri: Uri) -> Response {
    if !uri.path().ends_with("/.well-known/openid-configuration") {
        return StatusCode::NOT_FOUND.into_response();
    }
    inner.discovery_hits.fetch_add(1, Ordering::SeqCst);
    let delay = inner.discovery_delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await;
    }
    if let Some(r) = status(inner.discovery_status.load(Ordering::SeqCst)) {
        return r;
    }
    let base = &inner.base;
    Json(json!({
        "issuer": *inner.discovery_issuer.lock().unwrap(),
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "jwks_uri": format!("{base}/jwks"),
        "end_session_endpoint": format!("{base}/logout"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "code_challenge_methods_supported": ["S256"],
        "id_token_signing_alg_values_supported": ["ES256"],
        "authorization_response_iss_parameter_supported": inner.iss_supported.load(Ordering::SeqCst),
    }))
    .into_response()
}

async fn jwks(State(inner): State<Arc<Inner>>) -> Response {
    inner.jwks_hits.fetch_add(1, Ordering::SeqCst);
    if let Some(r) = status(inner.jwks_status.load(Ordering::SeqCst)) {
        return r;
    }
    let keys: Vec<Value> = inner
        .keys
        .lock()
        .unwrap()
        .iter()
        .map(|k| {
            json!({"kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256",
                   "kid": k.kid, "x": k.x, "y": k.y})
        })
        .collect();
    Json(json!({ "keys": keys })).into_response()
}

async fn authorize(
    State(inner): State<Arc<Inner>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let redirect_uri = q.get("redirect_uri").cloned().unwrap_or_default();
    if q.get("response_type").map(String::as_str) != Some("code")
        || q.get("response_mode").map(String::as_str) != Some("query")
        || q.get("code_challenge_method").map(String::as_str) != Some("S256")
    {
        return (StatusCode::BAD_REQUEST, "code flow with PKCE S256 only").into_response();
    }
    let code = issue(
        &inner,
        q.get("nonce").map(String::as_str),
        q.get("code_challenge").map(String::as_str),
        &redirect_uri,
    );
    let mut url = url::Url::parse(&redirect_uri).unwrap();
    url.query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", q.get("state").map(String::as_str).unwrap_or(""));
    if inner.send_iss.load(Ordering::SeqCst) {
        let iss = inner.discovery_issuer.lock().unwrap().clone();
        url.query_pairs_mut().append_pair("iss", &iss);
    }
    Redirect::to(url.as_str()).into_response()
}

async fn token(
    State(inner): State<Arc<Inner>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    inner.token_hits.fetch_add(1, Ordering::SeqCst);
    if let Some(r) = status(inner.token_status.load(Ordering::SeqCst)) {
        return r;
    }
    let bad = |e: &str| (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
    let secret_ok = form.get("client_id").map(String::as_str) == Some(CLIENT_ID)
        && form.get("client_secret").map(String::as_str) == Some(CLIENT_SECRET);
    if !secret_ok {
        return bad("invalid_client");
    }
    let Some(grant) = form
        .get("code")
        .and_then(|c| inner.codes.lock().unwrap().remove(c))
    else {
        return bad("invalid_grant");
    };
    if form.get("redirect_uri") != Some(&grant.redirect_uri) {
        return bad("invalid_grant");
    }
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    if grant.challenge.as_deref() != Some(computed.as_str()) {
        return bad("invalid_grant");
    }
    let now = chrono::Utc::now().timestamp();
    let mut claims = json!({
        "iss": *inner.token_iss.lock().unwrap(),
        "aud": CLIENT_ID,
        "sub": "user-1",
        "email": "dev@example.com",
        "email_verified": true,
        "iat": now,
        "exp": now + 300,
    });
    if let Some(nonce) = grant.nonce {
        claims["nonce"] = json!(nonce);
    }
    for (k, v) in grant.claims {
        claims[k] = v;
    }
    Json(json!({
        "access_token": "at",
        "token_type": "Bearer",
        "id_token": sign(&inner, &claims),
    }))
    .into_response()
}
