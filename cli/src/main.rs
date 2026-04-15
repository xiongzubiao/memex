use memex_cli::{
    auth, auto_detect,
    config_file::{self, OperationKind},
};

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use zeroclaw::agent::Agent;

/// Run an interactive REPL loop with an agent, returning the last response.
async fn interactive_loop(agent: &mut Agent, prompt_label: &str) -> Option<String> {
    let mut last_response = None;
    loop {
        eprint!("\n{prompt_label}");
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err() {
            break;
        }
        let input = input.trim();
        if input.is_empty()
            || input.eq_ignore_ascii_case("quit")
            || input.eq_ignore_ascii_case("exit")
            || input.eq_ignore_ascii_case("done")
        {
            break;
        }
        match agent.turn(input).await {
            Ok(reply) => {
                println!("{reply}");
                last_response = Some(reply);
            }
            Err(e) => {
                eprintln!("Agent error: {e}");
                break;
            }
        }
    }
    last_response
}

fn memex_root() -> PathBuf {
    if let Ok(root) = std::env::var("MEMEX_ROOT") {
        return PathBuf::from(root);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".memex")
}

fn runtime_options(root: &Path) -> zeroclaw::providers::ProviderRuntimeOptions {
    zeroclaw::providers::ProviderRuntimeOptions {
        zeroclaw_dir: Some(root.join(".zeroclaw")),
        secrets_encrypt: true,
        ..Default::default()
    }
}

fn create_provider(
    root: &Path,
    model_ref: &config_file::ModelRef,
) -> anyhow::Result<Box<dyn zeroclaw::providers::Provider>> {
    if model_ref.provider == "dry-run" {
        Ok(Box::new(memex_agent::dry_run::DryRunProvider))
    } else {
        zeroclaw::providers::create_provider_with_options(
            &model_ref.provider,
            None,
            &runtime_options(root),
        )
    }
}

fn create_llm_provider(
    root: &Path,
    model_ref: &config_file::ModelRef,
) -> anyhow::Result<Box<dyn memex_core::LlmProvider>> {
    Ok(Box::new(memex_agent::tools::ProviderLlmAdapter(
        create_provider(root, model_ref)?,
    )))
}

fn provider_defaults(provider: auth::AuthProvider) -> config_file::ProviderDefaults {
    match provider {
        auth::AuthProvider::Codex => config_file::ProviderDefaults::codex(),
        auth::AuthProvider::Gemini => config_file::ProviderDefaults::gemini(),
    }
}

#[derive(Parser)]
#[command(
    name = "memex",
    about = "Personal knowledge base with multi-LLM brainstorming"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new memex
    Init,
    /// Manage OAuth credentials for codex and gemini
    Auth {
        #[command(subcommand)]
        sub: AuthSub,
    },
    /// Start a multi-LLM brainstorming session
    Brainstorm {
        #[command(subcommand)]
        sub: BrainstormSub,
    },
    /// Ingest sources into memex (files, URLs, directories, ZIP exports)
    Ingest {
        /// Paths or URLs to ingest (auto-detects format: md, code, json, jsonl, zip, etc.)
        sources: Vec<String>,
        /// Show what would be ingested without writing anything
        #[arg(long)]
        preview: bool,
    },
    /// Query memex with a natural language question
    Query {
        /// The question to ask (omit for interactive agent mode)
        question: Option<String>,
    },
    /// Run wiki health checks (dangling links, orphans, contradictions)
    Lint {
        /// Automatically apply fixable issues (create stub pages for missing links)
        #[arg(long)]
        fix: bool,
    },
    /// Wiki management (reindex, show index, view log, stats)
    Wiki {
        #[command(subcommand)]
        sub: WikiSub,
    },
    /// Check that all configured LLM providers are reachable
    Doctor,
    /// View or update memex configuration
    Config {
        #[command(subcommand)]
        sub: ConfigSub,
    },
}

