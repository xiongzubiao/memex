use crate::error::Result;
use crate::storage::content_hash;
use chrono::Utc;
use rusqlite::Connection;

/// Insert content into the content-addressable store. Returns the SHA-256 hash.
/// Deduplicates: identical content returns the same hash without inserting.
pub fn insert_content(conn: &Connection, doc: &str) -> Result<String> {
    let hash = content_hash(doc.as_bytes());
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO content (hash, doc, created_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![hash, doc, now],
    )?;
    Ok(hash)
}

/// Get document text by content hash.
pub fn get_content(conn: &Connection, hash: &str) -> Result<String> {
    let doc: String = conn.query_row("SELECT doc FROM content WHERE hash = ?1", [hash], |row| {
        row.get(0)
    })?;
    Ok(doc)
}

/// Delete orphaned content rows (not referenced by any document).
/// Also removes associated chunks from the vector store.
pub fn cleanup_orphaned_content(conn: &Connection, old_hash: &str) -> Result<()> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM documents WHERE hash = ?1",
        [old_hash],
        |row| row.get(0),
    )?;
    if count == 0 {
        crate::vector::delete_chunks(conn, old_hash)?;
        conn.execute("DELETE FROM content WHERE hash = ?1", [old_hash])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn insert_content_deduplicates() {
        let conn = setup_db();
        let hash1 = insert_content(&conn, "hello world").unwrap();
        let hash2 = insert_content(&conn, "hello world").unwrap();
        assert_eq!(hash1, hash2);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM content", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn different_content_different_hash() {
        let conn = setup_db();
        let h1 = insert_content(&conn, "hello").unwrap();
        let h2 = insert_content(&conn, "world").unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn get_content_by_hash() {
        let conn = setup_db();
        let hash = insert_content(&conn, "test content").unwrap();
        let doc = get_content(&conn, &hash).unwrap();
        assert_eq!(doc, "test content");
    }
}
