use std::collections::HashMap;

/// `{repo}/{name}` from the path params, the key used by manifest paths.
pub fn image_name(params: &HashMap<String, String>) -> String {
    let repo = params.get("repo").cloned().unwrap_or_default();
    let name = params.get("name").cloned().unwrap_or_default();
    format!("{}/{}", repo, name)
}

fn digest_hex(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

/// Blobs are content-addressed and shared at the repository level
/// (`UNIQUE(repository_id, digest)`), so the path never carries the image name.
pub fn blob_path(repo_name: &str, digest: &str) -> String {
    format!("oci/{}/_blobs/sha256/{}", repo_name, digest_hex(digest))
}

pub fn manifest_path(image_name: &str, name: &str, digest: &str) -> String {
    format!(
        "oci/{}/manifests/{}/sha256/{}",
        image_name,
        name,
        digest_hex(digest)
    )
}
