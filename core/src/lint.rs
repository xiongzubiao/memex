use crate::Memex;
use crate::embed::CURRENT_MODEL_NAME;
use crate::error::Result;
use crate::types::{LintIssue, LintIssueKind, LintReport};
use crate::validate;

impl Memex {
    /// Run deterministic lint checks on the wiki.
    ///
    /// Checks performed:
    /// - Stale index: wiki file on disk has different hash than documents.hash
    /// - Untracked file: .md file in wiki/ directory with no matching documents row
    /// - Missing file: wiki documents row with no .md file on disk
    /// - Dangling wiki links: `[[page-stem]]` references to pages that don't exist
    /// - Missing cross-references: body mentions an existing page title/stem without `[[link]]`
    pub fn lint(&self) -> Result<LintReport> {
        let wiki_dir = self.wiki_dir();
        let mut issues = Vec::new();

        // Collect DB state: all wiki documents.
        let db_docs = self.search.all_wiki_documents()?;
        let db_paths: std::collections::HashSet<String> =
            db_docs.iter().map(|d| d.path.clone()).collect();

        // Collect disk state: all .md files in wiki/.
        let mut disk_files: std::collections::HashSet<String> = std::collections::HashSet::new();
        if wiki_dir.exists() {
            for entry in std::fs::read_dir(&wiki_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "md") {
                    let rel = format!(
                        "wiki/{}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    );
                    disk_files.insert(rel);
                }
            }
        }

        // Check: Stale index — file hash differs from documents.hash.
        for doc in &db_docs {
            let full_path = self.root.join(&doc.path);
            if full_path.exists()
                && let Ok(disk_hash) = crate::storage::file_hash(&full_path)
                && disk_hash != doc.hash
            {
                let stem = std::path::Path::new(&doc.path)
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                issues.push(LintIssue {
                    kind: LintIssueKind::StaleIndex,
                    page: stem,
                    target: doc.path.clone(),
                });
            }
        }

        // Check: Untracked file — on disk but no DB row.
        for rel in &disk_files {
            if !db_paths.contains(rel) {
                issues.push(LintIssue {
                    kind: LintIssueKind::UntrackedFile,
                    page: String::new(),
                    target: rel.clone(),
                });
            }
        }

        // Check: Missing file — DB row but no file on disk.
        for doc in &db_docs {
            if !disk_files.contains(&doc.path) {
                let stem = std::path::Path::new(&doc.path)
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                issues.push(LintIssue {
                    kind: LintIssueKind::MissingFile,
                    page: stem,
                    target: doc.path.clone(),
                });
            }
        }

        // Check: Outdated embeddings — chunks with a model name that differs
        // from the current model constant.
        let outdated_models = self.search.outdated_chunk_models(CURRENT_MODEL_NAME)?;
        for (old_model, count) in &outdated_models {
            issues.push(LintIssue {
                kind: LintIssueKind::OutdatedEmbedding,
                page: format!("{count} chunks"),
                target: format!("{old_model} => {CURRENT_MODEL_NAME}"),
            });
        }

        // Parse all pages ONCE for link checks: (stem, title, body, title_lower)
        if !wiki_dir.exists() {
            return Ok(LintReport { issues });
        }

        let mut pages: Vec<(String, String, String, String)> = Vec::new();
        for entry in std::fs::read_dir(&wiki_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md")
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                let stem = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if let Ok((fm, body)) = validate::parse_frontmatter(&content) {
                    let title_lower = fm.title.to_lowercase();
                    pages.push((stem, fm.title, body, title_lower));
                }
            }
        }

        let stems: std::collections::HashSet<&str> =
            pages.iter().map(|(s, _, _, _)| s.as_str()).collect();

