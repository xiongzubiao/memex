//! `Request::Delete` handler — remove a wiki page from disk + index.

use crate::daemon::error::DaemonError;
use crate::daemon::handler::{HandlerState, acquire_slug_lock, error_events, get_or_open_memex};
use crate::daemon::protocol::Event;

pub(super) async fn handle_delete(slug: String, force: bool, state: &HandlerState) -> Vec<Event> {
    let root = state.writer.bound_root().to_path_buf();
    let memex = match get_or_open_memex(state.writer.memex_handle(), &root) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };
    let search = memex.search();

    // Acquire the per-slug lock so no concurrent write/delete races.
    let _slug_guard = acquire_slug_lock(&state.writer, &slug).await;

    // Resolve the wiki doc.
    let docs = match search.resolve_ref_documents(&slug) {
        Ok(d) => d,
        Err(e) => return error_events(DaemonError::Storage(e.to_string())),
    };
    let wiki_doc = match docs.into_iter().find(|d| d.doc_type == "wiki") {
        Some(d) => d,
        None => {
            return error_events(DaemonError::BadRequest(format!(
                "wiki page not found: {slug}"
            )));
        }
    };
    let target_path = wiki_doc.path.clone();

    // Backlink scan via chunks_fts — guards against deleting a page
    // that is still linked from elsewhere. The `[[<slug>]]` text lives
    // in chunk bodies, so scan there and `GROUP BY hash` to dedupe.
    // Post-delete dangling references are reported by `memex lint`,
    // not by this handler.
    if !force {
        let pattern = format!("\"[[{slug}]]\"");
        let backlinks: Vec<String> = match search.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT d.path FROM documents d \
                 JOIN chunks_fts cf ON cf.hash = d.hash \
                 WHERE d.doc_type = 'wiki' AND d.path != ?1 \
                   AND cf.chunk_text MATCH ?2",
            )?;
            let rows: Vec<String> = stmt
                .query_map(rusqlite::params![&target_path, &pattern], |r| {
                    r.get::<_, String>(0)
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        }) {
            Ok(rows) => rows,
            Err(e) => return error_events(DaemonError::Storage(e.to_string())),
        };

        if !backlinks.is_empty() {
            return error_events(DaemonError::Conflict(format!(
                "{} wiki page(s) link to [[{}]]: {}\nRe-run with --force to delete anyway.",
                backlinks.len(),
                slug,
                backlinks.join(", ")
            )));
        }
    }

    // Remove the file.
    let abs = root.join(&target_path);
    if let Err(e) = std::fs::remove_file(&abs)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return error_events(DaemonError::Storage(format!(
            "remove wiki file {}: {e}",
            abs.display()
        )));
    }
    // file already gone — fall through to DB cleanup

    if let Err(e) = search.delete_document_with_cleanup(&target_path) {
        return error_events(DaemonError::Storage(e.to_string()));
    }

    vec![Event::Deleted { slug }, Event::Done { status: 0 }]
}
