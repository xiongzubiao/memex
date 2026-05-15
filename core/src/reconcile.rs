use std::collections::HashSet;
use std::path::Path;

use walkdir::WalkDir;

use crate::Memex;
use crate::embed::Embedder;
use crate::error::{MemexError, Result};

#[derive(Debug, Clone, Copy, Default)]
pub struct ReconcileOptions {
    pub force: bool,
}

#[derive(Debug, Default)]
pub struct ReconcileReport {
    pub indexed: usize,
    pub skipped: usize,
    pub deleted: usize,
    pub skipped_symlinks: usize,
    pub hash_mismatches: usize,
}

/// Output of `reconcile_walk`: the file lists the caller should index
/// + the doc-row paths it should delete. Lets the daemon chunk the
/// per-file indexing so the embed-model lock isn't held for an entire
/// long reconcile sweep.
#[derive(Debug, Default)]
pub struct ReconcilePlan {
    pub wiki_paths: Vec<std::path::PathBuf>,
    pub raw_paths: Vec<std::path::PathBuf>,
    pub to_delete: Vec<String>,
    pub skipped_symlinks: usize,
}

/// Build a reconcile plan: walk wiki+raw, compute the set of stale
/// `documents` paths to delete, and apply the same safety threshold as
/// `reconcile`. Caller is responsible for actually invoking
/// `index_wiki_file` / `index_raw_file` per path and
/// `delete_document_with_cleanup` per stale path. The split lets the
/// caller release the embed-model lock between batches of files
/// during long sweeps.
pub fn reconcile_walk(memex: &Memex, opts: ReconcileOptions) -> Result<ReconcilePlan> {
    let mut plan = ReconcilePlan::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut walk_report = ReconcileReport::default();
    plan.wiki_paths = walk_dir(
        &memex.wiki_dir(),
        "wiki",
        memex,
        &mut seen,
        &mut walk_report,
    )?;
    plan.raw_paths = walk_dir(&memex.raw_dir(), "raw", memex, &mut seen, &mut walk_report)?;
    plan.skipped_symlinks = walk_report.skipped_symlinks;

    let to_delete: Vec<String> = memex.search().with_connection(|conn| {
        let mut s = conn.prepare("SELECT path FROM documents")?;
        let paths: Vec<String> = s
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .filter(|p| !seen.contains(p))
            .collect();
        Ok(paths)
    })?;

    let total_existing: i64 = memex.search().with_connection(|c| {
        Ok(c.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?)
    })?;
    let threshold = std::cmp::max(10, (total_existing as f64 * 0.5) as i64) as usize;
    if !opts.force && to_delete.len() > threshold {
        return Err(MemexError::Other(anyhow::anyhow!(
            "reconciliation would delete {} documents ({}% of {}). \
             Refusing. Possible causes: wiki/raw unmounted, sync moved files away, accidental rm. \
             Investigate the missing files (e.g. remount or restore) and restart the daemon.",
            to_delete.len(),
            (to_delete.len() as f64 / total_existing.max(1) as f64 * 100.0).round() as i64,
            total_existing,
        )));
    }
    plan.to_delete = to_delete;
    Ok(plan)
}

/// Reconcile the on-disk `wiki/` and `raw/` trees with the SQLite index.
///
/// Index-only — no embedding. Use `reconcile_with_embed` from the
/// daemon path to also fill in vector chunks; without that, files
/// indexed here will have a matching mtime+size in `documents` but no
/// chunks, and the stat-based skip in `index_wiki_file` prevents any
/// later write path from filling them in. Symptom: queries return
/// `retrieval_empty` despite documents existing.
pub fn reconcile(memex: &Memex, opts: ReconcileOptions) -> Result<ReconcileReport> {
    reconcile_inner(memex, opts, None)
}

/// Like `reconcile`, but uses the provided embedder to populate
/// vector chunks for newly-indexed (or re-indexed) documents. Daemon
/// startup and watcher rescan use this path so the index is fully
/// query-ready.
pub fn reconcile_with_embed(
    memex: &Memex,
    opts: ReconcileOptions,
    model: &mut dyn Embedder,
) -> Result<ReconcileReport> {
    reconcile_inner(memex, opts, Some(model))
}

