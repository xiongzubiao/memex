use clap::{Parser, Subcommand};
use memex_core::retrieval::result_stem;
use memex_core::Memex;
use memex_core::search::WikiSearch;
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
    /// Manage the memex daemon (query-path persistence)
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Query the wiki via the daemon
    Query {
        /// The question
        question: String,
        /// Return structured retrieval data instead of synthesized answer
        #[arg(long)]
        raw: bool,
        /// How many pages to retrieve (default 5)
        #[arg(long, default_value = "5")]
        top_k: usize,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Run the daemon in the foreground (logs to file; block until SIGTERM/idle)
    Start,
    /// Stop the running daemon (SIGTERM via PID file)
    Stop,
    /// Report daemon status (PID + ping)
    Status,
}

/// Embed a query: load ONNX on demand, embed, fall back to hash on failure.
/// Thin wrapper over `memex_core::retrieval::embed_query`.
fn embed_query(text: &str) -> Vec<f32> {
    let mut model = memex_core::retrieval::load_default_model();
    memex_core::retrieval::embed_query(model.as_mut(), text)
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

    // All three modes use the same shared hybrid_retrieve_expanded — they
    // differ only in where the expansion terms come from and what's printed.
    use memex_core::retrieval::{Expansion, Signal};

    let q_emb = embed_query(query);
    let expansion = if has_typed_flags {
        Expansion {
            lex: lex.to_vec(),
            vec_embs: vec.iter().map(|t| embed_query(t)).collect(),
            hyde_embs: hyde.iter().map(|t| embed_query(t)).collect(),
        }
    } else if !expand.is_empty() {
        // Legacy --expand: each term runs as a lex probe (wiki 2x + source).
        Expansion {
            lex: expand.to_vec(),
            vec_embs: vec![],
            hyde_embs: vec![],
        }
    } else {
        Expansion::default()
    };
    let hit = memex_core::retrieval::hybrid_retrieve_expanded(search, query, &q_emb, &expansion)?;

    // Typed-flags mode suppresses the signal line (matches prior behavior);
    // default and legacy modes print it.
    if !has_typed_flags {
        let signal = match hit.signal {
            Signal::Strong => "strong",
            Signal::Weak => "weak",
        };
        println!("signal: {signal}");
    }

    for result in &hit.results {
        let stem = result_stem(result);
        println!(
            "{}\t{}\t{:.3}\t{}\t{}",
            result.docid, result.collection, result.score, stem, result.snippet
        );
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
            eprintln!("not found: {reference}");
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
        && let Some(Ok(m)) = memex_core::embed::catch_unwind_silent(|| {
            memex_core::embed::load_model(path_str, "embedding-gemma-300m")
        })
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
    // Phase 1 — input (no lock).
    let content = if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        open_editor_for_page(name)?
    };

    let stem = slugify(name);
    if stem.is_empty() {
        anyhow::bail!("Name slugifies to empty string: {name:?}");
    }

    let (fm, body) =
        memex_core::validate::parse_frontmatter(&content).map_err(|e| anyhow::anyhow!("{e}"))?;
    if fm.title.trim().is_empty() {
        anyhow::bail!("Page title is empty");
    }

    // Phase 2 — acquire writer lock.
    let root = memex_cli::memex_root();
    let memex = memex_core::Memex::open_writer(root.clone())?;
    let search = memex.search();

    let wiki_dir = memex.wiki_dir();
    let page_path = wiki_dir.join(format!("{stem}.md"));
    let rel_path = Path::new("wiki").join(format!("{stem}.md"));
    let rel_path_str = rel_path.to_string_lossy().to_string();

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

    let old_hash: Option<String> = if is_overwrite {
        search.get_document_hash(&rel_path_str)?
    } else {
        None
    };

    // Forward linking
    let existing_pages = search.all_stems_and_titles()?;
    let (linked_body, linked_stems) =
        memex_core::crosslink::forward_link(&body, &existing_pages, &stem);

    let suggest_create: Vec<String> = memex_core::validate::extract_wiki_links(&linked_body)
        .into_iter()
        .filter(|link| link != &stem && !existing_pages.iter().any(|(s, _)| s == link))
        .collect();

    let final_content = reconstruct_page(&content, &linked_body);
    memex_core::storage::atomic_write(&page_path, final_content.as_bytes())?;

    let hash = search.insert_content(&final_content)?;

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

    let summary = fm
        .summary
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| memex_core::index::extract_summary(&linked_body, 120));
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

    embed_document(search, &hash, &linked_body);

    if let Some(ref old_h) = old_hash
        && old_h != &hash
    {
        let _ = search.cleanup_orphaned_content(old_h);
    }

    // Source ingestion
    for source_path_str in sources {
        let source_path = std::path::PathBuf::from(source_path_str);
        if !source_path.exists() {
            eprintln!("warning: source not found: {source_path_str} (skipping)");
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
        embed_document(search, &source_hash, &source_content);
    }

    let wiki_page_count = search.wiki_page_count()?;

    // Backward linking with failure reporting
    let mut backlinked_stems: Vec<String> = Vec::new();
    let mut backlink_failed: Vec<(String, String)> = Vec::new();

    match std::fs::read_dir(&wiki_dir) {
        Ok(iter) => {
            for entry in iter {
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
                let (_, other_body) = match memex_core::validate::parse_frontmatter(&other_content)
                {
                    Ok(pair) => pair,
                    Err(_) => continue,
                };
                let (updated_body, was_linked) = memex_core::crosslink::backward_link_page(
                    &other_body,
                    &stem,
                    &fm.title,
                    &other_stem,
                );
                if !was_linked {
                    continue;
                }

                let updated_content = reconstruct_page(&other_content, &updated_body);
                match memex_core::storage::atomic_write(&path, updated_content.as_bytes()) {
                    Ok(()) => {
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
                    Err(e) => {
                        let reason = match &e {
                            memex_core::error::MemexError::FileOpExhausted { .. } => {
                                "file busy after retry".to_string()
                            }
                            other => format!("{other}"),
                        };
                        backlink_failed.push((other_stem, reason));
                    }
                }
            }
        }
        Err(e) => {
            backlink_failed.push(("(scan)".into(), format!("could not scan wiki dir: {e}")));
        }
    }

    // Output
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
    if !backlink_failed.is_empty() {
        for (stem, reason) in &backlink_failed {
            println!("backlink-failed: {stem} ({reason})");
        }
        println!("backlink-failed-count: {}", backlink_failed.len());
    }

    Ok(())
}

fn run_delete(page_ref: &str, force: bool) -> anyhow::Result<()> {
    let root = memex_cli::memex_root();

    // Phase 1: resolve + confirm (reader, no lock).
    let reader = memex_core::Memex::open(root.clone())?;
    let rs = reader.search();
    let docs = rs.resolve_ref_documents(page_ref)?;
    if docs.is_empty() {
        die(format!("not found: {page_ref}"));
    }
    let wiki_docs: Vec<_> = docs.iter().filter(|d| d.collection == "wiki").collect();
    if wiki_docs.is_empty() {
        die(format!(
            "Error: {page_ref} resolves to a source document, not a wiki page"
        ));
    }
    if wiki_docs.len() > 1 {
        die(format!(
            "Error: {page_ref} is ambiguous, matches {} documents",
            wiki_docs.len()
        ));
    }
    let doc_pre = wiki_docs[0];
    let confirmed_docid = doc_pre.docid.clone();
    let confirmed_title = doc_pre.title.clone();

    if !force {
        let is_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
        if is_tty {
            let path = std::path::PathBuf::from(&doc_pre.path);
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            eprint!("Delete {stem} \"{confirmed_title}\"? [y/N] ");
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).ok();
            if !answer.trim().eq_ignore_ascii_case("y") {
                eprintln!("Aborted.");
                return Ok(());
            }
        }
    }
    drop(reader); // release SQLite conn before taking writer lock

    // Phase 2: acquire writer lock + re-resolve by docid.
    let memex = memex_core::Memex::open_writer(root.clone())?;
    let search = memex.search();
    let docs2 = search.resolve_ref_documents(page_ref)?;
    let wiki_docs2: Vec<_> = docs2.iter().filter(|d| d.collection == "wiki").collect();
    let doc = match wiki_docs2.first() {
        Some(d) if d.docid == confirmed_docid => *d,
        Some(d) => {
            die(format!(
                "Error: {page_ref} changed between confirmation and delete \
                 (confirmed: docid={confirmed_docid}, now: docid={}). Re-run to verify.",
                d.docid
            ));
        }
        None => {
            die(format!(
                "Error: {page_ref} no longer exists (another process deleted it)."
            ));
        }
    };

    let path = std::path::PathBuf::from(&doc.path);
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

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let docid = doc.docid.clone();

    // Phase 3 (under lock): find dangling, remove file, delete row.
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

    memex_core::storage::retry_io(&full_path, "remove wiki file", || {
        std::fs::remove_file(&full_path)
    })?;
    let path_str = path.to_string_lossy().to_string();
    search.delete_document_with_cleanup(&path_str)?;

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
    let memex = memex_core::Memex::open(root)?;
    let report = memex.lint()?;

    if !fix {
        if report.issues.is_empty() {
            println!("No issues found.");
            return Ok(());
        }
        for issue in &report.issues {
            match issue.kind {
                memex_core::types::LintIssueKind::StaleIndex => println!(
                    "stale-index: {} (file modified, index outdated)",
                    issue.page
                ),
                memex_core::types::LintIssueKind::DanglingLink => {
                    println!("dangling: {} -> [[{}]]", issue.page, issue.target)
                }
                memex_core::types::LintIssueKind::MissingLink => {
                    println!("missing-link: {} -> [[{}]]", issue.page, issue.target)
                }
                memex_core::types::LintIssueKind::UntrackedFile => {
                    println!("untracked: {} (no DB row)", issue.target)
                }
                memex_core::types::LintIssueKind::MissingFile => {
                    println!("missing-file: {} (DB row, no file)", issue.page)
                }
                memex_core::types::LintIssueKind::OutdatedEmbedding => {
                    println!("outdated-embeddings: {} ({})", issue.page, issue.target)
                }
            }
        }
        return Ok(());
    }

    // --fix path: per-fix lock with re-verify via apply_fix_locked.
    let mut applied = 0usize;
    let mut stale = 0usize;
    let mut remaining: Vec<&memex_core::types::LintIssue> = Vec::new();

    for issue in &report.issues {
        // Only StaleIndex and OutdatedEmbedding have auto-fix paths.
        match issue.kind {
            memex_core::types::LintIssueKind::StaleIndex
            | memex_core::types::LintIssueKind::OutdatedEmbedding => {
                match memex.apply_fix_locked(issue) {
                    Ok(memex_core::FixOutcome::Applied) => {
                        match issue.kind {
                            memex_core::types::LintIssueKind::StaleIndex => {
                                println!("fixed: {} (reindexed from disk)", issue.page)
                            }
                            memex_core::types::LintIssueKind::OutdatedEmbedding => {
                                println!("re-embedded: {}", issue.page)
                            }
                            _ => unreachable!(),
                        }
                        applied += 1;
                    }
                    Ok(memex_core::FixOutcome::Stale) => {
                        println!("already-fixed: {}", issue.page);
                        stale += 1;
                    }
                    Err(e) => {
                        eprintln!("Error: failed to fix {}", issue.page);
                        eprintln!("  caused by: {e}");
                    }
                }
            }
            _ => remaining.push(issue),
        }
    }

    for issue in remaining {
        match issue.kind {
            memex_core::types::LintIssueKind::DanglingLink => {
                println!("dangling: {} -> [[{}]]", issue.page, issue.target)
            }
            memex_core::types::LintIssueKind::MissingLink => {
                println!("missing-link: {} -> [[{}]]", issue.page, issue.target)
            }
            memex_core::types::LintIssueKind::UntrackedFile => {
                println!("untracked: {} (no DB row)", issue.target)
            }
            memex_core::types::LintIssueKind::MissingFile => {
                println!("missing-file: {} (DB row, no file)", issue.page)
            }
            _ => {}
        }
    }

    if applied + stale > 0 {
        println!("lint-fix-summary: applied={applied} stale={stale}");
    }
    Ok(())
}

