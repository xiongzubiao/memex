use crate::storage::content_hash;

/// Default docid length (hex characters).
pub const DEFAULT_DOCID_LEN: usize = 6;

/// Maximum docid length (full SHA-256 hex string).
pub const MAX_DOCID_LEN: usize = 64;

/// Extract docid from a content hash (first `len` chars).
pub fn docid_from_content_hash(hash: &str, len: usize) -> String {
    hash[..len.min(hash.len())].to_string()
}

/// Compute a docid from doc_type + path (fallback for identical content).
pub fn docid_from_path(doc_type: &str, path: &str, len: usize) -> String {
    let input = format!("{doc_type}:{path}");
    let hash = content_hash(input.as_bytes());
    hash[..len.min(hash.len())].to_string()
}

/// Allocate a unique docid, extending length on collision.
/// Falls back to path-hash if content hash can't disambiguate (identical content).
pub fn allocate_docid(
    content_hash_str: &str,
    doc_type: &str,
    path: &str,
    existing_docids: &[String],
) -> String {
    for len in DEFAULT_DOCID_LEN..=MAX_DOCID_LEN {
        let candidate = docid_from_content_hash(content_hash_str, len);
        if !existing_docids.contains(&candidate) {
            return candidate;
        }
        if len >= content_hash_str.len() {
            break;
        }
    }
    // Fallback: hash doc_type + path
    for len in DEFAULT_DOCID_LEN..=MAX_DOCID_LEN {
        let candidate = docid_from_path(doc_type, path, len);
        if !existing_docids.contains(&candidate) {
            return candidate;
        }
    }
    format!(
        "{}-{}",
        &content_hash_str[..6],
        &docid_from_path(doc_type, path, 6)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docid_from_content_hash_basic() {
        let hash = "abc123def456abc123def456abc123def456abc123def456abc123def456abcd";
        let docid = docid_from_content_hash(hash, 6);
        assert_eq!(docid, "abc123");
    }

    #[test]
    fn docid_extends_on_collision() {
        let hash = "abc123def456";
        let existing = vec!["abc123".to_string()];
        let docid = allocate_docid(hash, "wiki", "wiki/test.md", &existing);
        assert_eq!(docid.len(), 7);
        assert_eq!(&docid[..7], "abc123d");
    }

    #[test]
    fn docid_path_hash_fallback() {
        let fallback = docid_from_path("source", "/path/to/file.md", 6);
        assert_eq!(fallback.len(), 6);
        assert_ne!(fallback, "abc123");
    }

    #[test]
    fn allocate_docid_no_collision() {
        let hash = "abc123def456";
        let docid = allocate_docid(hash, "wiki", "wiki/test.md", &[]);
        assert_eq!(docid, "abc123");
    }
}
