//! Index a single raw document into `documents` + `documents_fts`,
//! optionally embedding its body chunks if a model is provided.
//!
//! The raw file layout is `<raw_dir>/<H[..2]>/<H[2..]>` where
//! `H = sha256(body)`.  The indexer verifies that re-hashing the parsed body
//! matches the hash encoded in the file path; mismatches are returned as
//! `IndexOutcome::HashMismatch` and never written to the index.

use std::path::Path;

use crate::Memex;
use crate::embed::Embedder;
use crate::error::Result;
use crate::storage::{content_hash, file_mtime_iso};

#[derive(Debug, PartialEq, Eq)]
pub enum IndexOutcome {
    Inserted,
    Updated,
    Skipped,
    HashMismatch,
}

pub fn index_raw_file(
    memex: &Memex,
    path: &Path,
    model: Option<&mut dyn Embedder>,
) -> Result<IndexOutcome> {
    let rel = memex.relativize(path);
    let rel_str = crate::storage::rel_path_string(rel);

    // Stat first so reconcile can skip unchanged files without reading them.
    let meta = std::fs::metadata(path)?;
    let size = meta.len() as i64;
    let mtime = file_mtime_iso(path);

    if let Some(existing) = memex.search().get_document_meta("raw", &rel_str)?
        && existing.mtime == mtime
        && existing.size == size
    {
        return Ok(IndexOutcome::Skipped);
    }

    let bytes = std::fs::read(path)?;
    let file = String::from_utf8(bytes).map_err(|e| {
        crate::error::MemexError::Other(anyhow::anyhow!("non-utf8 raw file: {e}"))
    })?;
    // parse_raw_frontmatter gives us the schema-typed fields (source,
    // title, ...) and the body slice. The body hash here doubles as the
    // path-integrity check (raw paths encode the body hash) and as
    // commit_doc's hash (commit_doc recomputes it; cheap for the safety
    // of having the path-hash check verified before any DB write).
    let (fm, body) = crate::raw::parse_raw_frontmatter(&file)?;
    let body_hash = content_hash(body.as_bytes());

    let path_hash = path_to_hash(path);
    if path_hash.as_deref() != Some(body_hash.as_str()) {
        tracing::warn!(
            path = %rel_str,
            expected = %body_hash,
            found = ?path_hash,
            "raw file body hash does not match path"
        );
        return Ok(IndexOutcome::HashMismatch);
    }

    let title = fm
        .title
        .clone()
        .unwrap_or_else(|| derive_title_fallback(body, fm.source.as_deref()));
    let source = fm.source.clone();

    let result = memex.search().with_transaction(|tx| {
        crate::search::commit_doc(
            tx,
            &crate::search::DocSpec {
                doc_type: "raw",
                path: &rel_str,
                title: &title,
                tags: "",
                source: source.as_deref(),
                mtime: &mtime,
                body,
                size,
            },
        )
    })?;
    let outcome = match result.outcome {
        crate::search::CommitOutcome::Inserted => IndexOutcome::Inserted,
        crate::search::CommitOutcome::Unchanged | crate::search::CommitOutcome::Updated => {
            IndexOutcome::Updated
        }
    };

    // embed_document atomically stamps embed_model + embedded_at by
    // hash inside its own transaction; no follow-up UPDATE needed.
    if matches!(outcome, IndexOutcome::Inserted | IndexOutcome::Updated)
        && let Some(model) = model
    {
        crate::retrieval::embed_document(memex.search(), &body_hash, &title, body, model)?;
    }
    Ok(outcome)
}

fn path_to_hash(path: &Path) -> Option<String> {
    let parent = path.parent()?.file_name()?.to_str()?;
    let leaf = path.file_name()?.to_str()?;
    if parent.len() != 2 {
        return None;
    }
    let combined = format!("{parent}{leaf}");
    if combined.len() != 64 || !combined.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(combined)
}

fn derive_title_fallback(body: &str, source: Option<&str>) -> String {
    if let Some(line) = body.lines().next()
        && let Some(rest) = line.trim_start().strip_prefix("# ")
    {
        let t = rest.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Some(s) = source {
        if crate::raw::is_url(s)
            && let Some(stem) = s.rsplit('/').find(|seg| !seg.is_empty())
        {
            return stem.to_string();
        }
        if let Some(stem) = std::path::Path::new(s).file_stem().and_then(|x| x.to_str()) {
            return stem.to_string();
        }
        return s.to_string();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn index_raw_file_inserts_document_and_passes_integrity_check() {
        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let body = "# Auth Tokens Explained\n\nbody body body";
        let hash = crate::storage::content_hash(body.as_bytes());
        let raw_path = crate::raw::raw_path_for_hash(&memex.raw_dir(), &hash);
        std::fs::create_dir_all(raw_path.parent().unwrap()).unwrap();
        let fm = crate::raw::RawFrontmatter {
            source: Some("https://x/p".into()),
            source_kind: Some("url".into()),
            ingested_at: Some("2026-04-26T10:00:00Z".into()),
            converter: Some("markitdown".into()),
            title: Some("Auth Tokens Explained".into()),
            ..Default::default()
        };
        std::fs::write(&raw_path, crate::raw::assemble_raw_file(&fm, body)).unwrap();

        let outcome = index_raw_file(&memex, &raw_path, None).unwrap();
        assert_eq!(outcome, IndexOutcome::Inserted);

        let conn = memex.search().conn_for_test();
        let (title, source, hash_db, embed_model): (String, String, String, Option<String>) = conn
            .query_row(
                "SELECT title, source, hash, embed_model FROM documents WHERE doc_type='raw' LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(title, "Auth Tokens Explained");
        assert_eq!(source, "https://x/p");
        assert_eq!(hash_db, hash);
        assert!(embed_model.is_none(), "model=None should leave embed_model NULL");
    }

    #[test]
    fn index_raw_file_rejects_hash_mismatch() {
        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let bogus = memex
            .raw_dir()
            .join("ab")
            .join("cdef00000000000000000000000000000000000000000000000000000000ffff");
        std::fs::create_dir_all(bogus.parent().unwrap()).unwrap();
        let fm = crate::raw::RawFrontmatter {
            source: Some("x".into()),
            ..Default::default()
        };
        std::fs::write(&bogus, crate::raw::assemble_raw_file(&fm, "different body")).unwrap();

        let outcome = index_raw_file(&memex, &bogus, None).unwrap();
        assert_eq!(outcome, IndexOutcome::HashMismatch);

        let count: i64 = memex
            .search()
            .conn_for_test()
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE doc_type='raw'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "mismatching files must not be indexed");
    }
}
