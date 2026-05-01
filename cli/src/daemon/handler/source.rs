//! `Request::SourceAdd` and `Request::SourceDelete` handlers.

use crate::daemon::error::DaemonError;
use crate::daemon::handler::{
    HandlerState, error_events, get_or_open_memex, validate_source_path,
};
use crate::daemon::protocol::Event;

pub(super) async fn handle_source_add(
    source_path: String,
    content: String,
    collections: Vec<String>,
    state: &HandlerState,
) -> Vec<Event> {
    if content.trim().is_empty() {
        return error_events(DaemonError::BadRequest("content is empty".into()));
    }

    if let Err(e) = validate_source_path(&source_path) {
        return error_events(DaemonError::BadRequest(e));
    }

    if let Err(e) = memex_core::search::validate_collection_names(&collections) {
        return error_events(DaemonError::BadRequest(e));
    }

    let memex = match get_or_open_memex(state.writer.memex_handle(), state.writer.bound_root()) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };

    let body_hash = memex_core::storage::content_hash(content.as_bytes());
    let raw_path = memex_core::raw::raw_path_for_hash(&memex.raw_dir(), &body_hash);
    if let Some(parent) = raw_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let derived_title = derive_source_title(&content, &source_path);

    if !raw_path.exists() {
        let kind = if memex_core::raw::is_url(&source_path) { "url" } else { "path" };
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let fm = memex_core::raw::RawFrontmatter {
            source: Some(source_path.clone()),
            source_kind: Some(kind.into()),
            ingested_at: Some(now),
            converter: None,
            title: Some(derived_title.clone()),
            agent: None,
            session_id: None,
        };
        let file = memex_core::raw::assemble_raw_file(&fm, &content);
        if let Err(e) = memex_core::storage::atomic_write(&raw_path, file.as_bytes()) {
            return error_events(DaemonError::Internal(format!("atomic_write failed: {e}")));
        }
    }

    let index_result = {
        let mut guard = state.writer.embed_model().lock().await;
        memex_core::index_raw::index_raw_file(&memex, &raw_path, Some(guard.as_mut()))
    };
    if let Err(e) = index_result {
        return error_events(DaemonError::Internal(format!("index_raw_file: {e}")));
    }

    if !collections.is_empty() {
        let rel = memex_core::storage::rel_path_string(
            raw_path.strip_prefix(memex.root()).unwrap_or(&raw_path),
        );
        if let Err(e) = memex
            .search()
            .union_document_collections_by_path("raw", &rel, &collections)
        {
            tracing::warn!(?e, "collections union failed");
        }
    }

    let docid = memex_core::docid::short(&body_hash).to_string();
    vec![
        Event::SourceAdded { docid },
        Event::Done { status: 0 },
    ]
}

pub(super) async fn handle_source_delete(
    ref_: String,
    force: bool,
    state: &HandlerState,
) -> Vec<Event> {
    let root = state.writer.bound_root().to_path_buf();
    let memex = match get_or_open_memex(state.writer.memex_handle(), &root) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };
    let search = memex.search();

    // Resolve ref_ → source document.
    let source_doc = match resolve_source_ref(search, &ref_) {
        Ok(Some(d)) => d,
        Ok(None) => {
            return error_events(DaemonError::BadRequest(format!(
                "source not found: '{ref_}'. Use a 7-char docid prefix (from `memex source list`)."
            )));
        }
        Err(e) => return error_events(DaemonError::Internal(format!("resolve: {e}"))),
    };

    // Find wiki pages referencing this source. The canonical format is
    // "#<docid>" (used by `memex write --source`); the ingest path
    // stores the source's original path/URL verbatim in the wiki's
    // frontmatter `sources:` field. Both forms appear in the wild —
    // scan for either, union, dedup.
    let docid = memex_core::docid::short(&source_doc.hash).to_string();
    let docid_ref = format!("#{docid}");
    let mut referencing: Vec<String> = search
        .wiki_pages_referencing_source(memex.root(), &docid_ref)
        .unwrap_or_default();
    if let Ok(raw_body) = std::fs::read_to_string(memex.root().join(&source_doc.path))
        && let Ok(fm) = memex_core::raw::parse_raw_frontmatter(&raw_body).map(|(fm, _)| fm)
        && let Some(orig) = fm.source.as_deref()
    {
        let by_path = search
            .wiki_pages_referencing_source(memex.root(), orig)
            .unwrap_or_default();
        referencing.extend(by_path);
    }
    referencing.sort();
    referencing.dedup();

    if !referencing.is_empty() && !force {
        return error_events(DaemonError::BadRequest(format!(
            "{} wiki pages reference this source: {}. Pass --force to delete anyway \
             (the references will become dangling and 'memex lint' will report them).",
            referencing.len(),
            referencing.join(", ")
        )));
    }

    // Remove the on-disk raw file first so the DB row never outlives a
    // stale file on disk, mirroring `handle_delete`'s wiki-side ordering.
    // `path_str` is DB-relative (`raw/<aa>/<rest>`), so joining with root
    // gives the absolute path under `raw_dir`.
    let path_str = source_doc.path.clone();
    let abs = root.join(&path_str);
    if let Err(e) = std::fs::remove_file(&abs)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return error_events(DaemonError::Storage(format!(
            "remove raw file {}: {e}",
            abs.display()
        )));
    }

    if let Err(e) = search.delete_document_with_cleanup(&path_str) {
        return error_events(DaemonError::Internal(format!("delete: {e}")));
    }

    vec![
        Event::SourceDeleted {
            docid,
            source_path: path_str,
            dangling_wiki_pages: referencing,
        },
        Event::Done { status: 0 },
    ]
}

/// Resolve a source ref (docid prefix) to a source document row.
/// Returns Ok(None) if not found, Err on lookup error.
/// Note: "path:<src>" syntax is no longer supported (removed in foundation rewrite).
fn resolve_source_ref(
    search: &memex_core::search::Bm25Search,
    ref_: &str,
) -> Result<Option<memex_core::types::Document>, memex_core::error::MemexError> {
    let docs = search.resolve_ref_documents(ref_)?;
    Ok(docs.into_iter().find(|d| d.doc_type == "raw"))
}

/// First H1 line, falling back to URL last segment / file stem / verbatim.
pub(super) fn derive_source_title(content: &str, source_path: &str) -> String {
    if let Some(line) = content.lines().next() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let t = rest.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    if memex_core::raw::is_url(source_path)
        && let Some(stem) = source_path.rsplit('/').find(|s| !s.is_empty())
    {
        return crate::slugify(stem);
    }
    if let Some(stem) = std::path::Path::new(source_path)
        .file_stem()
        .and_then(|s| s.to_str())
    {
        return stem.to_string();
    }
    source_path.to_string()
}
