/// What a manifest references: its config and layers, which are blobs, and
/// for an index its child manifests. Best-effort: an unknown shape yields none.
pub fn split_refs(manifest_json: &[u8]) -> (Vec<String>, Vec<String>) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(manifest_json) else {
        return (Vec::new(), Vec::new());
    };
    let digest = |d: &serde_json::Value| d.get("digest")?.as_str().map(String::from);
    let listed = |key: &str| {
        v.get(key)
            .and_then(|l| l.as_array())
            .into_iter()
            .flatten()
            .filter_map(digest)
    };
    let blobs = v
        .get("config")
        .and_then(digest)
        .into_iter()
        .chain(listed("layers"))
        .collect();
    (blobs, listed("manifests").collect())
}

#[cfg(test)]
mod tests {
    use super::split_refs;

    #[test]
    fn image_manifest_and_index_shapes() {
        let image = br#"{"config":{"digest":"sha256:c"},"layers":[{"digest":"sha256:l1"},{"digest":"sha256:l2"}]}"#;
        let (blobs, children) = split_refs(image);
        assert_eq!(blobs, ["sha256:c", "sha256:l1", "sha256:l2"]);
        assert!(children.is_empty());
        let index =
            br#"{"manifests":[{"digest":"sha256:m1","platform":{}},{"digest":"sha256:m2"}]}"#;
        let (blobs, children) = split_refs(index);
        assert!(blobs.is_empty());
        assert_eq!(children, ["sha256:m1", "sha256:m2"]);
        assert_eq!(split_refs(b"not json"), (vec![], vec![]));
        assert_eq!(split_refs(br#"{"layers":"nope"}"#), (vec![], vec![]));
    }
}
