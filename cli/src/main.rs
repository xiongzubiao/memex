use clap::{Parser, Subcommand};
use memex_core::Memex;
use memex_core::search::{self, MIN_SCORE, SearchResult, WikiSearch};
use memex_core::types::LintIssueKind;
use std::io::Read as _;
use std::path::Path;

#[derive(Parser)]
#[command(name = "memex", about = "Personal wiki storage and search engine")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// BM25 full-text search
    Search {
        /// Search query (full sentence or keywords)
        query: String,
        /// Keyword expansion terms (BM25 only, repeatable)
        #[arg(long)]
        lex: Vec<String>,
        /// Semantic expansion terms (vector only, repeatable)
        #[arg(long)]
        vec: Vec<String>,
        /// Hypothetical document expansion (vector only, repeatable)
        #[arg(long)]
        hyde: Vec<String>,
        /// Legacy expansion terms for RRF fusion (repeatable)
        #[arg(long)]
        expand: Vec<String>,
    },
    /// Read wiki pages by docid, stem, or title
    Read {
        /// Page references (docid, filename stem, or title)
        refs: Vec<String>,
    },
    /// Write a wiki page (opens $EDITOR or reads piped stdin)
    Write {
        /// Page name or title (normalized to kebab-case filename)
        name: String,
        /// Overwrite existing page instead of reporting conflict
        #[arg(long)]
        force: bool,
        /// Reduced output (only written: and wiki_pages:)
        #[arg(long)]
        quiet: bool,
        /// Source file paths to attach (repeatable)
        #[arg(long = "source")]
        sources: Vec<String>,
    },
    /// Delete a wiki page
    Delete {
        /// Page reference (docid, filename stem, or title)
        page_ref: String,
        /// Skip confirmation prompt (required when stdin is a TTY)
        #[arg(long)]
        force: bool,
    },
    /// Check wiki for dangling links and missing cross-references
    Lint {
        /// Auto-fix stale index entries by reindexing from disk
        #[arg(long)]
        fix: bool,
    },
}

/// Compute the display stem for a search result.
///
/// For wiki documents: filename basename without extension (e.g. "caching").
/// For source documents: the absolute path (e.g. "/home/user/notes.txt").
fn result_stem(result: &SearchResult) -> String {
    if result.collection == "wiki" {
        result
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string()
    } else {
        result.path.to_string_lossy().to_string()
    }
}

/// Convert vector search results to SearchResults by looking up document metadata.
///
/// For each `VectorResult` (keyed by content hash), looks up the document(s)
/// referencing that hash. Uses the best chunk text as the snippet instead of
/// the document summary.
fn vector_search_as_results(
    search: &memex_core::search::Bm25Search,
    query_embedding: &[f32],
) -> anyhow::Result<Vec<SearchResult>> {
    let vec_results = search.with_connection(|conn| {
        memex_core::vector::vector_search_collapsed(conn, query_embedding, 20)
    })?;

    let mut results = Vec::new();
    for vr in &vec_results {
        let docs = search.lookup_documents_by_hash(&vr.hash)?;
        for doc in docs {
            results.push(SearchResult {
                path: std::path::PathBuf::from(&doc.path),
                title: doc.title,
                score: vr.score,
                snippet: vr.chunk_text.clone(),
                collection: doc.collection,
                docid: doc.docid,
            });
        }
    }
    Ok(results)
}

/// Check if the ONNX embedding model can actually be loaded.
///
/// Probes the real runtime by attempting to load the model file with
/// `catch_unwind_silent`.  Returns `true` only if both the model file
/// exists AND the ONNX Runtime shared library is available.
fn onnx_model_available() -> bool {
    let Some(mp) = dirs::home_dir().map(|h| h.join(".memex/models/embedding-gemma-300m.onnx"))
    else {
        return false;
    };
    if !mp.exists() {
        return false;
    }
    let Some(path_str) = mp.to_str() else {
        return false;
    };
    let path_owned = path_str.to_string();
    catch_unwind_silent(|| memex_core::embed::load_model(&path_owned, "embedding-gemma-300m"))
        .is_ok_and(|r| r.is_ok())
}

