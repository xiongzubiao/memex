use crate::embed::EMBEDDING_DIM;
use crate::error::Result;
use rusqlite::Connection;

/// A single chunk-level vector search result.
///
/// Body text is no longer stored in `chunks`; callers slice the doc body
/// from disk using `(pos, len)` to materialize the snippet.
pub struct VectorResult {
    pub hash: String,
    pub seq: i32,
    pub pos: usize,
    pub len: usize,
    /// Cosine similarity in [0, 1]: 1.0 = identical direction, 0.0 = orthogonal.
    /// Converted from sqlite-vec's cosine distance via `1 - distance`.
    pub score: f32,
}

/// Store a chunk: position metadata in `chunks`, embedding in the sqlite-vec
/// `chunks_vec` virtual table, and chunk text in `chunks_fts` for chunk-level
/// BM25. The three rows are keyed by (hash, seq).
pub fn store_chunk(
    conn: &Connection,
    hash: &str,
    seq: i32,
    pos: usize,
    len: usize,
    chunk_text: &str,
    embedding: &[f32],
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO chunks (hash, seq, pos, len) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![hash, seq, pos as i64, len as i64],
    )?;

    let hash_seq = format!("{hash}_{seq}");
    let blob = embedding_to_blob(embedding);
    // sqlite-vec's vec0 virtual tables silently ignore `INSERT OR REPLACE`'s
    // conflict clause — a duplicate hash_seq leaves the old vector in
    // place. Use DELETE-then-INSERT so re-embed paths actually update.
    // (QMD's store.ts:3247-3251 documents this gotcha.)
    conn.execute(
        "DELETE FROM chunks_vec WHERE hash_seq = ?1",
        rusqlite::params![&hash_seq],
    )?;
    conn.execute(
        "INSERT INTO chunks_vec (hash_seq, embedding) VALUES (?1, ?2)",
        rusqlite::params![hash_seq, blob],
    )?;

    // chunks_fts: contentless FTS5, no INSERT OR REPLACE. Delete-then-insert
    // by (hash, seq) so re-embed/re-ingest replaces the indexed chunk text.
    conn.execute(
        "DELETE FROM chunks_fts WHERE hash = ?1 AND seq = ?2",
        rusqlite::params![hash, seq],
    )?;
    conn.execute(
        "INSERT INTO chunks_fts(chunk_text, hash, seq) VALUES (?1, ?2, ?3)",
        rusqlite::params![chunk_text, hash, seq],
    )?;
    Ok(())
}

