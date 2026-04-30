use rusqlite::Connection;

/// Length of the short hash form (hex characters) used in display contexts.
pub const SHORT_LEN: usize = 7;

/// A resolved document reference: row id and full content hash.
#[derive(Debug, Clone)]
pub struct DocRef {
    pub id: i64,
    pub hash: String,
}

/// Errors returned by `resolve_prefix`.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no document matching `{prefix}`")]
    NotFound { prefix: String },
    #[error(
        "ambiguous prefix `{prefix}` matches {} documents: {}",
        candidates.len(),
        candidates.join(", ")
    )]
    Ambiguous {
        prefix: String,
        candidates: Vec<String>,
    },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// Return the short (first `SHORT_LEN` chars) prefix of a hash for display.
pub fn short(hash: &str) -> &str {
    let n = SHORT_LEN.min(hash.len());
    &hash[..n]
}

/// Resolve a hash prefix to a unique document via `documents.hash LIKE 'prefix%'`.
///
/// Returns:
/// - `Ok(DocRef)` on a unique match;
/// - `Err(ResolveError::NotFound)` when no row matches;
/// - `Err(ResolveError::Ambiguous)` when multiple rows match (with up to 16
///   candidate hashes for the caller to display).
pub fn resolve_prefix(conn: &Connection, prefix: &str) -> Result<DocRef, ResolveError> {
    let mut stmt =
        conn.prepare("SELECT id, hash FROM documents WHERE hash LIKE ?1 || '%' LIMIT 2")?;
    let rows: Vec<(i64, String)> = stmt
        .query_map(rusqlite::params![prefix], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    match rows.len() {
        0 => Err(ResolveError::NotFound {
            prefix: prefix.to_string(),
        }),
        1 => Ok(DocRef {
            id: rows[0].0,
            hash: rows[0].1.clone(),
        }),
        _ => {
            let mut full_stmt = conn.prepare(
                "SELECT hash FROM documents WHERE hash LIKE ?1 || '%' ORDER BY hash LIMIT 16",
            )?;
            let candidates: Vec<String> = full_stmt
                .query_map(rusqlite::params![prefix], |r| r.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            Err(ResolveError::Ambiguous {
                prefix: prefix.to_string(),
                candidates,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Build an in-memory `documents` table matching the new schema closely
    /// enough for resolve_prefix queries (id + hash + UNIQUE(doc_type,path)).
    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"CREATE TABLE documents (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                doc_type TEXT NOT NULL,
                path TEXT NOT NULL,
                title TEXT NOT NULL,
                hash TEXT NOT NULL,
                tags TEXT NOT NULL DEFAULT '',
                source TEXT,
                mtime TEXT NOT NULL,
                size INTEGER NOT NULL,
                embed_model TEXT,
                embedded_at TEXT,
                UNIQUE(doc_type, path)
            );"#,
        )
        .unwrap();
        conn
    }

    fn insert(conn: &Connection, hash: &str, path: &str) {
        conn.execute(
            "INSERT INTO documents (doc_type, path, title, hash, mtime, size) \
             VALUES ('wiki', ?1, 'T', ?2, '2026-04-26T00:00:00Z', 0)",
            rusqlite::params![path, hash],
        )
        .unwrap();
    }

    #[test]
    fn short_returns_first_seven_chars() {
        let h = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert_eq!(short(h), "abcdef0");
    }

    #[test]
    fn resolve_prefix_not_found() {
        let conn = fixture();
        let err = resolve_prefix(&conn, "deadbee").unwrap_err();
        assert!(matches!(err, ResolveError::NotFound { .. }));
    }

    #[test]
    fn resolve_prefix_unique_match() {
        let conn = fixture();
        insert(
            &conn,
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "p1",
        );
        let r = resolve_prefix(&conn, "abcdef").unwrap();
        assert!(r.hash.starts_with("abcdef"));
    }

    #[test]
    fn resolve_prefix_ambiguous_returns_candidates() {
        let conn = fixture();
        insert(
            &conn,
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "p1",
        );
        insert(
            &conn,
            "abcdef9999999999abcdef0123456789abcdef0123456789abcdef0123456789",
            "p2",
        );
        let err = resolve_prefix(&conn, "abcdef").unwrap_err();
        match err {
            ResolveError::Ambiguous { prefix, candidates } => {
                assert_eq!(prefix, "abcdef");
                assert_eq!(candidates.len(), 2);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn resolve_prefix_full_64_char_hash_is_unambiguous() {
        let conn = fixture();
        let full = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        insert(&conn, full, "p1");
        let r = resolve_prefix(&conn, full).unwrap();
        assert_eq!(r.hash, full);
    }
}