/// Initialize the ONNX Runtime. Thin wrapper over
/// `memex_core::embed::init_runtime` that additionally wraps the call in
/// `catch_unwind_silent` (ort can panic rather than return Err on some
/// malformed dylibs).
fn init_ort_runtime() -> Result<(), String> {
    match memex_core::embed::catch_unwind_silent(memex_core::embed::init_runtime) {
        Some(Ok(())) => Ok(()),
        Some(Err(e)) => Err(e.to_string()),
        None => Err("ONNX Runtime init panicked".to_string()),
    }
}

/// Print an error message and exit with code 1. Used for `run_delete`'s
/// hard-fail paths (ref not found, ambiguous, wrong collection, TOCTOU
/// detection). These bypass `main()`'s `actionable_hint` pipeline on purpose
/// — the messages are complete diagnostic output, and there's no `MemexError`
/// variant that would add a useful hint.
fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

fn main() {
    let cli = Cli::parse();
    let result = dispatch(cli);
    let exit_code = match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            let mut src = e.source();
            while let Some(s) = src {
                eprintln!("  caused by: {s}");
                src = s.source();
            }
            if let Some(hint) = actionable_hint(&e) {
                eprintln!("{hint}");
            }
            exit_code_for(&e)
        }
    };
    std::process::exit(exit_code);
}

fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Commands::Search {
            query,
            lex,
            vec,
            hyde,
            expand,
        } => {
            let _ = init_ort_runtime();
            run_search(&query, &lex, &vec, &hyde, &expand)
        }
        Commands::Read { refs } => run_read(&refs),
        Commands::Write {
            name,
            force,
            quiet,
            sources,
        } => {
            let _ = init_ort_runtime();
            run_write(&name, force, quiet, &sources)
        }
        Commands::Delete { page_ref, force } => run_delete(&page_ref, force),
        Commands::Lint { fix } => {
            if fix {
                let _ = init_ort_runtime();
            }
            run_lint(fix)
        }
        Commands::Daemon { action } => match action {
            DaemonAction::Start => {
                init_ort_runtime().map_err(|e| anyhow::anyhow!(e))?;
                std::process::exit(memex_cli::daemon::start_foreground()?)
            }
            DaemonAction::Stop => std::process::exit(memex_cli::daemon::stop()?),
            DaemonAction::Status => std::process::exit(memex_cli::daemon::status()?),
        },
        Commands::Query {
            question,
            raw,
            top_k,
        } => {
            let code = if raw {
                memex_cli::daemon::query_raw(&question, top_k, None)?
            } else {
                memex_cli::daemon::query_synth(&question, top_k, None)?
            };
            std::process::exit(code);
        }
    }
}

