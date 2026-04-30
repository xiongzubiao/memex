use crate::Memex;
use crate::embed::CURRENT_MODEL_NAME;
use crate::error::Result;
use crate::types::{LintIssue, LintIssueKind, LintReport};
use crate::validate;

/// Extract the slug from any wiki document's `documents.path` field.
///
/// Wiki documents land in the DB under one of two path conventions
/// depending on the writer: `handle_write` (daemon path) stores
/// `"slug"` directly, while filesystem-driven writers (`rebuild()`,
/// the test-only `seed_wiki_page` helper, watcher reindex) store
/// `"wiki/slug.md"`. Both forms reduce to the same slug via
/// `file_stem()`. Lint normalizes through this helper so each
/// disk-vs-DB check works regardless of how the row was written.
fn slug_of_db_path(db_path: &str) -> String {
    std::path::Path::new(db_path)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string()
}

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

        // Collect DB state: all wiki documents, keyed by slug.
        let db_docs = self.search.all_wiki_documents()?;
        let db_slugs: std::collections::HashSet<String> = db_docs
            .iter()
            .map(|d| slug_of_db_path(&d.path))
            .collect();

        // Collect disk state: slug of every .md file in wiki/.
        let mut disk_slugs: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        if wiki_dir.exists() {
            for entry in std::fs::read_dir(&wiki_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "md") {
                    let stem = path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    disk_slugs.insert(stem);
                }
            }
        }

        // Check: Stale index — body hash differs from documents.hash.
        // documents.hash stores a hash of the body only (no frontmatter),
        // so we parse and hash the body to get a comparable value.
        for doc in &db_docs {
            let slug = slug_of_db_path(&doc.path);
            let full_path = wiki_dir.join(format!("{slug}.md"));
            if full_path.exists()
                && let Ok(content) = std::fs::read_to_string(&full_path) {
                    let disk_body_hash = match validate::parse_frontmatter(&content) {
                        Ok((_, body)) => crate::storage::content_hash(body.as_bytes()),
                        Err(_) => crate::storage::content_hash(content.as_bytes()),
                    };
                    if disk_body_hash != doc.hash {
                        issues.push(LintIssue {
                            kind: LintIssueKind::StaleIndex,
                            page: slug,
                            target: doc.path.clone(),
                        });
                    }
                }
        }

        // Check: Untracked file — on disk but no DB row. `page` carries
        // the slug so user-facing fix output names the page; `target`
        // carries the path used both for re-verification and for the
        // commit_doc upsert key.
        for slug in &disk_slugs {
            if !db_slugs.contains(slug) {
                issues.push(LintIssue {
                    kind: LintIssueKind::UntrackedFile,
                    page: slug.clone(),
                    target: format!("wiki/{slug}.md"),
                });
            }
        }

        // Check: Missing file — DB row but no file on disk. `target`
        // preserves the DB-stored path so the fix-flow can look it up.
        for doc in &db_docs {
            let slug = slug_of_db_path(&doc.path);
            if !disk_slugs.contains(&slug) {
                issues.push(LintIssue {
                    kind: LintIssueKind::MissingFile,
                    page: slug,
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

        // Check: raw hash-vs-path integrity. Raw files are content-
        // addressed — the body's sha256 IS the filename. An external
        // edit that changes the body without renaming the file leaves
        // the index pointing at a name that no longer matches the
        // bytes. Detect and surface; `lint --fix` renames the file to
        // its new body hash.
        for doc in self.search.all_raw_documents()? {
            let abs = self.root().join(&doc.path);
            let Ok(file) = std::fs::read_to_string(&abs) else {
                // File missing — different issue (we don't model that
                // for raw yet; reconcile would remove the row).
                continue;
            };
            let body = match crate::raw::parse_raw_frontmatter(&file) {
                Ok((_, b)) => b.to_string(),
                // No parseable frontmatter — treat the whole file as body.
                Err(_) => file,
            };
            let body_hash = crate::storage::content_hash(body.as_bytes());
            if body_hash != doc.hash {
                issues.push(LintIssue {
                    kind: LintIssueKind::RawHashMismatch,
                    page: doc.path.clone(),
                    target: body_hash,
                });
            }
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

        // Build a single RegexSet covering ALL stem + title patterns.
        // Each page contributes 2 patterns (stem-as-words, title); the
        // pattern_target parallel array maps the pattern index back to
        // the target stem so we can attribute matches. This reduces
        // missing-link from O(n²) scans (one regex pair built and run
        // per page-pair, ~250K pair checks for n=500) to O(n) scans
        // (one set.matches(body) per page); the set is built once.
        let mut patterns: Vec<String> = Vec::with_capacity(pages.len() * 2);
        let mut pattern_target: Vec<&str> = Vec::with_capacity(pages.len() * 2);
        for (other_stem, _, _, other_title_lower) in &pages {
            let stem_words = other_stem.replace('-', " ").to_lowercase();
            patterns.push(format!(r"(?i)\b{}\b", regex::escape(&stem_words)));
            pattern_target.push(other_stem.as_str());
            patterns.push(format!(r"(?i)\b{}\b", regex::escape(other_title_lower)));
            pattern_target.push(other_stem.as_str());
        }
        // RegexSet::new can fail when the combined pattern size exceeds
        // the regex crate's compiled-size limit (~10 MB by default,
        // hit in practice with very large wikis). Don't silently drop
        // ALL missing-link checks on failure — log so the user knows
        // why their lint output is incomplete. The set is None in that
        // case and the missing-link check below short-circuits.
        let cross_link_set = match regex::RegexSet::new(&patterns) {
            Ok(set) => Some(set),
            Err(e) => {
                tracing::warn!(
                    pattern_count = patterns.len(),
                    error = %e,
                    "lint: missing-link cross-reference check disabled — failed to compile regex set"
                );
                None
            }
        };

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

            // Check: Missing cross-references — single RegexSet scan.
            // Dedupe per (page, target) since stem-pattern and
            // title-pattern can both match the same target.
            if let Some(set) = &cross_link_set {
                let mut seen: std::collections::HashSet<&str> =
                    std::collections::HashSet::new();
                for idx in set.matches(body).iter() {
                    let target = pattern_target[idx];
                    if target == stem.as_str() {
                        continue;
                    }
                    if links.iter().any(|l| l == target) {
                        continue;
                    }
                    if !seen.insert(target) {
                        continue;
                    }
                    issues.push(LintIssue {
                        kind: LintIssueKind::MissingLink,
                        page: stem.clone(),
                        target: target.to_string(),
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
            // Stale means on-disk body hash != stored hash. Re-read both.
            let full_path = root.join(&issue.target);
            let Ok(content) = std::fs::read_to_string(&full_path) else {
                return Ok(false); // file gone; nothing to fix
            };
            let actual = match validate::parse_frontmatter(&content) {
                Ok((_, body)) => crate::storage::content_hash(body.as_bytes()),
                Err(_) => crate::storage::content_hash(content.as_bytes()),
            };
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
        LintIssueKind::MissingFile => {
            // Present iff: file still gone AND a row still exists for it.
            // If someone manually deleted the row, the fix would no-op anyway,
            // but checking both keeps the "stale" path honest.
            let full_path = root.join(&issue.target);
            if full_path.exists() {
                return Ok(false);
            }
            Ok(search.get_document_hash(&issue.target)?.is_some())
        }
        LintIssueKind::UntrackedFile => {
            // Present iff: file still on disk AND no DB row matches the
            // file's slug. Slug-vs-path matching mirrors lint() — the
            // daemon write path stores `path="<slug>"` while reindex stores
            // `path="wiki/<slug>.md"`; both reduce to the same slug.
            let full_path = root.join(&issue.target);
            if !full_path.exists() {
                return Ok(false);
            }
            let slug = std::path::Path::new(&issue.target)
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let already_indexed = search
                .all_wiki_documents()?
                .iter()
                .any(|d| slug_of_db_path(&d.path) == slug);
            Ok(!already_indexed)
        }
        LintIssueKind::RawHashMismatch => {
            // issue.page is the DB-relative raw path; the row is still
            // present iff the on-disk body hash still differs from the
            // stored hash. Re-read both under the writer's snapshot.
            let abs = root.join(&issue.page);
            let Ok(content) = std::fs::read_to_string(&abs) else {
                return Ok(false);
            };
            let body = match crate::raw::parse_raw_frontmatter(&content) {
                Ok((_, b)) => b.to_string(),
                Err(_) => content,
            };
            let actual = crate::storage::content_hash(body.as_bytes());
            let stored = search.get_document_hash(&issue.page)?.unwrap_or_default();
            Ok(actual != stored)
        }
        // Report-only kinds: always "present". Link issues need LLM
        // judgment to fix correctly (the LLM has to decide between
        // rewrite, point-at-correct-slug, restore-deleted-page, or
        // create-new-page), so lint never auto-fixes them.
        LintIssueKind::DanglingLink | LintIssueKind::MissingLink => Ok(true),
    }
}

/// Apply a single lint fix to the provided `&Bm25Search`.
///
/// Auto-fix semantics (passive sync only — no auto-cross-linking;
/// authoring belongs to `memex write` / `memex ingest`):
/// - `StaleIndex` and `UntrackedFile`: re-parse the file and commit_doc
///   it as a wiki document. Same physical fix, different starting state.
/// - `MissingFile`: file is gone; remove the documents row + chunks + FTS.
/// - `OutdatedEmbedding`: re-embed under the current model.
/// - `RawHashMismatch`: rename the file to its new content-addressed
///   path, drop old chunks, re-embed at the new hash.
///
/// Link issues (`DanglingLink`, `MissingLink`) are report-only — fixing
/// them requires LLM judgment and is left to the caller.
pub(crate) fn apply_fix_inner(
    search: &Bm25Search,
    root: &std::path::Path,
    issue: &LintIssue,
) -> crate::error::Result<()> {
    match issue.kind {
        LintIssueKind::StaleIndex | LintIssueKind::UntrackedFile => {
            fix_wiki_reindex(search, root, issue)
        }
        LintIssueKind::OutdatedEmbedding => fix_outdated_embedding(search, root),
        LintIssueKind::MissingFile => fix_missing_file(search, issue),
        LintIssueKind::RawHashMismatch => fix_raw_hash_mismatch(search, root, issue),
        // Link issues are LLM-judgment only; never auto-fixed by lint.
        LintIssueKind::DanglingLink | LintIssueKind::MissingLink => Ok(()),
    }
}

/// `StaleIndex` and `UntrackedFile` share one physical fix: read the
/// file off disk, commit_doc as wiki, set collections, embed.
/// `StaleIndex` upserts an existing row; `UntrackedFile` inserts.
fn fix_wiki_reindex(
    search: &Bm25Search,
    root: &std::path::Path,
    issue: &LintIssue,
) -> crate::error::Result<()> {
    let full_path = root.join(&issue.target);
    let content = std::fs::read_to_string(&full_path)?;
    let (title, body, tags, _summary, collections) =
        crate::search::parse_page_for_indexing(&content).ok_or_else(|| {
            crate::error::MemexError::ValidationFailure {
                details: format!("page {} has no valid frontmatter", issue.target),
            }
        })?;
    let mtime = crate::storage::file_mtime_iso(&full_path);
    let size = content.len() as i64;
    let result = search.with_transaction(|tx| {
        crate::search::commit_doc(
            tx,
            &crate::search::DocSpec {
                doc_type: "wiki",
                path: &issue.target,
                title: &title,
                tags: &tags,
                source: None,
                mtime: &mtime,
                body: &body,
                size,
            },
        )
    })?;
    search.set_document_collections_by_path("wiki", &issue.target, &collections)?;
    let mut model = crate::retrieval::load_default_model()?;
    // embed_document atomically stamps embed_model + embedded_at by
    // hash inside its own tx. Without that stamping (an earlier shape
    // had a separate UPDATE in this fn) a freshly-fixed row stayed at
    // its old embed_model and lint kept re-flagging it.
    crate::retrieval::embed_document(search, &result.body_hash, &title, &body, &mut model)?;
    Ok(())
}

/// Re-embed every documents row whose `embed_model` differs from the
/// current model. Iterates by hash since `chunks` are hash-keyed; one
/// embed_document call refreshes every row that shared the body.
fn fix_outdated_embedding(
    search: &Bm25Search,
    root: &std::path::Path,
) -> crate::error::Result<()> {
    let mut model = crate::retrieval::load_default_model()?;
    let outdated = search.outdated_chunk_hashes(crate::embed::CURRENT_MODEL_NAME)?;
    for hash in &outdated {
        // Body lives on disk; locate the page via documents.path.
        let path = match search.path_by_hash(hash)? {
            Some(p) => p,
            None => continue,
        };
        let full_path = root.join(&path);
        let Ok(content) = std::fs::read_to_string(&full_path) else {
            continue;
        };
        let (title, body) = match crate::validate::parse_frontmatter(&content) {
            Ok((fm, b)) => (fm.title, b.to_string()),
            Err(_) => (String::new(), content.clone()),
        };
        // embed_document stamps every row with this hash, so dedup'd
        // raw rows all get refreshed in one shot and the loop
        // terminates instead of re-flagging on the next pass.
        crate::retrieval::embed_document(search, hash, &title, &body, &mut model)?;
    }
    Ok(())
}

/// Drop a documents row whose file is gone. `delete_document_with_cleanup`
/// is path-keyed; `issue.target` preserves the DB-stored path so this
/// matches whichever convention the row used (`<slug>` or
/// `wiki/<slug>.md`).
fn fix_missing_file(
    search: &Bm25Search,
    issue: &LintIssue,
) -> crate::error::Result<()> {
    search.delete_document_with_cleanup(&issue.target)?;
    Ok(())
}

/// Repair a raw file whose body hash no longer matches the hash
/// encoded in its filename. Two sub-cases:
/// - the canonical destination file already exists (a duplicate-content
///   collision after a manual edit) — drop the duplicate file + row
/// - normal rename: move the file to its new content-addressed path,
///   commit the new row, drop the old row and its chunks
fn fix_raw_hash_mismatch(
    search: &Bm25Search,
    root: &std::path::Path,
    issue: &LintIssue,
) -> crate::error::Result<()> {
    let old_rel = &issue.page;
    let new_hash = &issue.target;
    let old_abs = root.join(old_rel);
    let file = std::fs::read_to_string(&old_abs)?;
    let (fm, body) = match crate::raw::parse_raw_frontmatter(&file) {
        Ok((fm, b)) => (Some(fm), b.to_string()),
        Err(_) => (None, file.clone()),
    };
    // Re-verify under the writer's snapshot — between detection and
    // fix the file may have been edited again.
    let actual = crate::storage::content_hash(body.as_bytes());
    if &actual != new_hash {
        return Err(crate::error::MemexError::ValidationFailure {
            details: format!(
                "raw mismatch resolved before fix could run: {old_rel} now hashes to {actual}, not {new_hash}"
            ),
        });
    }
    let raw_dir = root.join("raw");
    let new_abs = crate::raw::raw_path_for_hash(&raw_dir, new_hash);
    if let Some(parent) = new_abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if new_abs.exists() {
        return fix_raw_hash_duplicate(search, old_rel, &old_abs);
    }
    let title = fm.as_ref().and_then(|f| f.title.clone()).unwrap_or_default();
    let source = fm.as_ref().and_then(|f| f.source.clone());
    fix_raw_hash_rename(search, root, old_rel, &old_abs, &new_abs, new_hash, &title, source.as_deref(), &body)
}

/// `new_abs` already exists — the body is canonical under another
/// path, usually because two raw files converged on the same content
/// after a manual edit. Renaming would clobber the canonical file and
/// commit_doc would overwrite the canonical row's metadata. Drop the
/// duplicate row + file; canonical row untouched.
///
/// Tx first, then file removal. The reverse ordering's failure mode
/// would leave a dangling DB row pointing at a missing file — and lint
/// doesn't model "missing raw file" (only reconcile does), so the row
/// would survive until the next reconcile. This ordering's failure mode
/// is recoverable: tx succeeds, remove fails, leaves an orphan file at
/// the OLD path; reconcile re-indexes it under that path, lint detects
/// RawHashMismatch again, and the fix replays (the second tx is a no-op
/// delete).
fn fix_raw_hash_duplicate(
    search: &Bm25Search,
    old_rel: &str,
    old_abs: &std::path::Path,
) -> crate::error::Result<()> {
    search.with_transaction(|tx| drop_raw_row_and_chunks(tx, old_rel))?;
    if let Err(e) = std::fs::remove_file(old_abs) {
        // Tx already committed, can't undo it.
        tracing::warn!(
            path = %old_abs.display(),
            error = %e,
            "lint --fix: tx done but duplicate raw file removal failed; \
             reconcile will re-index and lint will retry"
        );
    }
    Ok(())
}

/// Rename the raw file to its content-addressed path, commit the new
/// row, drop the old row + chunks. If the DB tx fails after the rename,
/// roll the file back so disk and DB stay consistent.
#[allow(clippy::too_many_arguments)]
fn fix_raw_hash_rename(
    search: &Bm25Search,
    root: &std::path::Path,
    old_rel: &str,
    old_abs: &std::path::Path,
    new_abs: &std::path::Path,
    new_hash: &str,
    title: &str,
    source: Option<&str>,
    body: &str,
) -> crate::error::Result<()> {
    std::fs::rename(old_abs, new_abs)?;
    let new_rel = crate::storage::rel_path_string(
        new_abs.strip_prefix(root).unwrap_or(new_abs),
    );
    let mtime = crate::storage::file_mtime_iso(new_abs);
    let size = std::fs::metadata(new_abs).map(|m| m.len() as i64).unwrap_or(0);
    let tx_result = search.with_transaction(|tx| {
        crate::search::commit_doc(
            tx,
            &crate::search::DocSpec {
                doc_type: "raw",
                path: &new_rel,
                title,
                tags: "",
                source,
                mtime: &mtime,
                body,
                size,
            },
        )?;
        // commit_doc only drops chunks when the path-keyed upsert
        // finds a prior row at the SAME path. We're inserting at
        // new_rel (a different path), so the old hash's chunks are
        // still around — drop the old row and its chunks together.
        drop_raw_row_and_chunks(tx, old_rel)?;
        Ok(())
    });
    if let Err(e) = tx_result {
        // DB transaction failed; restore the file so disk + DB
        // remain in sync.
        if let Err(re) = std::fs::rename(new_abs, old_abs) {
            tracing::error!(
                file_at = %new_abs.display(),
                db_row_at = %old_rel,
                tx_error = %e,
                rollback_error = %re,
                "lint --fix: DB tx failed AND rename rollback failed; \
                 filesystem and DB are now inconsistent until next reconcile"
            );
        }
        return Err(e);
    }
    // Re-embed at the new hash. Failure here is non-fatal: the file
    // is renamed, the BM25 index is consistent, only the semantic-
    // search vectors are missing. Reconcile / lint --fix
    // OutdatedEmbedding will catch up. Log and continue rather than
    // returning Err — that would make the caller think the rename
    // itself failed.
    match crate::retrieval::load_default_model() {
        Ok(mut model) => {
            if let Err(e) = crate::retrieval::embed_document(
                search, new_hash, title, body, &mut model,
            ) {
                tracing::warn!(
                    path = %new_rel,
                    error = %e,
                    "lint --fix: re-embed failed for renamed raw \
                     (BM25 index intact; reconcile will retry)"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                path = %new_rel,
                error = %e,
                "lint --fix: embedding model unavailable post-rename \
                 (BM25 index intact; reconcile will retry)"
            );
        }
    }
    Ok(())
}

/// Common subroutine: read a raw documents row by path, FTS-delete its
/// inverted index entry, drop hash-keyed chunks if no other doc shares
/// the hash, and DELETE the row. Used by both the duplicate and rename
/// branches of RawHashMismatch.
///
/// FTS5 contentless-table delete needs the original column values;
/// passing empty strings would leave ghost tokens. chunks/chunks_vec
/// have no FK cascade from documents, so the hash-keyed sweep is
/// explicit. The reference count guards against wiping a sibling row's
/// chunks when two docs share a body.
fn drop_raw_row_and_chunks(
    tx: &rusqlite::Connection,
    old_rel: &str,
) -> crate::error::Result<()> {
    let prior: Option<(i64, String, String, String)> = tx
        .query_row(
            "SELECT id, title, tags, hash FROM documents WHERE doc_type='raw' AND path=?1",
            [old_rel],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok();
    if let Some((id, prior_title, prior_tags, prior_hash)) = prior.as_ref() {
        let _ = tx.execute(
            "INSERT INTO documents_fts(documents_fts, rowid, path, title, tags, body) \
             VALUES('delete', ?1, ?2, ?3, ?4, '')",
            rusqlite::params![id, old_rel, prior_title, prior_tags],
        );
        let other_refs: i64 = tx.query_row(
            "SELECT COUNT(*) FROM documents WHERE hash = ?1 AND id != ?2",
            rusqlite::params![prior_hash, id],
            |r| r.get(0),
        )?;
        if other_refs == 0 {
            crate::vector::delete_chunks(tx, prior_hash)?;
        }
    }
    tx.execute(
        "DELETE FROM documents WHERE doc_type = 'raw' AND path = ?1",
        [old_rel],
    )?;
    Ok(())
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

    /// Fixture embedding used only to populate chunk rows in lint tests.
    /// The tests assert on model names, not on similarity, so any non-empty
    /// 768-dim vector suffices.
    fn fixture_embedding() -> Vec<f32> {
        vec![0.1f32; crate::embed::EMBEDDING_DIM]
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

    /// Regression: a freshly indexed wiki page must NOT be flagged as
    /// stale-index. The bug here was that the body extractor used by the
    /// writer (`commit_doc` → `storage::split_frontmatter`) did not match
    /// the extractor used by lint detection (`validate::parse_frontmatter`,
    /// previously `.trim()`d the body), so the on-disk hash never matched
    /// the stored hash for files with trailing whitespace.
    #[test]
    fn lint_does_not_flag_freshly_indexed_page_as_stale() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(root.join("wiki")).unwrap();

        // Canonical file with trailing newline — the case that exposed the bug.
        write_and_index(
            &root,
            "fresh.md",
            "---\ntitle: Fresh\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nbody content\n",
        );
        let memex = open_and_reindex(&root);

        let report = memex.lint().unwrap();
        let stale: Vec<_> = report
            .issues
            .iter()
            .filter(|i| i.kind == crate::types::LintIssueKind::StaleIndex)
            .collect();
        assert!(
            stale.is_empty(),
            "freshly indexed page must not be flagged stale; got {stale:?}"
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

    /// Regression: when the DB stores a wiki document under the slug-only
    /// path convention used by `handle_write` (e.g. `path = "test"`, not
    /// `path = "wiki/test.md"`), and the matching file exists on disk,
    /// lint must NOT report both `untracked` and `missing-file`. Both checks
    /// now normalize through `slug_of_db_path`.
    #[test]
    fn lint_clean_when_db_path_stored_as_slug_only() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(root.join("wiki")).unwrap();

        // Write file + reindex. Reindex stores path = "wiki/test.md".
        write_and_index(
            &root,
            "test.md",
            "---\ntitle: Test\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nbody\n",
        );
        let memex = open_and_reindex(&root);

        // Rewrite the path column to the slug-only form (the daemon's
        // `handle_write` writes documents this way).
        memex
            .search()
            .with_connection(|conn| {
                let n = conn
                    .execute(
                        "UPDATE documents SET path = 'test' WHERE doc_type = 'wiki' AND path = 'wiki/test.md'",
                        [],
                    )
                    .unwrap();
                assert_eq!(n, 1, "expected to rewrite one wiki row");
                Ok(())
            })
            .unwrap();

        let report = memex.lint().unwrap();
        let untracked = report
            .issues
            .iter()
            .filter(|i| i.kind == crate::types::LintIssueKind::UntrackedFile)
            .count();
        let missing = report
            .issues
            .iter()
            .filter(|i| i.kind == crate::types::LintIssueKind::MissingFile)
            .count();
        assert_eq!(untracked, 0, "DB row exists for the file; nothing should be untracked");
        assert_eq!(missing, 0, "file exists for the DB row; nothing should be missing");
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

    /// Set `documents.embed_model` for the given path; in the new schema
    /// stale-embedding detection lives on the document, not on chunks.
    fn set_embed_model(search: &crate::search::Bm25Search, path: &str, model: &str) {
        search
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE documents SET embed_model = ?1 WHERE path = ?2",
                    rusqlite::params![model, path],
                )
                .unwrap();
                Ok(())
            })
            .unwrap();
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

        // Mark the document as embedded with an old model.
        let search = memex.search();
        let _ = search
            .get_document_hash("wiki/embed-page.md")
            .unwrap()
            .expect("document should have a hash");
        set_embed_model(search, "wiki/embed-page.md", "old-model");

        // Use fixture_embedding only to keep the helper alive in case future
        // tests need it; this test asserts at the document-level metadata.
        let _ = fixture_embedding();

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

    /// Without the embed_model stamp post-fix, the OutdatedEmbedding
    /// fix path was an infinite loop: re-embed regenerated chunks but
    /// left documents.embed_model at "old-model", so the next lint
    /// pass detected the same issue. Stamp must move to current after
    /// successful embed.
    #[test]
    fn apply_fix_outdated_embedding_clears_the_issue() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(root.join("wiki")).unwrap();

        write_and_index(
            &root,
            "stale.md",
            "---\ntitle: Stale\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nstale body\n",
        );
        let memex = open_and_reindex(&root);
        let search = memex.search();
        let hash = search
            .get_document_hash("wiki/stale.md")
            .unwrap()
            .expect("indexed doc has hash");
        // Seed a chunk + mark embed_model as old so OutdatedEmbedding fires.
        memex
            .search()
            .with_transaction(|tx| {
                crate::vector::store_chunk(
                    tx,
                    &hash,
                    0,
                    0,
                    11,
                    &vec![0.1f32; crate::embed::EMBEDDING_DIM],
                )?;
                Ok(())
            })
            .unwrap();
        set_embed_model(search, "wiki/stale.md", "old-model");

        // Pre-fix: lint reports OutdatedEmbedding.
        let pre = memex.lint().unwrap();
        assert!(
            pre.issues
                .iter()
                .any(|i| i.kind == crate::types::LintIssueKind::OutdatedEmbedding),
            "test setup: should detect outdated embedding"
        );

        // Apply the fix.
        let issue = crate::types::LintIssue {
            kind: crate::types::LintIssueKind::OutdatedEmbedding,
            page: "wiki/stale.md".into(),
            target: format!("old-model -> {}", crate::embed::CURRENT_MODEL_NAME),
        };
        crate::lint::apply_fix_inner(memex.search(), &root, &issue).unwrap();

        // Post-fix: embed_model is now current, lint must NOT re-flag.
        let post = memex.lint().unwrap();
        let still_outdated: Vec<_> = post
            .issues
            .iter()
            .filter(|i| i.kind == crate::types::LintIssueKind::OutdatedEmbedding)
            .collect();
        assert!(
            still_outdated.is_empty(),
            "fix must clear OutdatedEmbedding, got {still_outdated:?}"
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
        let _ = search
            .get_document_hash("wiki/embed-test.md")
            .unwrap()
            .expect("document should have a hash");

        // No outdated documents yet — issue should NOT be present.
        let issue = LintIssue {
            kind: LintIssueKind::OutdatedEmbedding,
            page: "1 chunks".to_string(),
            target: format!("old-model => {}", crate::embed::CURRENT_MODEL_NAME),
        };
        let result = crate::lint::is_issue_still_present(search, &root, &issue).unwrap();
        assert!(!result, "no outdated docs => issue not present");

        set_embed_model(search, "wiki/embed-test.md", "old-model");

        // Now the issue SHOULD be present.
        let result = crate::lint::is_issue_still_present(search, &root, &issue).unwrap();
        assert!(result, "outdated doc exists => issue present");
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

        let search = memex.search();
        let _ = search
            .get_document_hash("wiki/current-page.md")
            .unwrap()
            .expect("document should have a hash");
        set_embed_model(search, "wiki/current-page.md", crate::embed::CURRENT_MODEL_NAME);

        let _ = fixture_embedding();

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

    /// Index a raw file at its canonical content-addressed path, modify
    /// the body on disk without renaming, and assert lint reports a
    /// `RawHashMismatch` whose `target` is the recomputed body hash.
    #[test]
    fn lint_detects_raw_hash_mismatch() {
        use crate::raw::{RawFrontmatter, assemble_raw_file, raw_path_for_hash};

        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let body = "# Auth\n\noriginal body";
        let original_hash = crate::storage::content_hash(body.as_bytes());
        let raw_path = raw_path_for_hash(&memex.raw_dir(), &original_hash);
        std::fs::create_dir_all(raw_path.parent().unwrap()).unwrap();
        let fm = RawFrontmatter {
            source: Some("https://x/p".into()),
            title: Some("Auth".into()),
            ..Default::default()
        };
        std::fs::write(&raw_path, assemble_raw_file(&fm, body)).unwrap();
        let outcome = crate::index_raw::index_raw_file(&memex, &raw_path, None).unwrap();
        assert_eq!(outcome, crate::index_raw::IndexOutcome::Inserted);

        // Mutate the body on disk; the filename still encodes the OLD hash.
        let new_body = "# Auth\n\nedited body, longer than before";
        std::fs::write(&raw_path, assemble_raw_file(&fm, new_body)).unwrap();
        let new_hash = crate::storage::content_hash(new_body.as_bytes());
        assert_ne!(original_hash, new_hash, "test setup: bodies must differ");

        let report = memex.lint().unwrap();
        let mismatches: Vec<_> = report
            .issues
            .iter()
            .filter(|i| i.kind == crate::types::LintIssueKind::RawHashMismatch)
            .collect();
        assert_eq!(mismatches.len(), 1, "should detect one raw mismatch");
        assert_eq!(mismatches[0].target, new_hash);
    }

    #[test]
    fn is_issue_still_present_raw_hash_mismatch_reflects_disk_state() {
        use crate::raw::{RawFrontmatter, assemble_raw_file, raw_path_for_hash};
        use crate::types::{LintIssue, LintIssueKind};

        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let body = "# T\n\nbody";
        let original_hash = crate::storage::content_hash(body.as_bytes());
        let raw_path = raw_path_for_hash(&memex.raw_dir(), &original_hash);
        std::fs::create_dir_all(raw_path.parent().unwrap()).unwrap();
        let fm = RawFrontmatter {
            title: Some("T".into()),
            ..Default::default()
        };
        std::fs::write(&raw_path, assemble_raw_file(&fm, body)).unwrap();
        crate::index_raw::index_raw_file(&memex, &raw_path, None).unwrap();
        let rel = raw_path
            .strip_prefix(memex.root())
            .unwrap()
            .to_string_lossy()
            .to_string();

        // Disk matches DB → issue not present.
        let issue = LintIssue {
            kind: LintIssueKind::RawHashMismatch,
            page: rel.clone(),
            target: "deadbeef".to_string(),
        };
        let result =
            crate::lint::is_issue_still_present(memex.search(), memex.root(), &issue).unwrap();
        assert!(!result, "matching disk hash => issue not present");

        // Mutate body → issue is present again.
        std::fs::write(&raw_path, assemble_raw_file(&fm, "different body")).unwrap();
        let result =
            crate::lint::is_issue_still_present(memex.search(), memex.root(), &issue).unwrap();
        assert!(result, "mutated body => issue present");
    }

    /// `apply_fix_inner` on a MissingFile issue removes the documents row
    /// (plus chunks + FTS). After fix, lint should not re-report it.
    #[test]
    fn apply_fix_inner_removes_missing_file_row() {
        use crate::types::{LintIssue, LintIssueKind};

        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(root.join("wiki")).unwrap();
        write_and_index(
            &root,
            "ghost.md",
            "---\ntitle: Ghost\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nGhost body.\n",
        );
        let memex = open_and_reindex(&root);
        // Confirm the row exists.
        assert!(
            memex.search().get_document_hash("wiki/ghost.md").unwrap().is_some(),
            "row should exist post-reindex"
        );
        // Delete the file off disk.
        std::fs::remove_file(root.join("wiki/ghost.md")).unwrap();

        let issue = LintIssue {
            kind: LintIssueKind::MissingFile,
            page: "ghost".into(),
            target: "wiki/ghost.md".into(),
        };
        crate::lint::apply_fix_inner(memex.search(), &root, &issue).unwrap();
        assert!(
            memex.search().get_document_hash("wiki/ghost.md").unwrap().is_none(),
            "row should be gone after MissingFile fix"
        );

        // And lint should now be clean.
        let report = memex.lint().unwrap();
        let missing: Vec<_> = report
            .issues
            .iter()
            .filter(|i| i.kind == LintIssueKind::MissingFile)
            .collect();
        assert!(missing.is_empty(), "no MissingFile issues post-fix");
    }

    /// Two raw files converge on the same body hash after a manual edit:
    /// `lint --fix` must not clobber the existing canonical file or its
    /// row. The duplicate file gets removed, the canonical row stays.
    #[test]
    fn apply_fix_does_not_clobber_existing_canonical_raw() {
        use crate::raw::{RawFrontmatter, assemble_raw_file, raw_path_for_hash};
        use crate::types::{LintIssue, LintIssueKind};

        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let raw_dir = memex.raw_dir();

        // Canonical file B: title "Original" at body hash H.
        let body = "shared body content";
        let canonical_hash = crate::storage::content_hash(body.as_bytes());
        let canonical_path = raw_path_for_hash(&raw_dir, &canonical_hash);
        std::fs::create_dir_all(canonical_path.parent().unwrap()).unwrap();
        std::fs::write(
            &canonical_path,
            assemble_raw_file(
                &RawFrontmatter {
                    title: Some("Original".into()),
                    source: Some("https://x/original".into()),
                    ..Default::default()
                },
                body,
            ),
        )
        .unwrap();
        crate::index_raw::index_raw_file(&memex, &canonical_path, None).unwrap();
        let canonical_rel = canonical_path
            .strip_prefix(memex.root())
            .unwrap()
            .to_string_lossy()
            .to_string();

        // Stale file A: title "Edited" at OTHER body hash, but the body
        // on disk has been edited to match B's content (same hash H).
        let stale_hash = "00".repeat(32);
        let stale_path = raw_path_for_hash(&raw_dir, &stale_hash);
        std::fs::create_dir_all(stale_path.parent().unwrap()).unwrap();
        std::fs::write(
            &stale_path,
            assemble_raw_file(
                &RawFrontmatter {
                    title: Some("Edited".into()),
                    source: Some("https://x/edited".into()),
                    ..Default::default()
                },
                body,
            ),
        )
        .unwrap();
        // Insert a row for the stale file at its OLD path with the OLD
        // hash (the hash before the manual edit). commit_doc recomputes
        // hash from body, so use raw SQL to capture the production state
        // where the row's hash column is stale relative to disk content.
        let stale_rel_for_insert = stale_path
            .strip_prefix(memex.root())
            .unwrap()
            .to_string_lossy()
            .to_string();
        let old_body_hash = "feedface".to_string() + &"de".repeat(28);
        memex
            .search()
            .with_transaction(|tx| {
                tx.execute(
                    "INSERT INTO documents (doc_type, path, title, hash, tags, source, mtime, size) \
                     VALUES ('raw', ?1, 'Edited', ?2, '', 'https://x/edited', '2026-04-29T00:00:00Z', ?3)",
                    rusqlite::params![&stale_rel_for_insert, &old_body_hash, body.len() as i64],
                )?;
                // Seed a chunk at the OLD hash so the test can verify
                // the chunks-leak fix actually deletes it.
                let dummy_embedding = vec![0.0f32; 768];
                crate::vector::store_chunk(tx, &old_body_hash, 0, 0, body.len(), &dummy_embedding)?;
                Ok(())
            })
            .unwrap();

        let stale_rel = stale_path
            .strip_prefix(memex.root())
            .unwrap()
            .to_string_lossy()
            .to_string();
        let issue = LintIssue {
            kind: LintIssueKind::RawHashMismatch,
            page: stale_rel.clone(),
            target: canonical_hash.clone(),
        };
        crate::lint::apply_fix_inner(memex.search(), memex.root(), &issue).unwrap();

        // Stale file removed; canonical file untouched.
        assert!(!stale_path.exists(), "stale duplicate file removed");
        assert!(canonical_path.exists(), "canonical file preserved");
        // Stale row gone.
        assert!(
            memex.search().get_document_hash(&stale_rel).unwrap().is_none(),
            "stale row removed"
        );
        // Canonical row still has its ORIGINAL title — not clobbered by
        // the renamed file's metadata.
        let canonical_title: String = memex
            .search()
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT title FROM documents WHERE doc_type='raw' AND path=?1",
                    [&canonical_rel],
                    |r| r.get::<_, String>(0),
                )
                .map_err(Into::into)
            })
            .unwrap();
        assert_eq!(
            canonical_title, "Original",
            "canonical title must not be overwritten by duplicate's metadata"
        );
        // Chunks for the OLD hash must be gone — chunks/chunks_vec are
        // hash-keyed and have no FK cascade from documents, so deleting
        // the row alone leaks vectors that vector_search would still
        // return.
        let leaked: i64 = memex
            .search()
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM chunks WHERE hash=?1",
                    [&old_body_hash],
                    |r| r.get::<_, i64>(0),
                )
                .map_err(Into::into)
            })
            .unwrap();
        assert_eq!(leaked, 0, "old-hash chunks must be cleaned, not leaked");
    }

    /// `is_issue_still_present` on UntrackedFile returns false once the
    /// row is reindexed (the fix path's effect) — locks in re-verification.
    #[test]
    fn is_issue_still_present_untracked_clears_after_indexing() {
        use crate::types::{LintIssue, LintIssueKind};

        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(root.join("wiki")).unwrap();
        // Drop a file on disk; do NOT index.
        std::fs::write(
            root.join("wiki/orphan.md"),
            "---\ntitle: Orphan\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nOrphan body.\n",
        )
        .unwrap();
        let memex = crate::Memex::open_writer(root.clone()).unwrap();

        let issue = LintIssue {
            kind: LintIssueKind::UntrackedFile,
            page: "orphan".into(),
            target: "wiki/orphan.md".into(),
        };
        // Before indexing: issue is present.
        let before =
            crate::lint::is_issue_still_present(memex.search(), &root, &issue).unwrap();
        assert!(before, "untracked file pre-index => issue present");

        // After reindex: row exists, issue is gone.
        memex.reindex().unwrap();
        let after =
            crate::lint::is_issue_still_present(memex.search(), &root, &issue).unwrap();
        assert!(!after, "indexed file => issue gone");
    }
}
