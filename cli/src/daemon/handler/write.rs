//! `Request::Write` handler — atomic wiki write + synchronous index/embed.

use crate::daemon::error::DaemonError;
use crate::daemon::handler::{
    HandlerState, acquire_slug_locks, async_atomic_write, error_events, get_or_open_memex,
};
use crate::daemon::protocol::Event;

/// Handle a write request: atomically write the wiki file and index it.
pub(super) async fn handle_write(
    title: String,
    content: String,
    tags: Vec<String>,
    source: Option<String>,
    force: bool,
    state: &HandlerState,
) -> Vec<Event> {
    let root = state.writer.bound_root();
    let memex = match get_or_open_memex(state.writer.memex_handle(), root) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };

    // Reject control characters in the title outright. The title is
    // interpolated verbatim into YAML (`title: {title}`), so an embedded
    // newline / tab / NUL produces unparseable frontmatter — the file
    // lands on disk but the watcher then can't index it. Better to
    // refuse at the request boundary than write an orphan file.
    if title.chars().any(|c| c.is_control()) {
        return error_events(DaemonError::BadRequest(
            "title contains control characters (newline, tab, etc.); strip them and retry"
                .into(),
        ));
    }

    let slug = memex_core::wiki::normalize_slug(&title);
    if slug.is_empty() {
        return error_events(DaemonError::BadRequest(
            "title normalizes to empty slug".into(),
        ));
    }
    // The atomic-write tmp filename is the binding length constraint —
    // it's longer than the final `<slug>.md`. Format from
    // core/src/storage.rs::atomic_write:
    //   .{slug}.md.{nonce}.tmp
    //   1 + slug + 3 + 1 + 8 + 4    (nonce = 8 hex chars)
    // Reject if it would exceed NAME_MAX. Without this guard the user
    // sees a confusing OS error pointing at the .tmp path.
    const TEMP_OVERHEAD: usize = 1 + 3 + 1 + 8 + 4;
    let max_slug_len = (libc::NAME_MAX as usize).saturating_sub(TEMP_OVERHEAD);
    if slug.len() > max_slug_len {
        return error_events(DaemonError::BadRequest(format!(
            "slug exceeds {max_slug_len} bytes after normalization (got {} bytes); shorten the title",
            slug.len()
        )));
    }

    let path = memex_core::wiki::wiki_path_for_slug(&memex.wiki_dir(), &slug);

    // Per-slug write lock: serialize concurrent writes to the same slug.
    let _slug_guard = acquire_slug_locks(&state.writer, vec![slug.clone()]).await;

    if path.exists() && !force {
        return error_events(DaemonError::BadRequest(format!(
            "wiki/{slug}.md already exists; use --force"
        )));
    }

    // Validate source docid before writing anything.
    if let Some(src_ref) = source.as_deref() {
        let docs = match memex.search().resolve_ref_documents(src_ref) {
            Ok(d) => d,
            Err(e) => return error_events(DaemonError::Storage(e.to_string())),
        };
        if !docs.iter().any(|d| d.doc_type == "raw") {
            return error_events(DaemonError::BadRequest(format!(
                "source not found: '{src_ref}'. Run `memex source list` to find docids."
            )));
        }
    }

    // Strip any incoming frontmatter; synthesize fresh frontmatter.
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let body = match memex_core::validate::parse_frontmatter(&content) {
        Ok((_, b)) => b.to_string(),
        Err(_) => content.clone(),
    };

    // Auto cross-link forward: scan body for mentions of existing
    // titles; replace first un-linked occurrence with `[[stem]]`.
    // Filtered by `auto_link_eligible` so short common-word titles
    // (api/auth/log/db/cache) don't false-positive on generic prose.
    let existing_pages = match memex.search().all_stems_and_titles() {
        Ok(p) => p,
        Err(e) => return error_events(DaemonError::Storage(e.to_string())),
    };
    let eligible_pages: Vec<(String, String)> = existing_pages
        .iter()
        .filter(|(stem, _)| memex_core::crosslink::auto_link_eligible(stem))
        .cloned()
        .collect();
    let (linked_body, linked) =
        memex_core::crosslink::forward_link(&body, &eligible_pages, &slug);

    // suggest_create: any [[stem]] left in the body whose target
    // page doesn't exist (after forward_link's pass).
    let suggest_create: Vec<String> = {
        let known: std::collections::HashSet<&str> =
            existing_pages.iter().map(|(s, _)| s.as_str()).collect();
        let mut out: Vec<String> = memex_core::validate::extract_wiki_links(&linked_body)
            .into_iter()
            .filter(|stem| stem != &slug && !known.contains(stem.as_str()))
            .collect();
        out.sort();
        out.dedup();
        out
    };

    let yaml_tags = if tags.is_empty() {
        "[]".to_string()
    } else {
        format!("\n  - {}", tags.join("\n  - "))
    };
    let yaml_sources = match source.as_deref() {
        Some(s) => format!("sources:\n  - \"#{s}\"\n"),
        None => "sources: []\n".to_string(),
    };
    let frontmatter_yaml = format!(
        "title: {title}\ntags: {yaml_tags}\ncreated_at: {now}\nupdated_at: {now}\n{yaml_sources}",
    );
    let file = format!("---\n{frontmatter_yaml}---\n\n{linked_body}");

    if let Err(e) = tokio::fs::create_dir_all(memex.wiki_dir()).await {
        return error_events(DaemonError::Internal(format!("create wiki dir: {e}")));
    }

    if let Err(e) = async_atomic_write(path.clone(), file.as_bytes().to_vec()).await {
        return error_events(e);
    }

    let index_result = {
        let mut guard = state.writer.embed_model().lock().await;
        memex_core::index_wiki::index_wiki_file(&memex, &path, Some(guard.as_mut()))
    };
    if let Err(e) = index_result {
        return error_events(DaemonError::Internal(format!("index_wiki_file: {e}")));
    }

    // Auto cross-link backward: only if the new slug itself passes
    // the eligibility filter — otherwise we'd sweep every existing
    // page that mentions a short common word. Re-embed each rewritten
    // page under the same lock so chunks stay consistent with the
    // new body hash.
    let backlinked = if memex_core::crosslink::auto_link_eligible(&slug) {
        let mut guard = state.writer.embed_model().lock().await;
        match memex_core::crosslink::maintain_backlinks(&memex, &slug, &title, guard.as_mut()) {
            Ok(b) => b,
            Err(e) => return error_events(DaemonError::Storage(e.to_string())),
        }
    } else {
        Vec::new()
    };

    let body_hash = memex_core::storage::content_hash(linked_body.as_bytes());
    let docid = memex_core::docid::short(&body_hash).to_string();

    vec![
        Event::Written {
            slug,
            docid,
            linked,
            backlinked,
            suggest_create,
        },
        Event::Done { status: 0 },
    ]
}
