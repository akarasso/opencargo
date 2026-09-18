//! Where a repository's bytes live, as keys: pure functions of opaque ids.
//! Every new key of a repository lies under its incarnation's prefix, so a
//! retired name and its recreation share nothing.

/// The prefix every new key of an incarnation lies under.
pub fn incarnation_prefix(incarnation: &str) -> String {
    format!("r/{incarnation}")
}

/// The name-keyed prefixes a repository's bytes were written under before
/// incarnations: its format's tree and its proxy cache.
pub fn name_keyed_prefixes(format: &str, name: &str) -> Vec<String> {
    vec![format!("{format}/{name}"), format!("_proxy_cache/{name}")]
}

/// `HostedKey`: where a hosted artifact lives, a function of the
/// incarnation's prefix, the package, the content's sha256 and the file
/// name. A format never builds a shared key itself.
pub fn hosted_key(repo_prefix: &str, package: &str, sha256: &str, filename: &str) -> String {
    format!("{repo_prefix}/{package}/{sha256}/{filename}")
}

/// A private scratch object under an incarnation, for a placement whose
/// digest is not known before its body ends.
pub fn draft_key(repo_prefix: &str, nonce: &str) -> String {
    format!("{repo_prefix}/_drafts/{nonce}")
}

/// Where an OCI blob of an incarnation lives: shared by every image of the
/// repository, keyed by its sha256 alone.
pub fn oci_blob_key(repo_prefix: &str, sha256: &str) -> String {
    hosted_key(repo_prefix, "_blobs", sha256, "blob")
}

/// Where an OCI manifest of one image lives.
pub fn oci_manifest_key(repo_prefix: &str, image: &str, sha256: &str) -> String {
    hosted_key(repo_prefix, image, sha256, "manifest")
}

/// The private tree of one OCI upload session: every segment lies under it.
pub fn upload_prefix(repo_prefix: &str, upload: &str) -> String {
    format!("{repo_prefix}/_uploads/{upload}")
}

/// One segment of an upload session, named by the offset it starts at and a
/// nonce, so two writers of one offset never share a key.
pub fn segment_key(upload_prefix: &str, start: u64, nonce: &str) -> String {
    format!("{upload_prefix}/{start:020}-{nonce}")
}

/// The last segment of a key: the file name a new key keeps.
pub fn file_name(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

/// A logical key placed at one generation: the only key a row records.
pub fn physical_key(logical: &str, generation: &str) -> String {
    format!("{logical}~{generation}")
}

/// The logical key of a physical one; a legacy key is its own.
pub fn logical_key(physical: &str) -> &str {
    match physical.rsplit_once('~') {
        Some((logical, generation)) if !generation.contains('/') => logical,
        _ => physical,
    }
}

/// Whether `key` is `prefix` itself or lies under it, segment by segment:
/// `r/ab` never contains `r/abc`.
pub fn under(key: &str, prefix: &str) -> bool {
    key == prefix
        || key
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_compare_as_segments() {
        assert!(under("r/ab/x", "r/ab"));
        assert!(under("r/ab", "r/ab"));
        assert!(!under("r/abc/x", "r/ab"), "a string-prefix sibling is another prefix");
    }

    #[test]
    fn a_physical_key_names_its_logical_key_and_generation() {
        assert_eq!(physical_key("r/i/p/f", "g1"), "r/i/p/f~g1");
        assert_eq!(hosted_key("r/i", "@s/p", "ab", "p-1.tgz"), "r/i/@s/p/ab/p-1.tgz");
        assert_eq!(draft_key("r/i", "n"), "r/i/_drafts/n");
        assert_eq!(file_name("npm/r/p/p-1.0.0.tgz"), "p-1.0.0.tgz");
        assert_eq!(logical_key("r/i/p/f.tgz~g1"), "r/i/p/f.tgz");
        assert_eq!(logical_key("npm/r/p/f.tgz"), "npm/r/p/f.tgz");
        assert_eq!(incarnation_prefix("i"), "r/i");
        assert_eq!(oci_blob_key("r/i", "ab"), "r/i/_blobs/ab/blob");
        assert_eq!(oci_manifest_key("r/i", "team/app", "ab"), "r/i/team/app/ab/manifest");
        assert_eq!(segment_key(&upload_prefix("r/i", "u"), 5, "n"), "r/i/_uploads/u/00000000000000000005-n");
        assert_eq!(
            name_keyed_prefixes("npm", "web"),
            vec!["npm/web".to_string(), "_proxy_cache/web".to_string()]
        );
    }
}
