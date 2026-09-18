//! An in-process S3 endpoint for the S3 adapter's tests: path-style, no
//! signature check, and the faults MinIO cannot be made to show on demand —
//! a completion or a copy that lands and answers late, a copy that answers
//! 200 with an error inside, a batch delete that fails one key.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};

#[derive(Default)]
struct Object {
    body: Bytes,
    modified: DateTime<Utc>,
    etag: String,
}

#[derive(Default)]
struct State {
    objects: BTreeMap<String, Object>,
    uploads: HashMap<String, (String, BTreeMap<u32, Bytes>)>,
    next: u64,
    delays: Vec<(&'static str, Duration)>,
    embedded_copy_errors: u32,
    failing_deletes: Vec<String>,
    failures: u32,
    checksum_headers: Vec<String>,
    requests: Vec<String>,
}

#[derive(Clone, Default)]
pub struct FakeS3 {
    state: Arc<Mutex<State>>,
}

pub struct Running {
    pub endpoint: String,
    pub fake: FakeS3,
    _task: tokio::task::JoinHandle<()>,
}

impl FakeS3 {
    pub async fn start() -> Running {
        let fake = FakeS3::default();
        let app = axum::Router::new().fallback({
            let fake = fake.clone();
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let fake = fake.clone();
                async move { fake.handle(method, uri, headers, body).await }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Running {
            endpoint,
            fake,
            _task: task,
        }
    }

    /// The next `op` (`complete`, `copy`) applies its effect, then answers
    /// only after `delay`.
    pub fn delay_next(&self, op: &'static str, delay: Duration) {
        self.state.lock().unwrap().delays.push((op, delay));
    }

    /// The next copy answers 200 with an `<Error>` body and copies nothing.
    pub fn embed_copy_error(&self) {
        self.state.lock().unwrap().embedded_copy_errors += 1;
    }

    /// The next `n` requests answer 500 with a body naming the bucket.
    pub fn fail_next(&self, n: u32) {
        self.state.lock().unwrap().failures += n;
    }

    /// A batch delete naming `key` reports it failed and keeps it.
    pub fn fail_delete_of(&self, key: &str) {
        self.state.lock().unwrap().failing_deletes.push(key.to_string());
    }

    /// Every checksum header a write carried.
    pub fn checksum_headers(&self) -> Vec<String> {
        self.state.lock().unwrap().checksum_headers.clone()
    }

    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }

    /// Multipart uploads opened and never completed or aborted.
    pub fn open_uploads(&self) -> usize {
        self.state.lock().unwrap().uploads.len()
    }

    pub fn keys(&self) -> Vec<String> {
        self.state.lock().unwrap().objects.keys().cloned().collect()
    }

    pub fn remove(&self, key: &str) {
        self.state.lock().unwrap().objects.remove(key);
    }

    fn delay_for(&self, op: &str) -> Option<Duration> {
        let mut state = self.state.lock().unwrap();
        let at = state.delays.iter().position(|(o, _)| *o == op)?;
        Some(state.delays.remove(at).1)
    }

    async fn handle(&self, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
        let path = percent_encoding::percent_decode_str(uri.path())
            .decode_utf8_lossy()
            .into_owned();
        let path = path.trim_start_matches('/');
        let (_bucket, key) = path.split_once('/').unwrap_or((path, ""));
        let key = key.to_string();
        let query: HashMap<String, String> = uri
            .query()
            .map(|q| url::form_urlencoded::parse(q.as_bytes()).into_owned().collect())
            .unwrap_or_default();
        self.state
            .lock()
            .unwrap()
            .requests
            .push(format!("{method} {key} {}", uri.query().unwrap_or("")));
        for (name, _) in headers.iter() {
            let name = name.as_str();
            if (name.starts_with("x-amz-checksum") || name == "x-amz-sdk-checksum-algorithm")
                && method == Method::PUT
            {
                self.state.lock().unwrap().checksum_headers.push(name.to_string());
            }
        }

        {
            let mut state = self.state.lock().unwrap();
            if state.failures > 0 {
                state.failures -= 1;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "<Error><Code>InternalError</Code><BucketName>test</BucketName></Error>",
                )
                    .into_response();
            }
        }
        match (method, key.is_empty()) {
            (Method::GET, true) if query.get("list-type").map(String::as_str) == Some("2") => {
                self.list(&query)
            }
            (Method::POST, true) if query.contains_key("delete") => self.batch_delete(&body),
            (Method::POST, false) if query.contains_key("uploads") => self.create_upload(&key),
            (Method::PUT, false) if query.contains_key("uploadId") => {
                self.put_part(&query, body)
            }
            (Method::POST, false) if query.contains_key("uploadId") => {
                self.complete(&key, &query).await
            }
            (Method::DELETE, false) if query.contains_key("uploadId") => {
                self.state.lock().unwrap().uploads.remove(&query["uploadId"]);
                StatusCode::NO_CONTENT.into_response()
            }
            (Method::PUT, false) => match headers.get("x-amz-copy-source") {
                Some(source) => {
                    let source = percent_encoding::percent_decode_str(source.to_str().unwrap_or(""))
                        .decode_utf8_lossy()
                        .into_owned();
                    let source = source.trim_start_matches('/');
                    let from = source.split_once('/').map(|(_, k)| k).unwrap_or("").to_string();
                    self.copy(&from, &key).await
                }
                None => {
                    let etag = self.store(&key, body);
                    ([("ETag", etag)], StatusCode::OK).into_response()
                }
            },
            (Method::GET, false) => self.get(&key, false),
            (Method::HEAD, false) => self.get(&key, true),
            (Method::DELETE, false) => {
                self.state.lock().unwrap().objects.remove(&key);
                StatusCode::NO_CONTENT.into_response()
            }
            _ => StatusCode::NOT_IMPLEMENTED.into_response(),
        }
    }

