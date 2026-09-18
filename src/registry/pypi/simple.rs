//! The Simple Repository API: one project page and the project index, in
//! HTML (PEP 503) and JSON (PEP 691), API 1.1 (PEP 700), with PEP 658's
//! metadata under both of PEP 714's names.

use serde_json::{json, Map, Value};

pub const API_VERSION: &str = "1.1";
pub const JSON_V1: &str = "application/vnd.pypi.simple.v1+json";
pub const HTML_V1: &str = "application/vnd.pypi.simple.v1+html";
pub const TEXT_HTML: &str = "text/html";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Yanked {
    No,
    Yes(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageFile {
    pub filename: String,
    /// As served: relative to the page it appears on.
    pub url: String,
    pub sha256: Option<String>,
    pub requires_python: Option<String>,
    pub yanked: Yanked,
    /// `Some(None)` when the metadata exists but its digest is unknown.
    pub core_metadata: Option<Option<String>>,
    pub size: Option<u64>,
    pub upload_time: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Page {
    pub name: String,
    pub files: Vec<PageFile>,
    pub versions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Json,
    Html,
    TextHtml,
}

impl Flavor {
    pub const fn content_type(self) -> &'static str {
        match self {
            Flavor::Json => JSON_V1,
            Flavor::Html => HTML_V1,
            Flavor::TextHtml => "text/html; charset=utf-8",
        }
    }
}

/// PEP 691 negotiation: the highest `q` wins, JSON on a tie; no header is
/// `text/html`; nothing acceptable is `None` (406).
pub fn negotiate(accept: Option<&str>) -> Option<Flavor> {
    let Some(accept) = accept.filter(|a| !a.trim().is_empty()) else {
        return Some(Flavor::TextHtml);
    };
    let mut best: Option<(f32, u8, Flavor)> = None;
    for item in accept.split(',') {
        let mut parts = item.split(';');
        let media = parts.next().unwrap_or("").trim().to_ascii_lowercase();
        let q = parts
            .filter_map(|p| p.trim().strip_prefix("q="))
            .find_map(|q| q.trim().parse::<f32>().ok())
            .unwrap_or(1.0);
        let flavor = match media.as_str() {
            JSON_V1 | "application/vnd.pypi.simple.latest+json" => (2, Flavor::Json),
            HTML_V1 | "application/vnd.pypi.simple.latest+html" => (1, Flavor::Html),
            TEXT_HTML | "text/*" | "*/*" => (0, Flavor::TextHtml),
            _ => continue,
        };
        if q <= 0.0 {
            continue;
        }
        let better = best.is_none_or(|(bq, bp, _)| q > bq || (q == bq && flavor.0 > bp));
        if better {
            best = Some((q, flavor.0, flavor.1));
        }
    }
    best.map(|(_, _, f)| f)
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

fn metadata_value(digest: &Option<String>) -> String {
    match digest {
        Some(sha) => format!("sha256={sha}"),
        None => "true".to_string(),
    }
}

fn head(title: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta name=\"pypi:repository-version\" content=\"{API_VERSION}\">\n<title>{}</title>\n</head>\n<body>\n",
        escape(title)
    )
}

pub fn render_page(page: &Page, flavor: Flavor) -> String {
    match flavor {
        Flavor::Json => page_json(page).to_string(),
        Flavor::Html | Flavor::TextHtml => page_html(page),
    }
}

fn page_html(page: &Page) -> String {
    let title = format!("Links for {}", page.name);
    let mut out = head(&title);
    out.push_str(&format!("<h1>{}</h1>\n", escape(&title)));
    for f in &page.files {
        let mut href = f.url.clone();
        if let Some(sha) = &f.sha256 {
            href.push_str(&format!("#sha256={sha}"));
        }
        out.push_str(&format!("<a href=\"{}\"", escape(&href)));
        if let Some(rp) = &f.requires_python {
            out.push_str(&format!(" data-requires-python=\"{}\"", escape(rp)));
        }
        if let Yanked::Yes(reason) = &f.yanked {
            out.push_str(&format!(" data-yanked=\"{}\"", escape(reason.as_deref().unwrap_or(""))));
        }
        if let Some(digest) = &f.core_metadata {
            let value = escape(&metadata_value(digest));
            out.push_str(&format!(
                " data-dist-info-metadata=\"{value}\" data-core-metadata=\"{value}\""
            ));
        }
        out.push_str(&format!(">{}</a><br/>\n", escape(&f.filename)));
    }
    out.push_str("</body>\n</html>\n");
    out
}

fn page_json(page: &Page) -> Value {
    let files: Vec<Value> = page
        .files
        .iter()
        .map(|f| {
            let mut o = Map::new();
            o.insert("filename".into(), json!(f.filename));
            o.insert("url".into(), json!(f.url));
            let hashes = f.sha256.as_ref().map_or_else(|| json!({}), |s| json!({"sha256": s}));
            o.insert("hashes".into(), hashes);
            if let Some(rp) = &f.requires_python {
                o.insert("requires-python".into(), json!(rp));
            }
            let yanked = match &f.yanked {
                Yanked::No => json!(false),
                Yanked::Yes(None) => json!(true),
                Yanked::Yes(Some(reason)) => json!(reason),
            };
            o.insert("yanked".into(), yanked);
            let metadata = match &f.core_metadata {
                None => json!(false),
                Some(None) => json!(true),
                Some(Some(sha)) => json!({"sha256": sha}),
            };
            o.insert("core-metadata".into(), metadata.clone());
            o.insert("dist-info-metadata".into(), metadata);
            if let Some(size) = f.size {
                o.insert("size".into(), json!(size));
            }
            if let Some(at) = &f.upload_time {
                o.insert("upload-time".into(), json!(at));
            }
            Value::Object(o)
        })
        .collect();
    json!({
        "meta": {"api-version": API_VERSION},
        "name": page.name,
        "versions": page.versions,
        "files": files,
    })
}

pub fn render_index(projects: &[String], flavor: Flavor) -> String {
    match flavor {
        Flavor::Json => json!({
            "meta": {"api-version": API_VERSION},
            "projects": projects.iter().map(|p| json!({"name": p})).collect::<Vec<_>>(),
        })
        .to_string(),
        Flavor::Html | Flavor::TextHtml => {
            let mut out = head("Simple index");
            for p in projects {
                out.push_str(&format!("<a href=\"{0}/\">{0}</a><br/>\n", escape(p)));
            }
            out.push_str("</body>\n</html>\n");
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> PageFile {
        PageFile {
            filename: "demo-1.0-py3-none-any.whl".into(),
            url: "../../files/demo/demo-1.0-py3-none-any.whl".into(),
            sha256: Some("ab".into()),
            requires_python: Some(">=3.8".into()),
            yanked: Yanked::Yes(Some("broken <x>".into())),
            core_metadata: Some(Some("cd".into())),
            size: Some(3),
            upload_time: None,
        }
    }

    #[test]
    fn negotiation_follows_pep_691() {
        assert_eq!(negotiate(None), Some(Flavor::TextHtml));
        assert_eq!(negotiate(Some(JSON_V1)), Some(Flavor::Json));
        assert_eq!(
            negotiate(Some("application/vnd.pypi.simple.v1+json, application/vnd.pypi.simple.v1+html;q=0.2, text/html;q=0.01")),
            Some(Flavor::Json)
        );
        assert_eq!(negotiate(Some("text/html;q=0.9, application/vnd.pypi.simple.v1+json;q=0.1")), Some(Flavor::TextHtml));
        assert_eq!(negotiate(Some("application/vnd.pypi.simple.latest+html")), Some(Flavor::Html));
        assert_eq!(negotiate(Some("application/json")), None, "406");
        assert_eq!(negotiate(Some("*/*")), Some(Flavor::TextHtml));
    }

    #[test]
    fn both_metadata_names_are_emitted_in_both_flavors() {
        let page = Page {
            name: "demo".into(),
            files: vec![file()],
            versions: vec!["1.0".into()],
        };
        let html = render_page(&page, Flavor::Html);
        assert!(html.contains("data-dist-info-metadata=\"sha256=cd\" data-core-metadata=\"sha256=cd\""));
        assert!(html.contains("href=\"../../files/demo/demo-1.0-py3-none-any.whl#sha256=ab\""));
        assert!(html.contains("data-requires-python=\"&gt;=3.8\""));
        assert!(html.contains("data-yanked=\"broken &lt;x&gt;\""));
        let json: Value = serde_json::from_str(&render_page(&page, Flavor::Json)).unwrap();
        let f = &json["files"][0];
        assert_eq!(f["core-metadata"], json!({"sha256": "cd"}));
        assert_eq!(f["dist-info-metadata"], json!({"sha256": "cd"}));
        assert_eq!(f["yanked"], json!("broken <x>"));
        assert_eq!(json["meta"]["api-version"], "1.1");
        assert_eq!(json["versions"], json!(["1.0"]));
    }
}
