//! An upstream project page, HTML or JSON, read back into its files with
//! every URL made absolute against the page. Both of PEP 714's metadata
//! names are read.

use serde_json::Value;

use super::simple::Yanked;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamFile {
    pub filename: String,
    pub url: url::Url,
    pub sha256: Option<String>,
    pub requires_python: Option<String>,
    pub yanked: Yanked,
    pub core_metadata: Option<Option<String>>,
    /// PEP 700, JSON pages only.
    pub upload_time: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpstreamPage {
    pub files: Vec<UpstreamFile>,
}

impl UpstreamPage {
    /// What the memo counts an entry as.
    pub fn weight(&self) -> usize {
        self.files
            .iter()
            .map(|f| 128 + f.filename.len() + f.url.as_str().len())
            .sum()
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `name="value"`, `name='value'`, `name=value` and bare `name` pairs of a
/// start tag's attribute text.
fn attributes(text: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        if start == i {
            break;
        }
        let name = text[start..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            out.push((name, None));
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let value = match b.get(i) {
            Some(&q) if q == b'"' || q == b'\'' => {
                let end = text[i + 1..].find(q as char).map_or(b.len(), |e| i + 1 + e);
                let v = &text[i + 1..end.min(b.len())];
                i = (end + 1).min(b.len());
                v
            }
            _ => {
                let start = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                &text[start..i]
            }
        };
        out.push((name, Some(unescape(value))));
    }
    out
}

fn metadata_attr(value: Option<&str>) -> Option<Option<String>> {
    match value?.trim() {
        "" | "false" => None,
        "true" => Some(None),
        v => Some(v.strip_prefix("sha256=").map(|s| s.to_ascii_lowercase())),
    }
}

fn split_fragment(url: &mut url::Url) -> Option<String> {
    let fragment = url.fragment().map(str::to_string);
    url.set_fragment(None);
    fragment
        .and_then(|f| f.strip_prefix("sha256=").map(str::to_string))
        .map(|s| s.to_ascii_lowercase())
}

pub fn parse_html(body: &str, page: &url::Url) -> UpstreamPage {
    let mut files = Vec::new();
    let lower = body.to_ascii_lowercase();
    let mut at = 0;
    while let Some(found) = lower[at..].find("<a") {
        let open = at + found;
        let after = open + 2;
        if !lower[after..].starts_with(|c: char| c.is_ascii_whitespace() || c == '>') {
            at = after;
            continue;
        }
        let Some(close) = lower[after..].find('>').map(|c| after + c) else {
            break;
        };
        let end = lower[close..].find("</a").map_or(lower.len(), |e| close + e);
        let attrs = attributes(&body[after..close]);
        let text = unescape(body[close + 1..end].trim());
        at = end;
        let get = |name: &str| {
            attrs
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_deref())
        };
        let Some(Some(href)) = get("href") else {
            continue;
        };
        let Ok(mut url) = page.join(href) else {
            continue;
        };
        let sha256 = split_fragment(&mut url);
        let filename = if text.is_empty() {
            url.path_segments().and_then(|mut s| s.next_back()).unwrap_or_default().to_string()
        } else {
            text
        };
        let core_metadata = get("data-core-metadata")
            .or_else(|| get("data-dist-info-metadata"))
            .and_then(|v| metadata_attr(Some(v.unwrap_or("true"))));
        files.push(UpstreamFile {
            filename,
            url,
            sha256,
            requires_python: get("data-requires-python").flatten().map(str::to_string),
            yanked: match get("data-yanked") {
                Some(reason) => Yanked::Yes(reason.filter(|r| !r.is_empty()).map(str::to_string)),
                None => Yanked::No,
            },
            core_metadata,
            upload_time: None,
        });
    }
    UpstreamPage { files }
}

fn json_metadata(v: Option<&Value>) -> Option<Option<String>> {
    match v? {
        Value::Bool(true) => Some(None),
        Value::Object(o) => Some(o.get("sha256").and_then(Value::as_str).map(|s| s.to_ascii_lowercase())),
        _ => None,
    }
}

pub fn parse_json(body: &[u8], page: &url::Url) -> Option<UpstreamPage> {
    let doc: Value = serde_json::from_slice(body).ok()?;
    let mut files = Vec::new();
    for f in doc.get("files")?.as_array()? {
        let (Some(filename), Some(href)) = (
            f.get("filename").and_then(Value::as_str),
            f.get("url").and_then(Value::as_str),
        ) else {
            continue;
        };
        let Ok(mut url) = page.join(href) else {
            continue;
        };
        let fragment = split_fragment(&mut url);
        let sha256 = f
            .get("hashes")
            .and_then(|h| h.get("sha256"))
            .and_then(Value::as_str)
            .map(|s| s.to_ascii_lowercase())
            .or(fragment);
        files.push(UpstreamFile {
            filename: filename.to_string(),
            url,
            sha256,
            requires_python: f.get("requires-python").and_then(Value::as_str).map(str::to_string),
            yanked: match f.get("yanked") {
                Some(Value::Bool(true)) => Yanked::Yes(None),
                Some(Value::String(r)) => Yanked::Yes(Some(r.clone()).filter(|r| !r.is_empty())),
                _ => Yanked::No,
            },
            core_metadata: json_metadata(f.get("core-metadata")).or_else(|| json_metadata(f.get("dist-info-metadata"))),
            upload_time: f.get("upload-time").and_then(Value::as_str).map(str::to_string),
        });
    }
    Some(UpstreamPage { files })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> url::Url {
        url::Url::parse("https://mirror.example/simple/demo/").unwrap()
    }

    #[test]
    fn html_reads_both_metadata_names_and_resolves_urls() {
        let html = r#"<!DOCTYPE html><html><body>
<a href="../../files/demo-1.0.tar.gz#sha256=AB" data-requires-python="&gt;=3.8">demo-1.0.tar.gz</a><br/>
<a href="https://files.pythonhosted.org/p/demo-1.0-py3-none-any.whl#sha256=cd" data-dist-info-metadata="sha256=EF" data-yanked="">demo-1.0-py3-none-any.whl</a>
<a href="demo-2.0-py3-none-any.whl" data-core-metadata="true" data-yanked="bad &amp; broken">demo-2.0-py3-none-any.whl</a>
<abbr>not a link</abbr>
</body></html>"#;
        let p = parse_html(html, &page());
        assert_eq!(p.files.len(), 3);
        assert_eq!(p.files[0].url.as_str(), "https://mirror.example/files/demo-1.0.tar.gz");
        assert_eq!(p.files[0].sha256.as_deref(), Some("ab"));
        assert_eq!(p.files[0].requires_python.as_deref(), Some(">=3.8"));
        assert_eq!(p.files[0].core_metadata, None);
        assert_eq!(p.files[1].core_metadata, Some(Some("ef".into())), "the PEP 658 name");
        assert_eq!(p.files[1].yanked, Yanked::Yes(None));
        assert_eq!(p.files[2].core_metadata, Some(None), "the PEP 714 name");
        assert_eq!(p.files[2].yanked, Yanked::Yes(Some("bad & broken".into())));
        assert_eq!(p.files[2].url.as_str(), "https://mirror.example/simple/demo/demo-2.0-py3-none-any.whl");
    }

    #[test]
    fn json_reads_both_metadata_names() {
        let json = br#"{"meta":{"api-version":"1.1"},"name":"demo","files":[
            {"filename":"a-1.0.tar.gz","url":"a-1.0.tar.gz","hashes":{"sha256":"AA"},"yanked":false},
            {"filename":"a-1.0-py3-none-any.whl","url":"https://x/a.whl","hashes":{},"dist-info-metadata":{"sha256":"BB"},"yanked":"why"},
            {"filename":"a-2.0-py3-none-any.whl","url":"/f/a2.whl","hashes":{"sha256":"cc"},"core-metadata":true,"requires-python":">=3","upload-time":"2026-01-02T03:04:05.000000Z"}
        ]}"#;
        let p = parse_json(json, &page()).unwrap();
        assert_eq!(p.files[0].sha256.as_deref(), Some("aa"));
        assert_eq!(p.files[0].core_metadata, None);
        assert_eq!(p.files[1].core_metadata, Some(Some("bb".into())));
        assert_eq!(p.files[1].sha256, None);
        assert_eq!(p.files[1].yanked, Yanked::Yes(Some("why".into())));
        assert_eq!(p.files[2].core_metadata, Some(None));
        assert_eq!(p.files[2].url.as_str(), "https://mirror.example/f/a2.whl");
        assert_eq!(p.files[2].upload_time.as_deref(), Some("2026-01-02T03:04:05.000000Z"));
        assert!(parse_json(b"not json", &page()).is_none());
    }
}