fn reconcile_inner(
    memex: &Memex,
    opts: ReconcileOptions,
    model: Option<&mut dyn Embedder>,
) -> Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let mut seen: HashSet<String> = HashSet::new();

    let wiki_paths = walk_dir(&memex.wiki_dir(), "wiki", memex, &mut seen, &mut report)?;
    let raw_paths = walk_dir(&memex.raw_dir(), "raw", memex, &mut seen, &mut report)?;

    // Reborrow with `ref mut`: each iteration's `Some(&mut **m)`
    // produces a fresh shorter-lifetime borrow that ends with the call.
    // `Option::as_deref_mut` would be tidier but the compiler can't
    // prove its reborrows are non-overlapping under the implicit
    // `'static` bound on `dyn Embedder`.
    // Per-file errors must not abort the whole pass — a single wiki
    // file with broken frontmatter would otherwise leave every other
    // file unindexed. Log and continue; the file stays surfaceable
    // via `memex lint`.
    let mut model = model;
    for entry in &wiki_paths {
        let result = match model {
            Some(ref mut m) => crate::index_wiki::index_wiki_file(memex, entry, Some(&mut **m)),
            None => crate::index_wiki::index_wiki_file(memex, entry, None),
        };
        match result {
            Ok(crate::index_wiki::IndexOutcome::Skipped) => report.skipped += 1,
            Ok(_) => report.indexed += 1,
            Err(e) => {
                tracing::warn!(path = %entry.display(), error = %e, "reconcile: skipping wiki file");
                report.skipped += 1;
            }
        }
    }
    for entry in &raw_paths {
        let result = match model {
            Some(ref mut m) => crate::index_raw::index_raw_file(memex, entry, Some(&mut **m)),
            None => crate::index_raw::index_raw_file(memex, entry, None),
        };
        match result {
            Ok(crate::index_raw::IndexOutcome::Skipped) => report.skipped += 1,
            Ok(crate::index_raw::IndexOutcome::HashMismatch) => report.hash_mismatches += 1,
            Ok(_) => report.indexed += 1,
            Err(e) => {
                tracing::warn!(path = %entry.display(), error = %e, "reconcile: skipping raw file");
                report.skipped += 1;
            }
        }
    }

    let to_delete: Vec<String> = memex.search().with_connection(|conn| {
        let mut s = conn.prepare("SELECT path FROM documents")?;
        let paths: Vec<String> = s
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .filter(|p| !seen.contains(p))
            .collect();
        Ok(paths)
    })?;

    let total_existing: i64 = memex.search().with_connection(|c| {
        Ok(c.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?)
    })?;
    let threshold = std::cmp::max(10, (total_existing as f64 * 0.5) as i64) as usize;
    if !opts.force && to_delete.len() > threshold {
        return Err(MemexError::Other(anyhow::anyhow!(
            "reconciliation would delete {} documents ({}% of {}). \
             Refusing. Possible causes: wiki/raw unmounted, sync moved files away, accidental rm. \
             Investigate the missing files (e.g. remount or restore) and restart the daemon.",
            to_delete.len(),
            (to_delete.len() as f64 / total_existing.max(1) as f64 * 100.0).round() as i64,
            total_existing,
        )));
    }
    for path in &to_delete {
        memex.search().delete_document_with_cleanup(path)?;
        report.deleted += 1;
    }
    Ok(report)
}

