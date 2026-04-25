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

fn register_strip_frontmatter(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        "strip_frontmatter",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let doc: String = ctx.get(0)?;
            let trimmed = doc.trim();
            if !trimmed.starts_with("---") {
                return Ok(doc);
            }
            let after_first = &trimmed[3..];
            match after_first.find("---") {
                Some(end) => Ok(after_first[end + 3..].trim().to_string()),
                None => Ok(doc),
            }
        },
    )?;
    Ok(())
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS content (
    hash       TEXT PRIMARY KEY,
    doc        TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS documents (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    doc_type    TEXT NOT NULL,
    path        TEXT NOT NULL,
    title       TEXT NOT NULL,
    hash        TEXT NOT NULL REFERENCES content(hash),
    docid       TEXT NOT NULL,
    tags        TEXT NOT NULL DEFAULT '',
    summary     TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    UNIQUE(doc_type, path)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_documents_docid ON documents(docid) WHERE docid != '';

CREATE TABLE IF NOT EXISTS collections (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    name    TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS document_collections (
    document_id     INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    collection_id   INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    PRIMARY KEY(document_id, collection_id)
);

CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
    path, title, tags, body,
    content='',
    tokenize='porter unicode61'
);

CREATE TRIGGER IF NOT EXISTS documents_ai AFTER INSERT ON documents BEGIN
    INSERT INTO documents_fts(rowid, path, title, tags, body)
    VALUES (
        new.id, new.path, new.title, new.tags,
        (SELECT strip_frontmatter(doc) FROM content WHERE hash = new.hash)
    );
END;

CREATE TRIGGER IF NOT EXISTS documents_ad AFTER DELETE ON documents BEGIN
    INSERT INTO documents_fts(documents_fts, rowid, path, title, tags, body)
    VALUES ('delete', old.id, old.path, old.title, old.tags,
        (SELECT strip_frontmatter(doc) FROM content WHERE hash = old.hash)
    );
END;

CREATE TRIGGER IF NOT EXISTS documents_au AFTER UPDATE ON documents BEGIN
    INSERT INTO documents_fts(documents_fts, rowid, path, title, tags, body)
    VALUES ('delete', old.id, old.path, old.title, old.tags,
        (SELECT strip_frontmatter(doc) FROM content WHERE hash = old.hash)
    );
    INSERT INTO documents_fts(rowid, path, title, tags, body)
    VALUES (
        new.id, new.path, new.title, new.tags,
        (SELECT strip_frontmatter(doc) FROM content WHERE hash = new.hash)
    );
END;

CREATE TABLE IF NOT EXISTS chunks (
    hash        TEXT NOT NULL REFERENCES content(hash),
    seq         INTEGER NOT NULL,
    chunk_text  TEXT NOT NULL,
    pos         INTEGER NOT NULL,
    len         INTEGER NOT NULL,
    model       TEXT NOT NULL,
    embedded_at TEXT NOT NULL,
    PRIMARY KEY(hash, seq)
);

-- sqlite-vec virtual table for similarity search. Keyed by
-- "{hash}_{seq}" so each chunk has a stable row ID that joins back to
-- `chunks` for snippet text and back to `documents` via `chunks.hash`.
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(
    hash_seq TEXT PRIMARY KEY,
    embedding float[768] distance=cosine
);

CREATE TABLE IF NOT EXISTS ingest_jobs (
    job_id          TEXT PRIMARY KEY,
    transcript_path TEXT NOT NULL,
    content_hash    TEXT NOT NULL,
    agent           TEXT NOT NULL,
    memex_root      TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending',
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    error           TEXT
);
"#;

pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    register_strip_frontmatter(conn)?;
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn strip_frontmatter_removes_yaml() {
        register_sqlite_vec_once();
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let result: String = conn
            .query_row(
                "SELECT strip_frontmatter('---\ntitle: Test\n---\nBody here')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(result, "Body here");
    }

    #[test]
    fn strip_frontmatter_no_frontmatter_passthrough() {
        register_sqlite_vec_once();
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let result: String = conn
            .query_row("SELECT strip_frontmatter('No frontmatter here')", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(result, "No frontmatter here");
    }
}
