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
        /// Start reading from this 1-indexed line number
        #[arg(long)]
        from_line: Option<usize>,
        /// Maximum number of lines to read (count, not end-line)
        #[arg(long)]
        max_lines: Option<usize>,
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
        /// Repair filesystem-vs-index drift (stale-index, untracked,
        /// missing-file, raw hash-mismatch) and embedding drift. Link
        /// issues are report-only.
        #[arg(long)]
        fix: bool,
    },
    /// Plan-pipeline subcommands (see also: `source plan` and `plan apply`).
    Plan {
        #[command(subcommand)]
        action: PlanAction,
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
        /// How many pages to retrieve (default 10)
        #[arg(long, default_value = "10")]
        top_k: usize,
        /// Restrict search to one or more collections
        #[arg(long = "collection")]
        collections: Vec<String>,
        /// Optional "what the user is really after" hint. Forces the full
        /// expansion+rerank pipeline and biases the focused-snippet line
        /// picker toward intent-bearing lines.
        #[arg(long)]
        intent: Option<String>,
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
        /// docid prefix (from `memex source list`)
        reference: String,
    },
    /// Delete a source document
    Delete {
        /// docid prefix (from `memex source list`)
        reference: String,
        /// Skip confirmation; required when wiki pages reference this source
        #[arg(long)]
        force: bool,
    },
    /// Run EXTRACT + MERGE-dry-run for a stored source. Streams plan JSON.
    Plan {
        /// docid prefix (from `memex source list`)
        docid: String,
    },
}

#[derive(Subcommand)]
enum PlanAction {
    /// Render plan JSON (stdin) as a human-readable table + diffs (stdout).
    Show {
        /// Pass plan JSON through unchanged for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Apply a plan: write each non-dropped proposal as a wiki page.
    /// Reads plan JSON from stdin; emits refreshed plan or summary.
    Apply,
}

/// CLI title→slug lookup. Routes through the daemon (auto-spawning
/// it on first use) so the warm embedding model is reused — every
/// other daemon-mediated CLI command (`query`, `ingest`, `write`,
/// `delete`, `source add/delete`, `backfill`) does the same. The
/// daemon-side handler short-circuits BM25-strong matches, so the
/// typical case (user typed the existing title) is sub-100ms once
/// the daemon is warm.
/// Send one request to the running daemon (auto-spawning it if needed)
/// and return the full event stream. Standardizes the
/// rt + connect_or_spawn + request boilerplate that every daemon-mediated
/// CLI subcommand otherwise duplicates.
fn send_to_daemon(
    request: memex_cli::daemon::protocol::Request,
    spawn_timeout_secs: u64,
) -> anyhow::Result<Vec<memex_cli::daemon::protocol::Event>> {
    let root = memex_cli::memex_root();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let paths = memex_cli::daemon::server::DaemonPaths::default_under(&root);
        let stream = memex_cli::daemon::client::connect_or_spawn(
            &paths.socket,
            &paths.lock,
            tokio::time::Instant::now() + std::time::Duration::from_secs(spawn_timeout_secs),
        )
        .await?;
        memex_cli::daemon::client::request(stream, &request).await
    })
}

fn run_search(title: &str) -> anyhow::Result<()> {
    let events = send_to_daemon(
        memex_cli::daemon::protocol::Request::Search {
            title: title.to_string(),
        },
        15,
    )?;
    for ev in &events {
        match ev {
            memex_cli::daemon::protocol::Event::SearchResult { slug: Some(s) } => {
                println!("{s}");
            }
            memex_cli::daemon::protocol::Event::SearchResult { slug: None } => {
                // No match — print nothing, exit 0. Matches the prior
                // direct-path behavior the agent skill docs document.
            }
            memex_cli::daemon::protocol::Event::Error { code, message, .. } => {
                anyhow::bail!("search error ({code}): {message}");
            }
            _ => {}
        }
    }
    Ok(())
}

