//! Shared fixtures for the per-module `#[cfg(test)] mod tests` blocks
//! in `bm25.rs`, `commit.rs`, `collections.rs`, `fusion.rs`, and
//! `lookup.rs`. Kept in one place so adding a new test fixture lives
//! next to the existing ones and the per-module test blocks stay
//! focused on assertions.

use tempfile::TempDir;

use super::{Db, DocSpec, upsert_document};

/// A `Db` backed by a temp file. `TempDir` is returned so the caller
/// can keep it alive for the duration of the test (drop = cleanup).
pub fn open_temp_search() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join(crate::INDEX_DB_NAME);
    let search = Db::open(&db_path).unwrap();
    (dir, search)
}

/// A valid wiki page (frontmatter + body) ready to be written to disk
/// or fed through `commit_doc`.
pub fn make_page(title: &str, body: &str) -> String {
    format!(
        "---\ntitle: {title}\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\n{body}\n"
    )
}

/// An in-memory connection with the schema initialized — for tests
/// that need to drive raw SQL without spinning a temp file.
pub fn setup_db() -> rusqlite::Connection {
    crate::schema::register_sqlite_vec_once();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::schema::init_schema(&conn).unwrap();
    conn
}

/// Insert a document via `upsert_document` so callers can seed the
/// `documents` + `titles_fts` rows without composing a `DocSpec` at
/// every test site.
pub fn insert_doc(
    conn: &rusqlite::Connection,
    doc_type: &str,
    path: &str,
    title: &str,
    body: &str,
) {
    let doc = format!("---\ntitle: {title}\n---\n\n{body}\n");
    upsert_document(
        conn,
        &DocSpec {
            doc_type,
            path,
            title,
            source: None,
            body,
            mtime: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(1000),
            size: doc.len() as i64,
        },
    )
    .unwrap();
}
