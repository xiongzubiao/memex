//! Pure-BM25 search functions plus the FTS5 query sanitizer they use.
//!
//! These are free functions — no `Bm25Search` self type — because they
//! operate on a passed-in `&rusqlite::Connection` and take normalized
//! collection lists. The methods on `Bm25Search` (`search_by_doc_type`,
//! `search_by_doc_type_in_collections`, `search_title_only`) are thin
//! wrappers that lock the connection and forward here.

use std::path::PathBuf;

use crate::error::Result;

use super::{SearchResult, normalize_bm25, normalize_collections};

/// BM25 search filtered by doc_type.
///
/// Returns results from the `documents_fts` virtual table joined with `documents`,
/// filtered by doc_type. BM25 column weights differ by doc_type:
/// - wiki: path=1.5, title=4.0, tags=1.5, body=1.0
/// - source: path=1.5, title=4.0, tags=0.0, body=1.0
pub fn search_bm25(
    conn: &rusqlite::Connection,
    query: &str,
    doc_type: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let sanitized = sanitize_query(query);
    if sanitized.is_empty() {
        return Ok(Vec::new());
    }

    search_bm25_raw(conn, &sanitized, doc_type, limit)
}

/// BM25 search filtered by doc_type and one or more collections.
pub(super) fn search_bm25_in_collections(
    conn: &rusqlite::Connection,
    query: &str,
    doc_type: &str,
    limit: usize,
    collections: &[String],
) -> Result<Vec<SearchResult>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let sanitized = sanitize_query(query);
    if sanitized.is_empty() {
        return Ok(Vec::new());
    }

    let collections = normalize_collections(collections);
    search_bm25_raw_in_collections(conn, &sanitized, doc_type, limit, &collections)
}

/// Inner BM25 search with a pre-sanitized FTS5 MATCH expression.
pub(super) fn search_bm25_raw(
    conn: &rusqlite::Connection,
    match_expr: &str,
    doc_type: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let bm25_expr = if doc_type == "raw" {
        "bm25(documents_fts, 1.5, 4.0, 0.0, 1.0)"
    } else {
        "bm25(documents_fts, 1.5, 4.0, 1.5, 1.0)"
    };

    let sql = format!(
        "SELECT d.doc_type, d.path, d.title, {bm25_expr} as score, d.hash \
         FROM documents_fts f \
         JOIN documents d ON d.id = f.rowid \
         WHERE documents_fts MATCH ?1 AND d.doc_type = ?2 \
         ORDER BY score \
         LIMIT ?3"
    );

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };

    let rows: Vec<(String, String, String, f64, String)> = match stmt.query_map(
        rusqlite::params![match_expr, doc_type, limit as i64],
        |row| {
            Ok((
                row.get::<_, String>(0)?, // doc_type
                row.get::<_, String>(1)?, // path
                row.get::<_, String>(2)?, // title
                row.get::<_, f64>(3)?,    // raw score
                row.get::<_, String>(4)?, // hash
            ))
        },
    ) {
        Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
        Err(_) => return Ok(Vec::new()),
    };

    let results = rows
        .into_iter()
        .map(|(coll, path, title, raw, hash)| {
            let score = normalize_bm25(raw) as f32;
            SearchResult {
                path: PathBuf::from(&path),
                title,
                score,
                snippet: String::new(),
                doc_type: coll,
                hash,
            }
        })
        .collect();

    Ok(results)
}