fn run_read(
    refs: &[String],
    from_line: Option<usize>,
    max_lines: Option<usize>,
) -> anyhow::Result<()> {
    // Hard error: slicing flags only make sense with a single ref.
    if (from_line.is_some() || max_lines.is_some()) && refs.len() != 1 {
        eprintln!(
            "error: --from-line and --max-lines require a single ref; got {} refs",
            refs.len()
        );
        std::process::exit(2);
    }

    let root = memex_cli::memex_root();
    let memex = Memex::open(root.clone())?;
    let search = memex.search();

    let wiki_dir = root.join("wiki");
    let canonical_wiki = wiki_dir.canonicalize().unwrap_or(wiki_dir.clone());

    let mut found_count = 0usize;
    for reference in refs {
        let docs = search.resolve_ref_documents(reference)?;
        if docs.is_empty() {
            eprintln!("not found: {reference}");
            continue;
        }
        found_count += 1;
        for doc in &docs {
            // Derive stem: basename without extension.
            let doc_path = Path::new(&doc.path);
            let stem = doc_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();

            // Read content: wiki pages from disk, source documents from disk.
            //
            // When slicing flags are set, wiki pages are read via
            // `read_body_from_disk` (frontmatter stripped) so line numbers
            // are consistent with `@@ -N,M @@` headers from `memex query`.
            // Without slicing flags, wiki pages are printed verbatim
            // (frontmatter included) — preserving existing behaviour.
            let body = if doc.doc_type == "wiki" {
                let full_path = root.join(&doc.path);
                let canonical = full_path.canonicalize().unwrap_or(full_path.clone());
                if !canonical.starts_with(&canonical_wiki) {
                    eprintln!("Error: path traversal rejected: {}", doc.path);
                    continue;
                }
                if from_line.is_some() || max_lines.is_some() {
                    // Frontmatter-stripped body so line N matches query output.
                    let raw = memex_core::read_body_from_disk(&root, &doc.doc_type, &doc.path)
                        .unwrap_or_else(|e| format!("(error reading content: {e})"));
                    // For files with 3+ blank lines between the closing
                    // fence and the body, split_frontmatter only consumes
                    // two; drop one more so that line 1 is the first
                    // content line in the user's output.
                    raw.strip_prefix('\n').map(str::to_string).unwrap_or(raw)
                } else {
                    std::fs::read_to_string(&full_path)
                        .unwrap_or_else(|e| format!("(error reading file: {e})"))
                }
            } else {
                // Source documents: read from disk via content-addressable store.
                memex_core::read_body_from_disk(&root, &doc.doc_type, &doc.path)
                    .unwrap_or_else(|e| format!("(error reading content: {e})"))
            };

            let docid = memex_core::docid::short(&doc.hash).to_string();
            let total_lines = body.lines().count();
            let (sliced, (start, end)) =
                memex_cli::slice_body(&body, from_line, max_lines);
            let header_suffix = if from_line.is_some() || max_lines.is_some() {
                format!(" [lines {}..{} of {}]", start, end, total_lines)
            } else {
                String::new()
            };
            println!("=== {} {} {}{} ===", docid, doc.doc_type, stem, header_suffix);
            print!("{sliced}");
            if !sliced.ends_with('\n') {
                println!();
            }
        }
    }
    // Non-zero exit if no ref resolved — important for scripting.
    // Mixed (some found, some not) still exits 0; the eprintln above
    // surfaced the missing ones individually.
    if found_count == 0 {
        std::process::exit(1);
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

/// `memex source delete <ref>`. Mutation — routed through the daemon
/// (`Request::SourceDelete`). Reports any wiki pages that now have
/// dangling source references.
fn run_source_delete(reference: &str, force: bool) -> anyhow::Result<()> {
    let events = send_to_daemon(
        memex_cli::daemon::protocol::Request::SourceDelete {
            ref_: reference.to_string(),
            force,
        },
        5,
    )?;
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
    Ok(())
}

/// `memex source show <ref>`. Read-only; resolves a docid prefix
/// (4+ chars; from `memex source list`) and prints the source body
/// to stdout.
fn run_source_show(reference: &str) -> anyhow::Result<()> {
    let root = memex_cli::memex_root();
    let memex = memex_core::Memex::open(root)?;
    let search = memex.search();

    let doc = {
        let docs = search.resolve_ref_documents(reference)?;
        docs.into_iter().find(|d| d.doc_type == "raw")
    };
    let doc = match doc {
        Some(d) => d,
        None => {
            anyhow::bail!(
                "source not found: '{reference}'. Use a docid prefix."
            );
        }
    };
    let body = memex_core::read_body_from_disk(memex.root(), &doc.doc_type, &doc.path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
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

    println!("{:<12} {:<10} {:<24} PATH", "HASH", "SIZE", "MTIME");
    for r in &rows {
        println!(
            "{:<12} {:<10} {:<24} {}",
            truncate_for_table(memex_core::docid::short(&r.hash), 12),
            human_size(r.size_bytes),
            &r.mtime,
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
    // Pass collections through verbatim. The daemon's `normalize_collections`
    // adds "default" for new sources; for re-adds we want the empty list
    // to signal "leave existing collections alone" (handle_source_add gates
    // its set_document_collections_by_path call on `!collections.is_empty()`).
    let events = send_to_daemon(
        memex_cli::daemon::protocol::Request::SourceAdd {
            source_path: source_path.to_string(),
            content,
            collections: collections.to_vec(),
        },
        5,
    )?;
    let mut docid = None;
    for ev in &events {
        if let memex_cli::daemon::protocol::Event::SourceAdded { docid: d } = ev {
            docid = Some(d.clone());
        }
        if let memex_cli::daemon::protocol::Event::Error { code, message, .. } = ev {
            anyhow::bail!("source add error ({code}): {message}");
        }
    }
    let docid = docid.ok_or_else(|| anyhow::anyhow!("daemon did not return SourceAdded event"))?;
    if verbose {
        eprintln!("Stored: {source_path} ({content_size} bytes) → {docid}");
        eprintln!("Use: memex write <slug> --source {docid}");
    }
    println!("{docid}");
    Ok(())
}

fn run_source_plan(docid: &str) -> anyhow::Result<()> {
    let events = send_to_daemon(
        memex_cli::daemon::protocol::Request::SourcePlan {
            source_id: docid.to_string(),
        },
        5,
    )?;
    for ev in &events {
        match ev {
            memex_cli::daemon::protocol::Event::PlanContent { json } => {
                println!("{json}");
                return Ok(());
            }
            memex_cli::daemon::protocol::Event::EmptyExtract { .. } => {
                // No extractable subjects → empty stdout, exit 0;
                // skill surfaces a user-facing message itself.
                return Ok(());
            }
            memex_cli::daemon::protocol::Event::Error { message, .. } => {
                anyhow::bail!("{message}");
            }
            _ => {}
        }
    }
    anyhow::bail!("daemon did not return PlanContent or EmptyExtract")
}

fn run_plan_show(json_only: bool) -> anyhow::Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        anyhow::bail!("empty stdin: pipe a plan JSON file");
    }
    let plan: memex_cli::daemon::plan::Plan = serde_json::from_str(&buf)
        .map_err(|e| anyhow::anyhow!("plan JSON parse: {e}"))?;
    if json_only {
        // Pass-through: re-emit (validates parseability).
        println!("{}", serde_json::to_string(&plan)?);
        return Ok(());
    }
    print!("{}", memex_cli::plan_show::format_plan(&plan));
    Ok(())
}

fn run_plan_apply() -> anyhow::Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        anyhow::bail!("empty stdin: pipe a plan JSON");
    }
    let events = send_to_daemon(
        memex_cli::daemon::protocol::Request::PlanApply { plan_json: buf },
        5,
    )?;
    // Find the terminal event and the Done status.
    let mut content: Option<String> = None;
    let mut applied: Option<Vec<String>> = None;
    let mut error: Option<String> = None;
    let mut status: i32 = 1;
    for ev in events {
        match ev {
            memex_cli::daemon::protocol::Event::PlanContent { json } => content = Some(json),
            memex_cli::daemon::protocol::Event::PlanApplied { committed } => applied = Some(committed),
            memex_cli::daemon::protocol::Event::Error { message, .. } => error = Some(message),
            memex_cli::daemon::protocol::Event::Done { status: s } => status = s,
            _ => {}
        }
    }
    match status {
        0 => {
            let n = applied.map(|v| v.len()).unwrap_or(0);
            println!("committed {n} wiki pages");
            Ok(())
        }
        3 | 4 => {
            if let Some(json) = content {
                println!("{json}");
            }
            std::process::exit(status);
        }
        _ => {
            if let Some(msg) = error {
                anyhow::bail!("{msg}");
            }
            anyhow::bail!("plan apply failed with status {status}");
        }
    }
}

/// Write a wiki page via the daemon. Sends Request::Write.
fn run_write(
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

    // Generic-YAML walk; parse_page_for_indexing requires created_at/updated_at.
    let tags = memex_core::storage::split_frontmatter(&content)
        .and_then(|(yaml, _)| serde_yaml::from_str::<serde_yaml::Value>(yaml).ok())
        .and_then(|v| v.get("tags").cloned())
        .and_then(|v| v.as_sequence().cloned())
        .map(|seq| {
            seq.into_iter()
                .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let events = send_to_daemon(
        memex_cli::daemon::protocol::Request::Write {
            title: name.to_string(),
            content,
            tags,
            source: source.map(str::to_string),
            force,
        },
        5,
    )?;
    let mut errored = false;
    for ev in &events {
        match ev {
            memex_cli::daemon::protocol::Event::Written {
                slug,
                docid,
                linked,
                backlinked,
                suggest_create,
            } => {
                println!("written: {slug} ({docid})");
                if !quiet {
                    if !linked.is_empty() {
                        println!("linked: {}", linked.join(", "));
                    }
                    if !backlinked.is_empty() {
                        println!("backlinked: {}", backlinked.join(", "));
                    }
                    if !suggest_create.is_empty() {
                        println!("suggest-create: {}", suggest_create.join(", "));
                    }
                }
            }
            memex_cli::daemon::protocol::Event::Error { code, message, .. } => {
                eprintln!("write error ({code}): {message}");
                errored = true;
            }
            _ => {}
        }
    }
    if errored {
        anyhow::bail!("daemon write failed");
    }

    Ok(())
}


fn run_delete(page_ref: &str, force: bool) -> anyhow::Result<()> {
    let root = memex_cli::memex_root();

    // Phase 1: resolve for TTY confirmation (read-only, no lock).
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
    let confirmed_title = doc_pre.title.clone();
    let slug = std::path::Path::new(&doc_pre.path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(page_ref)
        .to_string();

    if !force {
        let is_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
        if is_tty {
            eprint!("Delete {slug} \"{confirmed_title}\"? [y/N] ");
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).ok();
            if !answer.trim().eq_ignore_ascii_case("y") {
                eprintln!("Aborted.");
                return Ok(());
            }
        }
    }
    drop(reader);

    // Phase 2: delegate to daemon (holds the writer lock, runs FTS backlink scan).
    let events = send_to_daemon(
        memex_cli::daemon::protocol::Request::Delete {
            slug: slug.clone(),
            force,
        },
        5,
    )?;
    let mut exit_code = 0;
    for ev in &events {
        match ev {
            memex_cli::daemon::protocol::Event::Deleted { slug: deleted_slug } => {
                println!("deleted: {deleted_slug}");
            }
            memex_cli::daemon::protocol::Event::Error { code, message, status } => {
                eprintln!("delete error ({code}): {message}");
                exit_code = *status;
            }
            _ => {}
        }
    }
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn run_lint(fix: bool) -> anyhow::Result<()> {
    if fix {
        return run_lint_fix();
    }

    let root = memex_cli::memex_root();
    let memex = memex_core::Memex::open(root)?;
    let report = memex.lint()?;

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
            memex_core::types::LintIssueKind::RawHashMismatch => {
                println!(
                    "raw-mismatch: {} (body changed; new hash {})",
                    issue.page, issue.target
                )
            }
        }
    }
    Ok(())
}

/// `memex lint --fix` — daemon-routed mutation. The daemon owns all
/// writes, including stale-index reindex and embedding-model re-embed.
fn run_lint_fix() -> anyhow::Result<()> {
    let events = send_to_daemon(memex_cli::daemon::protocol::Request::LintFix {}, 15)?;
    let mut applied = 0u32;
    let mut stale = 0u32;
    let mut errored = 0u32;
    for ev in &events {
        use memex_cli::daemon::protocol::Event;
        match ev {
            Event::LintFixed { page, kind } => {
                match kind.as_str() {
                    "stale_index" => println!("fixed: {page} (reindexed from disk)"),
                    "outdated_embedding" => println!("re-embedded: {page}"),
                    "raw_hash_mismatch" => println!("renamed-and-re-embedded: {page}"),
                    "untracked_file" => println!("indexed: {page} (added DB row from disk)"),
                    "missing_file" => println!("removed: {page} (DB row for missing file)"),
                    other => println!("fixed: {page} ({other})"),
                }
                applied += 1;
            }
            Event::LintAlreadyFixed { page } => {
                println!("already-fixed: {page}");
                stale += 1;
            }
            Event::LintRemaining { page, kind, target } => match kind.as_str() {
                "dangling_link" => println!("dangling: {page} -> [[{target}]]"),
                "missing_link" => println!("missing-link: {page} -> [[{target}]]"),
                "untracked_file" => println!("untracked: {target} (no DB row)"),
                "missing_file" => println!("missing-file: {page} (DB row, no file)"),
                other => println!("{other}: {page}"),
            },
            Event::Error { code, message, .. } => {
                eprintln!("lint --fix error ({code}): {message}");
                errored += 1;
            }
            _ => {}
        }
    }
    if applied + stale > 0 {
        println!("lint-fix-summary: applied={applied} stale={stale}");
    }
    if errored > 0 {
        anyhow::bail!("lint --fix encountered {errored} error(s)");
    }
    Ok(())
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
    // Default discovery uses the agent's well-known directory layout. When
    // the user supplies `--path`, the help text promises "ingest every
    // *.jsonl under this directory (recursive)" — so walk the override
    // dir for the agent's file extension instead of replaying the layout.
    let sub = match (agent, root_override.is_some()) {
        (Agent::ClaudeCode, false) => "projects/*/*.jsonl",
        (Agent::Codex, false) => "sessions/*/*/*/*.jsonl",
        (Agent::GeminiCli, false) => "tmp/*/chats/session-*.json",
        (Agent::ClaudeCode | Agent::Codex, true) => "**/*.jsonl",
        (Agent::GeminiCli, true) => "**/session-*.json",
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

fn run_ingest(
    agent: Option<&Agent>,
    path: Option<&std::path::Path>,
    source: Option<&str>,
    collections: &[String],
) -> anyhow::Result<()> {
    if std::env::var("MEMEX_INTERNAL").as_deref() == Ok("1") {
        return Ok(());
    }
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
            }
        }
        _ => anyhow::bail!(
            "ingest requires one of:\n  \
             --agent <agent> <transcript-path>      (transcript mode)\n  \
             <file-path>                            (document file mode)\n  \
             --source <id>                          (stdin mode; pipe content via stdin)"
        ),
    };

    let events = send_to_daemon(request, 5)?;
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
            let collections = collections.clone();
            let path_str = p.to_string_lossy().to_string();
            handles.push(tokio::spawn(async move {
                let result =
                    memex_cli::daemon::ingest_async(&path_str, &agent_str, collections).await;
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

        // Ingest activity: pending/processing jobs + recent terminal
        // counts + last activity timestamp. The pending column is the
        // user-actionable signal ("am I waiting on something?"); the
        // terminal stats give a 30-day audit summary (rows older than
        // that are pruned at daemon startup).
        let activity = search.with_connection(|conn| {
            let pending: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM ingest_jobs WHERE status IN ('pending', 'processing')",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let completed: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM ingest_jobs WHERE status = 'completed'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let failed: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM ingest_jobs WHERE status = 'failed'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let last_activity: Option<String> = conn
                .query_row(
                    "SELECT MAX(updated_at) FROM ingest_jobs WHERE status IN ('completed', 'failed')",
                    [],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            Ok((pending, completed, failed, last_activity))
        });
        if let Ok((pending, completed, failed, last_activity)) = activity
            && (pending + completed + failed) > 0
        {
            println!("Ingest: {pending} pending, {completed} completed, {failed} failed (last 30 days)");
            if let Some(ts) = last_activity {
                println!("  last activity: {ts}");
            }
        }
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
            memex_cli::init_ort_runtime().map_err(|e| anyhow::anyhow!(e))?;
            run_search(&title)
        }
        Commands::Read {
            refs,
            from_line,
            max_lines,
        } => run_read(&refs, from_line, max_lines),
        Commands::Write {
            name,
            force,
            quiet,
            source,
        } => run_write(&name, force, quiet, source.as_deref()),
        Commands::Delete { page_ref, force } => run_delete(&page_ref, force),
        Commands::Lint { fix } => {
            if fix {
                memex_cli::init_ort_runtime().map_err(|e| anyhow::anyhow!(e))?;
            }
            run_lint(fix)
        }
        Commands::Daemon { action } => match action {
            DaemonAction::Start { foreground } => {
                memex_cli::init_ort_runtime().map_err(|e| anyhow::anyhow!(e))?;
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
            intent,
        } => {
            let code = if raw {
                memex_cli::daemon::query_raw(&question, top_k, collections, intent, None)?
            } else {
                memex_cli::daemon::query_synth(&question, top_k, collections, intent, None)?
            };
            std::process::exit(code);
        }
        Commands::Ingest {
            agent,
            path,
            source,
            collections,
        } => run_ingest(
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
            SourceAction::Plan { docid } => run_source_plan(&docid),
        },
        Commands::Plan { action } => match action {
            PlanAction::Show { json } => run_plan_show(json),
            PlanAction::Apply => run_plan_apply(),
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
