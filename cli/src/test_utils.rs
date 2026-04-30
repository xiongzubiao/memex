//! Test-only helpers. Enabled by `feature = "test-harness"`.
//!
//! `seed_wiki_page` mirrors the daemon's `handle_write` pipeline
//! (parse → forward_link → atomic_write → index → maintain_backlinks)
//! including the `auto_link_eligible` cutoff, so test fixtures
//! exercise the same auto-cross-link semantics production uses.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};

use crate::slugify;

/// Canonical wiki frontmatter + body, used by tests that need a
/// deterministic page on disk before invoking the binary against it.
pub fn make_page(title: &str, body: &str) -> String {
    format!(
        "---\ntitle: {title}\ntags:\n  - entity\ncreated_at: 2026-04-10T00:00:00Z\nupdated_at: 2026-04-10T00:00:00Z\nsources: []\n---\n\n{body}\n"
    )
}

/// Convenience wrapper that asserts success: build a page from
/// `make_page` and seed it under `force=true`. Used everywhere a test
/// just needs "this page exists on disk and in the index, please."
pub fn ingest_page(root: &Path, slug: &str, title: &str, body: &str) {
    let content = make_page(title, body);
    seed_wiki_page(root, slug, &content, true)
        .unwrap_or_else(|e| panic!("ingest failed: {e}"));
}

/// Write a wiki page to SQLite + filesystem. `content` must include
/// valid YAML frontmatter; `name` is slugified to a kebab-case stem.
pub fn seed_wiki_page(root: &Path, name: &str, content: &str, force: bool) -> Result<()> {
    crate::init_ort_runtime().map_err(|e| anyhow::anyhow!(e))?;

    let stem = slugify(name);
    if stem.is_empty() {
        anyhow::bail!("name slugifies to empty: {name:?}");
    }

    let (fm, body) = memex_core::validate::parse_frontmatter(content)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if fm.title.trim().is_empty() {
        anyhow::bail!("page title is empty");
    }

    let memex = memex_core::Memex::open_writer(root.to_path_buf())?;
    let wiki_dir = memex.wiki_dir();
    std::fs::create_dir_all(&wiki_dir).with_context(|| format!("create {wiki_dir:?}"))?;
    let page_path = wiki_dir.join(format!("{stem}.md"));

    if page_path.exists() && !force {
        anyhow::bail!("wiki/{stem}.md already exists; use force=true");
    }

    let existing_pages = memex.search().all_stems_and_titles()?;
    let eligible_pages: Vec<(String, String)> = existing_pages
        .iter()
        .filter(|(s, _)| memex_core::crosslink::auto_link_eligible(s))
        .cloned()
        .collect();
    let (linked_body, _linked) =
        memex_core::crosslink::forward_link(&body, &eligible_pages, &stem);
    let final_content =
        memex_core::crosslink::replace_body_preserving_frontmatter(content, &linked_body);
    memex_core::storage::atomic_write(&page_path, final_content.as_bytes())?;

    let mut guard = embedder_lock().lock().unwrap();
    memex_core::index_wiki::index_wiki_file(&memex, &page_path, Some(&mut *guard))?;

    if memex_core::crosslink::auto_link_eligible(&stem) {
        memex_core::crosslink::maintain_backlinks(&memex, &stem, &fm.title, &mut *guard)?;
    }
    Ok(())
}

/// Reuse one warm embedder for the whole test process — without this,
/// every `seed_wiki_page` call would reload ~300MB of ONNX (~1.5s),
/// adding tens of seconds to a fixture-heavy `cargo test`.
fn embedder_lock() -> &'static Mutex<memex_core::embed::EmbeddingModel> {
    static EMBEDDER: OnceLock<Mutex<memex_core::embed::EmbeddingModel>> = OnceLock::new();
    EMBEDDER.get_or_init(|| {
        let model = memex_core::retrieval::load_default_model()
            .expect("seed_wiki_page: failed to load default ONNX model");
        Mutex::new(model)
    })
}
