//! Collection-membership management. Every doc lives in at least one
//! collection (defaulting to `default`); search/listing filter by
//! membership. The free `normalize_collections` is the canonical input
//! cleaner — used both at write time (membership inserts) and read time
//! (search filters) so the two paths agree on what "no filter" means.

use crate::error::{MemexError, Result};

use super::{Db, mutex_err, sqlite_err};

/// Normalize collection names for storage.
///
/// Rules:
/// - trim whitespace
/// - lowercase
/// - remove empty names
/// - sort and dedup
/// - default to `["default"]` when nothing remains
pub fn normalize_collections(names: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = names
        .iter()
        .map(|name| name.trim().to_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    normalized.sort();
    normalized.dedup();
    if normalized.is_empty() {
        vec!["default".to_string()]
    } else {
        normalized
    }
}

/// Reject collection names that would render badly in `source list`
/// output, break shell composition, or smuggle structure into CSV/JSON
/// serialization paths. Empty names are filtered (defaulted to
/// "default") rather than rejected, matching `normalize_collections`.
/// Returns the offending name so callers can include it in a
/// user-facing error.
pub fn validate_collection_names(names: &[String]) -> std::result::Result<(), String> {
    for name in names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.chars().any(|c| c.is_control()) {
            return Err(format!(
                "collection name {trimmed:?} contains control characters (newline, tab, etc.)"
            ));
        }
        if trimmed.len() > 64 {
            return Err(format!("collection name {trimmed:?} exceeds 64 chars"));
        }
    }
    Ok(())
}

pub(super) fn document_id_by_path(
    conn: &rusqlite::Connection,
    doc_type: &str,
    path: &str,
) -> Result<i64> {
    conn.query_row(
        "SELECT id FROM documents WHERE doc_type = ?1 AND path = ?2",
        rusqlite::params![doc_type, path],
        |row| row.get(0),
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            MemexError::NotFound(format!("document not found: {doc_type}:{path}"))
        }
        other => sqlite_err(other),
    })
}

pub(super) fn ensure_default_document_collection(
    conn: &rusqlite::Connection,
    document_id: i64,
) -> Result<()> {
    let has_membership: i64 = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM document_collections WHERE document_id = ?1)",
            rusqlite::params![document_id],
            |row| row.get(0),
        )
        .map_err(sqlite_err)?;
    if has_membership != 0 {
        return Ok(());
    }

    conn.execute(
        "INSERT OR IGNORE INTO collections (name) VALUES ('default')",
        [],
    )
    .map_err(sqlite_err)?;
    conn.execute(
        "INSERT INTO document_collections (document_id, collection_id) \
         SELECT ?1, id FROM collections WHERE name = 'default'",
        rusqlite::params![document_id],
    )
    .map_err(sqlite_err)?;
    Ok(())
}

pub(super) fn set_document_collections_in_conn(
    conn: &rusqlite::Connection,
    doc_type: &str,
    path: &str,
    incoming: &[String],
) -> Result<()> {
    let document_id = document_id_by_path(conn, doc_type, path)?;
    let names = normalize_collections(incoming);

    conn.execute(
        "DELETE FROM document_collections WHERE document_id = ?1",
        rusqlite::params![document_id],
    )
    .map_err(sqlite_err)?;

    for name in names {
        conn.execute(
            "INSERT OR IGNORE INTO collections (name) VALUES (?1)",
            rusqlite::params![&name],
        )
        .map_err(sqlite_err)?;
        let collection_id: i64 = conn
            .query_row(
                "SELECT id FROM collections WHERE name = ?1",
                rusqlite::params![&name],
                |row| row.get(0),
            )
            .map_err(sqlite_err)?;
        conn.execute(
            "INSERT OR IGNORE INTO document_collections (document_id, collection_id) VALUES (?1, ?2)",
            rusqlite::params![document_id, collection_id],
        )
        .map_err(sqlite_err)?;
    }

    Ok(())
}