fn exit_code_for(err: &anyhow::Error) -> i32 {
    for cause in err.chain() {
        if let Some(me) = cause.downcast_ref::<memex_core::error::MemexError>() {
            return match me {
                memex_core::error::MemexError::LockTimeout { .. } => 2,
                memex_core::error::MemexError::FileOpExhausted { .. } => 3,
                _ => 1,
            };
        }
    }
    1
}

fn actionable_hint(err: &anyhow::Error) -> Option<String> {
    use memex_core::error::MemexError;
    let me = err.chain().find_map(|c| c.downcast_ref::<MemexError>())?;
    use std::io::ErrorKind as K;
    Some(match me {
        MemexError::LockTimeout { lock_path, .. } => format!(
            "To see the holder: `lsof {}` (macOS/Linux) or SysInternals `handle.exe` (Windows).\n\
             If your workload legitimately needs longer, raise MEMEX_LOCK_TIMEOUT_SECONDS.",
            lock_path.display()
        ),
        MemexError::LockAcquireIo { lock_path, source } => {
            let parent = lock_path
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "its parent directory".into());
            match source.kind() {
                K::PermissionDenied => format!(
                    "Check permissions on {} (is {} writable by your user?).",
                    lock_path.display(),
                    parent
                ),
                K::NotFound => format!(
                    "Parent directory of {} is missing. Create {} or check your MEMEX_ROOT.",
                    lock_path.display(),
                    parent
                ),
                _ => format!(
                    "Underlying I/O: check {}'s path and permissions.",
                    lock_path.display()
                ),
            }
        }
        MemexError::FileOpExhausted { .. } => String::from(
            "A process likely has the file open (editor, cloud sync, antivirus, backup).\n\
             Close any programs using that file, then retry.",
        ),
        MemexError::FileOpFailed { path, source, .. } => match source.kind() {
            K::NotFound => format!(
                "Check that the parent directory of {} exists.",
                path.display()
            ),
            K::PermissionDenied => format!(
                "Check write permission on the parent directory of {}.",
                path.display()
            ),
            K::InvalidInput | K::InvalidData => format!(
                "The path {} is invalid or contains unsupported characters.",
                path.display()
            ),
            _ => return None,
        },
        MemexError::MalformedConfig { path, .. } => format!(
            "Edit {} to fix the issue, or delete it to fall back to defaults.",
            path.display()
        ),
        MemexError::InvalidEnvVar { var, .. } => {
            format!("Set {var} to a number between 1 and 3600 (seconds), e.g. `export {var}=60`.")
        }
        _ => return None,
    })
}
