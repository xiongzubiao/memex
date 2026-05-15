//! BM25 search functions plus the FTS5 query sanitizer they use.
//!
//! Two indexes are queried:
//!
//! - `titles_fts(title)` — for `memex search <title>` lookups. Hit by
//!   `search_titles`.
//! - `chunks_fts(chunk_text, hash UNINDEXED, seq UNINDEXED)` — for
//!   chunk-granularity retrieval. Hit by `search_chunks`.
//!
//! Each function takes a `collections: &[String]` filter. An empty
//! slice is normalized to `["default"]`; every document is added to
//! the `default` collection at upsert, so an empty filter matches all
//! docs. Methods on `Db` are thin mutex-guarded wrappers
//! around these free functions.

use std::path::PathBuf;

use crate::error::Result;

use super::{SearchResult, normalize_collections};

/// Normalize raw BM25 score to [0, 1). Formula: |x| / (1 + |x|)
///
/// FTS5 BM25 scores are negative (lower = better): -10 is strong, -0.5 is weak.
/// This sigmoid-like transform is query-independent — a score of 0.67 always
/// means the same thing regardless of what other results exist.
/// Maps: strong(-10)→0.91, medium(-2)→0.67, weak(-0.5)→0.33, none(0)→0.
fn normalize_bm25(raw: f64) -> f64 {
    let abs = raw.abs();
    abs / (1.0 + abs)
}

/// Which FTS index to query. Determines table name, join condition,
/// and whether `chunk_seq` is populated on results.
#[derive(Debug, Clone, Copy)]
enum FtsTarget {
    Titles,
    Chunks,
}

/// BM25 search against `titles_fts(title)` filtered by doc_type and
/// collection membership. Pass `collections=&[]` for the default-only
/// (effectively "all docs") filter.
pub fn search_titles(
    conn: &rusqlite::Connection,
    query: &str,
    doc_type: &str,
    limit: usize,
    collections: &[String],
) -> Result<Vec<SearchResult>> {
    fts5_search(conn, query, doc_type, limit, collections, FtsTarget::Titles)
}

/// Chunk-level BM25 search via `chunks_fts`. One `SearchResult` per
/// matching chunk; multiple chunks of the same doc surface separately
/// so RRF can fuse at chunk granularity. `chunk_seq` is populated so
/// `populate_bodies` can confine the body slice to the matched chunk.
pub fn search_chunks(
    conn: &rusqlite::Connection,
    query: &str,
    doc_type: &str,
    limit: usize,
    collections: &[String],
) -> Result<Vec<SearchResult>> {
    fts5_search(conn, query, doc_type, limit, collections, FtsTarget::Chunks)
}

/// Shared body: trim + sanitize + normalize + run the FTS5 SELECT for
/// the chosen target. The two targets share param order, result-row
/// shape, and BM25-score normalization; only the SQL `FROM`/`JOIN`
/// clauses and the `chunk_seq` column differ.
fn fts5_search(
    conn: &rusqlite::Connection,
    query: &str,
    doc_type: &str,
    limit: usize,
    collections: &[String],
    target: FtsTarget,
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
    let placeholders = std::iter::repeat_n("?", collections.len())
        .collect::<Vec<_>>()
        .join(", ");

    // SELECT projects 6 columns in the same order for both targets:
    //   doc_type, path, title, raw_bm25_score, hash, seq (NULL for titles).
    let (fts_table, from_join, seq_col) = match target {
        FtsTarget::Titles => (
            "titles_fts",
            "titles_fts f JOIN documents d ON d.id = f.rowid",
            "NULL",
        ),
        FtsTarget::Chunks => (
            "chunks_fts",
            "chunks_fts c JOIN documents d ON d.hash = c.hash",
            "c.seq",
        ),
    };
    let sql = format!(
        "SELECT DISTINCT d.doc_type, d.path, d.title, bm25({fts_table}) as score, \
                         d.hash, {seq_col} \
         FROM {from_join} \
         JOIN document_collections dc ON dc.document_id = d.id \
         JOIN collections col ON col.id = dc.collection_id \
         WHERE {fts_table} MATCH ? AND d.doc_type = ? AND col.name IN ({placeholders}) \
         ORDER BY score \
         LIMIT ?"
    );

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };

    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(3 + collections.len());
    let limit = limit as i64;
    params.push(&sanitized);
    params.push(&doc_type);
    for name in &collections {
        params.push(name);
    }
    params.push(&limit);

    let rows: Vec<(String, String, String, f64, String, Option<i32>)> =
        match stmt.query_map(rusqlite::params_from_iter(params), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, f64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<i32>>(5)?,
            ))
        }) {
            Ok(m) => m.filter_map(|r| r.ok()).collect(),
            Err(_) => return Ok(Vec::new()),
        };

    Ok(rows
        .into_iter()
        .map(|(dt, path, title, raw, hash, seq)| SearchResult {
            path: PathBuf::from(&path),
            title,
            score: normalize_bm25(raw) as f32,
            body: String::new(),
            doc_type: dt,
            hash,
            chunk_seq: seq,
        })
        .collect())
}

