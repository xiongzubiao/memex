use crate::embed::{EMBEDDING_DIM, cosine_similarity};
use crate::error::Result;
use chrono::Utc;
use rusqlite::Connection;

/// A single chunk-level vector search result.
pub struct VectorResult {
    pub hash: String,
    pub seq: i32,
    pub chunk_text: String,
    pub score: f32, // cosine similarity (higher = more similar)
}

/// Store a chunk with its embedding.
#[allow(clippy::too_many_arguments)]
pub fn store_chunk(
    conn: &Connection,
    hash: &str,
    seq: i32,
    text: &str,
    pos: usize,
    len: usize,
    model: &str,
    embedding: &[f32],
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let blob = embedding_to_blob(embedding);
    conn.execute(
        "INSERT OR REPLACE INTO chunks (hash, seq, chunk_text, pos, len, model, embedded_at, embedding)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![hash, seq, text, pos as i64, len as i64, model, now, blob],
    )?;
    Ok(())
}

/// Vector search: find chunks most similar to the query embedding.
/// Returns results sorted by cosine similarity descending.
///
/// Loads all embeddings from the `chunks` table, computes cosine similarity
/// with the query in Rust, sorts, and returns the top `limit` results.
/// This is O(n) over all chunks but fine for personal wikis (<10k chunks).
pub fn vector_search(
    conn: &Connection,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Vec<VectorResult>> {
    let mut stmt = conn.prepare(
        "SELECT hash, seq, chunk_text, embedding FROM chunks WHERE embedding IS NOT NULL",
    )?;
    let mut scored: Vec<VectorResult> = stmt
        .query_map([], |row| {
            let hash: String = row.get(0)?;
            let seq: i32 = row.get(1)?;
            let chunk_text: String = row.get(2)?;
            let blob: Vec<u8> = row.get(3)?;
            Ok((hash, seq, chunk_text, blob))
        })?
        .filter_map(|r| r.ok())
        .map(|(hash, seq, chunk_text, blob)| {
            let emb = blob_to_embedding(&blob);
            let score = cosine_similarity(query_embedding, &emb);
            VectorResult {
                hash,
                seq,
                chunk_text,
                score,
            }
        })
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit);
    Ok(scored)
}