        for (stem, _title, body, _title_lower) in &pages {
            // Check: Dangling wiki links — use stems set, not filesystem
            let links = validate::extract_wiki_links(body);
            for link in &links {
                if !stems.contains(link.as_str()) {
                    issues.push(LintIssue {
                        kind: LintIssueKind::DanglingLink,
                        page: stem.clone(),
                        target: link.clone(),
                    });
                }
            }

            // Check: Missing cross-references — word-boundary matching
            for (other_stem, _other_title, _, other_title_lower) in &pages {
                if other_stem == stem {
                    continue;
                }
                if links.iter().any(|l| l == other_stem) {
                    continue;
                }
                let stem_words = other_stem.replace('-', " ").to_lowercase();

                // Use word boundaries to avoid false positives where short stems
                // match inside longer words (e.g. "go" in "algorithm").
                let stem_pattern = format!(r"\b{}\b", regex::escape(&stem_words));
                let title_pattern = format!(r"\b{}\b", regex::escape(other_title_lower));

                let stem_match = regex::RegexBuilder::new(&stem_pattern)
                    .case_insensitive(true)
                    .build()
                    .is_ok_and(|re| re.is_match(body));
                let title_match = regex::RegexBuilder::new(&title_pattern)
                    .case_insensitive(true)
                    .build()
                    .is_ok_and(|re| re.is_match(body));

                if stem_match || title_match {
                    issues.push(LintIssue {
                        kind: LintIssueKind::MissingLink,
                        page: stem.clone(),
                        target: other_stem.clone(),
                    });
                }
            }
        }

        Ok(LintReport { issues })
    }
}

use crate::search::Bm25Search;

/// Re-verify that the issue still describes the current committed state.
/// Uses the provided `&Bm25Search` connection — caller is responsible for
/// having the right snapshot (reader's stale snapshot if the connection is
/// long-lived; fresh snapshot if the connection was just opened under lock).
pub(crate) fn is_issue_still_present(
    search: &Bm25Search,
    root: &std::path::Path,
    issue: &LintIssue,
) -> crate::error::Result<bool> {
    match issue.kind {
        LintIssueKind::StaleIndex => {
            // Stale means on-disk hash != stored hash. Re-read both.
            let full_path = root.join(&issue.target);
            let Ok(content) = std::fs::read_to_string(&full_path) else {
                return Ok(false);  // file gone; nothing to fix
            };
            let actual = crate::storage::content_hash(content.as_bytes());
            let stored = search.get_document_hash(&issue.target)?.unwrap_or_default();
            Ok(actual != stored)
        }
        LintIssueKind::OutdatedEmbedding => {
            // OutdatedEmbedding is a DB-level issue: the index has chunks using an
            // older embedding model. If any outdated chunks remain, the issue is
            // still present. `issue.target` is human-readable model-name context
            // (not a path/hash), so we don't use it in this check.
            let outdated = search.outdated_chunk_hashes(crate::embed::CURRENT_MODEL_NAME)?;
            Ok(!outdated.is_empty())
        }
        // Report-only kinds: always "present" (no auto-fix path).
        LintIssueKind::DanglingLink
        | LintIssueKind::MissingLink
        | LintIssueKind::UntrackedFile
        | LintIssueKind::MissingFile => Ok(true),
    }
}

/// Apply a single lint fix to the provided `&Bm25Search`.
///
/// Currently only StaleIndex and OutdatedEmbedding have auto-fix semantics —
/// other kinds are report-only per the original spec.
pub(crate) fn apply_fix_inner(
    search: &Bm25Search,
    root: &std::path::Path,
    issue: &LintIssue,
) -> crate::error::Result<()> {
    match issue.kind {
        LintIssueKind::StaleIndex => {
            let full_path = root.join(&issue.target);
            let content = std::fs::read_to_string(&full_path)?;
            let old_hash = search.get_document_hash(&issue.target)?.unwrap_or_default();
            search.reindex_page_from_content(&issue.target, &content, &old_hash)?;
            // Re-embed the new content. Uses hash_embedding fallback if ONNX unavailable.
            let new_hash = search.get_document_hash(&issue.target)?.unwrap_or_default();
            let body = crate::validate::parse_frontmatter(&content)
                .map(|(_, b)| b)
                .unwrap_or_else(|_| content.clone());
            apply_embedding(search, &new_hash, &body);
            Ok(())
        }
        LintIssueKind::OutdatedEmbedding => {
            // Pull the actual hash (issue.target contains "old-model => current")
            // Fix by reading content and re-embedding.
            let outdated = search.outdated_chunk_hashes(crate::embed::CURRENT_MODEL_NAME)?;
            for hash in &outdated {
                let full_content = search.get_content(hash)?;
                let body = crate::validate::parse_frontmatter(&full_content)
                    .map(|(_, b)| b)
                    .unwrap_or(full_content);
                apply_embedding(search, hash, &body);
            }
            Ok(())
        }
        _ => Ok(()),  // report-only kinds
    }
}