    fn store(&self, key: &str, body: Bytes) -> String {
        let mut state = self.state.lock().unwrap();
        state.next += 1;
        let etag = format!("\"{:x}\"", state.next);
        state.objects.insert(
            key.to_string(),
            Object {
                body,
                modified: Utc::now(),
                etag: etag.clone(),
            },
        );
        etag
    }

    fn get(&self, key: &str, head: bool) -> Response {
        let state = self.state.lock().unwrap();
        let Some(object) = state.objects.get(key) else {
            return not_found(key);
        };
        let headers = [
            ("Content-Length", object.body.len().to_string()),
            ("ETag", object.etag.clone()),
            (
                "Last-Modified",
                object.modified.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
            ),
        ];
        if head {
            (StatusCode::OK, headers).into_response()
        } else {
            (StatusCode::OK, headers, object.body.clone()).into_response()
        }
    }

    fn list(&self, query: &HashMap<String, String>) -> Response {
        let prefix = query.get("prefix").cloned().unwrap_or_default();
        let state = self.state.lock().unwrap();
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult><IsTruncated>false</IsTruncated>",
        );
        for (key, object) in state.objects.iter().filter(|(k, _)| k.starts_with(&prefix)) {
            xml.push_str(&format!(
                "<Contents><Key>{}</Key><LastModified>{}</LastModified><ETag>{}</ETag><Size>{}</Size></Contents>",
                escape(key),
                object.modified.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
                escape(&object.etag),
                object.body.len()
            ));
        }
        xml.push_str("</ListBucketResult>");
        xml_response(xml)
    }

    fn batch_delete(&self, body: &Bytes) -> Response {
        let body = String::from_utf8_lossy(body);
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><DeleteResult>");
        let mut state = self.state.lock().unwrap();
        for part in body.split("<Key>").skip(1) {
            let key = unescape(part.split("</Key>").next().unwrap_or(""));
            if state.failing_deletes.contains(&key) {
                xml.push_str(&format!(
                    "<Error><Key>{}</Key><Code>InternalError</Code><Message>injected</Message></Error>",
                    escape(&key)
                ));
            } else {
                state.objects.remove(&key);
                xml.push_str(&format!("<Deleted><Key>{}</Key></Deleted>", escape(&key)));
            }
        }
        xml.push_str("</DeleteResult>");
        xml_response(xml)
    }

    fn create_upload(&self, key: &str) -> Response {
        let mut state = self.state.lock().unwrap();
        state.next += 1;
        let id = format!("upload-{}", state.next);
        state.uploads.insert(id.clone(), (key.to_string(), BTreeMap::new()));
        xml_response(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><InitiateMultipartUploadResult><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"
        ))
    }

    fn put_part(&self, query: &HashMap<String, String>, body: Bytes) -> Response {
        let number: u32 = query.get("partNumber").and_then(|n| n.parse().ok()).unwrap_or(0);
        let mut state = self.state.lock().unwrap();
        let Some((_, parts)) = state.uploads.get_mut(&query["uploadId"]) else {
            return (StatusCode::NOT_FOUND, "<Error><Code>NoSuchUpload</Code></Error>").into_response();
        };
        parts.insert(number, body);
        ([("ETag", format!("\"part-{number}\""))], StatusCode::OK).into_response()
    }

    async fn complete(&self, key: &str, query: &HashMap<String, String>) -> Response {
        let upload = self.state.lock().unwrap().uploads.remove(&query["uploadId"]);
        let Some((_, parts)) = upload else {
            return (StatusCode::NOT_FOUND, "<Error><Code>NoSuchUpload</Code></Error>").into_response();
        };
        let mut whole = Vec::new();
        for part in parts.values() {
            whole.extend_from_slice(part);
        }
        let etag = self.store(key, Bytes::from(whole));
        if let Some(delay) = self.delay_for("complete") {
            tokio::time::sleep(delay).await;
        }
        xml_response(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><CompleteMultipartUploadResult><ETag>{}</ETag></CompleteMultipartUploadResult>",
            escape(&etag)
        ))
    }

    async fn copy(&self, from: &str, to: &str) -> Response {
        {
            let mut state = self.state.lock().unwrap();
            if state.embedded_copy_errors > 0 {
                state.embedded_copy_errors -= 1;
                return xml_response(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>InternalError</Code><Message>injected</Message></Error>".to_string(),
                );
            }
        }
        let body = self.state.lock().unwrap().objects.get(from).map(|o| o.body.clone());
        let Some(body) = body else {
            return not_found(from);
        };
        let etag = self.store(to, body);
        if let Some(delay) = self.delay_for("copy") {
            tokio::time::sleep(delay).await;
        }
        xml_response(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><CopyObjectResult><ETag>{}</ETag><LastModified>{}</LastModified></CopyObjectResult>",
            escape(&etag),
            Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ")
        ))
    }
}

fn not_found(key: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        format!("<Error><Code>NoSuchKey</Code><Key>{}</Key></Error>", escape(key)),
    )
        .into_response()
}

fn xml_response(xml: String) -> Response {
    ([("Content-Type", "application/xml")], xml).into_response()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&")
}
