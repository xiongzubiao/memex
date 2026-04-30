//! `Request::Search` handler — title→slug lookup. Routes through the
//! daemon so the warm embedding model is reused (direct CLI search would
//! reload ONNX ~1.5s per invocation).
//!
//! `search_wiki_by_title` does its own strong-BM25 short-circuit, so a
//! query that exactly names an existing page returns without any embed
//! work. Only ambiguous queries pay the ~50ms vector re-rank.

use crate::daemon::error::DaemonError;
use crate::daemon::handler::{HandlerState, error_events, get_or_open_memex};
use crate::daemon::protocol::Event;

pub(super) async fn handle_search(title: String, state: &HandlerState) -> Vec<Event> {
    let reader = state.reader();
    let memex = match get_or_open_memex(&reader.memex_handle, &reader.bound_root) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };
    let search = memex.search();

    let mut guard = reader.embed_model.lock().await;
    let slug_result =
        memex_core::retrieval::search_wiki_by_title(search, &title, guard.as_mut(), memex.root());

    match slug_result {
        Ok(slug) => vec![Event::SearchResult { slug }, Event::Done { status: 0 }],
        Err(e) => error_events(DaemonError::Internal(format!("search failed: {e}"))),
    }
}
