//! `multipart/form-data` over a body already in memory: the legacy upload
//! form twine and poetry send.

use bytes::Bytes;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub body: Bytes,
}

/// The `boundary` parameter of a `multipart/form-data` content type.
pub fn boundary(content_type: &str) -> Option<String> {
    let (media, params) = content_type.split_once(';')?;
    if !media.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    params.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| v.trim().trim_matches('"').to_string())
            .filter(|b| !b.is_empty())
    })
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// `name="x"; filename="y"` out of a `Content-Disposition` value.
fn disposition_param(value: &str, key: &str) -> Option<String> {
    let mut rest = value;
    while let Some(at) = rest.find(';') {
        rest = &rest[at + 1..];
        let trimmed = rest.trim_start();
        let Some((k, v)) = trimmed.split_once('=') else {
            continue;
        };
        if !k.trim().eq_ignore_ascii_case(key) {
            continue;
        }
        let v = v.trim_start();
        return Some(match v.strip_prefix('"') {
            Some(quoted) => {
                let mut out = String::new();
                let mut chars = quoted.chars();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => out.extend(chars.next()),
                        '"' => break,
                        c => out.push(c),
                    }
                }
                out
            }
            None => v.split(';').next().unwrap_or("").trim().to_string(),
        });
    }
    None
}

pub fn parse(body: &Bytes, boundary: &str) -> Option<Vec<Part>> {
    let delimiter = format!("--{boundary}");
    let mut at = find(body, delimiter.as_bytes(), 0)? + delimiter.len();
    let mut parts = Vec::new();
    loop {
        if body.get(at..at + 2) == Some(b"--") {
            return Some(parts);
        }
        if body.get(at..at + 2) == Some(b"\r\n") {
            at += 2;
        }
        let headers_end = find(body, b"\r\n\r\n", at)?;
        let headers = std::str::from_utf8(&body[at..headers_end]).ok()?;
        let content = headers_end + 4;
        let next = find(body, format!("\r\n{delimiter}").as_bytes(), content)?;
        let disposition = headers.split("\r\n").find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.trim().eq_ignore_ascii_case("content-disposition").then_some(v)
        })?;
        parts.push(Part {
            name: disposition_param(disposition, "name")?,
            filename: disposition_param(disposition, "filename"),
            body: body.slice(content..next),
        });
        at = next + 2 + delimiter.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_twine_form_parses_into_its_fields_and_file() {
        let ct = "multipart/form-data; boundary=\"XyZ\"";
        let b = boundary(ct).unwrap();
        assert_eq!(b, "XyZ");
        let body = Bytes::from_static(
            b"--XyZ\r\nContent-Disposition: form-data; name=\":action\"\r\n\r\nfile_upload\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"content\"; filename=\"demo-1.0.tar.gz\"\r\nContent-Type: application/octet-stream\r\n\r\n\x00\x01\r\n--X\r\n\
--XyZ--\r\n",
        );
        let parts = parse(&body, &b).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, ":action");
        assert_eq!(parts[0].body.as_ref(), b"file_upload");
        assert_eq!(parts[1].filename.as_deref(), Some("demo-1.0.tar.gz"));
        assert_eq!(parts[1].body.as_ref(), b"\x00\x01\r\n--X", "a body may hold the delimiter's prefix");
        assert!(boundary("application/json").is_none());
        assert!(parse(&Bytes::from_static(b"--XyZ\r\nbroken"), "XyZ").is_none());
    }
}