/// Vector search: top-k chunks by cosine similarity to `query_embedding`.
///
/// Uses sqlite-vec's `vec0` index. When `doc_type` is Some, over-fetches
/// `limit * 3` raw matches (QMD's pattern) so the doc_type filter has
/// slack before the final truncation.
///
/// Returns results sorted by similarity descending.
pub fn vector_search(
    conn: &Connection,
    query_embedding: &[f32],
    limit: usize,
    doc_type: &str,
) -> Result<Vec<VectorResult>> {
    let q_blob = embedding_to_blob(query_embedding);
    // Over-fetch 3x so the doc_type filter has slack before truncation.
    let k = limit * 3;

    // Step 1: pull top-k hash_seq + distance from the vec0 index.
    // sqlite-vec's virtual table doesn't tolerate JOINs in the MATCH query
    // (see: github.com/tobi/qmd/pull/23), so we split into two steps.
    let mut stmt = conn.prepare(
        "SELECT hash_seq, distance FROM chunks_vec
         WHERE embedding MATCH ?1 AND k = ?2",
    )?;
    let raw: Vec<(String, f64)> = stmt
        .query_map(rusqlite::params![q_blob, k as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    if raw.is_empty() {
        return Ok(Vec::new());
    }

    // Step 2: look up chunk position metadata and apply doc_type filter.
    let distance_by_key: std::collections::HashMap<String, f64> = raw.iter().cloned().collect();
    let placeholders: String = std::iter::repeat_n("?", raw.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT c.hash, c.seq, c.pos, c.len
         FROM chunks c JOIN documents d ON d.hash = c.hash
         WHERE c.hash || '_' || c.seq IN ({placeholders}) AND d.doc_type = ?"
    );
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = raw
        .iter()
        .map(|(hs, _)| Box::new(hs.clone()) as Box<dyn rusqlite::ToSql>)
        .collect();
    params_vec.push(Box::new(doc_type.to_string()));
    let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();

    let mut stmt2 = conn.prepare(&sql)?;
    let rows: Vec<(String, i32, i64, i64)> = stmt2
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i32>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Re-attach the distance from step 1 and convert to similarity score.
    let mut results: Vec<VectorResult> = rows
        .into_iter()
        .filter_map(|(hash, seq, pos, len)| {
            let key = format!("{hash}_{seq}");
            distance_by_key.get(&key).map(|dist| VectorResult {
                hash,
                seq,
                pos: pos as usize,
                len: len as usize,
                score: (1.0 - dist) as f32,
            })
        })
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(limit);
    Ok(results)
}

/// Collapse chunk-level results to document-level: for each document
/// (content hash), keep the chunk with the highest similarity.
/// `doc_type` filters at the chunk-query level (see `vector_search`).
pub fn vector_search_collapsed(
    conn: &Connection,
    query_embedding: &[f32],
    limit: usize,
    doc_type: &str,
) -> Result<Vec<VectorResult>> {
    // Over-fetch chunk-level results so that collapsing to doc-level still
    // yields `limit` distinct docs. Worst case: all top-K chunks belong to
    // the same doc. `limit * 5` is a practical cushion without going O(n).
    let all = vector_search(conn, query_embedding, limit * 5, doc_type)?;

    let mut best: std::collections::HashMap<String, VectorResult> =
        std::collections::HashMap::new();
    for result in all {
        let entry = best.entry(result.hash.clone());
        match entry {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(result);
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if result.score > e.get().score {
                    e.insert(result);
                }
            }
        }
    }

    let mut collapsed: Vec<VectorResult> = best.into_values().collect();
    collapsed.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    collapsed.truncate(limit);
    Ok(collapsed)
}

/// List `(pos, len)` for all chunks of a given content hash, in seq order.
/// Used by the BM25 snippet backfill path to score and pick the best
/// chunk for a query+intent before rendering the snippet.
/// Returns empty when the doc was indexed without embedding (no chunks).
pub fn list_chunks_by_hash(conn: &Connection, hash: &str) -> Result<Vec<(usize, usize)>> {
    let mut stmt = conn.prepare("SELECT pos, len FROM chunks WHERE hash = ?1 ORDER BY seq")?;
    let rows = stmt
        .query_map([hash], |row| {
            Ok((row.get::<_, i64>(0)? as usize, row.get::<_, i64>(1)? as usize))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Delete all chunks for a given content hash from the chunks table, the
/// sqlite-vec index, and chunks_fts.
pub fn delete_chunks(conn: &Connection, hash: &str) -> Result<()> {
    // Fetch affected seqs first so we can delete matching chunks_vec rows.
    let seqs: Vec<i32> = {
        let mut stmt = conn.prepare("SELECT seq FROM chunks WHERE hash = ?1")?;
        stmt.query_map([hash], |row| row.get::<_, i32>(0))?
            .filter_map(|r| r.ok())
            .collect()
    };
    for seq in seqs {
        let hash_seq = format!("{hash}_{seq}");
        let _ = conn.execute("DELETE FROM chunks_vec WHERE hash_seq = ?1", [hash_seq]);
    }
    conn.execute("DELETE FROM chunks_fts WHERE hash = ?1", [hash])?;
    conn.execute("DELETE FROM chunks WHERE hash = ?1", [hash])?;
    Ok(())
}

/// Serialize an f32 slice to a little-endian byte blob. sqlite-vec accepts
/// this as the input format for `float[N]` vectors.
fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(embedding.len() * 4);
    for &val in embedding {
        blob.extend_from_slice(&val.to_le_bytes());
    }
    // Pad (or trim) to exactly EMBEDDING_DIM floats so sqlite-vec's
    // `float[768]` column accepts the blob.
    blob.resize(EMBEDDING_DIM * 4, 0);
    blob
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> Connection {
        crate::schema::register_sqlite_vec_once();
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        conn
    }

    /// Insert a minimal `wiki` documents row keyed by `hash` so the
    /// `vector_search` JOIN finds it. Test helper — production code
    /// goes through `commit_doc`.
    fn insert_wiki_doc(conn: &Connection, hash: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO documents (doc_type, path, title, hash, source, mtime, size, embed_model, embedded_at)
             VALUES ('wiki', ?1, '', ?2, NULL, '', 0, NULL, NULL)",
            rusqlite::params![format!("wiki/{hash}.md"), hash],
        )
        .unwrap();
    }

    fn fake_embedding(val: f32) -> Vec<f32> {
        vec![val; EMBEDDING_DIM]
    }

    /// Build a 768-dim embedding with value `a` in dim 0 and `b` in dim 1.
    fn dir_embedding(a: f32, b: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; EMBEDDING_DIM];
        v[0] = a;
        v[1] = b;
        v
    }

    #[test]
    fn store_and_search_vectors() {
        let conn = setup_db();
        insert_wiki_doc(&conn, "hash1");
        store_chunk(&conn, "hash1", 0, 0, 10, "", &fake_embedding(0.5)).unwrap();
        let results = vector_search(&conn, &fake_embedding(0.5), 10, "wiki").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].hash, "hash1");
        assert!(results[0].score > 0.99); // identical direction
    }

    #[test]
    fn chunk_to_doc_collapse() {
        let conn = setup_db();
        insert_wiki_doc(&conn, "hash1");
        insert_wiki_doc(&conn, "hash2");

        let query = dir_embedding(1.0, 0.0);

        let emb_a = dir_embedding(0.95, 0.05);
        store_chunk(&conn, "hash1", 0, 0, 7, "", &emb_a).unwrap();

        let emb_b = dir_embedding(0.9, 0.1);
        store_chunk(&conn, "hash1", 1, 0, 7, "", &emb_b).unwrap();

        let emb_c = dir_embedding(0.5, 0.5);
        store_chunk(&conn, "hash2", 0, 0, 7, "", &emb_c).unwrap();

        let results = vector_search_collapsed(&conn, &query, 10, "wiki").unwrap();
        // One row per hash; hash1 should win with its better chunk.
        let hashes: Vec<_> = results.iter().map(|r| r.hash.clone()).collect();
        assert!(hashes.contains(&"hash1".to_string()));
        assert!(hashes.contains(&"hash2".to_string()));
        assert_eq!(results[0].hash, "hash1");
    }

    #[test]
    fn store_chunk_inserts_pos_len_and_vec() {
        let conn = setup_db();
        store_chunk(&conn, "hash1", 0, 42, 100, "", &fake_embedding(0.5)).unwrap();
        let (pos, len): (i64, i64) = conn
            .query_row(
                "SELECT pos, len FROM chunks WHERE hash='hash1' AND seq=0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(pos, 42);
        assert_eq!(len, 100);
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chunks_vec WHERE hash_seq='hash1_0'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
}