/// Collapse chunk-level results to document-level.
/// For each document (content hash), keep the chunk with the highest similarity.
pub fn vector_search_collapsed(
    conn: &Connection,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Vec<VectorResult>> {
    // Get all chunk results (no limit, we collapse afterwards).
    let all = vector_search(conn, query_embedding, usize::MAX)?;

    // Keep the best chunk per content hash.
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

/// Delete all chunks for a given content hash.
pub fn delete_chunks(conn: &Connection, hash: &str) -> Result<()> {
    conn.execute("DELETE FROM chunks WHERE hash = ?1", [hash])?;
    Ok(())
}

/// Serialize an f32 slice to a little-endian byte blob.
fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(embedding.len() * 4);
    for &val in embedding {
        blob.extend_from_slice(&val.to_le_bytes());
    }
    blob
}

/// Deserialize a little-endian byte blob to an f32 vector.
/// If the blob length is not a multiple of 4, pads with zeros to EMBEDDING_DIM.
fn blob_to_embedding(blob: &[u8]) -> Vec<f32> {
    let mut emb = Vec::with_capacity(EMBEDDING_DIM);
    for chunk in blob.chunks_exact(4) {
        emb.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    // Pad if shorter than expected (shouldn't happen in practice).
    emb.resize(EMBEDDING_DIM, 0.0);
    emb
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        conn
    }

    /// Insert a placeholder row into the `content` table so that the
    /// foreign-key constraint on `chunks.hash` is satisfied.
    fn insert_content_row(conn: &Connection, hash: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO content (hash, doc, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![hash, format!("doc for {hash}"), "2026-01-01T00:00:00Z"],
        )
        .unwrap();
    }

    fn fake_embedding(val: f32) -> Vec<f32> {
        vec![val; 768]
    }

    #[test]
    fn store_and_search_vectors() {
        let conn = setup_db();
        insert_content_row(&conn, "hash1");
        store_chunk(
            &conn,
            "hash1",
            0,
            "chunk text",
            0,
            10,
            "test-model",
            &fake_embedding(0.5),
        )
        .unwrap();
        let results = vector_search(&conn, &fake_embedding(0.5), 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].hash, "hash1");
        assert!(results[0].score > 0.99); // identical vectors
    }

    /// Build a 768-dim embedding with value `a` in dim 0 and `b` in dim 1.
    /// Different (a, b) pairs produce genuinely different directions.
    fn dir_embedding(a: f32, b: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; 768];
        v[0] = a;
        v[1] = b;
        v
    }

    #[test]
    fn chunk_to_doc_collapse() {
        let conn = setup_db();
        insert_content_row(&conn, "hash1");
        insert_content_row(&conn, "hash2");

        // Query: pointing strongly along dim 0.
        let query = dir_embedding(1.0, 0.0);

        // hash1 chunk 0: very close to query direction.
        // cos(query, emb_a) = 1.0*0.95 / (1.0 * sqrt(0.95^2+0.05^2)) ~ 0.998
        let emb_a = dir_embedding(0.95, 0.05);
        // hash1 chunk 1: perpendicular-ish to query.
        // cos(query, emb_b) = 1.0*0.1 / (1.0 * sqrt(0.01+0.81)) ~ 0.11
        let emb_b = dir_embedding(0.1, 0.9);
        // hash2 chunk 0: moderately aligned with query.
        // cos(query, emb_c) = 1.0*0.6 / (1.0 * sqrt(0.36+0.64)) = 0.6
        let emb_c = dir_embedding(0.6, 0.8);

        store_chunk(&conn, "hash1", 0, "chunk a", 0, 7, "model", &emb_a).unwrap();
        store_chunk(&conn, "hash1", 1, "chunk b", 7, 7, "model", &emb_b).unwrap();
        store_chunk(&conn, "hash2", 0, "chunk c", 0, 7, "model", &emb_c).unwrap();

        let results = vector_search_collapsed(&conn, &query, 10).unwrap();
        assert_eq!(results.len(), 2); // two unique docs
        // hash1 best chunk (emb_a ~0.998) beats hash2 (emb_c ~0.6)
        assert_eq!(results[0].hash, "hash1");
        assert_eq!(results[1].hash, "hash2");
    }

    #[test]
    fn delete_chunks_removes_all() {
        let conn = setup_db();
        insert_content_row(&conn, "hash1");
        store_chunk(&conn, "hash1", 0, "a", 0, 1, "m", &fake_embedding(0.5)).unwrap();
        store_chunk(&conn, "hash1", 1, "b", 1, 1, "m", &fake_embedding(0.5)).unwrap();
        delete_chunks(&conn, "hash1").unwrap();
        let results = vector_search(&conn, &fake_embedding(0.5), 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn embedding_blob_roundtrip() {
        let emb = fake_embedding(0.42);
        let blob = embedding_to_blob(&emb);
        assert_eq!(blob.len(), 768 * 4);
        let restored = blob_to_embedding(&blob);
        assert_eq!(emb, restored);
    }

    #[test]
    fn store_chunk_replaces_on_conflict() {
        let conn = setup_db();
        insert_content_row(&conn, "hash1");
        store_chunk(
            &conn,
            "hash1",
            0,
            "old text",
            0,
            8,
            "m",
            &fake_embedding(0.1),
        )
        .unwrap();
        store_chunk(
            &conn,
            "hash1",
            0,
            "new text",
            0,
            8,
            "m",
            &fake_embedding(0.9),
        )
        .unwrap();
        let results = vector_search(&conn, &fake_embedding(0.9), 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk_text, "new text");
    }

    #[test]
    fn vector_search_respects_limit() {
        let conn = setup_db();
        for i in 0..5 {
            let hash = format!("hash{i}");
            insert_content_row(&conn, &hash);
            let val = (i as f32 + 1.0) / 5.0;
            store_chunk(
                &conn,
                &hash,
                0,
                &format!("chunk {i}"),
                0,
                7,
                "m",
                &fake_embedding(val),
            )
            .unwrap();
        }
        let results = vector_search(&conn, &fake_embedding(1.0), 3).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn vector_search_empty_table() {
        let conn = setup_db();
        let results = vector_search(&conn, &fake_embedding(1.0), 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn outdated_chunk_models_detected() {
        let conn = setup_db();
        insert_content_row(&conn, "hash1");
        insert_content_row(&conn, "hash2");

        // Store chunk with current model.
        store_chunk(
            &conn,
            "hash1",
            0,
            "current chunk",
            0,
            13,
            crate::embed::CURRENT_MODEL_NAME,
            &fake_embedding(0.5),
        )
        .unwrap();

        // Store chunks with an old model.
        store_chunk(
            &conn,
            "hash2",
            0,
            "old chunk a",
            0,
            11,
            "old-model",
            &fake_embedding(0.3),
        )
        .unwrap();
        store_chunk(
            &conn,
            "hash2",
            1,
            "old chunk b",
            11,
            11,
            "old-model",
            &fake_embedding(0.4),
        )
        .unwrap();

        // Query outdated models.
        let mut stmt = conn
            .prepare("SELECT model, COUNT(*) FROM chunks WHERE model != ?1 GROUP BY model")
            .unwrap();
        let rows: Vec<(String, i64)> = stmt
            .query_map(rusqlite::params![crate::embed::CURRENT_MODEL_NAME], |row| {
                Ok((
                    row.get::<_, String>(0).unwrap(),
                    row.get::<_, i64>(1).unwrap(),
                ))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "old-model");
        assert_eq!(rows[0].1, 2);

        // Query outdated hashes.
        let mut stmt2 = conn
            .prepare("SELECT DISTINCT hash FROM chunks WHERE model != ?1")
            .unwrap();
        let hashes: Vec<String> = stmt2
            .query_map(rusqlite::params![crate::embed::CURRENT_MODEL_NAME], |row| {
                row.get::<_, String>(0)
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], "hash2");
    }
}