/// Run a closure with panic output silenced.
///
/// The `ort` crate panics (rather than returning an error) when the ONNX
/// Runtime shared library cannot be loaded.  `catch_unwind` catches the
/// panic, but the default hook still prints a noisy backtrace to stderr.
/// This helper installs a no-op panic hook for the duration of the call,
/// then restores the original hook afterwards.
fn catch_unwind_silent<F: FnOnce() -> R + std::panic::UnwindSafe, R>(f: F) -> Result<R, ()> {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(f).map_err(|_| ());
    std::panic::set_hook(prev);
    result
}

/// Embed a query string using the ONNX model or hash_embedding fallback.
fn embed_query(text: &str) -> Vec<f32> {
    let model_path = dirs::home_dir().map(|h| h.join(".memex/models/embedding-gemma-300m.onnx"));

    if let Some(ref mp) = model_path
        && mp.exists()
        && let Some(path_str) = mp.to_str()
        && let Ok(Ok(mut model)) =
            catch_unwind_silent(|| memex_core::embed::load_model(path_str, "embedding-gemma-300m"))
    {
        memex_core::embed::embed_text(&mut model, text)
            .unwrap_or_else(|_| memex_core::embed::hash_embedding(text))
    } else {
        memex_core::embed::hash_embedding(text)
    }
}

fn run_search(
    query: &str,
    lex: &[String],
    vec: &[String],
    hyde: &[String],
    expand: &[String],
) -> anyhow::Result<()> {
    let root = memex_cli::memex_root();
    let memex = Memex::open(root)?;
    let search = memex.search();

    let has_typed_flags = !lex.is_empty() || !vec.is_empty() || !hyde.is_empty();

    if has_typed_flags {
        // Typed query mode: --lex/--vec/--hyde present.
        // Signal detection is skipped.
        let mut ranked_lists: Vec<Vec<SearchResult>> = Vec::new();
        let mut wiki_indices: Vec<usize> = Vec::new();

        // Primary query -> BM25 + vector (both).
        let wiki_bm25 = search.search_collection(query, "wiki", 20)?;
        let source_bm25 = search.search_collection(query, "source", 20)?;
        wiki_indices.push(ranked_lists.len());
        ranked_lists.push(wiki_bm25);
        ranked_lists.push(source_bm25);

        // Primary query -> vector search.
        let query_emb = embed_query(query);
        let vec_results = vector_search_as_results(search, &query_emb)?;
        if !vec_results.is_empty() {
            ranked_lists.push(vec_results);
        }

        // --lex terms -> BM25 only (wiki 2x + source 1x per term).
        for term in lex {
            let lex_wiki = search.search_collection(term, "wiki", 20)?;
            let lex_source = search.search_collection(term, "source", 20)?;
            wiki_indices.push(ranked_lists.len());
            ranked_lists.push(lex_wiki);
            ranked_lists.push(lex_source);
        }

        // --vec terms -> vector search only.
        for term in vec {
            let term_emb = embed_query(term);
            let vec_results = vector_search_as_results(search, &term_emb)?;
            if !vec_results.is_empty() {
                ranked_lists.push(vec_results);
            }
        }

        // --hyde terms -> vector search only (hypothetical document expansion).
        for term in hyde {
            let term_emb = embed_query(term);
            let vec_results = vector_search_as_results(search, &term_emb)?;
            if !vec_results.is_empty() {
                ranked_lists.push(vec_results);
            }
        }

        let fused = search::rrf_fuse(&ranked_lists, &wiki_indices, 60);

        for result in &fused {
            if result.score < MIN_SCORE {
                continue;
            }
            let stem = result_stem(result);
            println!(
                "{}\t{}\t{:.3}\t{}\t{}",
                result.docid, result.collection, result.score, stem, result.snippet
            );
        }
    } else if !expand.is_empty() {
        // Legacy --expand mode: BM25 probe + expansion terms.
        let wiki_results = search.search_collection(query, "wiki", 20)?;
        let source_results = search.search_collection(query, "source", 20)?;

        // Signal detection for legacy mode.
        let s1 = wiki_results.first().map(|r| r.score as f64).unwrap_or(0.0);
        let s2 = wiki_results.get(1).map(|r| r.score as f64).unwrap_or(0.0);
        let signal = if search::is_strong_signal(s1, s2) {
            "strong"
        } else {
            "weak"
        };
        println!("signal: {signal}");

        let mut ranked_lists: Vec<Vec<SearchResult>> = vec![wiki_results, source_results];
        let mut wiki_indices: Vec<usize> = vec![0];

        for term in expand {
            let exp_wiki = search.search_collection(term, "wiki", 20)?;
            let exp_source = search.search_collection(term, "source", 20)?;
            wiki_indices.push(ranked_lists.len());
            ranked_lists.push(exp_wiki);
            ranked_lists.push(exp_source);
        }

        let fused = search::rrf_fuse(&ranked_lists, &wiki_indices, 60);

        for result in &fused {
            if result.score < MIN_SCORE {
                continue;
            }
            let stem = result_stem(result);
            println!(
                "{}\t{}\t{:.3}\t{}\t{}",
                result.docid, result.collection, result.score, stem, result.snippet
            );
        }
    } else {
        // No flags: hybrid BM25 + vector search with signal detection.
        let wiki_results = search.search_collection(query, "wiki", 20)?;
        let source_results = search.search_collection(query, "source", 20)?;

        // Signal detection from wiki BM25 scores (before RRF).
        let s1 = wiki_results.first().map(|r| r.score as f64).unwrap_or(0.0);
        let s2 = wiki_results.get(1).map(|r| r.score as f64).unwrap_or(0.0);
        let signal = if search::is_strong_signal(s1, s2) {
            "strong"
        } else {
            "weak"
        };
        println!("signal: {signal}");

        // BM25 lists: wiki (2x via wiki_indices) + source (1x).
        let mut lists: Vec<Vec<SearchResult>> = vec![wiki_results, source_results];
        let wiki_indices: Vec<usize> = vec![0_usize];

        // Vector search on the primary query (1x weight, not in wiki_indices).
        let vec_results = vector_search_as_results(search, &embed_query(query))?;
        if !vec_results.is_empty() {
            lists.push(vec_results);
        }

        let fused = search::rrf_fuse(&lists, &wiki_indices, 60);

        for result in &fused {
            if result.score < MIN_SCORE {
                continue;
            }
            let stem = result_stem(result);
            println!(
                "{}\t{}\t{:.3}\t{}\t{}",
                result.docid, result.collection, result.score, stem, result.snippet
            );
        }
    }

    Ok(())
}