/// Per-token character cleaner. Mirrors QMD `sanitizeFTS5Term`
/// (`store.ts:2874`): keep Unicode letters/digits, underscore, and
/// apostrophe; lowercase. Used at every site where a raw token enters
/// the FTS5 MATCH expression.
fn sanitize_fts5_term(term: &str) -> String {
    term.chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '\'')
        .collect::<String>()
        .to_lowercase()
}

/// Detect a hyphenated compound word (e.g. `multi-agent`, `gpt-4`,
/// `DEC-0054`). Mirrors QMD `isHyphenatedToken` (`store.ts:2882`):
/// alphanumeric on both ends, at least one internal hyphen, body
/// limited to alphanumerics, apostrophes, and hyphens.
fn is_hyphenated_token(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 3 {
        return false;
    }
    if !chars[0].is_alphanumeric() || !chars[chars.len() - 1].is_alphanumeric() {
        return false;
    }
    let mut has_internal_hyphen = false;
    for c in &chars[1..chars.len() - 1] {
        if *c == '-' {
            has_internal_hyphen = true;
        } else if !c.is_alphanumeric() && *c != '\'' && *c != '-' {
            return false;
        }
    }
    has_internal_hyphen
}

/// Split a hyphenated term and rejoin parts as a space-separated phrase
/// for FTS5 (porter tokenizer matches the original through the phrase).
/// Mirrors QMD `sanitizeHyphenatedTerm` (`store.ts:2891`).
fn sanitize_hyphenated_term(term: &str) -> String {
    term.split('-')
        .map(sanitize_fts5_term)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sanitize a user query for FTS5 MATCH. Aligned with QMD's
/// `buildFTS5Query` (`store.ts:2919`).
///
/// Rules:
/// - `-token` is a negation prefix; produces `NOT "token"*` (or
///   `NOT "phrase"` when applied to a quoted phrase).
/// - `"phrase"` is a quoted phrase; sanitized internally and emitted
///   as an FTS5 phrase.
/// - Plain term: hyphenated → quoted phrase, otherwise prefix-match
///   (`"term"*`). Empty after sanitize → drop.
/// - Positives joined with ` AND `; negatives appended as `NOT term`.
/// - All-negative query returns empty (FTS5 `NOT` is binary; nothing
///   to subtract from).
pub fn sanitize_query(query: &str) -> String {
    let s = query.trim();
    if s.is_empty() {
        return String::new();
    }

    let mut positives: Vec<String> = Vec::new();
    let mut negatives: Vec<String> = Vec::new();

    let mut chars = s.chars().peekable();
    while chars.peek().is_some() {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }

        let negated = if chars.peek() == Some(&'-') {
            chars.next();
            true
        } else {
            false
        };

        if chars.peek() == Some(&'"') {
            chars.next();
            let mut phrase = String::new();
            while let Some(&c) = chars.peek() {
                if c == '"' {
                    chars.next();
                    break;
                }
                phrase.push(c);
                chars.next();
            }
            let sanitized = phrase
                .split_whitespace()
                .map(sanitize_fts5_term)
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if !sanitized.is_empty() {
                let fts = format!("\"{sanitized}\"");
                if negated {
                    negatives.push(format!("NOT {fts}"));
                } else {
                    positives.push(fts);
                }
            }
            continue;
        }

        let mut term = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() || c == '"' {
                break;
            }
            term.push(c);
            chars.next();
        }
        if term.is_empty() {
            // Lone `-` followed by whitespace/quote/EOF: drop silently.
            continue;
        }

        let fts = if is_hyphenated_token(&term) {
            let phrase = sanitize_hyphenated_term(&term);
            if phrase.is_empty() {
                continue;
            }
            format!("\"{phrase}\"")
        } else {
            let cleaned = sanitize_fts5_term(&term);
            if cleaned.is_empty() {
                continue;
            }
            format!("\"{cleaned}\"*")
        };
        if negated {
            negatives.push(format!("NOT {fts}"));
        } else {
            positives.push(fts);
        }
    }

    if positives.is_empty() {
        return String::new();
    }

    let mut result = positives.join(" AND ");
    for neg in &negatives {
        result.push_str(&format!(" {neg}"));
    }
    result
}

