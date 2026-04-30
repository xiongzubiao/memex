//! LLM result cache: dedupe identical (model, task, system, user) tuples.
//! Keyed by sha256 of the four inputs concatenated with NUL separators.
//! Stored in the `llm_cache` table (schema in `core/src/schema.rs`).

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::error::Result;

/// Build the cache key for a (model, task, system, user) tuple. Stable hex.
pub fn cache_key(model: &str, task: &str, system_prompt: &str, user_prompt: &str) -> String {
    let mut h = Sha256::new();
    h.update(model.as_bytes());
    h.update(b"\0");
    h.update(task.as_bytes());
    h.update(b"\0");
    h.update(system_prompt.as_bytes());
    h.update(b"\0");
    h.update(user_prompt.as_bytes());
    format!("{:x}", h.finalize())
}

/// Look up a cached result by key. Returns Ok(None) when the key isn't present.
pub fn lookup_cache(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT result FROM llm_cache WHERE hash=?1",
            rusqlite::params![key],
            |r| r.get::<_, String>(0),
        )
        .ok())
}

/// Insert a result. Uses INSERT OR IGNORE so a concurrent inserter doesn't
/// blow away an earlier value (first writer wins).
pub fn insert_cache(conn: &Connection, key: &str, result: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO llm_cache (hash, result, created_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![key, result, now],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fresh_conn() -> Connection {
        crate::schema::register_sqlite_vec_once();
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn cache_key_changes_with_model_or_prompt() {
        let k1 = cache_key("modelA", "expand", "sys", "user");
        let k2 = cache_key("modelB", "expand", "sys", "user");
        let k3 = cache_key("modelA", "synth", "sys", "user");
        let k4 = cache_key("modelA", "expand", "sys2", "user");
        let k5 = cache_key("modelA", "expand", "sys", "user2");
        assert_ne!(k1, k2);
        assert_ne!(k1, k3);
        assert_ne!(k1, k4);
        assert_ne!(k1, k5);
    }

    #[test]
    fn lookup_returns_cached_then_insert_round_trips() {
        let conn = fresh_conn();
        let key = cache_key("m", "t", "s", "u");
        assert!(lookup_cache(&conn, &key).unwrap().is_none());
        insert_cache(&conn, &key, "hello").unwrap();
        assert_eq!(lookup_cache(&conn, &key).unwrap().as_deref(), Some("hello"));
    }

    #[test]
    fn duplicate_insert_is_no_op() {
        let conn = fresh_conn();
        let key = cache_key("m", "t", "s", "u");
        insert_cache(&conn, &key, "first").unwrap();
        insert_cache(&conn, &key, "second").unwrap();
        // INSERT OR IGNORE: original value preserved
        assert_eq!(lookup_cache(&conn, &key).unwrap().as_deref(), Some("first"));
    }
}