/// Embed and persist chunks for a content hash. Uses hash_embedding fallback
/// if ONNX model isn't available — keeps chunks populated so vector search
/// works (with lower quality) even without the model installed.
///
/// Wraps `load_model` in `catch_unwind` because the `ort` crate panics
/// (rather than returning Result::Err) when the ONNX Runtime shared library
/// cannot be loaded. Without the panic guard, a missing libonnxruntime
/// would crash the whole `lint --fix` process.
fn apply_embedding(search: &Bm25Search, hash: &str, body: &str) {
    let model_path = dirs::home_dir().map(|h| h.join(".memex/models/embedding-gemma-300m.onnx"));

    let chunks = crate::embed::chunk_text(body, 900, 0.15);

    let mut model_opt: Option<crate::embed::EmbeddingModel> = None;
    if let Some(ref mp) = model_path
        && mp.exists()
        && let Some(path_str) = mp.to_str()
    {
        let path_owned = path_str.to_string();
        if let Some(Ok(m)) = crate::embed::catch_unwind_silent(move || {
            crate::embed::load_model(&path_owned, "embedding-gemma-300m")
        }) {
            model_opt = Some(m);
        }
    }
    let model_name = if model_opt.is_some() { "embedding-gemma-300m" } else { "hash-embedding" };

    let _ = search.with_connection(|conn| {
        crate::vector::delete_chunks(conn, hash)?;
        for (seq, chunk) in chunks.iter().enumerate() {
            let embedding = if let Some(ref mut model) = model_opt {
                crate::embed::embed_text(model, &chunk.text)
                    .unwrap_or_else(|_| crate::embed::hash_embedding(&chunk.text))
            } else {
                crate::embed::hash_embedding(&chunk.text)
            };
            let _ = crate::vector::store_chunk(
                conn, hash, seq as i32, &chunk.text,
                chunk.pos, chunk.len, model_name, &embedding,
            );
        }
        Ok(())
    });
}


