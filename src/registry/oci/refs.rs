/// Extract the blob digests an image manifest references: its config blob and
/// every layer. Best-effort: a manifest list or unknown shape yields none.
pub fn extract_blob_digests(manifest_json: &[u8]) -> Vec<String> {
    let mut digests = Vec::new();
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(manifest_json) {
        if let Some(d) = v
            .get("config")
            .and_then(|c| c.get("digest"))
            .and_then(|d| d.as_str())
        {
            digests.push(d.to_string());
        }
        if let Some(layers) = v.get("layers").and_then(|l| l.as_array()) {
            for layer in layers {
                if let Some(d) = layer.get("digest").and_then(|d| d.as_str()) {
                    digests.push(d.to_string());
                }
            }
        }
    }
    digests
}