impl Db {
    /// Get the collection names attached to a document by doc_type and path.
    pub fn document_collections_by_path(&self, doc_type: &str, path: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let document_id = document_id_by_path(&conn, doc_type, path)?;
        let mut stmt = conn
            .prepare(
                "SELECT c.name \
                 FROM document_collections dc \
                 JOIN collections c ON c.id = dc.collection_id \
                 WHERE dc.document_id = ?1 \
                 ORDER BY c.name",
            )
            .map_err(sqlite_err)?;
        let rows = stmt
            .query_map(rusqlite::params![document_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sqlite_err)?;
        let mut names = Vec::new();
        for row in rows {
            names.push(row.map_err(sqlite_err)?);
        }
        if names.is_empty() {
            Ok(vec!["default".to_string()])
        } else {
            Ok(names)
        }
    }

    /// Replace all collection memberships for a document.
    pub fn set_document_collections_by_path(
        &self,
        doc_type: &str,
        path: &str,
        names: &[String],
    ) -> Result<()> {
        let names = normalize_collections(names);
        let mut conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let tx = conn.transaction().map_err(sqlite_err)?;
        let document_id = document_id_by_path(&tx, doc_type, path)?;

        tx.execute(
            "DELETE FROM document_collections WHERE document_id = ?1",
            rusqlite::params![document_id],
        )
        .map_err(sqlite_err)?;

        for name in names {
            tx.execute(
                "INSERT OR IGNORE INTO collections (name) VALUES (?1)",
                rusqlite::params![&name],
            )
            .map_err(sqlite_err)?;
            let collection_id: i64 = tx
                .query_row(
                    "SELECT id FROM collections WHERE name = ?1",
                    rusqlite::params![&name],
                    |row| row.get(0),
                )
                .map_err(sqlite_err)?;
            tx.execute(
                "INSERT OR IGNORE INTO document_collections (document_id, collection_id) VALUES (?1, ?2)",
                rusqlite::params![document_id, collection_id],
            )
            .map_err(sqlite_err)?;
        }

        tx.commit().map_err(sqlite_err)?;
        Ok(())
    }

    /// Merge incoming collection names with the current memberships.
    pub fn union_document_collections_by_path(
        &self,
        doc_type: &str,
        path: &str,
        incoming: &[String],
    ) -> Result<()> {
        let mut conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let tx = conn.transaction().map_err(sqlite_err)?;
        let document_id = document_id_by_path(&tx, doc_type, path)?;

        let mut current = {
            let mut stmt = tx
                .prepare(
                    "SELECT c.name \
                     FROM document_collections dc \
                     JOIN collections c ON c.id = dc.collection_id \
                     WHERE dc.document_id = ?1 \
                     ORDER BY c.name",
                )
                .map_err(sqlite_err)?;
            let rows = stmt
                .query_map(rusqlite::params![document_id], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(sqlite_err)?;
            let mut names = Vec::new();
            for row in rows {
                names.push(row.map_err(sqlite_err)?);
            }
            if names.is_empty() {
                vec!["default".to_string()]
            } else {
                names
            }
        };

        current.extend(incoming.iter().cloned());
        let names = normalize_collections(&current);

        tx.execute(
            "DELETE FROM document_collections WHERE document_id = ?1",
            rusqlite::params![document_id],
        )
        .map_err(sqlite_err)?;

        for name in names {
            tx.execute(
                "INSERT OR IGNORE INTO collections (name) VALUES (?1)",
                rusqlite::params![&name],
            )
            .map_err(sqlite_err)?;
            let collection_id: i64 = tx
                .query_row(
                    "SELECT id FROM collections WHERE name = ?1",
                    rusqlite::params![&name],
                    |row| row.get(0),
                )
                .map_err(sqlite_err)?;
            tx.execute(
                "INSERT OR IGNORE INTO document_collections (document_id, collection_id) VALUES (?1, ?2)",
                rusqlite::params![document_id, collection_id],
            )
            .map_err(sqlite_err)?;
        }

        tx.commit().map_err(sqlite_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::search::test_helpers::{insert_doc, open_temp_search, setup_db};

    #[test]
    fn document_collections_default_fallback_for_uninitialized_document() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(Path::new("wiki/alpha.md"), "Alpha", "Body.", 1000)
            .unwrap();

        assert_eq!(
            search
                .document_collections_by_path("wiki", "wiki/alpha.md")
                .unwrap(),
            vec!["default".to_string()]
        );
    }

    #[test]
    fn document_collections_union_normalizes_and_keeps_default() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(Path::new("wiki/alpha.md"), "Alpha", "Body.", 1000)
            .unwrap();

        search
            .union_document_collections_by_path(
                "wiki",
                "wiki/alpha.md",
                &["Team".to_string(), "DEFAULT".to_string(), "".to_string()],
            )
            .unwrap();
        assert_eq!(
            search
                .document_collections_by_path("wiki", "wiki/alpha.md")
                .unwrap(),
            vec!["default".to_string(), "team".to_string()]
        );

        assert_eq!(
            normalize_collections(&[
                "  Team ".to_string(),
                "default".to_string(),
                "".to_string(),
                "TEAM".to_string()
            ]),
            vec!["default".to_string(), "team".to_string()]
        );

        search
            .set_document_collections_by_path(
                "wiki",
                "wiki/alpha.md",
                &[
                    "Research".to_string(),
                    "team".to_string(),
                    "Team".to_string(),
                ],
            )
            .unwrap();
        assert_eq!(
            search
                .document_collections_by_path("wiki", "wiki/alpha.md")
                .unwrap(),
            vec!["research".to_string(), "team".to_string()]
        );
    }

    #[test]
    fn document_collections_cascade_delete_membership_rows() {
        let conn = setup_db();

        // upsert_document (used by insert_doc) already attaches the
        // 'default' membership, so we just verify cascade delete.
        insert_doc(&conn, "wiki", "wiki/alpha.md", "Alpha", "Body.");
        let pre: i64 = conn
            .query_row("SELECT COUNT(*) FROM document_collections", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(pre >= 1);

        conn.execute(
            "DELETE FROM documents WHERE doc_type = ?1 AND path = ?2",
            rusqlite::params!["wiki", "wiki/alpha.md"],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM document_collections", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }
}