fn run_read(refs: &[String]) -> anyhow::Result<()> {
    let root = memex_cli::memex_root();
    let memex = Memex::open(root.clone())?;
    let search = memex.search();

    let wiki_dir = root.join("wiki");
    let canonical_wiki = wiki_dir.canonicalize().unwrap_or(wiki_dir.clone());

    for reference in refs {
        let docs = search.resolve_ref_documents(reference)?;
        if docs.is_empty() {
            eprintln!("Not found: {reference}");
            continue;
        }
        for doc in &docs {
            // Derive stem: basename without extension.
            let doc_path = Path::new(&doc.path);
            let stem = doc_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();

            // Read content: wiki pages from disk, source documents from content table.
            let body = if doc.collection == "wiki" {
                let full_path = root.join(&doc.path);
                let canonical = full_path.canonicalize().unwrap_or(full_path.clone());
                if !canonical.starts_with(&canonical_wiki) {
                    eprintln!("Error: path traversal rejected: {}", doc.path);
                    continue;
                }
                std::fs::read_to_string(&full_path)
                    .unwrap_or_else(|e| format!("(error reading file: {e})"))
            } else {
                // Source documents: read from the content-addressable store.
                search
                    .get_content(&doc.hash)
                    .unwrap_or_else(|e| format!("(error reading content: {e})"))
            };

            println!("=== {} {} {} ===", doc.docid, doc.collection, stem);
            print!("{body}");
            if !body.ends_with('\n') {
                println!();
            }
        }
    }
    Ok(())
}