#[derive(Subcommand)]
enum BrainstormSub {
    /// Start a new brainstorm session (omit task for interactive mode)
    New {
        /// What to brainstorm (e.g. "Design a rate-limiting API")
        task: Option<String>,
        /// Task type preset: software, general, research, article, book, strategy
        #[arg(long = "type", value_name = "PRESET",
              value_parser = clap::builder::PossibleValuesParser::new(memex_agent::preset::available_presets()))]
        task_type: Option<String>,
    },
    /// Resume an interrupted brainstorm session
    Resume {
        /// Session ID (omit to resume latest)
        session_id: Option<String>,
    },
    /// List all brainstorm sessions
    List,
    /// Show details of a brainstorm session
    Show { session_id: String },
    /// Export a brainstorm session to file
    Export {
        session_id: String,
        /// Output file path (default: stdout)
        #[arg(long, short = 'o')]
        output: Option<String>,
    },
}

#[derive(Subcommand)]
enum WikiSub {
    /// Rebuild index.md from all wiki pages
    Reindex,
    /// Display the wiki index
    Show,
    /// Show recent operations log
    Log,
    /// Show wiki page count and source count
    Stats,
    /// BM25 full-text search (no LLM, instant)
    Search {
        /// The search query
        query: String,
        /// Number of results to return
        #[arg(long, short = 'k', default_value = "10")]
        top_k: usize,
    },
}

#[derive(Subcommand)]
enum ConfigSub {
    /// Display current configuration
    Show,
    /// Set a configuration value (e.g. memex config set provider.model claude-opus-4-6)
    Set { key: String, value: String },
}

#[derive(Subcommand)]
enum AuthSub {
    /// Login with OAuth (imports existing provider CLI cache when available)
    Login {
        /// Provider to login: codex or gemini
        #[arg(long, value_parser = ["codex", "gemini"])]
        provider: String,
        /// Use OAuth device-code flow
        #[arg(long)]
        device_code: bool,
        /// Also set this provider's global default model in config.toml
        #[arg(long)]
        make_default: bool,
    },
    /// Show memex-managed auth status
    Status,
    /// Remove provider auth state from memex
    Logout {
        /// Provider to logout: codex or gemini
        #[arg(long, value_parser = ["codex", "gemini"])]
        provider: String,
    },
}

