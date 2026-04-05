use clap::{Parser, Subcommand};
use std::sync::Arc;
use zeroclaw::memory::Memory as _;

fn brainstormer_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".brainstormer")
}

fn build_agent(
    provider: Box<dyn zeroclaw::providers::Provider>,
    memory: Arc<dyn zeroclaw::memory::Memory>,
    model_name: String,
) -> anyhow::Result<zeroclaw::agent::Agent> {
    zeroclaw::agent::Agent::builder()
        .provider(provider)
        .tools(vec![])
        .memory(memory)
        .observer(Arc::new(zeroclaw::observability::NoopObserver) as Arc<dyn zeroclaw::observability::Observer>)
        .tool_dispatcher(Box::new(zeroclaw::agent::dispatcher::NativeToolDispatcher))
        .model_name(model_name)
        .temperature(0.7)
        .workspace_dir(std::path::PathBuf::from("."))
        .build()
}

#[derive(Parser)]
#[command(name = "brainstormer", about = "Multi-LLM brainstorming with convergence")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a new brainstorming session
    New {
        /// Task type preset
        #[arg(long, default_value = "software")]
        r#type: String,
        /// Interaction mode
        #[arg(long, default_value = "autopilot")]
        mode: String,
        /// Skip convergence loop (single pass)
        #[arg(long)]
        no_loop: bool,
        /// Dry run with simulated LLM responses (no API keys needed)
        #[arg(long)]
        dry_run: bool,
        /// Output file path (default: stdout)
        #[arg(long, short = 'o')]
        output: Option<String>,
        /// Input files for additional context (can be repeated)
        #[arg(long, short = 'i')]
        input: Vec<String>,
        /// URL to fetch as additional context
        #[arg(long)]
        url: Option<String>,
    },
    /// Resume an interrupted session
    Resume {
        /// Session ID to resume
        session_id: Option<String>,
    },
    /// List all sessions
    List,
    /// Show session details
    Show { session_id: String },
    /// Show per-model preference stats
    Stats,
    /// Manage configuration
    Config {
        #[command(subcommand)]
        what: ConfigCommands,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Manage LLM providers
    Providers,
    /// Manage task type presets
    Presets,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::New {
            r#type,
            mode,
            no_loop,
            dry_run,
            output,
            input,
            url,
        } => {
            use brainstormer::cli::auto_detect;
            use brainstormer::types::*;
            use brainstormer::template;
            use brainstormer::agent_setup;
            use brainstormer::pipeline::Pipeline;

            // 1. Auto-detect providers (or use dry-run)
            let providers = auto_detect::detect_providers();
            if providers.is_empty() && !dry_run {
                eprintln!("No LLM providers found.");
                eprintln!("Set at least one of: ANTHROPIC_API_KEY, OPENAI_API_KEY, or GEMINI_API_KEY");
                eprintln!("Or use --dry-run for testing without API keys.");
                std::process::exit(1);
            }

            if dry_run {
                eprintln!("Providers: dry-run (simulated responses)");
            } else {
                eprintln!(
                    "Providers: {}",
                    providers
                        .iter()
                        .map(|p| p.provider.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }

            // 2. Load preset
            let preset = template::load_preset(&r#type)?;
            eprintln!(
                "Preset: {} ({} sections, {} dimensions)",
                preset.name,
                preset.sections.len(),
                preset.dimensions.len()
            );

            // 3. Build session config
            let mode = match mode.as_str() {
                "copilot" => Mode::Copilot,
                "cruise" => Mode::Cruise,
                _ => Mode::Autopilot,
            };

            let dry_run_model = ModelRef { provider: "dry-run".into(), model: "dry-run".into() };

            let mut brainstorm_models: Vec<ModelRef> = if dry_run {
                vec![dry_run_model.clone(), dry_run_model.clone()]
            } else {
                providers.iter().map(|p| p.frontier_model.clone()).collect()
            };
            let mut review_models: Vec<ModelRef> = if dry_run {
                vec![dry_run_model.clone(), dry_run_model.clone()]
            } else {
                providers.iter().map(|p| p.midtier_model.clone()).collect()
            };

            // With 1 provider, duplicate the model for a second perspective
            while brainstorm_models.len() < 2 {
                brainstorm_models.push(brainstorm_models[0].clone());
            }
            while review_models.len() < 2 {
                review_models.push(review_models[0].clone());
            }

            let merge_llm = if dry_run {
                dry_run_model
            } else {
                providers[0].frontier_model.clone()
            };

            let config = SessionConfig {
                task_type: r#type,
                mode,
                do_loop: !no_loop,
                brainstorm_models,
                review_models,
                max_rounds: 5,
                merge_llm,
            };

            // 4. Create provider, memory, observer
            let provider: Box<dyn zeroclaw::providers::Provider> = if dry_run {
                Box::new(brainstormer::dry_run::DryRunProvider)
            } else {
                zeroclaw::providers::create_provider(
                    &providers[0].frontier_model.provider,
                    None, // Uses env var
                )?
            };

            // Use SqliteMemory for session persistence, fallback to NoneMemory on error
            let brainstormer_dir = brainstormer_dir();
            std::fs::create_dir_all(&brainstormer_dir).ok();
            let memory: Arc<dyn zeroclaw::memory::Memory> =
                match zeroclaw::memory::SqliteMemory::new(&brainstormer_dir) {
                    Ok(m) => {
                        eprintln!("Session storage: {}", brainstormer_dir.display());
                        Arc::new(m)
                    }
                    Err(e) => {
                        eprintln!("Warning: SQLite memory failed ({}), using in-memory only", e);
                        Arc::new(zeroclaw::memory::NoneMemory)
                    }
                };
            let brainstorm_observer = agent_setup::build_observer();

            // 5. Build agent
            let system_prompt = template::load_system_prompt(config.mode.as_str())?;

            let mut agent = build_agent(provider, memory.clone(), config.merge_llm.model.clone())?;

            eprintln!("Session configured. Starting brainstorm...");
            eprintln!("  Mode: {:?}", config.mode);
            eprintln!("  Loop: {}", config.do_loop);
            eprintln!("  Models: {}", config.brainstorm_models.len());
            eprintln!("  Orchestrator: {}/{}", config.merge_llm.provider, config.merge_llm.model);

            // 6. Read task from stdin or use preset description
            eprintln!("\nDescribe what you want to brainstorm:");
            let mut task_input = String::new();
            std::io::stdin().read_line(&mut task_input)?;
            let task_description = task_input.trim().to_string();

            if task_description.is_empty() {
                eprintln!("No task provided. Exiting.");
                std::process::exit(1);
            }

            // Load file context
            let file_contents: Vec<String> = input
                .iter()
                .map(|path| brainstormer::input::load_file_context(path))
                .collect::<Result<Vec<_>, _>>()?;

            // Fetch URL context if provided
            let url_content = if let Some(ref url) = url {
                Some(brainstormer::input::fetch_url_context(url).await?)
            } else {
                None
            };

            let task_description = brainstormer::input::assemble_context(
                &task_description,
                &file_contents,
                url_content.as_deref(),
            );

            // 7. Run pipeline
            let provider_factory = if dry_run {
                brainstormer::dry_run::dry_run_provider_factory()
            } else {
                brainstormer::tools::brainstorm_swarm::default_provider_factory()
            };

            let mut pipeline = Pipeline::new(
                config.clone(),
                brainstorm_observer,
                memory,
                task_description.clone(),
                provider_factory,
                system_prompt,
            );

            match pipeline.run(&mut agent).await {
                Ok(result) => {
                    if let Some(ref output_path) = output {
                        let export_content = brainstormer::export::format_export(
                            &task_description,
                            &config,
                            &result,
                            pipeline.rounds_completed(),
                            pipeline.converged(),
                            pipeline.cost_usd(),
                        );
                        brainstormer::export::write_export(
                            std::path::Path::new(output_path),
                            &export_content,
                        )?;
                        eprintln!("Exported to {}", output_path);
                    }
                    eprintln!("\n=== FINAL DOCUMENT ===\n");
                    println!("{}", result);
                }
                Err(e) => {
                    eprintln!("Pipeline error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Resume { session_id } => {
            use brainstormer::resume;

            let brainstormer_dir = brainstormer_dir();
            let memory: Arc<dyn zeroclaw::memory::Memory> =
                match zeroclaw::memory::SqliteMemory::new(&brainstormer_dir) {
                    Ok(m) => Arc::new(m),
                    Err(e) => {
                        eprintln!("Could not open session store: {}", e);
                        std::process::exit(1);
                    }
                };

            let sid = session_id.unwrap_or_else(|| {
                eprintln!("No session ID provided. Use `brainstormer list` to find sessions.");
                std::process::exit(1);
            });

            let state = resume::load_session(memory.as_ref(), &sid).await?;
            if !state.is_resumable() {
                eprintln!("Session '{}' is not resumable (missing config or draft).", sid);
                eprintln!("Use `brainstormer list` to find sessions, or start a new one.");
                std::process::exit(1);
            }

            let config = state.config.unwrap();
            eprintln!("Resuming session {} from round {}", sid, state.last_round + 1);
            eprintln!("  Type: {}", config.task_type);
            eprintln!("  Mode: {:?}", config.mode);

            let system_prompt = brainstormer::template::load_system_prompt(config.mode.as_str())?;

            let provider_factory = brainstormer::tools::brainstorm_swarm::default_provider_factory();
            let provider = zeroclaw::providers::create_provider(
                &config.merge_llm.provider,
                None,
            )?;
            let brainstorm_observer = brainstormer::agent_setup::build_observer();

            let mut agent = build_agent(provider, memory.clone(), config.merge_llm.model.clone())?;

            let task_description = state.task.unwrap_or_else(|| "Resumed session".into());

            let mut pipeline = brainstormer::pipeline::Pipeline::resume_from(
                config,
                brainstorm_observer,
                memory,
                task_description,
                provider_factory,
                state.last_draft.unwrap(),
                state.last_round + 1,
                system_prompt,
            );

            match pipeline.run(&mut agent).await {
                Ok(result) => {
                    eprintln!("\n=== FINAL DOCUMENT ===\n");
                    println!("{}", result);
                }
                Err(e) => {
                    eprintln!("Pipeline error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::List => {
            let brainstormer_dir = brainstormer_dir();
            match zeroclaw::memory::SqliteMemory::new(&brainstormer_dir) {
                Ok(memory) => {
                    let entries = memory
                        .recall("brainstorm:", 100, None, None, None)
                        .await?;
                    // Group by session ID
                    let mut sessions: std::collections::HashMap<String, Vec<&zeroclaw::memory::MemoryEntry>> =
                        std::collections::HashMap::new();
                    for entry in &entries {
                        if entry.key.starts_with("brainstorm:") {
                            let parts: Vec<&str> = entry.key.split(':').collect();
                            if parts.len() >= 2 {
                                sessions
                                    .entry(parts[1].to_string())
                                    .or_default()
                                    .push(entry);
                            }
                        }
                    }
                    if sessions.is_empty() {
                        println!("No sessions found.");
                    } else {
                        println!("Sessions ({}):", sessions.len());
                        for (id, entries) in &sessions {
                            let has_config = entries.iter().any(|e| e.key.ends_with(":config"));
                            let has_final = entries.iter().any(|e| e.key.ends_with(":final_output"));
                            let rounds = entries
                                .iter()
                                .filter(|e| e.key.contains(":round:"))
                                .count();
                            let status = if has_final {
                                "complete"
                            } else {
                                "in progress"
                            };
                            let short_id = &id[..id.len().min(8)];
                            println!(
                                "  {} | {} rounds | {} | {}",
                                short_id,
                                rounds / 2, // round state + draft = 2 entries per round
                                status,
                                if has_config { "has config" } else { "no config" },
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Could not open session store: {}", e);
                }
            }
        }
        Commands::Show { session_id } => {
            let brainstormer_dir = brainstormer_dir();
            match zeroclaw::memory::SqliteMemory::new(&brainstormer_dir) {
                Ok(memory) => {
                    // Try to find the config
                    let entries = memory
                        .recall(&format!("brainstorm:{}:", session_id), 50, None, None, None)
                        .await?;
                    if entries.is_empty() {
                        // Try prefix match
                        let all = memory.recall("brainstorm:", 200, None, None, None).await?;
                        let matching: Vec<_> = all
                            .iter()
                            .filter(|e| e.key.contains(&session_id))
                            .collect();
                        if matching.is_empty() {
                            println!("No session found matching '{}'", session_id);
                        } else {
                            println!("Session entries for '{}':", session_id);
                            for entry in matching {
                                println!(
                                    "  {} ({} chars) [{}]",
                                    entry.key,
                                    entry.content.len(),
                                    entry.timestamp
                                );
                            }
                        }
                    } else {
                        println!("Session {}:", session_id);
                        for entry in &entries {
                            if entry.key.ends_with(":config") {
                                println!("  Config: {}", &entry.content[..entry.content.len().min(200)]);
                            } else if entry.key.contains(":round:") {
                                println!(
                                    "  {} ({} chars)",
                                    entry.key.rsplit(':').next().unwrap_or("?"),
                                    entry.content.len()
                                );
                            } else if entry.key.ends_with(":final_output") {
                                println!("  Final output: {} chars", entry.content.len());
                            }
                        }
                    }
                }
                Err(e) => eprintln!("Could not open session store: {}", e),
            }
        }
        Commands::Stats => {
            let brainstormer_dir = brainstormer_dir();
            match zeroclaw::memory::SqliteMemory::new(&brainstormer_dir) {
                Ok(memory) => {
                    let entries = memory.recall("brainstorm:", 500, None, None, None).await?;
                    let session_count = {
                        let mut ids: std::collections::HashSet<String> =
                            std::collections::HashSet::new();
                        for entry in &entries {
                            if entry.key.starts_with("brainstorm:") {
                                let parts: Vec<&str> = entry.key.split(':').collect();
                                if parts.len() >= 2 {
                                    ids.insert(parts[1].to_string());
                                }
                            }
                        }
                        ids.len()
                    };
                    let complete = entries
                        .iter()
                        .filter(|e| e.key.ends_with(":final_output"))
                        .count();
                    let total_rounds: usize = entries
                        .iter()
                        .filter(|e| e.key.contains(":round:") && !e.key.contains(":draft:"))
                        .count();

                    println!("Brainstormer Stats");
                    println!("  Sessions: {} ({} complete)", session_count, complete);
                    println!("  Total rounds: {}", total_rounds);
                    println!(
                        "  Storage: {}",
                        brainstormer_dir.join("memory/brain.db").display()
                    );
                }
                Err(e) => eprintln!("Could not open session store: {}", e),
            }
        }
        Commands::Config { what } => {
            match what {
                ConfigCommands::Providers => {
                    use brainstormer::cli::auto_detect;
                    let providers = auto_detect::detect_providers();
                    if providers.is_empty() {
                        println!("No providers detected.");
                        println!("Set environment variables to add providers:");
                        println!("  ANTHROPIC_API_KEY  -> Anthropic (claude-opus-4-6 / claude-sonnet-4-6)");
                        println!("  OPENAI_API_KEY     -> OpenAI (gpt-5.4 / gpt-5.4-mini)");
                        println!("  GEMINI_API_KEY     -> Google (gemini-3.1-pro / gemini-3.1-flash)");
                    } else {
                        println!("Detected providers:");
                        for p in &providers {
                            println!("  {} (via {})", p.provider, p.env_var);
                            println!("    Frontier: {}/{}", p.frontier_model.provider, p.frontier_model.model);
                            println!("    Mid-tier: {}/{}", p.midtier_model.provider, p.midtier_model.model);
                        }
                    }
                }
                ConfigCommands::Presets => {
                    println!("Available presets:");
                    for name in &["software", "general", "research", "article", "book", "strategy"] {
                        match brainstormer::template::load_preset(name) {
                            Ok(preset) => {
                                println!("  {} - {} sections, {} dimensions",
                                    preset.name, preset.sections.len(), preset.dimensions.len());
                                if !preset.sections.is_empty() {
                                    println!("    Sections: {}", preset.sections.join(", "));
                                }
                                println!("    Dimensions: {}", preset.dimensions.join(", "));
                            }
                            Err(e) => println!("  {} - error: {}", name, e),
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