/// Open $EDITOR with a frontmatter template. Returns the edited content.
fn open_editor_for_page(name: &str) -> anyhow::Result<String> {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let template =
        format!("---\ntitle: {name}\ntags: []\ncreated_at: {now}\nupdated_at: {now}\n---\n\n");

    let tmp = std::env::temp_dir().join(format!("memex-{}.md", std::process::id()));
    std::fs::write(&tmp, &template)?;

    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());

    let status = std::process::Command::new(&editor)
        .arg(&tmp)
        .status()
        .map_err(|e| anyhow::anyhow!("Failed to open editor '{editor}': {e}"))?;

    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("Editor exited with non-zero status");
    }

    let content = std::fs::read_to_string(&tmp)?;
    let _ = std::fs::remove_file(&tmp);

    if content.trim().is_empty() || content.trim() == template.trim() {
        anyhow::bail!("Aborted: empty or unchanged content");
    }

    Ok(content)
}

/// Convert a human-readable name into a URL-safe filename stem.
fn slugify(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    slug.split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Replace the body after frontmatter with `new_body`, preserving frontmatter verbatim.
///
/// Locates the closing `---` of the frontmatter block and searches for the body
/// only *after* it, avoiding false matches where body text also appears in a
/// frontmatter field (e.g. a short body like "TODO" matching `title: TODO`).
fn reconstruct_page(original: &str, new_body: &str) -> String {
    let trimmed = original.trim_start();
    if !trimmed.starts_with("---") {
        return original.to_string();
    }
    let after_open = &trimmed[3..];
    let Some(close_idx) = after_open.find("---") else {
        return original.to_string();
    };
    // Byte offset in `original` just past the closing ---
    let trim_offset = original.len() - trimmed.len();
    let body_region_start = trim_offset + 3 + close_idx + 3;

    if let Ok((_, old_body)) = memex_core::validate::parse_frontmatter(original)
        && !old_body.is_empty()
        && let Some(rel) = original[body_region_start..].find(&old_body)
    {
        let abs = body_region_start + rel;
        return format!("{}{}", &original[..abs], new_body);
    }
    // Body empty or not found: keep everything up to body region, append new body.
    let prefix = original[..body_region_start].trim_end();
    format!("{prefix}\n\n{new_body}")
}

/// Chunk a document body and store embeddings for each chunk.
///
/// Tries to load the ONNX embedding model from `~/.memex/models/`. If the model
/// is unavailable or fails to load, falls back to deterministic hash-based
/// embeddings so the chunks table is always populated and vector search still
/// works (with lower quality).
fn embed_document(search: &memex_core::search::Bm25Search, hash: &str, body: &str) {
    let model_path = dirs::home_dir().map(|h| h.join(".memex/models/embedding-gemma-300m.onnx"));

    let chunks = memex_core::embed::chunk_text(body, 900, 0.15);

    // Try loading the real model; fall back to hash_embedding on failure.
    let mut model_opt: Option<memex_core::embed::EmbeddingModel> = None;
    if let Some(ref mp) = model_path
        && mp.exists()
        && let Some(path_str) = mp.to_str()
        && let Ok(Ok(m)) =
            catch_unwind_silent(|| memex_core::embed::load_model(path_str, "embedding-gemma-300m"))
    {
        model_opt = Some(m);
    }

    let model_name = if model_opt.is_some() {
        "embedding-gemma-300m"
    } else {
        "hash-embedding"
    };

    let _ = search.with_connection(|conn| {
        // Delete existing chunks for this hash before re-embedding.
        memex_core::vector::delete_chunks(conn, hash)?;
        for (seq, chunk) in chunks.iter().enumerate() {
            let embedding = if let Some(ref mut model) = model_opt {
                memex_core::embed::embed_text(model, &chunk.text)
                    .unwrap_or_else(|_| memex_core::embed::hash_embedding(&chunk.text))
            } else {
                memex_core::embed::hash_embedding(&chunk.text)
            };
            let _ = memex_core::vector::store_chunk(
                conn,
                hash,
                seq as i32,
                &chunk.text,
                chunk.pos,
                chunk.len,
                model_name,
                &embedding,
            );
        }
        Ok(())
    });
}

fn run_write(name: &str, force: bool, quiet: bool, sources: &[String]) -> anyhow::Result<()> {
    // 1. Get content: piped stdin or interactive $EDITOR (like git commit).
    let content = if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        // Piped: read from stdin.
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        // Interactive terminal: open $EDITOR with a template.
        open_editor_for_page(name)?
    };

    // 2. Normalize name to kebab-case.
    let stem = slugify(name);
    if stem.is_empty() {
        anyhow::bail!("Name slugifies to empty string: {name:?}");
    }

    // 3. Validate frontmatter.
    let (fm, body) =
        memex_core::validate::parse_frontmatter(&content).map_err(|e| anyhow::anyhow!("{e}"))?;
    if fm.title.trim().is_empty() {
        anyhow::bail!("Page title is empty");
    }

    // 4. Lazy init — Memex::open creates wiki/ and DB.
    let root = memex_cli::memex_root();
    let memex = Memex::open(root.clone())?;
    let search = memex.search();

    let wiki_dir = memex.wiki_dir();
    let page_path = wiki_dir.join(format!("{stem}.md"));
    let rel_path = Path::new("wiki").join(format!("{stem}.md"));
    let rel_path_str = rel_path.to_string_lossy().to_string();

    // 5. Conflict detection: if page exists and --force not set, show conflict and exit.
    let is_overwrite = search.lookup_stem(&stem)?.is_some();
    if is_overwrite && !force {
        let existing_modified = search
            .get_last_modified(&rel_path)
            .ok()
            .flatten()
            .unwrap_or_else(|| "unknown".to_string());
        let existing_title = memex_core::index::extract_title_and_summary(
            &std::fs::read_to_string(&page_path).unwrap_or_default(),
            120,
        )
        .map(|(t, _)| t)
        .unwrap_or_default();
        println!("conflict: {stem}");
        println!("existing: \"{existing_title}\" (updated_at {existing_modified})");
        return Ok(());
    }

    // 6. Capture old content hash before overwrite (for orphan cleanup).
    let old_hash: Option<String> = if is_overwrite {
        search.get_document_hash(&rel_path_str)?
    } else {
        None
    };

    // 7. Forward linking.
    let existing_pages = search.all_stems_and_titles()?;
    let (linked_body, linked_stems) =
        memex_core::crosslink::forward_link(&body, &existing_pages, &stem);

    // Detect suggest-create candidates: wiki links that don't match any existing stem.
    let suggest_create: Vec<String> = memex_core::validate::extract_wiki_links(&linked_body)
        .into_iter()
        .filter(|link| link != &stem && !existing_pages.iter().any(|(s, _)| s == link))
        .collect();

    // 8. Reconstruct page content with linked body.
    let final_content = reconstruct_page(&content, &linked_body);

    // 9. Write file to disk.
    memex_core::storage::atomic_write(&page_path, final_content.as_bytes())?;

    // 10. Insert content into content-addressable store.
    let hash = search.insert_content(&final_content)?;

    // 11. Allocate docid.
    //     On --force overwrite, reuse the existing docid so it stays stable.
    let docid = if is_overwrite {
        search.lookup_path_docid(&rel_path)?.unwrap_or_else(|| {
            let existing = search.existing_docids().unwrap_or_default();
            let existing_vec: Vec<String> = existing.into_iter().collect();
            memex_core::docid::allocate_docid(&hash, "wiki", &rel_path_str, &existing_vec)
        })
    } else {
        let existing = search.existing_docids()?;
        let existing_vec: Vec<String> = existing.into_iter().collect();
        memex_core::docid::allocate_docid(&hash, "wiki", &rel_path_str, &existing_vec)
    };

    // 12. Compute summary.
    let summary = fm
        .summary
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| memex_core::index::extract_summary(&linked_body, 120));

    // 13. Insert/update documents row.
    let tags = fm.tags.join(", ");
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let created_at = if is_overwrite {
        fm.created_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    } else {
        now.clone()
    };
    search.upsert_document(
        "wiki",
        &rel_path_str,
        &fm.title,
        &hash,
        &docid,
        &tags,
        &summary,
        &created_at,
        &now,
    )?;

    // 14. Chunk and embed the wiki page body (use linked body for consistency
    //     with the stored content hash, which is for the linked content).
    embed_document(search, &hash, &linked_body);

    // 15. Orphan cleanup: if --force overwrite and hash changed, clean up old content.
    // cleanup_orphaned_content already deletes associated chunks internally.
    if let Some(ref old_h) = old_hash
        && old_h != &hash
    {
        let _ = search.cleanup_orphaned_content(old_h);
    }

    // 16. Handle --source: ingest source files.
    for source_path_str in sources {
        let source_path = std::path::PathBuf::from(source_path_str);
        if !source_path.exists() {
            eprintln!("Warning: source not found: {source_path_str}");
            continue;
        }
        let source_path = std::fs::canonicalize(&source_path)?;
        let source_content = std::fs::read_to_string(&source_path)?;
        let source_hash = search.insert_content(&source_content)?;
        let source_abs = source_path.to_string_lossy().to_string();
        let existing_docids = search.existing_docids()?;
        let existing_vec: Vec<String> = existing_docids.into_iter().collect();
        let source_docid =
            memex_core::docid::allocate_docid(&source_hash, "source", &source_abs, &existing_vec);
        let source_title = source_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let source_summary = memex_core::index::extract_summary(&source_content, 120);
        search.upsert_document(
            "source",
            &source_abs,
            &source_title,
            &source_hash,
            &source_docid,
            "",
            &source_summary,
            &now,
            &now,
        )?;

        // Chunk and embed source document.
        embed_document(search, &source_hash, &source_content);
    }

    // 17. Get wiki page count.
    let wiki_page_count = search.wiki_page_count()?;

    // 18. Backward linking (always runs; --quiet only suppresses output).
    let mut backlinked_stems: Vec<String> = Vec::new();
    {
        for entry in std::fs::read_dir(&wiki_dir)? {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let other_stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            if other_stem == stem {
                continue;
            }
            let other_content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let other_result = memex_core::validate::parse_frontmatter(&other_content);
            let (_, other_body) = match other_result {
                Ok(pair) => pair,
                Err(_) => continue,
            };
            let (updated_body, was_linked) = memex_core::crosslink::backward_link_page(
                &other_body,
                &stem,
                &fm.title,
                &other_stem,
            );
            if was_linked {
                // Rewrite the file with the updated body.
                let updated_content = reconstruct_page(&other_content, &updated_body);
                if memex_core::storage::atomic_write(&path, updated_content.as_bytes()).is_err() {
                    continue;
                }

                // Re-index using the real file content hash (not a synthetic one)
                // so that lint stale-index checks stay consistent.
                let other_rel = Path::new("wiki").join(format!("{other_stem}.md"));
                let other_rel_str = other_rel.to_string_lossy().to_string();
                let old_other_hash = search
                    .get_document_hash(&other_rel_str)
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let _ = search.reindex_page_from_content(
                    &other_rel_str,
                    &updated_content,
                    &old_other_hash,
                );
                backlinked_stems.push(other_stem);
            }
        }
    }

    // 19. Output.
    println!("written: {docid}");
    println!("wiki_pages: {wiki_page_count}");
    if !quiet {
        if !linked_stems.is_empty() {
            println!("linked: {}", linked_stems.join(", "));
        }
        if !backlinked_stems.is_empty() {
            println!("backlinked: {}", backlinked_stems.join(", "));
        }
        if !suggest_create.is_empty() {
            println!("suggest-create: {}", suggest_create.join(", "));
        }
    }

    Ok(())
}