impl super::Db {
    /// BM25 search against `titles_fts` filtered by doc_type and
    /// collection membership. Pass `collections=&[]` for default-only
    /// (effectively "all docs") filtering.
    pub fn search_by_doc_type(
        &self,
        query: &str,
        doc_type: &str,
        limit: usize,
        collections: &[String],
    ) -> Result<Vec<SearchResult>> {
        let conn = self.conn.lock().map_err(|e| super::mutex_err(&e))?;
        search_titles(&conn, query, doc_type, limit, collections)
    }

    /// Chunk-level BM25 search via `chunks_fts`. Returns one result
    /// per matching chunk; multiple chunks of the same doc are kept as
    /// separate results so RRF can fuse at chunk granularity. Pass
    /// `collections=&[]` for default-only filtering.
    pub fn search_chunks_by_doc_type(
        &self,
        query: &str,
        doc_type: &str,
        limit: usize,
        collections: &[String],
    ) -> Result<Vec<SearchResult>> {
        let conn = self.conn.lock().map_err(|e| super::mutex_err(&e))?;
        search_chunks(&conn, query, doc_type, limit, collections)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::search::test_helpers::{insert_doc, open_temp_search, setup_db};

    // -- normalize_bm25 -----------------------------------------------------

    #[test]
    fn score_normalization() {
        let s = normalize_bm25(-10.0);
        assert!((s - 10.0 / 11.0).abs() < 1e-9, "got {s}");

        let s = normalize_bm25(-2.0);
        assert!((s - 2.0 / 3.0).abs() < 1e-9, "got {s}");

        let s = normalize_bm25(-0.5);
        assert!((s - 1.0 / 3.0).abs() < 1e-9, "got {s}");

        assert_eq!(normalize_bm25(0.0), 0.0);

        let s = normalize_bm25(10.0);
        assert!((s - 10.0 / 11.0).abs() < 1e-9);
    }

    // -- sanitize_query -----------------------------------------------------

    #[test]
    fn sanitize_query_bare_words() {
        let result = sanitize_query("hello world");
        assert_eq!(result, "\"hello\"* AND \"world\"*");
    }

    #[test]
    fn sanitize_query_quoted_phrase() {
        let result = sanitize_query("\"exact match\"");
        assert_eq!(result, "\"exact match\"");
    }

    #[test]
    fn sanitize_query_negation() {
        let result = sanitize_query("rust -python");
        assert_eq!(result, "\"rust\"* NOT \"python\"*");
    }

    #[test]
    fn sanitize_query_hyphenated() {
        let result = sanitize_query("multi-agent");
        assert_eq!(result, "\"multi agent\"");
    }

    #[test]
    fn sanitize_query_mixed() {
        let result = sanitize_query("\"exact match\" multi-agent -python");
        assert_eq!(
            result,
            "\"exact match\" AND \"multi agent\" NOT \"python\"*"
        );
    }

    #[test]
    fn sanitize_query_empty() {
        assert_eq!(sanitize_query(""), "");
        assert_eq!(sanitize_query("   "), "");
    }

    #[test]
    fn sanitize_query_special_chars() {
        let result = sanitize_query("hello (world)");
        assert_eq!(result, "\"hello\"* AND \"world\"*");
    }

    #[test]
    fn sanitize_query_apostrophe_preserved() {
        let result = sanitize_query("Caroline's don't");
        assert_eq!(result, "\"caroline's\"* AND \"don't\"*");
    }

    #[test]
    fn sanitize_query_negated_quoted_phrase() {
        let result = sanitize_query("rust -\"machine learning\"");
        assert_eq!(result, "\"rust\"* NOT \"machine learning\"");
    }

    #[test]
    fn sanitize_query_negated_hyphenated() {
        // Negation applies to the whole compound, not just the leading segment.
        let result = sanitize_query("agents -multi-agent");
        assert_eq!(result, "\"agents\"* NOT \"multi agent\"");
    }

    #[test]
    fn sanitize_query_question_mark_stripped() {
        let result = sanitize_query("When did Melanie run a charity race?");
        assert_eq!(
            result,
            "\"when\"* AND \"did\"* AND \"melanie\"* AND \"run\"* AND \"a\"* AND \"charity\"* AND \"race\"*"
        );
    }

    #[test]
    fn sanitize_query_only_negative_returns_empty() {
        // FTS5 NOT is binary; can't run a query with only negatives.
        assert_eq!(sanitize_query("-foo -bar"), "");
    }

    #[test]
    fn sanitize_query_lone_dash_dropped() {
        let result = sanitize_query("foo - bar");
        assert_eq!(result, "\"foo\"* AND \"bar\"*");
    }

    #[test]
    fn sanitize_query_trailing_hyphen_not_compound() {
        // `foo-` doesn't match isHyphenatedToken (last char isn't
        // alphanumeric); falls through to bare-term sanitization.
        let result = sanitize_query("foo-");
        assert_eq!(result, "\"foo\"*");
    }

    // -- search_titles + Db::search_by_doc_type ----------------------------

    #[test]
    fn search_filters_by_doc_type() {
        // search_titles now hits titles_fts only — body-token tests
        // belong to chunk-level BM25 (search_chunks_*).
        let conn = setup_db();
        insert_doc(
            &conn,
            "wiki",
            "wiki/rust-borrow.md",
            "Rust Borrow Checker",
            "irrelevant body",
        );
        insert_doc(
            &conn,
            "raw",
            "raw/ab/cdrust-book.md",
            "Rust Programming Language",
            "irrelevant body",
        );
        let wiki_results = search_titles(&conn, "borrow checker", "wiki", 10, &[]).unwrap();
        assert!(!wiki_results.is_empty());
        for r in &wiki_results {
            assert_eq!(r.doc_type, "wiki");
        }
        let raw_results = search_titles(&conn, "programming language", "raw", 10, &[]).unwrap();
        assert!(!raw_results.is_empty());
        for r in &raw_results {
            assert_eq!(r.doc_type, "raw");
        }
    }

    #[test]
    fn search_by_doc_type_filters_by_collections() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/team-a.md"),
                "Alpha Team A",
                "Alpha content for the team-a collection.",
                1000,
            )
            .unwrap();
        search
            .set_document_collections_by_path("wiki", "wiki/team-a.md", &["team-a".to_string()])
            .unwrap();

