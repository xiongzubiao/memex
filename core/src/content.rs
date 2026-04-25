use crate::error::Result;
use crate::storage::content_hash;
use chrono::Utc;
use rusqlite::Connection;
use std::collections::HashMap;

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

/// Fetch bodies for many content hashes in one round-trip. Returns a map
/// from hash → body. Hashes with no matching content row are absent from
/// the map; the caller decides how to handle the miss (treat as empty,
/// warn, etc.). Issues a single `WHERE hash IN (...)` query and binds each
/// hash as a parameter.
pub fn get_content_batch(conn: &Connection, hashes: &[&str]) -> Result<HashMap<String, String>> {
    if hashes.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders: String = std::iter::repeat("?")
        .take(hashes.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT hash, doc FROM content WHERE hash IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> =
        hashes.iter().map(|h| h as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = HashMap::with_capacity(hashes.len());
    for r in rows {
        if let Ok((h, d)) = r {
            out.insert(h, d);
        }
    }
    Ok(out)
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
        crate::schema::register_sqlite_vec_once();
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

    #[test]
    fn get_content_batch_returns_all_present() {
        let conn = setup_db();
        let h1 = insert_content(&conn, "alpha").unwrap();
        let h2 = insert_content(&conn, "beta").unwrap();
        let h3 = insert_content(&conn, "gamma").unwrap();
        let map =
            get_content_batch(&conn, &[h1.as_str(), h2.as_str(), h3.as_str()]).unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(map.get(&h1).unwrap(), "alpha");
        assert_eq!(map.get(&h2).unwrap(), "beta");
        assert_eq!(map.get(&h3).unwrap(), "gamma");
    }

    #[test]
    fn get_content_batch_skips_missing() {
        let conn = setup_db();
        let h = insert_content(&conn, "present").unwrap();
        let map = get_content_batch(&conn, &[h.as_str(), "nonexistent-hash"]).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&h).unwrap(), "present");
        assert!(map.get("nonexistent-hash").is_none());
    }

    #[test]
    fn get_content_batch_empty_input() {
        let conn = setup_db();
        let map = get_content_batch(&conn, &[]).unwrap();
        assert!(map.is_empty());
    }
}