fn run_delete(page_ref: &str, force: bool) -> anyhow::Result<()> {
    let root = memex_cli::memex_root();
    let memex = Memex::open(root.clone())?;
    let search = memex.search();

    // 1. Resolve via three-tier resolution — must be exactly one wiki document.
    let docs = search.resolve_ref_documents(page_ref)?;
    if docs.is_empty() {
        eprintln!("Not found: {page_ref}");
        std::process::exit(1);
    }
    let wiki_docs: Vec<_> = docs.iter().filter(|d| d.collection == "wiki").collect();
    if wiki_docs.is_empty() {
        eprintln!("Error: {page_ref} resolves to a source document, not a wiki page");
        std::process::exit(1);
    }
    if wiki_docs.len() > 1 {
        eprintln!(
            "Error: {page_ref} is ambiguous, matches {} documents",
            wiki_docs.len()
        );
        std::process::exit(1);
    }
    let doc = wiki_docs[0];
    let path = std::path::PathBuf::from(&doc.path);

    // 2. Path traversal guard.
    let full_path = root.join(&path);
    let canonical = full_path.canonicalize().unwrap_or(full_path.clone());
    {
        let wiki_base = root.join("wiki");
        let canonical_wiki = wiki_base.canonicalize().unwrap_or(wiki_base);
        if !canonical.starts_with(&canonical_wiki) {
            eprintln!("Error: path traversal rejected: {}", path.display());
            return Ok(());
        }
    }

    // 3. Derive stem.
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let docid = doc.docid.clone();
    let title = doc.title.clone();

    // 4. Confirm if TTY and --force not set. Agents (not a TTY) skip automatically.
    if !force {
        let is_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
        if is_tty {
            eprint!("Delete {stem} \"{title}\"? [y/N] ");
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).ok();
            if !answer.trim().eq_ignore_ascii_case("y") {
                eprintln!("Aborted.");
                return Ok(());
            }
        }
    }

    // 5. Find pages with incoming links to this page (will become dangling).
    let wiki_dir = memex.wiki_dir();
    let mut dangling_pages: Vec<String> = Vec::new();
    if wiki_dir.is_dir() {
        for entry in std::fs::read_dir(&wiki_dir)? {
            let entry = entry?;
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let other_stem = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            if other_stem == stem {
                continue;
            }
            let other_content = std::fs::read_to_string(&p).unwrap_or_default();
            let links = memex_core::validate::extract_wiki_links(&other_content);
            if links.iter().any(|l| l == &stem) {
                dangling_pages.push(other_stem);
            }
        }
    }

    // 6. Delete file from disk.
    std::fs::remove_file(&full_path)?;

    // 7. Delete documents row with orphan cleanup (FTS trigger fires).
    let path_str = path.to_string_lossy().to_string();
    search.delete_document_with_cleanup(&path_str)?;

    // 8. Output.
    let wiki_count = search.wiki_page_count()?;
    println!("deleted: {docid}");
    println!("wiki_pages: {wiki_count}");
    if !dangling_pages.is_empty() {
        println!("dangling: {}", dangling_pages.join(", "));
    }

    Ok(())
}