/// Collect candidate file paths under `root` for indexing, accumulating
/// symlink-skip and seen-path bookkeeping into `report` / `seen` as we go.
/// Returns absolute paths so the caller can pass them straight to
/// `index_wiki_file` / `index_raw_file`.
fn walk_dir(
    root: &Path,
    doc_type: &str,
    memex: &Memex,
    seen: &mut HashSet<String>,
    report: &mut ReconcileReport,
) -> Result<Vec<std::path::PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    // Wiki has a flat namespace: <wiki_dir>/<slug>.md (depth 1).
    // Raw is content-addressed in 2 levels: <raw_dir>/<hh>/<rest> (depth 2).
    // Cap the walk so files dropped into subdirectories aren't silently
    // indexed under a name that lint can't see (lint walks top-level only,
    // so a subdir file gets indexed by reconcile/watcher and then surfaced
    // forever as `missing-file:` in lint).
    let max_depth = if doc_type == "raw" { 2 } else { 1 };
    let mut out = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).max_depth(max_depth) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(error = %e, "walkdir error; failing reconciliation");
                return Err(MemexError::Other(anyhow::anyhow!("walk error: {e}")));
            }
        };
        if entry.file_type().is_symlink() {
            report.skipped_symlinks += 1;
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if doc_type == "wiki" && path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let rel = path
            .strip_prefix(memex.root())
            .unwrap_or(path)
            .to_path_buf();
        if rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            continue;
        }
        let rel_str = crate::storage::rel_path_string(&rel);
        seen.insert(rel_str.clone());
        out.push(path.to_path_buf());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_wiki(memex: &crate::Memex, slug: &str, body: &str) {
        let path = memex.wiki_dir().join(format!("{slug}.md"));
        std::fs::write(
            path,
            format!(
                "---\ntitle: {slug}
sources: []\ncreated_at: 2026-04-26T00:00:00Z\nupdated_at: 2026-04-26T00:00:00Z\n---\n\n{body}"
            ),
        )
        .unwrap();
    }

    #[test]
    fn reconcile_indexes_new_files() {
        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        make_wiki(&memex, "alpha", "alpha body");
        make_wiki(&memex, "beta", "beta body");
        let report = reconcile(&memex, ReconcileOptions::default()).unwrap();
        assert_eq!(report.indexed, 2);
        assert_eq!(report.deleted, 0);
    }

    #[test]
    fn reconcile_removes_missing_files() {
        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        make_wiki(&memex, "alpha", "x");
        make_wiki(&memex, "beta", "y");
        reconcile(&memex, ReconcileOptions::default()).unwrap();
        std::fs::remove_file(memex.wiki_dir().join("beta.md")).unwrap();
        let report = reconcile(&memex, ReconcileOptions::default()).unwrap();
        assert_eq!(report.deleted, 1);
    }

    #[test]
    fn reconcile_skips_symlinks_inside_wiki() {
        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        make_wiki(&memex, "real", "real body");
        let target = dir.path().join("outside.md");
        std::fs::write(
            &target,
            "---\ntitle: outside
sources: []\ncreated_at: 2026-04-26T00:00:00Z\nupdated_at: 2026-04-26T00:00:00Z\n---\n\nx",
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, memex.wiki_dir().join("symlinked.md")).unwrap();
        let report = reconcile(&memex, ReconcileOptions::default()).unwrap();
        assert_eq!(report.indexed, 1, "the symlink target must not be indexed");
        assert_eq!(report.skipped_symlinks, 1);
    }

    #[test]
    fn reconcile_aborts_on_safety_threshold() {
        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        for i in 0..20 {
            make_wiki(&memex, &format!("p{i:02}"), "x");
        }
        reconcile(&memex, ReconcileOptions::default()).unwrap();
        for i in 0..15 {
            std::fs::remove_file(memex.wiki_dir().join(format!("p{i:02}.md"))).unwrap();
        }
        let err = reconcile(&memex, ReconcileOptions::default()).unwrap_err();
        assert!(err.to_string().contains("would delete"));
        let still_indexed: i64 = memex
            .search()
            .conn_for_test()
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE doc_type='wiki'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_indexed, 20, "abort must leave DB untouched");
    }

    #[test]
    fn reconcile_force_bypasses_threshold() {
        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        for i in 0..20 {
            make_wiki(&memex, &format!("p{i:02}"), "x");
        }
        reconcile(&memex, ReconcileOptions::default()).unwrap();
        for i in 0..15 {
            std::fs::remove_file(memex.wiki_dir().join(format!("p{i:02}.md"))).unwrap();
        }
        let report = reconcile(&memex, ReconcileOptions { force: true }).unwrap();
        assert_eq!(report.deleted, 15);
    }

    /// A wiki file modified outside of memex causes reconcile to
    /// recompute the body hash and re-index. Asserts the documents.hash
    /// changes (proxy for the chunks-regenerated behavior; commit_doc
    /// drops chunks on hash change).
    #[test]
    fn reconcile_reindexes_externally_edited_wiki_file() {
        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        make_wiki(&memex, "alpha", "original body");
        reconcile(&memex, ReconcileOptions::default()).unwrap();
        let original_hash = memex
            .search()
            .get_document_hash("wiki/alpha.md")
            .unwrap()
            .expect("alpha indexed");

        // Externally rewrite the body — different length, so the
        // mtime+size shortcut at index_wiki_file:50 doesn't fire even
        // on filesystems with 1-second mtime resolution.
        std::fs::write(
            memex.wiki_dir().join("alpha.md"),
            "---\ntitle: alpha
sources: []\ncreated_at: 2026-04-26T00:00:00Z\nupdated_at: 2026-04-26T00:00:00Z\n---\n\nedited body, longer than the original",
        )
        .unwrap();
        reconcile(&memex, ReconcileOptions::default()).unwrap();

        let new_hash = memex
            .search()
            .get_document_hash("wiki/alpha.md")
            .unwrap()
            .expect("alpha still indexed");
        assert_ne!(
            original_hash, new_hash,
            "external edit should change the indexed body hash"
        );
    }

    /// One file with broken frontmatter must NOT abort the entire
    /// reconcile pass. Other valid files in the same wiki directory
    /// must still be indexed. Found via strict e2e: a single
    /// `---\n---\n` (empty frontmatter, parser-rejected) used to
    /// propagate an error out of the per-file index call and skip
    /// every subsequent file in the walk.
    #[test]
    fn reconcile_skips_bad_files_but_indexes_valid_neighbors() {
        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();

        // Bad: empty frontmatter — parser rejects.
        std::fs::write(memex.wiki_dir().join("bad.md"), "---\n---\n\nbody\n").unwrap();
        // Bad: missing required created_at / updated_at.
        std::fs::write(
            memex.wiki_dir().join("incomplete.md"),
            "---\ntitle: Incomplete\n---\n\nbody\n",
        )
        .unwrap();
        // Good: valid frontmatter.
        make_wiki(&memex, "valid", "valid body");

        let report = reconcile(&memex, ReconcileOptions::default()).unwrap();
        assert_eq!(report.indexed, 1, "exactly one valid file should index");

        // The two bad files leave the DB clean; only valid-* survives.
        let docs = memex.search().all_wiki_documents().unwrap();
        let paths: Vec<&str> = docs.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["wiki/valid.md"]);
    }

    /// Files dropped into a wiki subdirectory must NOT be indexed.
    /// Wiki has a flat namespace; a subdir file would otherwise get
    /// indexed under a slug (file_stem) that lint can't see (lint
    /// only walks the top level), and lint would report it forever
    /// as `missing-file:`. Found via strict e2e — the watcher had a
    /// recursive watch and reconcile had unbounded WalkDir, so
    /// subdir wikis got indexed and the false missing-file persisted.
    #[test]
    fn reconcile_skips_subdir_wiki_files() {
        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        // Top-level: should index.
        make_wiki(&memex, "top-level", "top body");
        // Subdir: should NOT index.
        let subdir = memex.wiki_dir().join("nested");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(
            subdir.join("buried.md"),
            "---\ntitle: buried
sources: []\ncreated_at: 2026-04-29T00:00:00Z\nupdated_at: 2026-04-29T00:00:00Z\n---\n\nburied body",
        )
        .unwrap();

        let report = reconcile(&memex, ReconcileOptions::default()).unwrap();
        assert_eq!(
            report.indexed, 1,
            "only the top-level file should be indexed; subdir files are out-of-spec"
        );

        // Confirm via DB: only top-level row.
        let docs = memex.search().all_wiki_documents().unwrap();
        let paths: Vec<&str> = docs.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["wiki/top-level.md"]);
    }

    /// Frontmatter-only edit on raw: editing the frontmatter (e.g.,
    /// correcting a `source:` URL) without changing the body must update
    /// `documents.source` and `documents.title` while leaving the body
    /// hash unchanged. The fact that the body hash is stable means
    /// commit_doc takes the unchanged path and skips re-embed.
    #[test]
    fn reconcile_updates_raw_metadata_without_changing_body_hash() {
        use crate::raw::{RawFrontmatter, assemble_raw_file, raw_path_for_hash};

        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let body = "# Auth Tokens\n\nThe body content stays the same.";
        let body_hash = crate::storage::content_hash(body.as_bytes());
        let raw_path = raw_path_for_hash(&memex.raw_dir(), &body_hash);
        std::fs::create_dir_all(raw_path.parent().unwrap()).unwrap();
        let original_fm = RawFrontmatter {
            source: Some("https://wrong-url.example/a".into()),
            source_kind: Some("url".into()),
            ingested_at: Some("2026-04-29T10:00:00Z".into()),
            title: Some("Auth Tokens".into()),
            ..Default::default()
        };
        std::fs::write(&raw_path, assemble_raw_file(&original_fm, body)).unwrap();
        crate::index_raw::index_raw_file(&memex, &raw_path, None).unwrap();
        let raw_rel = raw_path
            .strip_prefix(memex.root())
            .unwrap_or(&raw_path)
            .to_string_lossy()
            .to_string();
        let pre_hash = memex
            .search()
            .get_document_hash(&raw_rel)
            .unwrap()
            .expect("raw indexed");

        // Hand-edit the frontmatter only — body bytes identical.
        let corrected_fm = RawFrontmatter {
            source: Some("https://corrected-url.example/a".into()),
            ..original_fm.clone()
        };
        std::fs::write(&raw_path, assemble_raw_file(&corrected_fm, body)).unwrap();
        // Re-run the per-file indexer (what reconcile would dispatch).
        crate::index_raw::index_raw_file(&memex, &raw_path, None).unwrap();

        let post_hash = memex
            .search()
            .get_document_hash(&raw_rel)
            .unwrap()
            .expect("raw still indexed");
        assert_eq!(
            pre_hash, post_hash,
            "frontmatter-only edit must leave body hash unchanged"
        );
        // Source column should reflect the corrected URL.
        let src: Option<String> = memex
            .search()
            .with_connection(|c| {
                Ok(c.query_row(
                    "SELECT source FROM documents WHERE doc_type='raw' AND path=?1",
                    rusqlite::params![&raw_rel],
                    |r| r.get::<_, Option<String>>(0),
                )?)
            })
            .unwrap();
        assert_eq!(src.as_deref(), Some("https://corrected-url.example/a"));
    }
}
