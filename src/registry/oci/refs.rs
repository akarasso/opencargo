/// Every digest a manifest references: its config blob and layers, or, for
/// an index, the child manifests. Best-effort: an unknown shape yields none.
pub fn extract_refs(manifest_json: &[u8]) -> Vec<String> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(manifest_json) else {
        return Vec::new();
    };
    let digest = |d: &serde_json::Value| d.get("digest")?.as_str().map(String::from);
    let listed = |key: &str| {
        v.get(key)
            .and_then(|l| l.as_array())
            .into_iter()
            .flatten()
            .filter_map(digest)
    };
    v.get("config")
        .and_then(digest)
        .into_iter()
        .chain(listed("layers"))
        .chain(listed("manifests"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::extract_refs;

    #[test]
    fn image_manifest_and_index_shapes() {
        let image = br#"{"config":{"digest":"sha256:c"},"layers":[{"digest":"sha256:l1"},{"digest":"sha256:l2"}]}"#;
        assert_eq!(extract_refs(image), ["sha256:c", "sha256:l1", "sha256:l2"]);
        let index = br#"{"manifests":[{"digest":"sha256:m1","platform":{}},{"digest":"sha256:m2"}]}"#;
        assert_eq!(extract_refs(index), ["sha256:m1", "sha256:m2"]);
        assert!(extract_refs(b"not json").is_empty());
        assert!(extract_refs(br#"{"layers":"nope"}"#).is_empty());
    }
}