/// Inner BM25 search restricted to a set of collection names.
fn search_bm25_raw_in_collections(
    conn: &rusqlite::Connection,
    match_expr: &str,
    doc_type: &str,
    limit: usize,
    collections: &[String],
) -> Result<Vec<SearchResult>> {
    let bm25_expr = if doc_type == "raw" {
        "bm25(documents_fts, 1.5, 4.0, 0.0, 1.0)"
    } else {
        "bm25(documents_fts, 1.5, 4.0, 1.5, 1.0)"
    };
    let placeholders = std::iter::repeat_n("?", collections.len())
        .collect::<Vec<_>>()
        .join(", ");

    let sql = format!(
        "SELECT DISTINCT d.doc_type, d.path, d.title, {bm25_expr} as score, d.hash \
         FROM documents_fts f \
         JOIN documents d ON d.id = f.rowid \
         JOIN document_collections dc ON dc.document_id = d.id \
         JOIN collections c ON c.id = dc.collection_id \
         WHERE documents_fts MATCH ? AND d.doc_type = ? AND c.name IN ({placeholders}) \
         ORDER BY score \
         LIMIT ?"
    );

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };

    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(3 + collections.len());
    let limit = limit as i64;
    params.push(&match_expr);
    params.push(&doc_type);
    for name in collections {
        params.push(name);
    }
    params.push(&limit);

    let rows: Vec<(String, String, String, f64, String)> =
        match stmt.query_map(rusqlite::params_from_iter(params), |row| {
            Ok((
                row.get::<_, String>(0)?, // doc_type
                row.get::<_, String>(1)?, // path
                row.get::<_, String>(2)?, // title
                row.get::<_, f64>(3)?,    // raw score
                row.get::<_, String>(4)?, // hash
            ))
        }) {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(_) => return Ok(Vec::new()),
        };

    let results = rows
        .into_iter()
        .map(|(coll, path, title, raw, hash)| {
            let score = normalize_bm25(raw) as f32;
            SearchResult {
                path: PathBuf::from(&path),
                title,
                score,
                snippet: String::new(),
                doc_type: coll,
                hash,
            }
        })
        .collect();

    Ok(results)
}

/// Sanitize a user query for FTS5 MATCH (following QMD pattern).
///
/// Rules:
/// - Quoted phrases preserved: `"exact match"` → FTS5 phrase
/// - Negation: `-term` → NOT clause
/// - Hyphens: `multi-agent` → `"multi agent"` (phrase)
/// - Bare words: `term` → `"term"*` (prefix match)
/// - Positives AND-joined
pub fn sanitize_query(query: &str) -> String {
    let query = query.trim();
    if query.is_empty() {
        return String::new();
    }

    let mut positives: Vec<String> = Vec::new();
    let mut negatives: Vec<String> = Vec::new();

    let mut chars = query.chars().peekable();
    while chars.peek().is_some() {
        // Skip whitespace
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }

        if chars.peek().is_none() {
            break;
        }

        // Quoted phrase: preserve as-is
        if chars.peek() == Some(&'"') {
            chars.next(); // consume opening quote
            let mut phrase = String::new();
            while let Some(&c) = chars.peek() {
                if c == '"' {
                    chars.next(); // consume closing quote
                    break;
                }
                phrase.push(c);
                chars.next();
            }
            if !phrase.is_empty() {
                // Clean the phrase: only keep alphanumeric, whitespace, underscore
                let cleaned: String = phrase
                    .chars()
                    .map(|c| {
                        if c.is_alphanumeric() || c == '_' || c.is_whitespace() {
                            c
                        } else {
                            ' '
                        }
                    })
                    .collect();
                let cleaned = cleaned.trim().to_string();
                if !cleaned.is_empty() {
                    positives.push(format!("\"{cleaned}\""));
                }
            }
            continue;
        }

        // Collect a word (non-whitespace token)
        let mut word = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            if c == '"' {
                break; // let the quoted-phrase branch handle it
            }
            word.push(c);
            chars.next();
        }

        if word.is_empty() {
            continue;
        }

        // Negation: -term → NOT clause
        if let Some(rest) = word.strip_prefix('-') {
            let clean: String = rest
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
                .to_lowercase();
            if !clean.is_empty() {
                negatives.push(format!("NOT \"{clean}\""));
            }
            continue;
        }

        // Hyphenated word: multi-agent → "multi agent" (phrase)
        if word.contains('-') {
            let parts: Vec<String> = word
                .split('-')
                .map(|p| {
                    p.chars()
                        .filter(|c| c.is_alphanumeric() || *c == '_')
                        .collect::<String>()
                        .to_lowercase()
                })
                .filter(|p| !p.is_empty())
                .collect();
            if !parts.is_empty() {
                let phrase = parts.join(" ");
                positives.push(format!("\"{phrase}\""));
            }
            continue;
        }

        // Bare word: term → "term"* (prefix match)
        let clean: String = word
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>()
            .to_lowercase();
        if !clean.is_empty() {
            positives.push(format!("\"{clean}\"*"));
        }
    }

    if positives.is_empty() && negatives.is_empty() {
        return String::new();
    }

    let mut result = positives.join(" AND ");
    for neg in &negatives {
        if result.is_empty() {
            // Can't have only negations in FTS5
            return String::new();
        }
        result.push_str(&format!(" {neg}"));
    }
    result
}
