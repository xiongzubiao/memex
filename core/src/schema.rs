use crate::error::Result;
use rusqlite::Connection;
use std::sync::Once;

/// Register the sqlite-vec extension as a SQLite auto-extension. Idempotent
/// via `Once`: the FFI function must be registered exactly once per process,
/// and **before any connection is opened** — `sqlite3_auto_extension` only
/// affects connections opened *after* the call. Every opener in this crate
/// calls this before `Connection::open`.
pub fn register_sqlite_vec_once() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: `sqlite3_vec_init` is the extension's C entry point.
        // `sqlite3_auto_extension` takes a function pointer and stores it
        // in a global list consulted by every subsequent SQLite connection.
        unsafe {
            type EntryFn = unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut i8,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> i32;
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<*const (), EntryFn>(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS documents (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    doc_type     TEXT NOT NULL CHECK (doc_type IN ('wiki', 'raw')),
    path         TEXT NOT NULL,
    title        TEXT NOT NULL,
    hash         TEXT NOT NULL,
    source       TEXT,
    mtime        INTEGER NOT NULL,
    size         INTEGER NOT NULL,
    embed_model  TEXT,
    embedded_at  TEXT,
    UNIQUE(doc_type, path)
);
CREATE INDEX IF NOT EXISTS idx_documents_hash ON documents(hash);
CREATE INDEX IF NOT EXISTS idx_documents_path ON documents(path);

CREATE TABLE IF NOT EXISTS collections (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    name    TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS document_collections (
    document_id     INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    collection_id   INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    PRIMARY KEY(document_id, collection_id)
);

CREATE TABLE IF NOT EXISTS chunks (
    hash    TEXT NOT NULL,
    seq     INTEGER NOT NULL,
    pos     INTEGER NOT NULL,
    len     INTEGER NOT NULL,
    PRIMARY KEY(hash, seq)
);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(
    hash_seq TEXT PRIMARY KEY,
    embedding float[768] distance=cosine
);

-- Title-only FTS for `memex search <title>` lookups. The wider
-- `documents_fts(path, title, tags, body)` was retired when retrieval
-- moved to `chunks_fts`; only title lookups still need a doc-level
-- FTS index, and they only consult the `title` column.
CREATE VIRTUAL TABLE IF NOT EXISTS titles_fts USING fts5(
    title,
    content='',
    tokenize='porter unicode61'
);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    chunk_text,
    hash UNINDEXED,
    seq UNINDEXED,
    tokenize='porter unicode61'
);

CREATE TABLE IF NOT EXISTS llm_cache (
    hash        TEXT PRIMARY KEY,
    result      TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ingest_jobs (
    job_id        TEXT PRIMARY KEY,
    job_type      TEXT NOT NULL CHECK (job_type IN ('transcript', 'document')),
    source_path   TEXT NOT NULL,
    agent         TEXT,
    content_hash  TEXT NOT NULL,
    collections   TEXT NOT NULL DEFAULT '[]',
    status        TEXT NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending', 'processing', 'completed', 'failed')),
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    error         TEXT
);
"#;

pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_with_schema() -> Connection {
        register_sqlite_vec_once();
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn columns_of(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    #[test]
    fn documents_has_expected_columns() {
        let conn = open_with_schema();
        let cols = columns_of(&conn, "documents");
        for expected in [
            "id",
            "doc_type",
            "path",
            "title",
            "hash",
            "source",
            "mtime",
            "size",
            "embed_model",
            "embedded_at",
        ] {
            assert!(
                cols.iter().any(|c| c == expected),
                "expected `{expected}` column in documents, got: {cols:?}"
            );
        }
        for absent in ["docid", "summary", "active", "created_at", "tags"] {
            assert!(
                !cols.iter().any(|c| c == absent),
                "did not expect `{absent}` column in documents, got: {cols:?}"
            );
        }
    }

    #[test]
    fn titles_fts_replaces_documents_fts() {
        let conn = open_with_schema();
        let titles_fts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='titles_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(titles_fts, 1, "expected titles_fts virtual table");
        let documents_fts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='documents_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(documents_fts, 0, "documents_fts should be retired");
    }

    #[test]
    fn chunks_has_no_text_or_model_columns() {
        let conn = open_with_schema();
        let cols = columns_of(&conn, "chunks");
        for required in ["hash", "seq", "pos", "len"] {
            assert!(
                cols.iter().any(|c| c == required),
                "expected `{required}` column in chunks, got: {cols:?}"
            );
        }
        for absent in ["chunk_text", "model", "embedded_at", "algo_version"] {
            assert!(
                !cols.iter().any(|c| c == absent),
                "did not expect `{absent}` column in chunks, got: {cols:?}"
            );
        }
    }

    #[test]
    fn llm_cache_table_exists() {
        let conn = open_with_schema();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='llm_cache'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "expected llm_cache table to exist");
    }

    #[test]
    fn content_table_does_not_exist() {
        let conn = open_with_schema();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='content'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "did not expect a `content` table");
    }

    #[test]
    fn ingest_jobs_has_no_memex_root_column() {
        let conn = open_with_schema();
        let cols = columns_of(&conn, "ingest_jobs");
        assert!(
            !cols.iter().any(|c| c == "memex_root"),
            "did not expect `memex_root` column in ingest_jobs, got: {cols:?}"
        );
    }
}
