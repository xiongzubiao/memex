use crate::error::Result;
use rusqlite::Connection;

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
    collection  TEXT NOT NULL,
    path        TEXT NOT NULL,
    title       TEXT NOT NULL,
    hash        TEXT NOT NULL REFERENCES content(hash),
    docid       TEXT NOT NULL,
    tags        TEXT NOT NULL DEFAULT '',
    summary     TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    UNIQUE(collection, path)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_documents_docid ON documents(docid) WHERE docid != '';

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
    embedding   BLOB,
    PRIMARY KEY(hash, seq)
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