        search
            .index_page(
                Path::new("wiki/team-b.md"),
                "Alpha Team B",
                "Alpha content for the team-b collection.",
                1000,
            )
            .unwrap();
        search
            .set_document_collections_by_path("wiki", "wiki/team-b.md", &["team-b".to_string()])
            .unwrap();

        let results = search
            .search_by_doc_type("alpha", "wiki", 10, &["team-a".to_string()])
            .unwrap();
        let paths: std::collections::HashSet<PathBuf> =
            results.into_iter().map(|r| r.path).collect();
        assert!(paths.contains(&PathBuf::from("wiki/team-a.md")));
        assert!(!paths.contains(&PathBuf::from("wiki/team-b.md")));
    }

    #[test]
    fn bm25_index_and_search() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/rust-borrow.md"),
                "Rust Borrow Checker",
                "The borrow checker enforces ownership rules at compile time.",
                1000,
            )
            .unwrap();
        search
            .index_page(
                Path::new("wiki/python-gc.md"),
                "Python Garbage Collection",
                "Python uses reference counting with a cyclic garbage collector.",
                1000,
            )
            .unwrap();

        let results = search
            .search_by_doc_type("borrow checker rust", "wiki", 10, &[])
            .unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].path, PathBuf::from("wiki/rust-borrow.md"));
        for r in &results {
            assert!(
                (0.0..=1.0).contains(&r.score),
                "score out of range: {}",
                r.score
            );
        }
    }

    #[test]
    fn bm25_remove_page() {
        let (_dir, search) = open_temp_search();
        search
            .index_page(
                Path::new("wiki/ephemeral.md"),
                "Ephemeral Page",
                "This page will be removed shortly.",
                1000,
            )
            .unwrap();
        let before = search
            .search_by_doc_type("ephemeral", "wiki", 10, &[])
            .unwrap();
        assert_eq!(before.len(), 1);
        search.remove_page("wiki/ephemeral.md").unwrap();
        let after = search
            .search_by_doc_type("ephemeral", "wiki", 10, &[])
            .unwrap();
        assert!(after.is_empty());
    }

    #[test]
    fn bm25_empty_search() {
        let (_dir, search) = open_temp_search();
        let results = search
            .search_by_doc_type("anything", "wiki", 10, &[])
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn bm25_malformed_query_returns_empty() {
        let (_dir, search) = open_temp_search();
        let results = search.search_by_doc_type("***", "wiki", 10, &[]).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn bm25_index_page_replace() {
        let (_dir, search) = open_temp_search();
        search
            .index_page(
                Path::new("wiki/page.md"),
                "Original Title",
                "Original body content.",
                1000,
            )
            .unwrap();
        search
            .index_page(
                Path::new("wiki/page.md"),
                "Updated Quantum Computing",
                "Body content.",
                2000,
            )
            .unwrap();
        let results = search
            .search_by_doc_type("quantum computing", "wiki", 10, &[])
            .unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Updated Quantum Computing");
    }
}