fn run_lint(fix: bool) -> anyhow::Result<()> {
    let root = memex_cli::memex_root();
    let memex = Memex::open(root)?;
    let report = memex.lint()?;

    if report.issues.is_empty() && !fix {
        println!("No issues found.");
        return Ok(());
    }

    // If --fix, attempt to fix stale-index and outdated-embedding issues first,
    // then report remaining.
    let mut remaining_issues = Vec::new();
    let mut fixed_count = 0;

    for issue in &report.issues {
        if fix && issue.kind == LintIssueKind::StaleIndex {
            // Fix stale index: re-read file from disk, update content + documents row.
            let search = memex.search();
            let full_path = memex.root().join(&issue.target);
            let rel_path = &issue.target;
            match std::fs::read_to_string(&full_path) {
                Ok(content) => {
                    // Get old hash for orphan cleanup.
                    let old_hash = search
                        .get_document_hash(rel_path)
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    match search.reindex_page_from_content(rel_path, &content, &old_hash) {
                        Ok(()) => {
                            // Re-embed after reindex so the page retains vector coverage.
                            // Orphan cleanup deletes old chunks; we need fresh ones.
                            let new_hash = search
                                .get_document_hash(rel_path)
                                .ok()
                                .flatten()
                                .unwrap_or_default();
                            let body = memex_core::validate::parse_frontmatter(&content)
                                .map(|(_, b)| b)
                                .unwrap_or_else(|_| content.clone());
                            embed_document(search, &new_hash, &body);
                            println!("fixed: {} (reindexed from disk)", issue.page);
                            fixed_count += 1;
                            continue;
                        }
                        Err(e) => {
                            eprintln!("Error fixing {}: {e}", issue.page);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error reading {}: {e}", issue.target);
                }
            }
        }
        remaining_issues.push(issue);
    }

    // If --fix, re-embed all chunks with outdated model — but only if the
    // real ONNX model is available.  Without it, embed_document falls back to
    // hash-embedding which would write the same "hash-embedding" model name,
    // creating an infinite re-embed loop on every lint --fix.
    if fix {
        let onnx_available = onnx_model_available();
        let search = memex.search();
        let outdated_hashes =
            search.outdated_chunk_hashes(memex_core::embed::CURRENT_MODEL_NAME)?;
        if !outdated_hashes.is_empty() {
            if !onnx_available {
                eprintln!(
                    "note: {} documents have hash-based embeddings; \
                     install the ONNX model to upgrade them",
                    outdated_hashes.len()
                );
            } else {
                let mut re_embedded = 0usize;
                for hash in &outdated_hashes {
                    let full_content = match search.get_content(hash) {
                        Ok(content) => content,
                        Err(e) => {
                            eprintln!("Error reading content for hash {hash}: {e}");
                            continue;
                        }
                    };
                    let body = memex_core::validate::parse_frontmatter(&full_content)
                        .map(|(_, b)| b)
                        .unwrap_or(full_content);
                    embed_document(search, hash, &body);
                    re_embedded += 1;
                }
                if re_embedded > 0 {
                    println!("re-embedded: {re_embedded} documents (model upgrade)");
                    fixed_count += re_embedded;
                    // Remove OutdatedEmbedding issues since they've been fixed.
                    remaining_issues.retain(|i| i.kind != LintIssueKind::OutdatedEmbedding);
                }
            }
        }
    }

    if remaining_issues.is_empty() && fixed_count > 0 {
        return Ok(());
    }

    if remaining_issues.is_empty() {
        println!("No issues found.");
        return Ok(());
    }

    for issue in &remaining_issues {
        match issue.kind {
            LintIssueKind::StaleIndex => {
                println!(
                    "stale-index: {} (file modified, index outdated)",
                    issue.page
                );
            }
            LintIssueKind::DanglingLink => {
                println!("dangling: {} -> [[{}]]", issue.page, issue.target);
            }
            LintIssueKind::MissingLink => {
                println!("missing-link: {} -> [[{}]]", issue.page, issue.target);
            }
            LintIssueKind::UntrackedFile => {
                println!("untracked: {} (no DB row)", issue.target);
            }
            LintIssueKind::MissingFile => {
                println!("missing-file: {} (DB row, no file)", issue.page);
            }
            LintIssueKind::OutdatedEmbedding => {
                println!("outdated-embeddings: {} ({})", issue.page, issue.target);
            }
        }
    }

    Ok(())
}

/// Try to initialize the ONNX Runtime from known install locations.
///
/// With `load-dynamic`, `ort` needs to be pointed at the dylib before any
/// session is created. We check `~/.memex/lib/` (our postinstall download)
/// and common system paths. If none is found, ort falls back to its own
/// search (ORT_DYLIB_PATH env, system library path) which may also fail —
/// embed_document and embed_query handle that gracefully via catch_unwind.
fn init_ort_runtime() {
    let lib_name = if cfg!(target_os = "windows") {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    };

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".memex/lib").join(lib_name));
    }
    if cfg!(target_os = "macos") {
        candidates.push(std::path::PathBuf::from("/opt/homebrew/lib").join(lib_name));
        candidates.push(std::path::PathBuf::from("/usr/local/lib").join(lib_name));
    } else if cfg!(target_os = "linux") {
        candidates.push(std::path::PathBuf::from("/usr/lib").join(lib_name));
        candidates.push(std::path::PathBuf::from("/usr/lib/x86_64-linux-gnu").join(lib_name));
    } else if cfg!(target_os = "windows") {
        if let Ok(pf) = std::env::var("ProgramFiles") {
            candidates.push(
                std::path::PathBuf::from(pf)
                    .join("onnxruntime/lib")
                    .join(lib_name),
            );
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            candidates.push(
                std::path::PathBuf::from(local)
                    .join("onnxruntime/lib")
                    .join(lib_name),
            );
        }
    }

    for path in &candidates {
        if path.exists() {
            let _ = catch_unwind_silent(|| memex_core::embed::init_runtime(path));
            return;
        }
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Search {
            query,
            lex,
            vec,
            hyde,
            expand,
        } => {
            init_ort_runtime();
            run_search(&query, &lex, &vec, &hyde, &expand)?;
        }
        Commands::Read { refs } => run_read(&refs)?,
        Commands::Write {
            name,
            force,
            quiet,
            sources,
        } => {
            init_ort_runtime();
            run_write(&name, force, quiet, &sources)?;
        }
        Commands::Delete { page_ref, force } => run_delete(&page_ref, force)?,
        Commands::Lint { fix } => {
            if fix {
                init_ort_runtime();
            }
            run_lint(fix)?;
        }
    }

    Ok(())
}