#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    /// Write a page to disk AND reindex so the DB is in sync.
    fn write_and_index(root: &std::path::Path, filename: &str, content: &str) {
        std::fs::write(root.join("wiki").join(filename), content).unwrap();
    }

    /// Open a memex as writer, write files, and reindex to sync DB with disk.
    fn open_and_reindex(root: &std::path::Path) -> crate::Memex {
        let memex = crate::Memex::open_writer(root.to_path_buf()).unwrap();
        memex.reindex().unwrap();
        memex
    }

    #[test]
    fn lint_detects_dangling_link() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(root.join("wiki")).unwrap();

        write_and_index(
            &root,
            "page-a.md",
            "---\ntitle: Page A\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nSee [[nonexistent-page]] for details.\n",
        );

        let memex = open_and_reindex(&root);
        let report = memex.lint().unwrap();
        let dangling: Vec<_> = report
            .issues
            .iter()
            .filter(|i| i.kind == crate::types::LintIssueKind::DanglingLink)
            .collect();
        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].page, "page-a");
        assert_eq!(dangling[0].target, "nonexistent-page");
    }

    #[test]
    fn lint_detects_missing_link() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(root.join("wiki")).unwrap();

        write_and_index(
            &root,
            "caching.md",
            "---\ntitle: Caching Strategies\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nCaching is important.\n",
        );

        write_and_index(
            &root,
            "performance.md",
            "---\ntitle: Performance\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nImprove performance with caching strategies and other techniques.\n",
        );

        let memex = open_and_reindex(&root);
        let report = memex.lint().unwrap();
        let missing: Vec<_> = report
            .issues
            .iter()
            .filter(|i| {
                i.kind == crate::types::LintIssueKind::MissingLink
                    && i.page == "performance"
                    && i.target == "caching"
            })
            .collect();
        assert!(
            !missing.is_empty(),
            "should detect that performance mentions caching strategies without linking"
        );
    }

    #[test]
    fn lint_empty_wiki() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        let memex = crate::Memex::open(root.clone()).unwrap();

        let report = memex.lint().unwrap();
        assert!(report.issues.is_empty());
    }

    #[test]
    fn lint_no_false_positives_when_linked() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(root.join("wiki")).unwrap();

        write_and_index(
            &root,
            "caching.md",
            "---\ntitle: Caching\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nCaching info.\n",
        );

        write_and_index(
            &root,
            "performance.md",
            "---\ntitle: Performance\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nImprove performance with [[caching]] and other techniques.\n",
        );

        let memex = open_and_reindex(&root);
        let report = memex.lint().unwrap();
        let missing: Vec<_> = report
            .issues
            .iter()
            .filter(|i| {
                i.kind == crate::types::LintIssueKind::MissingLink
                    && i.page == "performance"
                    && i.target == "caching"
            })
            .collect();
        assert!(
            missing.is_empty(),
            "should not flag missing link when already linked via [[caching]]"
        );
    }

    #[test]
    fn lint_detects_stale_index() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(root.join("wiki")).unwrap();

        // Write and index a page.
        write_and_index(
            &root,
            "stale-page.md",
            "---\ntitle: Stale Page\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nOriginal content.\n",
        );
        let memex = open_and_reindex(&root);

        // Modify the file on disk without reindexing.
        std::fs::write(
            root.join("wiki/stale-page.md"),
            "---\ntitle: Stale Page\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nModified content that is different.\n",
        )
        .unwrap();

        let report = memex.lint().unwrap();
        let stale: Vec<_> = report
            .issues
            .iter()
            .filter(|i| i.kind == crate::types::LintIssueKind::StaleIndex)
            .collect();
        assert_eq!(stale.len(), 1, "should detect one stale-index issue");
        assert_eq!(stale[0].page, "stale-page");
    }

    #[test]
    fn lint_detects_untracked_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        let memex = crate::Memex::open(root.clone()).unwrap();

        // Write a file to disk without indexing it.
        std::fs::write(
            root.join("wiki/untracked.md"),
            "---\ntitle: Untracked\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nNot indexed.\n",
        )
        .unwrap();

        let report = memex.lint().unwrap();
        let untracked: Vec<_> = report
            .issues
            .iter()
            .filter(|i| i.kind == crate::types::LintIssueKind::UntrackedFile)
            .collect();
        assert_eq!(untracked.len(), 1, "should detect one untracked file");
        assert!(untracked[0].target.contains("untracked.md"));
    }

    #[test]
    fn lint_detects_missing_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(root.join("wiki")).unwrap();

        // Write and index, then delete the file.
        write_and_index(
            &root,
            "will-delete.md",
            "---\ntitle: Will Delete\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nContent.\n",
        );
        let memex = open_and_reindex(&root);

        // Remove file from disk but leave DB row.
        std::fs::remove_file(root.join("wiki/will-delete.md")).unwrap();

        let report = memex.lint().unwrap();
        let missing: Vec<_> = report
            .issues
            .iter()
            .filter(|i| i.kind == crate::types::LintIssueKind::MissingFile)
            .collect();
        assert_eq!(missing.len(), 1, "should detect one missing file");
        assert_eq!(missing[0].page, "will-delete");
    }

    #[test]
    fn lint_detects_outdated_embeddings() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(root.join("wiki")).unwrap();

        write_and_index(
            &root,
            "embed-page.md",
            "---\ntitle: Embed Page\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nSome content for embedding.\n",
        );
        let memex = open_and_reindex(&root);

        // Insert a chunk with an outdated model name directly via the DB.
        let search = memex.search();
        let hash = search
            .get_document_hash("wiki/embed-page.md")
            .unwrap()
            .expect("document should have a hash");

        // Insert a content row so the FK is satisfied, then insert a chunk
        // with model="old-model".
        let embedding = crate::embed::hash_embedding("Some content for embedding.");
        search
            .with_connection(|conn| {
                crate::vector::store_chunk(
                    conn,
                    &hash,
                    99, // unique seq to avoid overwriting real chunks
                    "test chunk",
                    0,
                    10,
                    "old-model",
                    &embedding,
                )?;
                Ok(())
            })
            .unwrap();

        let report = memex.lint().unwrap();
        let outdated: Vec<_> = report
            .issues
            .iter()
            .filter(|i| i.kind == crate::types::LintIssueKind::OutdatedEmbedding)
            .collect();
        assert!(
            !outdated.is_empty(),
            "should detect outdated embedding issues"
        );
        assert!(
            outdated[0].target.contains("old-model"),
            "issue target should mention the old model name, got: {}",
            outdated[0].target
        );
        assert!(
            outdated[0]
                .target
                .contains(crate::embed::CURRENT_MODEL_NAME),
            "issue target should mention the current model name, got: {}",
            outdated[0].target
        );
    }

    #[test]
    fn is_issue_still_present_outdated_embedding_reflects_db_state() {
        use crate::types::{LintIssue, LintIssueKind};

        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(root.join("wiki")).unwrap();

        write_and_index(
            &root,
            "embed-test.md",
            "---\ntitle: Embed Test\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nSome content.\n",
        );
        let memex = open_and_reindex(&root);
        let search = memex.search();
        let hash = search
            .get_document_hash("wiki/embed-test.md")
            .unwrap()
            .expect("document should have a hash");

        // No outdated chunks yet — issue should NOT be present.
        let issue = LintIssue {
            kind: LintIssueKind::OutdatedEmbedding,
            page: "1 chunks".to_string(),
            target: format!("old-model => {}", crate::embed::CURRENT_MODEL_NAME),
        };
        let result = crate::lint::is_issue_still_present(search, &root, &issue).unwrap();
        assert!(!result, "no outdated chunks => issue not present");

        // Insert a chunk with an outdated model name.
        let embedding = crate::embed::hash_embedding("Some content.");
        search
            .with_connection(|conn| {
                crate::vector::store_chunk(
                    conn,
                    &hash,
                    99,
                    "test chunk",
                    0,
                    12,
                    "old-model",
                    &embedding,
                )?;
                Ok(())
            })
            .unwrap();

        // Now the issue SHOULD be present.
        let result = crate::lint::is_issue_still_present(search, &root, &issue).unwrap();
        assert!(result, "outdated chunk exists => issue present");
    }

    #[test]
    fn lint_no_outdated_when_model_matches() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(root.join("wiki")).unwrap();

        write_and_index(
            &root,
            "current-page.md",
            "---\ntitle: Current Page\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nContent.\n",
        );
        let memex = open_and_reindex(&root);

        // Insert a chunk with the current model name.
        let search = memex.search();
        let hash = search
            .get_document_hash("wiki/current-page.md")
            .unwrap()
            .expect("document should have a hash");

        let embedding = crate::embed::hash_embedding("Content.");
        search
            .with_connection(|conn| {
                crate::vector::store_chunk(
                    conn,
                    &hash,
                    0,
                    "test chunk",
                    0,
                    10,
                    crate::embed::CURRENT_MODEL_NAME,
                    &embedding,
                )?;
                Ok(())
            })
            .unwrap();

        let report = memex.lint().unwrap();
        let outdated: Vec<_> = report
            .issues
            .iter()
            .filter(|i| i.kind == crate::types::LintIssueKind::OutdatedEmbedding)
            .collect();
        assert!(
            outdated.is_empty(),
            "should not detect outdated embeddings when model matches"
        );
    }
}
