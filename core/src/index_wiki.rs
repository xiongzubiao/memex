//! Index a single wiki markdown file into `documents` + `titles_fts`,
//! optionally embedding its body chunks if a model is provided.
//!
//! Production callers (daemon watcher, ingest handler, reconciliation) load
//! the ONNX model **once at startup** and pass `Some(&mut model)` per event.
//! Tests pass `None` so the unit suite never touches ONNX. A separate
//! backfill pass handles documents whose `embed_model` is NULL.

use std::path::Path;

use crate::Memex;
use crate::embed::Embedder;
use crate::error::Result;
use crate::search::{CommitOutcome, DocSpec, commit_doc};
use crate::validate;

#[derive(Debug, PartialEq, Eq)]
pub enum IndexOutcome {
    Inserted,
    Updated,
    Skipped,
}

impl From<CommitOutcome> for IndexOutcome {
    fn from(c: CommitOutcome) -> Self {
        match c {
            CommitOutcome::Inserted => IndexOutcome::Inserted,
            // Same hash as before is treated as Updated here so the
            // mtime/size refresh from the upsert is reflected in the
            // outcome the caller logs.
            CommitOutcome::Unchanged | CommitOutcome::Updated => IndexOutcome::Updated,
        }
    }
}

pub fn index_wiki_file(
    memex: &Memex,
    path: &Path,
    model: Option<&mut dyn Embedder>,
) -> Result<IndexOutcome> {
    // Span carries `path` into warns emitted deeper in the chunking/
    // embedding stack. Silent at info-or-below levels.
    let _span = tracing::info_span!("index_wiki_file", path = %path.display()).entered();
    let rel = path.strip_prefix(memex.root()).unwrap_or(path);
    let rel_str = crate::storage::rel_path_string(rel);

    // Stat first so reconcile can skip unchanged files without reading them.
    let meta = std::fs::metadata(path)?;
    let size = meta.len() as i64;
    let mtime = meta.modified()?;

    if let Some(existing) = memex.search().get_document_meta("wiki", &rel_str)?
        && existing.mtime == mtime
        && existing.size == size
    {
        return Ok(IndexOutcome::Skipped);
    }

    let bytes = std::fs::read(path)?;
    let content = String::from_utf8(bytes)
        .map_err(|e| crate::error::MemexError::Other(anyhow::anyhow!("non-utf8 wiki body: {e}")))?;
    // parse_frontmatter is wiki-schema-aware and gives us the body
    // slice that commit_doc will hash and feed to chunks_fts.
    let (fm, body) = validate::parse_frontmatter(&content).map_err(|e| {
        crate::error::MemexError::Other(anyhow::anyhow!("frontmatter parse failed: {e}"))
    })?;

    let result = memex.search().with_transaction(|tx| {
        commit_doc(
            tx,
            &DocSpec {
                doc_type: "wiki",
                path: &rel_str,
                title: &fm.title,
                source: None,
                mtime,
                body: &body,
                size,
            },
        )
    })?;

    let outcome: IndexOutcome = result.outcome.into();

    // embed_document atomically stamps embed_model + embedded_at by
    // hash inside its own transaction, so no follow-up UPDATE is
    // needed here. Hash-keyed stamping handles the case where two
    // wiki pages share a body — both rows land on the same chunks
    // and the same model marker in one tx.
    if outcome != IndexOutcome::Skipped
        && let Some(model) = model
    {
        crate::retrieval::embed_document(
            memex.search(),
            &result.body_hash,
            &fm.title,
            &body,
            model,
        )?;
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn index_wiki_file_inserts_document_and_fts_without_model() {
        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let path = memex.wiki_dir().join("auth-tokens.md");
        std::fs::write(
            &path,
            "---\ntitle: Auth Tokens
created_at: 2026-04-26T00:00:00Z\nupdated_at: 2026-04-26T00:00:00Z\nsources: []\n---\n\n# Auth Tokens\n\nBearer tokens auth users.\n",
        )
        .unwrap();

        let outcome = index_wiki_file(&memex, &path, None).unwrap();
        assert_eq!(outcome, IndexOutcome::Inserted);

        let conn = memex.search().conn_for_test();
        let (path_db, title, hash, embed_model): (
            String,
            String,
            String,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT path, title, hash, embed_model FROM documents WHERE doc_type='wiki' LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(path_db, "wiki/auth-tokens.md");
        assert_eq!(title, "Auth Tokens");
        assert_eq!(hash.len(), 64);
        assert!(
            embed_model.is_none(),
            "model=None should leave embed_model NULL"
        );

        let fts_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM titles_fts WHERE titles_fts MATCH 'auth'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_hits, 1);

        // chunks + chunks_fts are populated at commit time so chunk-level
        // BM25 works without an embedder. The vector index (chunks_vec)
        // is still empty until the embed step runs.
        let chunk_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chunks WHERE hash=?1",
                rusqlite::params![hash],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            chunk_count >= 1,
            "chunks rows should be populated at commit"
        );
        let vec_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chunks_vec WHERE hash_seq LIKE ?1 || '%'",
                rusqlite::params![hash],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            vec_count, 0,
            "chunks_vec must remain empty without an embedder"
        );
    }

    #[test]
    fn index_wiki_file_skips_when_mtime_size_match() {
        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let path = memex.wiki_dir().join("p.md");
        std::fs::write(
            &path,
            "---\ntitle: P
sources: []\ncreated_at: 2026-04-26T00:00:00Z\nupdated_at: 2026-04-26T00:00:00Z\n---\n\nbody",
        )
        .unwrap();
        let outcome1 = index_wiki_file(&memex, &path, None).unwrap();
        assert!(matches!(
            outcome1,
            IndexOutcome::Inserted | IndexOutcome::Updated
        ));
        let outcome2 = index_wiki_file(&memex, &path, None).unwrap();
        assert!(matches!(outcome2, IndexOutcome::Skipped));
    }
}