fn setup_tracing(root: &std::path::Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    if root.exists() {
        let file_appender = tracing_appender::rolling::never(root, "memex.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        tracing_subscriber::fmt()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_target(false)
            .init();

        Some(guard)
    } else {
        // Before `memex init`: no logging (tracing events are silently dropped)
        None
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = memex_root();
    let _tracing_guard = setup_tracing(&root);
    let cli = Cli::parse();
    match cli.command {
        Commands::Init => {
            let root = memex_root();
            eprintln!("Initializing memex at {}", root.display());

            let providers = auto_detect::detect_providers();
            let provider_name = if providers.is_empty() {
                eprintln!("No LLM providers detected. Defaulting to dry-run.");
                eprintln!(
                    "Set ANTHROPIC_API_KEY, OPENAI_API_KEY, or GEMINI_API_KEY to enable AI features."
                );
                "dry-run".to_string()
            } else {
                eprintln!(
                    "Detected providers: {}",
                    providers
                        .iter()
                        .map(|p| p.provider.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                providers[0].frontier_model.provider.clone()
            };

            let model_name = if provider_name == "dry-run" {
                "dry-run".to_string()
            } else {
                providers[0].midtier_model.model.clone()
            };

            let init_model_ref = if provider_name == "dry-run" {
                config_file::ModelRef {
                    provider: "dry-run".to_string(),
                    model: "default".to_string(),
                }
            } else {
                config_file::ModelRef {
                    provider: providers[0].midtier_model.provider.clone(),
                    model: providers[0].midtier_model.model.clone(),
                }
            };
            memex_core::Memex::open(
                root.clone(),
                create_llm_provider(&root, &init_model_ref)?,
                &model_name,
            )
            .map_err(|e| anyhow::anyhow!("{}", e))?;

            memex_agent::identity::scaffold_identity_files(&root)?;

            // Write config.toml matching spec's Configuration section
            let config_content = config_file::build_config_toml(&providers);
            std::fs::write(root.join("config.toml"), config_content)?;

            eprintln!("Memex initialized at {}", root.display());
            eprintln!("  Wiki:    {}/wiki/", root.display());
            eprintln!("  Sources: {}/sources/", root.display());
            eprintln!("  Config:  {}/config.toml", root.display());
        }

        Commands::Auth { sub } => {
            let root = memex_root();
            match sub {
                AuthSub::Login {
                    provider,
                    device_code,
                    make_default,
                } => {
                    let provider = auth::normalize_provider(&provider)?;
                    let result = auth::login(&root, provider, device_code).await?;
                    config_file::apply_provider_defaults_and_persist(
                        &root,
                        &provider_defaults(provider),
                        make_default,
                    )?;

                    match result.source {
                        auth::LoginSource::Imported(path) => {
                            println!(
                                "Imported {} credentials from {}",
                                result.provider.cli_name(),
                                path.display()
                            );
                        }
                        auth::LoginSource::DeviceCode => {
                            println!("OAuth login completed for {}", result.provider.cli_name());
                        }
                    }

                    if let Some(account_id) = result.account_id {
                        println!("Account: {account_id}");
                    }
                    println!("Auth state saved at {}", root.join("auth.json").display());
                    println!("Config updated at {}", root.join("config.toml").display());
                }
                AuthSub::Status => {
                    let entries = auth::status(&root).await?;
                    if entries.is_empty() {
                        println!("No memex auth credentials configured.");
                    } else {
                        println!("Memex auth profiles:");
                        for entry in entries {
                            let expires = match entry.expires_at {
                                Some(ts) if ts <= chrono::Utc::now() => {
                                    format!("expired ({})", ts.to_rfc3339())
                                }
                                Some(ts) => {
                                    let mins = (ts - chrono::Utc::now()).num_minutes();
                                    format!("expires in {mins}m ({})", ts.to_rfc3339())
                                }
                                None => "expires: n/a".to_string(),
                            };
                            let account = entry.account_id.unwrap_or_else(|| "unknown".to_string());
                            println!(
                                "  {}  account={}  {}",
                                entry.provider.canonical(),
                                account,
                                expires
                            );
                        }
                    }
                }
                AuthSub::Logout { provider } => {
                    let provider = auth::normalize_provider(&provider)?;
                    let removed = auth::logout(&root, provider).await?;
                    if removed {
                        println!("Removed auth for {}", provider.cli_name());
                    } else {
                        println!("No auth found for {}", provider.cli_name());
                    }
                }
            }
        }

        Commands::Ingest { sources, preview } => {
            use memex_core::types::Source;

            let root = memex_root();

            if sources.is_empty() {
                eprintln!("No sources provided. Usage: memex ingest <path|url> [...]");
                std::process::exit(1);
            }

            if preview {
                for source_str in &sources {
                    println!("Would ingest: {source_str}");
                }
            } else if sources.len() == 1
                && atty::is(atty::Stream::Stdin)
                && !sources[0].ends_with(".zip")
                && !sources[0].ends_with(".tgz")
                && !sources[0].ends_with(".tar.gz")
            {
                // Single source on a tty: interactive agent mode (not for archives)
                let cfg = config_file::load_memex_config(&root)?;
                let model_ref = cfg.resolve_model(OperationKind::Ingest);
                let memex = std::sync::Arc::new(
                    memex_core::Memex::open(
                        root.clone(),
                        create_llm_provider(&root, &model_ref)?,
                        &model_ref.model,
                    )
                    .map_err(|e| anyhow::anyhow!("{}", e))?,
                );
                let threshold = memex_agent::config::parse_small_memex_threshold(&cfg.raw_content);

                // Canonicalize and chdir to source's parent so file_read can access it
                let source_path = std::fs::canonicalize(&sources[0])
                    .unwrap_or_else(|_| PathBuf::from(&sources[0]));
                let file_name = source_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                if let Some(parent) = source_path.parent() {
                    let _ = std::env::set_current_dir(parent);
                }

                let mut agent = memex_agent::copilot::build_copilot_agent(
                    memex,
                    create_provider(&root, &model_ref)?,
                    &model_ref.model,
                    threshold,
                )?;
                let prompt = memex_agent::copilot::format_copilot_prompt(
                    memex_agent::copilot::CopilotMode::Ingest,
                    &format!("Ingest: {file_name}"),
                );
                match agent.turn(&prompt).await {
                    Ok(response) => {
                        println!("{response}");
                        interactive_loop(&mut agent, "> ").await;
                    }
                    Err(e) => {
                        eprintln!("Agent error: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                // Batch mode: non-interactive pipeline
                let cfg = config_file::load_memex_config(&root)?;
                let model_ref = cfg.resolve_model(OperationKind::Ingest);
                let mut memex = memex_core::Memex::open(
                    root.clone(),
                    create_llm_provider(&root, &model_ref)?,
                    &model_ref.model,
                )
                .map_err(|e| anyhow::anyhow!("{}", e))?;
                memex.set_progress(|msg| eprintln!("  {msg}"));

                for source_str in &sources {
                    let source = if source_str.starts_with("http://")
                        || source_str.starts_with("https://")
                    {
                        Source::Url {
                            url: source_str.clone(),
                        }
                    } else {
                        let path = PathBuf::from(source_str);
                        if path.is_dir() {
                            Source::Directory { path }
                        } else {
                            Source::File { path }
                        }
                    };

                    eprintln!("Ingesting: {source_str}");
                    match memex.ingest(&source).await {
                        Ok(report) => {
                            println!(
                                "Ingested '{}': {} pages created, {} updated",
                                source_str,
                                report.pages_created.len(),
                                report.pages_updated.len()
                            );
                            for page in &report.pages_created {
                                println!("  + {}", page.display());
                            }
                            for page in &report.pages_updated {
                                println!("  ~ {}", page.display());
                            }
                            for warning in &report.warnings {
                                eprintln!("  Warning: {warning}");
                            }
                        }
                        Err(e) => {
                            eprintln!("Error ingesting '{}': {}", source_str, e);
                        }
                    }
                }
            }
        }

        Commands::Query { question } => {
            let root = memex_root();
            let _ = std::env::set_current_dir(&root);

            if let Some(question) = question {
                // Single-shot mode: query and exit
                let cfg = config_file::load_memex_config(&root)?;
                let model_ref = cfg.resolve_model(OperationKind::Query);
                let memex = memex_core::Memex::open(
                    root.clone(),
                    create_llm_provider(&root, &model_ref)?,
                    &model_ref.model,
                )
                .map_err(|e| anyhow::anyhow!("{}", e))?;

                eprintln!("Querying: {question}");
                match memex.query(&question).await {
                    Ok(result) => {
                        println!("{}", result.answer);
                        if !result.citations.is_empty() {
                            println!("\nCitations:");
                            for citation in &result.citations {
                                println!("  - {} ({})", citation.title, citation.page.display());
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Query error: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                // Interactive agent mode
                if !atty::is(atty::Stream::Stdin) {
                    eprintln!("Interactive query mode requires a terminal.");
                    eprintln!("Provide a question argument for non-interactive use.");
                    std::process::exit(1);
                }

                let cfg = config_file::load_memex_config(&root)?;
                let model_ref = cfg.resolve_model(OperationKind::Query);
                let memex = std::sync::Arc::new(
                    memex_core::Memex::open(
                        root.clone(),
                        create_llm_provider(&root, &model_ref)?,
                        &model_ref.model,
                    )
                    .map_err(|e| anyhow::anyhow!("{}", e))?,
                );
                let threshold = memex_agent::config::parse_small_memex_threshold(&cfg.raw_content);

                let mut agent = memex_agent::copilot::build_copilot_agent(
                    memex,
                    create_provider(&root, &model_ref)?,
                    &model_ref.model,
                    threshold,
                )?;

                eprintln!("Memex query agent (type 'quit' to exit)\n");
                eprint!("Query> ");
                let mut first_input = String::new();
                if std::io::stdin().read_line(&mut first_input).is_err()
                    || first_input.trim().is_empty()
                {
                    return Ok(());
                }
                let prompt = memex_agent::copilot::format_copilot_prompt(
                    memex_agent::copilot::CopilotMode::Query,
                    first_input.trim(),
                );
                match agent.turn(&prompt).await {
                    Ok(reply) => {
                        println!("{reply}");
                        interactive_loop(&mut agent, "Query> ").await;
                    }
                    Err(e) => eprintln!("Agent error: {e}"),
                }
            }
        }

        Commands::Lint { fix } => {
            let root = memex_root();
            let _ = std::env::set_current_dir(&root);
            let cfg = config_file::load_memex_config(&root)?;
            let model_ref = cfg.resolve_model(OperationKind::Lint);
            let memex = memex_core::Memex::open(
                root.clone(),
                create_llm_provider(&root, &model_ref)?,
                &model_ref.model,
            )
            .map_err(|e| anyhow::anyhow!("{}", e))?;

            eprintln!("Running wiki health checks...");
            match memex.lint().await {
                Ok(report) => {
                    if report.issues.is_empty() {
                        println!("No issues found.");
                    } else {
                        let fixable: Vec<_> = report
                            .issues
                            .iter()
                            .filter(|i| i.proposed_fix.is_some())
                            .collect();
                        println!(
                            "{} issue(s) found ({} fixable):",
                            report.issues.len(),
                            fixable.len()
                        );
                        for issue in &report.issues {
                            println!("  [{:?}] {}", issue.kind, issue.description);
                            if let Some(ref f) = issue.proposed_fix {
                                println!("    Fix: {}", f.description);
                            }
                        }

                        if fix && !fixable.is_empty() {
                            eprintln!("\nApplying {} fix(es)...", fixable.len());
                            let mut applied = 0;
                            for issue in &fixable {
                                if let Some(ref f) = issue.proposed_fix {
                                    match memex.apply_fix(f).await {
                                        Ok(()) => applied += 1,
                                        Err(e) => eprintln!("  Fix failed: {e}"),
                                    }
                                }
                            }
                            eprintln!("{applied} fix(es) applied.");
                        } else if !fixable.is_empty() && !fix {
                            eprintln!(
                                "\nRun with --fix to apply {} fixable issue(s).",
                                fixable.len()
                            );
                        }
                    }
                    if !report.suggested_questions.is_empty() {
                        println!("\nSuggested questions:");
                        for q in &report.suggested_questions {
                            println!("  - {q}");
                        }
                    }
                    if !report.suggested_sources.is_empty() {
                        println!("\nSuggested sources:");
                        for s in &report.suggested_sources {
                            println!("  - {s}");
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Lint error: {e}");
                    std::process::exit(1);
                }
            }
        }

        Commands::Wiki { sub } => {
            let root = memex_root();
            match sub {
                WikiSub::Reindex => {
                    let cfg = config_file::load_memex_config(&root)?;
                    let model_ref = cfg.resolve_model(OperationKind::Global);
                    let memex = memex_core::Memex::open(
                        root.clone(),
                        create_llm_provider(&root, &model_ref)?,
                        &model_ref.model,
                    )
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                    memex.reindex().map_err(|e| anyhow::anyhow!("{}", e))?;
                    println!("Index rebuilt.");
                }
                WikiSub::Show => {
                    let index_path = root.join("index.md");
                    if index_path.exists() {
                        println!("{}", std::fs::read_to_string(&index_path)?);
                    } else {
                        println!("No index found. Run `memex init` first.");
                    }
                }
                WikiSub::Log => {
                    let log_path = root.join("log.md");
                    if log_path.exists() {
                        let content = std::fs::read_to_string(&log_path)?;
                        let lines: Vec<&str> = content.lines().collect();
                        let start = lines.len().saturating_sub(20);
                        for line in &lines[start..] {
                            println!("{line}");
                        }
                    } else {
                        println!("No log found.");
                    }
                }
                WikiSub::Stats => {
                    let wiki_dir = root.join("wiki");
                    let sources_dir = root.join("sources");
                    let page_count = if wiki_dir.exists() {
                        std::fs::read_dir(&wiki_dir)?
                            .filter_map(|e| e.ok())
                            .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
                            .count()
                    } else {
                        0
                    };
                    let source_count = if sources_dir.exists() {
                        // Count non-meta files across all source subdirectories
                        let mut count = 0;
                        for sub_entry in std::fs::read_dir(&sources_dir)?
                            .filter_map(|e| e.ok())
                            .filter(|e| e.path().is_dir())
                        {
                            fn count_files(dir: &std::path::Path) -> usize {
                                std::fs::read_dir(dir)
                                    .ok()
                                    .map(|entries| {
                                        entries
                                            .filter_map(|e| e.ok())
                                            .map(|e| {
                                                if e.path().is_dir() {
                                                    count_files(&e.path())
                                                } else if !e
                                                    .file_name()
                                                    .to_string_lossy()
                                                    .ends_with(".meta.json")
                                                {
                                                    1
                                                } else {
                                                    0
                                                }
                                            })
                                            .sum()
                                    })
                                    .unwrap_or(0)
                            }
                            count += count_files(&sub_entry.path());
                        }
                        count
                    } else {
                        0
                    };
                    println!("Memex Stats");
                    println!("  Root:    {}", root.display());
                    println!("  Pages:   {page_count}");
                    println!("  Sources: {source_count}");
                }
                WikiSub::Search { query, top_k } => {
                    use memex_core::search::WikiSearch;

                    let db_path = root.join(memex_core::SEARCH_DB_NAME);
                    let search = memex_core::search::Bm25Search::open(&db_path)
                        .map_err(|e| anyhow::anyhow!("{}", e))?;
                    let results = search
                        .search(&query, top_k, None)
                        .await
                        .map_err(|e| anyhow::anyhow!("{}", e))?;

                    if results.is_empty() {
                        println!("No results.");
                    } else {
                        for r in &results {
                            println!("{:.3}  {}  {}", r.score, r.path.display(), r.title);
                        }
                    }
                }
            }
        }

        Commands::Doctor => {
            let root = memex_root();
            match auth::auth_summary_line(&root).await {
                Ok(summary) => println!("Memex auth: {summary}"),
                Err(err) => println!("Memex auth: error — {err}"),
            }

            let providers = auto_detect::detect_providers();
            if providers.is_empty() {
                println!("No providers detected.");
                println!("Set ANTHROPIC_API_KEY, OPENAI_API_KEY, or GEMINI_API_KEY.");
            } else {
                println!("Detected providers:");
                for p in &providers {
                    print!("  {} ({}): ", p.provider, p.env_var);
                    match zeroclaw::providers::create_provider_with_options(
                        &p.frontier_model.provider,
                        None,
                        &runtime_options(&root),
                    ) {
                        Ok(_) => println!("ok"),
                        Err(e) => println!("error — {e}"),
                    }
                }
            }
        }

        Commands::Config { sub } => match sub {
            ConfigSub::Show => {
                let root = memex_root();
                let config_path = root.join("config.toml");
                if config_path.exists() {
                    println!("{}", std::fs::read_to_string(&config_path)?);
                } else {
                    println!("No config found. Run `memex init` first.");
                }
            }
            ConfigSub::Set { key, value } => {
                println!("(stub) Would set {key} = {value}");
                println!("Config set is not yet implemented.");
            }
        },

        Commands::Brainstorm { sub } => match sub {
            BrainstormSub::New { task, task_type } => {
                let root = memex_root();
                let _ = std::env::set_current_dir(&root);

                let cfg = config_file::load_memex_config(&root)?;
                let model_ref = cfg.resolve_model(OperationKind::Global);
                let memex = std::sync::Arc::new(
                    memex_core::Memex::open(
                        root.clone(),
                        create_llm_provider(&root, &model_ref)?,
                        &model_ref.model,
                    )
                    .map_err(|e| anyhow::anyhow!("{}", e))?,
                );

                let brainstorm_config =
                    memex_agent::config::parse_brainstorm_config(&cfg.raw_content);

                // Build orchestrator provider wrapped with cost tracking
                let cost_stats = std::sync::Arc::new(memex_agent::cost::CostStats::default());
                let orch_ref =
                    memex_agent::builder::ModelRef::parse(&brainstorm_config.orchestrator);
                let raw_provider = zeroclaw::providers::create_provider_with_options(
                    &orch_ref.provider,
                    None,
                    &runtime_options(&root),
                )
                .unwrap_or_else(|_| Box::new(memex_agent::dry_run::DryRunProvider));
                let orchestrator_provider: Box<dyn zeroclaw::providers::Provider> =
                    Box::new(memex_agent::cost::CostTrackingProvider::new(
                        raw_provider,
                        std::sync::Arc::clone(&cost_stats),
                    ));

                // Build agent with progress reporting
                let mut agent = memex_agent::builder::MemexAgentBuilder::new(
                    memex.clone(),
                    brainstorm_config,
                    orchestrator_provider,
                )
                .provider_runtime_options(runtime_options(&root))
                .progress(true)
                .build()?;

                // Build task string with preset context if --type provided
                let build_task_with_preset = |task_str: &str| -> String {
                    if let Some(ref tt) = task_type
                        && let Some(preset) = memex_agent::preset::get_preset(tt)
                    {
                        let ctx = memex_agent::preset::format_preset_context(&preset);
                        return format!("{ctx}\n\n{task_str}");
                    }
                    task_str.to_string()
                };

                if let Some(task) = task {
                    // Single-shot: run to completion and exit
                    let (session_id, session_dir) =
                        memex_agent::session::create_session(&root, &task)?;
                    eprintln!("Starting brainstorm session: {session_id}");

                    let task_with_preset = build_task_with_preset(&task);
                    let prompt = memex_agent::copilot::format_copilot_prompt(
                        memex_agent::copilot::CopilotMode::Brainstorm,
                        &task_with_preset,
                    );
                    match agent.turn(&prompt).await {
                        Ok(response) => {
                            println!("{response}\n");
                            memex_agent::session::complete_session(&session_dir, &response)?;
                            eprintln!("Session saved to: {}", session_dir.display());
                        }
                        Err(e) => {
                            eprintln!("Agent error: {e}");
                            std::process::exit(1);
                        }
                    }
                } else {
                    // Interactive agent mode
                    if !atty::is(atty::Stream::Stdin) {
                        eprintln!("Interactive brainstorm mode requires a terminal.");
                        eprintln!("Provide a task argument for non-interactive use.");
                        std::process::exit(1);
                    }

                    eprintln!("Memex brainstorm agent (type 'quit' to exit)\n");
                    eprint!("What would you like to brainstorm? ");
                    let mut task_input = String::new();
                    if std::io::stdin().read_line(&mut task_input).is_err()
                        || task_input.trim().is_empty()
                    {
                        return Ok(());
                    }
                    let task_str = task_input.trim();

                    let (session_id, session_dir) =
                        memex_agent::session::create_session(&root, task_str)?;
                    eprintln!("Starting brainstorm session: {session_id}");

                    let task_with_preset = build_task_with_preset(task_str);
                    let prompt = memex_agent::copilot::format_copilot_prompt(
                        memex_agent::copilot::CopilotMode::Brainstorm,
                        &task_with_preset,
                    );
                    match agent.turn(&prompt).await {
                        Ok(response) => {
                            println!("{response}\n");
                            let last_response =
                                interactive_loop(&mut agent, "> ").await.unwrap_or(response);
                            memex_agent::session::complete_session(&session_dir, &last_response)?;
                            eprintln!("Session saved to: {}", session_dir.display());
                        }
                        Err(e) => {
                            eprintln!("Agent error: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                // Print cost summary
                eprintln!(
                    "Session cost: {}",
                    memex_agent::cost::format_cost_summary(&cost_stats)
                );
            }
            BrainstormSub::Resume { session_id } => {
                println!(
                    "(stub) Resume session: {}",
                    session_id.as_deref().unwrap_or("<latest>")
                );
                println!("Full resume support coming in Plan 2.");
            }
            BrainstormSub::List => {
                let root = memex_root();
                let sessions = memex_agent::session::list_sessions(&root);
                if sessions.is_empty() {
                    println!("No brainstorm sessions found.");
                } else {
                    for (id, meta) in &sessions {
                        let status = meta
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let task_desc = meta.get("task").and_then(|v| v.as_str()).unwrap_or("");
                        println!("{id}  [{status}]  {task_desc}");
                    }
                }
            }
            BrainstormSub::Show { session_id } => {
                let root = memex_root();
                let session_dir = root.join("sources/brainstorms").join(&session_id);
                let output_path = session_dir.join("final-output.md");
                if output_path.exists() {
                    println!("{}", std::fs::read_to_string(&output_path)?);
                } else {
                    eprintln!("Session {session_id} not found or has no output.");
                }
            }
            BrainstormSub::Export { session_id, output } => {
                println!(
                    "(stub) Export session {} to {}",
                    session_id,
                    output.as_deref().unwrap_or("stdout")
                );
                println!("Full export coming in Plan 2.");
            }
        },
    }

    Ok(())
}
