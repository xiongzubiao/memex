use clap::{Parser, Subcommand, ValueEnum};
use memex_core::Memex;
use std::io::Read as _;
use std::path::Path;

#[derive(Parser)]
#[command(name = "memex", about = "Personal wiki storage and search engine")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Debug, ValueEnum)]
enum Agent {
    /// Claude Code sessions (~/.claude/projects/*/*.jsonl)
    ClaudeCode,
    /// OpenAI Codex CLI sessions (~/.codex/sessions/*/*/*/*.jsonl)
    Codex,
    /// Google Gemini CLI sessions (~/.gemini/tmp/*/chats/session-*.json)
    GeminiCli,
}

impl Agent {
    fn as_str(&self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude-code",
            Agent::Codex => "codex",
            Agent::GeminiCli => "gemini-cli",
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Search wiki pages by title
    Search {
        /// Page title to search for
        title: String,
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
        /// docid of an already-stored source (from `memex source add`); attach to this page
        #[arg(long)]
        source: Option<String>,
        /// Bypass daemon and write directly to SQLite (escape hatch)
        #[arg(long)]
        direct: bool,
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
        /// Restrict search to one or more collections
        #[arg(long = "collection")]
        collections: Vec<String>,
    },
    /// Ingest a session transcript (`--agent` or auto-detected) or a pre-converted
    /// document file (`<path>`), or stdin content (`--source <id>`).
    Ingest {
        /// Agent override for transcript ingestion. If omitted with a positional
        /// path, memex infers the agent from file content.
        #[arg(long, requires = "path", conflicts_with = "source")]
        agent: Option<Agent>,
        /// Filesystem path: transcript file (with --agent or auto-detected) OR
        /// text/Markdown document file that memex reads directly.
        #[arg(conflicts_with = "source")]
        path: Option<std::path::PathBuf>,
        /// Source identifier for stdin-piped content (URL, logical name, or any
        /// string that doesn't resolve to a readable file). Reads stdin as content.
        #[arg(long)]
        source: Option<String>,
        /// Collections to associate with the ingested content
        #[arg(long = "collection")]
        collections: Vec<String>,
    },
    /// Bulk-ingest historical sessions via the daemon
    Backfill {
        /// Agent type (claude-code, codex, gemini-cli)
        agent: Agent,
        /// Restrict ingestion to one or more collections
        #[arg(long = "collection")]
        collections: Vec<String>,
        /// Override discovery: ingest every *.jsonl under this directory (recursive)
        #[arg(long)]
        path: Option<std::path::PathBuf>,
    },
    /// Show daemon and ingestion status
    Status,
    /// Manage source documents (the raw material wiki pages reference)
    Source {
        #[command(subcommand)]
        action: SourceAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Run the daemon (daemonized by default)
    Start {
        /// Run in foreground instead of daemonizing (for debugging)
        #[arg(long, default_value = "false")]
        foreground: bool,
    },
    /// Stop the running daemon (SIGTERM via PID file)
    Stop,
    /// Report daemon status (PID + ping)
    Status,
}

#[derive(Subcommand)]
enum SourceAction {
    /// Store source content (stdin = bytes), print docid to stdout
    Add {
        /// Source identifier — URL, file path, or arbitrary label
        path: String,
        /// Restrict source to one or more collections
        #[arg(long = "collection")]
        collections: Vec<String>,
        /// Print human-readable info to stderr (size, summary). Stdout
        /// stays just the docid for shell composition.
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// List source documents
    List {
        /// Filter to one or more collections
        #[arg(long = "collection")]
        collections: Vec<String>,
        /// Emit one JSON object per row (for scripting)
        #[arg(long)]
        json: bool,
    },
    /// Print source content to stdout
    Show {
        /// docid (e.g. src-abc123) or 'path:<source-path>'
        reference: String,
    },
    /// Delete a source document
    Delete {
        /// docid or 'path:<source-path>'
        reference: String,
        /// Skip confirmation; required when wiki pages reference this source
        #[arg(long)]
        force: bool,
    },
}

fn run_search(title: &str) -> anyhow::Result<()> {
    let root = memex_cli::memex_root();
    let memex = Memex::open(root)?;
    let search = memex.search();
    let mut model = memex_core::retrieval::load_default_model()?;

    if let Some(slug) = memex_core::retrieval::search_wiki_by_title(search, title, &mut model)? {
        println!("{slug}");
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
            let body = if doc.doc_type == "wiki" {
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

            println!("=== {} {} {} ===", doc.docid, doc.doc_type, stem);
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

/// `memex source delete <ref>`. Mutation — routed through the daemon
/// (`Request::SourceDelete`). Reports any wiki pages that now have
/// dangling source references.
fn run_source_delete(reference: &str, force: bool) -> anyhow::Result<()> {
    let root = memex_cli::memex_root();
    let root_str = root.to_string_lossy().to_string();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let paths = memex_cli::daemon::server::DaemonPaths::default_under(&root);
        let stream = memex_cli::daemon::client::connect_or_spawn(
            &paths.socket,
            &paths.lock,
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .await?;
        let events = memex_cli::daemon::client::request(
            stream,
            &memex_cli::daemon::protocol::Request::SourceDelete {
                ref_: reference.to_string(),
                force,
                memex_root: root_str,
            },
        )
        .await?;
        let mut exit_code = 0;
        for ev in &events {
            match ev {
                memex_cli::daemon::protocol::Event::SourceDeleted {
                    docid,
                    source_path,
                    dangling_wiki_pages,
                } => {
                    println!("deleted: {docid} ({source_path})");
                    if !dangling_wiki_pages.is_empty() {
                        println!(
                            "warning: {} wiki page(s) still reference this source path in their `sources:` frontmatter: {}",
                            dangling_wiki_pages.len(),
                            dangling_wiki_pages.join(", ")
                        );
                        println!(
                            "  edit each page to drop the entry, or attach a replacement source via `memex write --source <docid>`."
                        );
                    }
                }
                memex_cli::daemon::protocol::Event::Error {
                    code,
                    message,
                    status,
                } => {
                    eprintln!("delete error ({code}): {message}");
                    exit_code = *status;
                }
                _ => {}
            }
        }
        if exit_code != 0 {
            std::process::exit(exit_code);
        }
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

/// `memex source show <ref>`. Read-only; resolves a docid or
/// `path:<source-path>` and prints the source body to stdout.
fn run_source_show(reference: &str) -> anyhow::Result<()> {
    let root = memex_cli::memex_root();
    let memex = memex_core::Memex::open(root)?;
    let search = memex.search();

    let doc = if let Some(rest) = reference.strip_prefix("path:") {
        search.lookup_source_by_path(rest)?
    } else {
        let docs = search.resolve_ref_documents(reference)?;
        docs.into_iter().find(|d| d.doc_type == "source")
    };
    let doc = match doc {
        Some(d) => d,
        None => {
            anyhow::bail!(
                "source not found: '{reference}'. Use 'src-...' for docid \
                 or 'path:<source-path>'."
            );
        }
    };
    let body = search.get_content(&doc.hash)?;
    print!("{body}");
    if !body.ends_with('\n') {
        println!();
    }
    Ok(())
}

/// `memex source list`. Read-only; bypasses the daemon and queries the
/// search DB directly.
fn run_source_list(collections: &[String], json: bool) -> anyhow::Result<()> {
    let root = memex_cli::memex_root();
    let memex = memex_core::Memex::open(root)?;
    let search = memex.search();
    let rows = search.list_sources(collections)?;

    if json {
        for r in &rows {
            println!("{}", serde_json::to_string(r)?);
        }
        return Ok(());
    }

    if rows.is_empty() {
        println!("No source documents.");
        return Ok(());
    }

    println!("{:<20} {:<10} {:<24} PATH", "DOCID", "SIZE", "CREATED");
    for r in &rows {
        println!(
            "{:<20} {:<10} {:<24} {}",
            truncate_for_table(&r.docid, 20),
            human_size(r.size_bytes),
            &r.created_at,
            truncate_for_table(&r.path, 80)
        );
    }
    Ok(())
}

fn truncate_for_table(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}

fn human_size(bytes: usize) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b < K {
        format!("{bytes}B")
    } else if b < K * K {
        format!("{:.1}K", b / K)
    } else {
        format!("{:.1}M", b / (K * K))
    }
}

/// `memex source add <path>`. Reads stdin into a UTF-8 string, sends
/// `Request::SourceAdd` to the daemon, prints the allocated docid on stdout.
fn run_source_add(
    source_path: &str,
    collections: &[String],
    verbose: bool,
) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    std::io::stdin().read_to_end(&mut buf)?;
    let content = String::from_utf8(buf)
        .map_err(|e| anyhow::anyhow!("source content is not valid UTF-8: {e}"))?;
    if content.trim().is_empty() {
        anyhow::bail!("empty source content on stdin");
    }
    let content_size = content.len();
    let root = memex_cli::memex_root();
    let root_str = root.to_string_lossy().to_string();
    // Pass collections through verbatim. The daemon's `normalize_collections`
    // adds "default" for new sources; for re-adds we want the empty list
    // to signal "leave existing collections alone" (handle_source_add gates
    // its set_document_collections_by_path call on `!collections.is_empty()`).
    let collections = collections.to_vec();

    let rt = tokio::runtime::Runtime::new()?;
    let docid = rt.block_on(async move {
        let paths = memex_cli::daemon::server::DaemonPaths::default_under(&root);
        let stream = memex_cli::daemon::client::connect_or_spawn(
            &paths.socket,
            &paths.lock,
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .await?;
        let events = memex_cli::daemon::client::request(
            stream,
            &memex_cli::daemon::protocol::Request::SourceAdd {
                source_path: source_path.to_string(),
                content,
                collections,
                memex_root: root_str,
            },
        )
        .await?;
        for ev in &events {
            if let memex_cli::daemon::protocol::Event::SourceAdded { docid } = ev {
                return Ok::<String, anyhow::Error>(docid.clone());
            }
            if let memex_cli::daemon::protocol::Event::Error { code, message, .. } = ev {
                anyhow::bail!("source add error ({code}): {message}");
            }
        }
        anyhow::bail!("daemon did not return SourceAdded event")
    })?;
    if verbose {
        eprintln!("Stored: {source_path} ({content_size} bytes) → {docid}");
        eprintln!("Use: memex write <slug> --source {docid}");
    }
    println!("{docid}");
    Ok(())
}

/// Write a wiki page via the daemon. Sends Request::Write.
fn run_write_via_daemon(
    name: &str,
    force: bool,
    quiet: bool,
    source: Option<&str>,
) -> anyhow::Result<()> {
    // Read content from stdin (same as direct path)
    let content = if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        open_editor_for_page(name)?
    };

    if content.trim().is_empty() {
        anyhow::bail!("empty content");
    }

    let root = memex_cli::memex_root();
    let root_str = root.to_string_lossy();

    // Extract tags from frontmatter if present
    let tags = memex_core::search::parse_page_for_indexing(&content)
        .map(|(_, _, tags_str, _, _)| {
            tags_str
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let rt = tokio::runtime::Runtime::new()?;
    let result: anyhow::Result<Vec<memex_cli::daemon::protocol::Event>> = rt.block_on(async {
        let paths = memex_cli::daemon::server::DaemonPaths::default_under(&root);
        let stream = memex_cli::daemon::client::connect_or_spawn(
            &paths.socket,
            &paths.lock,
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .await?;
        let events = memex_cli::daemon::client::request(
            stream,
            &memex_cli::daemon::protocol::Request::Write {
                title: name.to_string(),
                content,
                tags,
                source: source.map(str::to_string),
                force,
                memex_root: root_str.to_string(),
            },
        )
        .await?;
        Ok(events)
    });

    let events = result?;
    for ev in &events {
        match ev {
            memex_cli::daemon::protocol::Event::Written { slug, docid } => {
                if !quiet {
                    println!("written: {slug} ({docid})");
                }
            }
            memex_cli::daemon::protocol::Event::Error { code, message, .. } => {
                eprintln!("write error ({code}): {message}");
            }
            _ => {}
        }
    }

    // Daemon returns not_implemented for now — fall back to direct write
    let has_error = events.iter().any(|e| matches!(e, memex_cli::daemon::protocol::Event::Error { code, .. } if code == "not_implemented"));
    if has_error {
        eprintln!("memex: daemon write not yet implemented, falling back to direct write");
        // Re-read stdin won't work (already consumed). Just report the fallback.
        // The user should use --direct until daemon write is implemented.
        anyhow::bail!("daemon write routing not yet implemented. Use --direct flag.");
    }

    Ok(())
}

/// Write a wiki page directly to SQLite (--direct flag).
fn run_write(
    name: &str,
    force: bool,
    quiet: bool,
    source: Option<&str>,
) -> anyhow::Result<()> {
    if source.is_some() {
        anyhow::bail!(
            "--source attachment not supported with --direct; use the daemon path instead"
        );
    }
    // Phase 1 — input (no lock).
    let content = if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        open_editor_for_page(name)?
    };

    let stem = memex_cli::slugify(name);
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

    let mut model = memex_core::retrieval::load_default_model()?;
    memex_core::retrieval::embed_document(search, &hash, &linked_body, &mut model)?;

    if let Some(ref old_h) = old_hash
        && old_h != &hash
    {
        let _ = search.cleanup_orphaned_content(old_h);
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
    let wiki_docs: Vec<_> = docs.iter().filter(|d| d.doc_type == "wiki").collect();
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
    let wiki_docs2: Vec<_> = docs2.iter().filter(|d| d.doc_type == "wiki").collect();
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
/// hard-fail paths (ref not found, ambiguous, wrong doc_type, TOCTOU
/// detection). These bypass `main()`'s `actionable_hint` pipeline on purpose
/// — the messages are complete diagnostic output, and there's no `MemexError`
/// variant that would add a useful hint.
fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

// ---------------------------------------------------------------------------
// Session ingestion
// ---------------------------------------------------------------------------

/// Discover session files for an agent. `root_override`, when provided,
/// replaces the agent's default root (e.g. `~/.claude` for Claude Code);
/// the agent-specific subpattern (`projects/*/*.jsonl`, etc.) still applies.
fn discover_session_files(
    agent: &Agent,
    root_override: Option<&std::path::Path>,
) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let root = if let Some(p) = root_override {
        if !p.is_dir() {
            anyhow::bail!("{} is not a directory", p.display());
        }
        p.to_path_buf()
    } else {
        let home =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
        match agent {
            Agent::ClaudeCode => home.join(".claude"),
            Agent::Codex => home.join(".codex"),
            Agent::GeminiCli => home.join(".gemini"),
        }
    };
    let sub = match agent {
        Agent::ClaudeCode => "projects/*/*.jsonl",
        Agent::Codex => "sessions/*/*/*/*.jsonl",
        Agent::GeminiCli => "tmp/*/chats/session-*.json",
    };
    let pattern = root.join(sub);
    let files: Vec<std::path::PathBuf> = glob::glob(&pattern.to_string_lossy())
        .map_err(|e| anyhow::anyhow!("Glob error: {e}"))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(files)
}

// ---------------------------------------------------------------------------
// Daemon thin clients (ingest, backfill, status)
// ---------------------------------------------------------------------------

/// Thin hook client: read stdin JSON, extract transcript_path, send to daemon.
fn default_ingest_collections(collections: &[String]) -> Vec<String> {
    if collections.is_empty() {
        vec!["default".to_string()]
    } else {
        collections.to_vec()
    }
}

/// Map a CLI `Agent` to the protocol `TranscriptAgent` used in
/// `Request::Ingest`.
fn agent_to_protocol(agent: &Agent) -> memex_cli::daemon::protocol::TranscriptAgent {
    match agent {
        Agent::ClaudeCode => memex_cli::daemon::protocol::TranscriptAgent::ClaudeCode,
        Agent::Codex => memex_cli::daemon::protocol::TranscriptAgent::Codex,
        Agent::GeminiCli => memex_cli::daemon::protocol::TranscriptAgent::GeminiCli,
    }
}

/// Map a `core::transcript::TranscriptAgent` (returned by inference) to the
/// protocol's `TranscriptAgent` (sent over the daemon socket).
fn core_agent_to_protocol(
    agent: memex_core::transcript::TranscriptAgent,
) -> memex_cli::daemon::protocol::TranscriptAgent {
    match agent {
        memex_core::transcript::TranscriptAgent::ClaudeCode => {
            memex_cli::daemon::protocol::TranscriptAgent::ClaudeCode
        }
        memex_core::transcript::TranscriptAgent::Codex => {
            memex_cli::daemon::protocol::TranscriptAgent::Codex
        }
        memex_core::transcript::TranscriptAgent::GeminiCli => {
            memex_cli::daemon::protocol::TranscriptAgent::GeminiCli
        }
    }
}

fn run_ingest_client(
    agent: Option<&Agent>,
    path: Option<&std::path::Path>,
    source: Option<&str>,
    collections: &[String],
) -> anyhow::Result<()> {
    if std::env::var("MEMEX_INTERNAL").as_deref() == Ok("1") {
        return Ok(());
    }
    let root = memex_cli::memex_root();
    let root_str = root.to_string_lossy().to_string();
    let collections = if collections.is_empty() {
        vec!["default".to_string()]
    } else {
        collections.to_vec()
    };

    let request = match (agent, path, source) {
        // Transcript mode: explicit --agent + positional path
        (Some(agent), Some(p), None) => {
            let p_abs = std::fs::canonicalize(p)
                .map_err(|e| anyhow::anyhow!("transcript path: {e}"))?;
            memex_cli::daemon::protocol::Request::Ingest {
                source: memex_cli::daemon::protocol::IngestSource::Transcript {
                    path: p_abs.to_string_lossy().to_string(),
                    agent: agent_to_protocol(agent),
                },
                collections,
                memex_root: root_str,
            }
        }
        // Positional path alone (no --agent): try inference, fall back to document mode
        (None, Some(p), None) => {
            let p_abs = std::fs::canonicalize(p)
                .map_err(|e| anyhow::anyhow!("path: {e}"))?;
            if let Some(detected_agent) = memex_core::transcript::detect_transcript_agent(&p_abs) {
                memex_cli::daemon::protocol::Request::Ingest {
                    source: memex_cli::daemon::protocol::IngestSource::Transcript {
                        path: p_abs.to_string_lossy().to_string(),
                        agent: core_agent_to_protocol(detected_agent),
                    },
                    collections,
                    memex_root: root_str,
                }
            } else {
                let content = std::fs::read_to_string(&p_abs)
                    .map_err(|e| anyhow::anyhow!("read {}: {e}", p_abs.display()))?;
                if content.trim().is_empty() {
                    anyhow::bail!("empty document file: {}", p_abs.display());
                }
                memex_cli::daemon::protocol::Request::Ingest {
                    source: memex_cli::daemon::protocol::IngestSource::Document {
                        source_path: p_abs.to_string_lossy().to_string(),
                        content,
                    },
                    collections,
                    memex_root: root_str,
                }
            }
        }
        // Stdin mode: --source + content piped on stdin. Bail early on
        // oversize so we don't waste the IPC round-trip; the daemon
        // catches empty/binary content with a more informative message.
        (None, None, Some(src)) => {
            let max = memex_cli::daemon::config::INGEST_MAX_BYTES;
            let mut buf = Vec::with_capacity(64 * 1024);
            std::io::stdin()
                .take((max as u64) + 1)
                .read_to_end(&mut buf)?;
            if buf.len() > max {
                anyhow::bail!(
                    "source content > {}MB; raise [ingest].fetch_max_bytes if intentional",
                    max / (1024 * 1024)
                );
            }
            let content = String::from_utf8(buf)
                .map_err(|e| anyhow::anyhow!("source content is not valid UTF-8: {e}"))?;
            memex_cli::daemon::protocol::Request::Ingest {
                source: memex_cli::daemon::protocol::IngestSource::Document {
                    source_path: src.to_string(),
                    content,
                },
                collections,
                memex_root: root_str,
            }
        }
        _ => anyhow::bail!(
            "ingest requires one of:\n  \
             --agent <agent> <transcript-path>      (transcript mode)\n  \
             <file-path>                            (document file mode)\n  \
             --source <id>                          (stdin mode; pipe content via stdin)"
        ),
    };

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let paths =
            memex_cli::daemon::server::DaemonPaths::default_under(&memex_cli::memex_root());
        let stream = memex_cli::daemon::client::connect_or_spawn(
            &paths.socket,
            &paths.lock,
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .await?;
        let events = memex_cli::daemon::client::request(stream, &request).await?;
        let mut exit_code = 0;
        for ev in &events {
            match ev {
                memex_cli::daemon::protocol::Event::Error {
                    code,
                    message,
                    status,
                } => {
                    eprintln!("ingest error ({code}): {message}");
                    exit_code = *status;
                }
                memex_cli::daemon::protocol::Event::Stored { wiki_pages, .. } => {
                    println!("stored: {} pages", wiki_pages.len());
                }
                memex_cli::daemon::protocol::Event::Done { status } if *status != 0 => {
                    exit_code = *status;
                }
                _ => {}
            }
        }
        if exit_code != 0 {
            std::process::exit(exit_code);
        }
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

/// Discover session files for an agent (or a user-supplied directory) and
/// dispatch ingestion to the daemon. All sessions are fired concurrently;
/// the daemon's worker pool (`daemon.worker.max_count`) caps actual
/// parallelism, with excess jobs queued internally.
fn run_backfill(
    agent: &Agent,
    collections: &[String],
    path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let session_files = discover_session_files(agent, path)?;
    if session_files.is_empty() {
        println!("No session files found");
        return Ok(());
    }
    println!(
        "Discovered {} sessions; dispatching to daemon",
        session_files.len()
    );

    let root = memex_cli::memex_root();
    let root_str = root.to_string_lossy().to_string();
    let agent_str = agent.as_str().to_string();
    let collections = default_ingest_collections(collections);
    let total = session_files.len();

    let rt = tokio::runtime::Runtime::new()?;
    let (queued, skipped, errors) = rt.block_on(async move {
        // Pre-warm the daemon with a single spawn-or-connect before firing
        // N concurrent ingest tasks. Without this, the tasks race into
        // connect_or_spawn, each sees a cold socket, and several try to
        // spawn the daemon simultaneously — producing brief zombie children
        // under this CLI process.
        if let Err(e) = memex_cli::daemon::warm_up().await {
            eprintln!("warning: daemon pre-warm failed: {e}");
        }

        let mut handles = Vec::with_capacity(total);
        for p in session_files {
            let agent_str = agent_str.clone();
            let root_str = root_str.clone();
            let collections = collections.clone();
            let path_str = p.to_string_lossy().to_string();
            handles.push(tokio::spawn(async move {
                let result =
                    memex_cli::daemon::ingest_async(&path_str, &agent_str, &root_str, collections)
                        .await;
                (path_str, result)
            }));
        }

        let mut queued = 0u32;
        let mut skipped = 0u32;
        let mut errors = 0u32;
        for h in handles {
            match h.await {
                Ok((path_str, Ok(0))) => {
                    queued += 1;
                    eprintln!("queued: {path_str}");
                }
                Ok((_, Ok(_))) => skipped += 1,
                Ok((path_str, Err(e))) => {
                    eprintln!("error: {path_str}: {e}");
                    errors += 1;
                }
                Err(e) => {
                    eprintln!("task join error: {e}");
                    errors += 1;
                }
            }
        }
        (queued, skipped, errors)
    });

    println!("Queued {queued} (skipped {skipped}, errors {errors}) of {total} sessions");
    Ok(())
}

/// Show daemon status and recent ingestion activity.
fn run_status() -> anyhow::Result<()> {
    let code = memex_cli::daemon::status()?;
    if code != 0 {
        println!("Daemon: not running");
    }

    // Show wiki stats
    let root = memex_cli::memex_root();
    if let Ok(memex) = Memex::open(root) {
        let search = memex.search();
        let wiki_count = search.wiki_page_count().unwrap_or(0);
        let source_count = search.source_count().unwrap_or(0);
        println!("Wiki: {wiki_count} pages, {source_count} sources");
    }

    std::process::exit(code);
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
        Commands::Search { title } => {
            init_ort_runtime().map_err(|e| anyhow::anyhow!(e))?;
            run_search(&title)
        }
        Commands::Read { refs } => run_read(&refs),
        Commands::Write {
            name,
            force,
            quiet,
            source,
            direct,
        } => {
            if direct {
                init_ort_runtime().map_err(|e| anyhow::anyhow!(e))?;
                run_write(&name, force, quiet, source.as_deref())
            } else {
                run_write_via_daemon(&name, force, quiet, source.as_deref())
            }
        }
        Commands::Delete { page_ref, force } => run_delete(&page_ref, force),
        Commands::Lint { fix } => {
            if fix {
                init_ort_runtime().map_err(|e| anyhow::anyhow!(e))?;
            }
            run_lint(fix)
        }
        Commands::Daemon { action } => match action {
            DaemonAction::Start { foreground } => {
                init_ort_runtime().map_err(|e| anyhow::anyhow!(e))?;
                std::process::exit(memex_cli::daemon::start_background(foreground)?)
            }
            DaemonAction::Stop => std::process::exit(memex_cli::daemon::stop()?),
            DaemonAction::Status => std::process::exit(memex_cli::daemon::status()?),
        },
        Commands::Query {
            question,
            raw,
            top_k,
            collections,
        } => {
            let code = if raw {
                memex_cli::daemon::query_raw(&question, top_k, collections, None)?
            } else {
                memex_cli::daemon::query_synth(&question, top_k, collections, None)?
            };
            std::process::exit(code);
        }
        Commands::Ingest {
            agent,
            path,
            source,
            collections,
        } => run_ingest_client(
            agent.as_ref(),
            path.as_deref(),
            source.as_deref(),
            &collections,
        ),
        Commands::Backfill {
            agent,
            collections,
            path,
        } => run_backfill(&agent, &collections, path.as_deref()),
        Commands::Status => run_status(),
        Commands::Source { action } => match action {
            SourceAction::Add {
                path,
                collections,
                verbose,
            } => run_source_add(&path, &collections, verbose),
            SourceAction::List { collections, json } => run_source_list(&collections, json),
            SourceAction::Show { reference } => run_source_show(&reference),
            SourceAction::Delete { reference, force } => run_source_delete(&reference, force),
        },
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
        MemexError::EmbeddingUnavailable { path, .. } => format!(
            "Install the embedding model + ONNX Runtime:\n\
             \n  cd plugin && npm install      # downloads {} and libonnxruntime\n  \
             # or, manually place the 768-dim ONNX model at {} and libonnxruntime in ~/.memex/lib/",
            path.display(),
            path.display(),
        ),
        _ => return None,
    })
}
