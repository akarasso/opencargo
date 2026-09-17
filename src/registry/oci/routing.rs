use axum::http::Request;

/// Fold a nested image name (`team/app`) into one percent-encoded segment so
/// the single-segment `/v2/{repo}/{name}/...` routes match; axum decodes it
/// back into the `name` path param. Every other request passes untouched.
pub fn rewrite_nested_name<B>(mut req: Request<B>) -> Request<B> {
    let path = req.uri().path();
    let Some(rest) = path.strip_prefix("/v2/").filter(|r| !r.is_empty()) else {
        return req;
    };
    let segs: Vec<&str> = rest.split('/').collect();
    let Some((start, end)) = split_v2_path(&segs) else {
        return req;
    };
    if end - start < 2 {
        return req;
    }
    let new_path = format!(
        "/v2/{}/{}/{}",
        segs[..start].join("/"),
        segs[start..end].join("%2F"),
        segs[end..].join("/")
    );
    let new_uri_str = match req.uri().query() {
        Some(q) => format!("{new_path}?{q}"),
        None => new_path,
    };
    match new_uri_str.parse() {
        Ok(new_uri) => *req.uri_mut() = new_uri,
        Err(e) => {
            tracing::warn!(uri = %req.uri(), error = %e, "re-parse of rewritten OCI URI failed; keeping original");
        }
    }
    req
}

/// The `[start, end)` range of the image-name segments in a `/v2/` path split
/// on `/` (`segs[0]` is the repository). The endpoint marker is matched from
/// the right with its exact arity, so a name segment literally called `blobs`
/// or `manifests` never shifts the split.
fn split_v2_path(segs: &[&str]) -> Option<(usize, usize)> {
    let n = segs.len();
    let marker = if n >= 2 && segs[n - 2..] == ["tags", "list"] {
        n - 2
    } else if n >= 3 && segs[n - 3..n - 1] == ["blobs", "uploads"] {
        n - 3
    } else if n >= 2 && matches!(segs[n - 2], "manifests" | "blobs") {
        n - 2
    } else {
        return None;
    };
    (marker > 1).then_some((1, marker))
}

#[cfg(test)]
mod tests {
    use super::rewrite_nested_name;

    fn rewritten(uri: &str) -> String {
        let req = axum::http::Request::builder()
            .uri(uri)
            .body(())
            .expect("test URI should build");
        rewrite_nested_name(req).uri().to_string()
    }

    #[test]
    fn six_route_shapes() {
        for (input, expected) in [
            ("/v2/", "/v2/"),
            (
                "/v2/r/team/app/blobs/sha256:abc",
                "/v2/r/team%2Fapp/blobs/sha256:abc",
            ),
            (
                "/v2/r/team/app/blobs/uploads/",
                "/v2/r/team%2Fapp/blobs/uploads/",
            ),
            (
                "/v2/r/team/app/blobs/uploads/uuid-1?digest=sha256:abc",
                "/v2/r/team%2Fapp/blobs/uploads/uuid-1?digest=sha256:abc",
            ),
            (
                "/v2/r/org/team/app/manifests/latest",
                "/v2/r/org%2Fteam%2Fapp/manifests/latest",
            ),
            (
                "/v2/r/team/app/manifests/sha256:abc",
                "/v2/r/team%2Fapp/manifests/sha256:abc",
            ),
            ("/v2/r/team/app/tags/list?n=5", "/v2/r/team%2Fapp/tags/list?n=5"),
            (
                "/v2/r/team/blobs/blobs/uploads/",
                "/v2/r/team%2Fblobs/blobs/uploads/",
            ),
            (
                "/v2/r/team/manifests/blobs/sha256:abc",
                "/v2/r/team%2Fmanifests/blobs/sha256:abc",
            ),
            (
                "/v2/r/uploads/tags/tags/list",
                "/v2/r/uploads%2Ftags/tags/list",
            ),
        ] {
            assert_eq!(rewritten(input), expected, "{input}");
        }
        for untouched in [
            "/v2",
            "/v2/r/app/blobs/sha256:abc",
            "/v2/r/app/blobs/uploads/",
            "/v2/r/app/manifests/latest",
            "/v2/r/app/tags/list",
            "/v2/r/manifests/latest",
            "/v2/r/team/app/unknown/x",
            "/npm-hosted/team/app/manifests/latest",
        ] {
            assert_eq!(rewritten(untouched), untouched, "{untouched}");
        }
    }
}
