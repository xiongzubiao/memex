use crate::cli::dashboard;
use crate::hooks::copilot::{format_copilot_feedback, parse_copilot_input};
use crate::hooks::output_capture::tag_with_round;
use crate::hooks::sequence_enforcement::PipelineState;
use crate::observer::BrainstormObserver;
use crate::template;
use crate::tools::brainstorm_swarm::{collect_outputs, dispatch_parallel, ProviderFactory};
use crate::tools::convergence::{build_convergence_result, evaluate_section, parse_semantic_delta};
use crate::tools::merge_quality::should_proceed_despite_failure;
use crate::types::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use zeroclaw::agent::Agent;
use zeroclaw::memory::Memory;

/// Cost accumulator for a pipeline run.
#[derive(Debug, Clone, Default)]
struct CostTracker {
    tokens_in: u64,
    tokens_out: u64,
}

impl CostTracker {
    fn add_estimate(&mut self, text_len: usize, is_input: bool) {
        // Rough estimate: 1 token ≈ 4 chars
        let tokens = (text_len / 4) as u64;
        if is_input {
            self.tokens_in += tokens;
        } else {
            self.tokens_out += tokens;
        }
    }

    fn estimate_usd(&self) -> f64 {
        // Rough: $10/M input, $30/M output (frontier model pricing)
        (self.tokens_in as f64 * 10.0 + self.tokens_out as f64 * 30.0) / 1_000_000.0
    }
}

/// Per-section objection history for convergence guard.
#[derive(Debug, Clone, Default)]
struct SectionHistory {
    objections: Vec<(String, String)>, // (model_id, objection_text)
}

/// Runs the full brainstormer pipeline.
pub struct Pipeline {
    config: SessionConfig,
    state: Arc<Mutex<PipelineState>>,
    observer: Arc<BrainstormObserver>,
    memory: Arc<dyn Memory>,
    rounds: Vec<RoundState>,
    task_description: String,
    provider_factory: ProviderFactory,
    cost: CostTracker,
    section_histories: HashMap<String, SectionHistory>,
    initial_draft: Option<String>,
    start_round: u32,
    system_prompt: String,
}

impl Pipeline {
    pub fn new(
        config: SessionConfig,
        observer: Arc<BrainstormObserver>,
        memory: Arc<dyn Memory>,
        task_description: String,
        provider_factory: ProviderFactory,
        system_prompt: String,
    ) -> Self {
        Self {
            config,
            state: Arc::new(Mutex::new(PipelineState::new())),
            observer,
            memory,
            rounds: Vec::new(),
            task_description,
            provider_factory,
            cost: CostTracker::default(),
            section_histories: HashMap::new(),
            initial_draft: None,
            start_round: 1,
            system_prompt,
        }
    }

    /// Create a pipeline that resumes from a prior session's state.
    /// Skips brainstorm/merge1 stages (already done) and starts from review.
    #[allow(clippy::too_many_arguments)]
    pub fn resume_from(
        config: SessionConfig,
        observer: Arc<BrainstormObserver>,
        memory: Arc<dyn Memory>,
        task_description: String,
        provider_factory: ProviderFactory,
        last_draft: String,
        start_round: u32,
        system_prompt: String,
    ) -> Self {
        let mut pipeline = Self::new(config, observer, memory, task_description, provider_factory, system_prompt);
        pipeline.state.lock().unwrap().advance(Stage::Brainstorm);
        pipeline.state.lock().unwrap().advance(Stage::Merge1);
        pipeline.initial_draft = Some(last_draft);
        pipeline.start_round = start_round;
        pipeline
    }

    /// Run the full pipeline, returning the final document.
    pub async fn run(&mut self, agent: &mut Agent) -> anyhow::Result<String> {
        let preset = template::load_preset(&self.config.task_type)?;
        let sections_str = preset.sections.join(", ");
        let dimensions_str = preset.dimensions.join(", ");

        let system_prompt = self.system_prompt.clone();

        // Persist session config and task description to memory
        let session_id = uuid::Uuid::new_v4().to_string();
        let config_json = serde_json::to_string(&self.config)?;
        self.persist(&Self::session_key(&session_id, "config"), &config_json, &session_id).await;
        self.persist(&Self::session_key(&session_id, "task"), &self.task_description, &session_id).await;

        let mut current_round = self.start_round;
        let mut merged_draft = self.initial_draft.take().unwrap_or_default();
        let mut last_draft = if current_round > 1 { merged_draft.clone() } else { String::new() };
        let checkpoint_interval = 2u32; // Cruise mode: checkpoint every 2 rounds

        while current_round <= self.config.max_rounds {
            self.observer.on_round_start(current_round);
            eprintln!("\n=== Round {}/{} ===", current_round, self.config.max_rounds);

            // --- Stage 1: BRAINSTORM (first round only) ---
            if current_round == 1 {
                let brainstorm_vars = self.build_vars(&[
                    ("task", &self.task_description),
                    ("task_type", &self.config.task_type),
                    ("sections", &sections_str),
                ]);
                let brainstorm_prompt = self.render_prompt("brainstorm", &brainstorm_vars)?;
                self.cost
                    .add_estimate(brainstorm_prompt.len() * self.config.brainstorm_models.len(), true);

                self.state.lock().unwrap().advance(Stage::Brainstorm);

                eprintln!(
                    "  Dispatching brainstorm to {} models in parallel...",
                    self.config.brainstorm_models.len()
                );
                let mut brainstorm_results = dispatch_parallel(
                    &self.config.brainstorm_models,
                    &brainstorm_prompt,
                    Some(&system_prompt),
                    120,
                    &self.provider_factory,
                )
                .await;
                let (mut brainstorm_output, warnings) = collect_outputs(&brainstorm_results);
                self.cost.add_estimate(brainstorm_output.len(), false);
                for w in &warnings {
                    eprintln!("  Warning: {}", w);
                }
                eprintln!(
                    "  Brainstorm complete: {} models responded ({} chars)",
                    brainstorm_results.len() - warnings.len(),
                    brainstorm_output.len()
                );

                // Copilot: pause for user review with feedback loop
                if self.config.mode == Mode::Copilot {
                    loop {
                        let action = self.prompt_copilot("brainstorm", &brainstorm_output)?;
                        match action {
                            Some(CopilotAction::Reject { feedback }) => {
                                eprintln!("  Re-dispatching brainstorm with feedback...");
                                let amended_prompt = format!(
                                    "{}\n\n## User Feedback\n{}",
                                    brainstorm_prompt, feedback
                                );
                                brainstorm_results = dispatch_parallel(
                                    &self.config.brainstorm_models,
                                    &amended_prompt,
                                    Some(&system_prompt),
                                    120,
                                    &self.provider_factory,
                                )
                                .await;
                                let (new_output, new_warnings) =
                                    collect_outputs(&brainstorm_results);
                                self.cost.add_estimate(new_output.len(), false);
                                for w in &new_warnings {
                                    eprintln!("  Warning: {}", w);
                                }
                                brainstorm_output = new_output;
                            }
                            Some(CopilotAction::Edit { content }) => {
                                eprintln!("  User provided edited content");
                                brainstorm_output = content;
                                break;
                            }
                            _ => break, // Accept or None
                        }
                    }
                }

                // --- Stage 2: MERGE1 ---
                let merge_vars = self.build_vars(&[
                    ("task", &self.task_description),
                    ("outputs", &brainstorm_output),
                ]);
                let merge_prompt = self.render_prompt("merge", &merge_vars)?;
                self.cost.add_estimate(merge_prompt.len(), true);

                self.state.lock().unwrap().advance(Stage::Merge1);
                let tagged = tag_with_round(&merge_prompt, current_round);
                let merge1_with_system = format!("{}\n\n{}", system_prompt, tagged);
                merged_draft = agent.turn(&merge1_with_system).await?;
                self.cost.add_estimate(merged_draft.len(), false);
                eprintln!("  Merge1 complete ({} chars)", merged_draft.len());

                // Copilot: pause for merge review with feedback loop
                if self.config.mode == Mode::Copilot {
                    loop {
                        let action = self.prompt_copilot("merge", &merged_draft)?;
                        match action {
                            Some(CopilotAction::Reject { feedback }) => {
                                eprintln!("  Re-running merge with feedback...");
                                let amended_merge = format!(
                                    "{}\n\n## User Feedback\n{}",
                                    merge_prompt, feedback
                                );
                                let tagged =
                                    tag_with_round(&amended_merge, current_round);
                                let merge_with_system =
                                    format!("{}\n\n{}", system_prompt, tagged);
                                merged_draft =
                                    agent.turn(&merge_with_system).await?;
                                self.cost
                                    .add_estimate(merged_draft.len(), false);
                            }
                            Some(CopilotAction::Edit { content }) => {
                                eprintln!("  User provided edited content");
                                merged_draft = content;
                                break;
                            }
                            _ => break, // Accept or None
                        }
                    }
                }
            }

            // --- Stage 3: REVIEW (parallel dispatch) ---
            let review_vars = self.build_vars(&[
                ("task", &self.task_description),
                ("draft", &merged_draft),
                ("dimensions", &dimensions_str),
            ]);
            let review_prompt = self.render_prompt("review", &review_vars)?;
            self.cost
                .add_estimate(review_prompt.len() * self.config.review_models.len(), true);

            self.state.lock().unwrap().advance(Stage::Review);

            eprintln!(
                "  Dispatching review to {} models in parallel...",
                self.config.review_models.len()
            );
            let review_results = dispatch_parallel(
                &self.config.review_models,
                &review_prompt,
                Some(&system_prompt),
                120,
                &self.provider_factory,
            )
            .await;
            let (review_output, review_warnings) = collect_outputs(&review_results);
            self.cost.add_estimate(review_output.len(), false);
            for w in &review_warnings {
                eprintln!("  Warning: {}", w);
            }
            eprintln!(
                "  Review complete: {} models responded ({} chars)",
                review_results.len() - review_warnings.len(),
                review_output.len()
            );

            // Track objections per section for convergence guard
            self.track_objections(&review_results, &preset.sections, current_round);

            // Dispatch semantic diff prompts to a mid-tier model for each section.
            // Results feed into evaluate_convergence_with_votes as the delta signal.
            let semantic_diff_results: Vec<(String, String)> = if !last_draft.is_empty() {
                let mut join_set = tokio::task::JoinSet::new();
                for section in &preset.sections {
                    let diff_prompt = crate::tools::convergence::build_semantic_diff_prompt(
                        section, &merged_draft, &last_draft,
                        current_round, current_round.saturating_sub(1),
                    );
                    self.cost.add_estimate(diff_prompt.len(), true);
                    let model = self.config.review_models[0].clone();
                    let factory = self.provider_factory.clone();
                    let section_name = section.clone();
                    join_set.spawn(async move {
                        let results = dispatch_parallel(&[model], &diff_prompt, None, 60, &factory).await;
                        results.into_iter().next()
                            .and_then(|r| r.ok())
                            .map(|(_, response)| (section_name, response))
                    });
                }
                let mut results = Vec::new();
                while let Some(Ok(Some(result))) = join_set.join_next().await {
                    self.cost.add_estimate(result.1.len(), false);
                    results.push(result);
                }
                eprintln!("  Semantic diff: {}/{} sections evaluated", results.len(), preset.sections.len());
                results
            } else {
                vec![]
            };

            let review_result = review_output;

            if self.config.mode == Mode::Copilot {
                self.prompt_copilot("review", &review_result)?;
            }

            // --- Stage 4: MERGE2 (incorporate critiques) ---
            let merge2_vars = self.build_vars(&[
                ("task", &self.task_description),
                ("outputs", &merged_draft),
                ("critiques", &review_result),
            ]);
            let merge2_prompt = self.render_prompt("merge", &merge2_vars)?;
            self.cost.add_estimate(merge2_prompt.len(), true);

            self.state.lock().unwrap().advance(Stage::Merge2);
            let tagged = tag_with_round(&merge2_prompt, current_round);
            let merge2_with_system = format!("{}\n\n{}", system_prompt, tagged);
            merged_draft = agent.turn(&merge2_with_system).await?;
            self.cost.add_estimate(merged_draft.len(), false);
            eprintln!("  Merge2 complete ({} chars)", merged_draft.len());

            // --- Stage 5: QUALITY CHECK (retry up to 3 attempts, using review model) ---
            let quality_vars = self.build_vars(&[
                ("draft", &merged_draft),
                ("critiques", &review_result),
            ]);
            let quality_prompt = self.render_prompt("merge_quality", &quality_vars)?;
            self.cost.add_estimate(quality_prompt.len(), true);

            self.state.lock().unwrap().advance(Stage::QualityCheck);
            let tagged = tag_with_round(&quality_prompt, current_round);
            #[allow(unused_assignments)]
            let mut quality_passed = false;
            let max_quality_attempts = 3;
            for attempt in 1..=max_quality_attempts {
                // Use a review model (mid-tier) instead of the merge LLM for independence
                let quality_results = dispatch_parallel(
                    &[self.config.review_models[0].clone()],
                    &tagged,
                    Some(&system_prompt),
                    120,
                    &self.provider_factory,
                )
                .await;
                let quality_result = quality_results
                    .into_iter()
                    .find_map(|r| r.ok().map(|(_, text)| text))
                    .unwrap_or_else(|| {
                        "FAIL: quality check model unavailable".to_string()
                    });
                self.cost.add_estimate(quality_result.len(), false);
                quality_passed = quality_result.to_lowercase().contains("pass");
                if quality_passed {
                    eprintln!("  Quality check: PASS");
                    break;
                }
                if attempt < max_quality_attempts {
                    eprintln!(
                        "  Quality check: FAIL (attempt {}/{}), retrying...",
                        attempt, max_quality_attempts
                    );
                } else if should_proceed_despite_failure(attempt, max_quality_attempts) {
                    eprintln!(
                        "  Quality check: FAIL after {} attempts, proceeding with warning",
                        max_quality_attempts
                    );
                }
            }

            // --- Stage 6: EVALUATE (convergence) ---
            self.state.lock().unwrap().advance(Stage::Evaluate);

            // Use LLM-based semantic diff when we have a previous draft
            let convergence = if !last_draft.is_empty() {
                self.evaluate_convergence_with_votes(
                    &merged_draft,
                    &last_draft,
                    &preset.sections,
                    current_round,
                    &review_results,
                    &semantic_diff_results,
                )
            } else {
                ConvergenceResult {
                    sections: preset
                        .sections
                        .iter()
                        .map(|s| SectionConvergence {
                            name: s.clone(),
                            converged: false,
                            trend: Trend::Same,
                            agreement: "0/0".into(),
                            irreconcilable: false,
                        })
                        .collect(),
                    all_converged: false,
                    should_loop: true,
                }
            };

            // Display dashboard with real cost tracking
            let dashboard_output = dashboard::render_dashboard(
                current_round,
                self.config.max_rounds,
                &self.config.task_type,
                &convergence.sections,
                self.cost.estimate_usd(),
                self.cost.tokens_in,
                self.cost.tokens_out,
                &[],
            );
            eprintln!("{}", dashboard_output);

            // Record round state
            let round_state = RoundState {
                round_num: current_round,
                stage: Stage::Evaluate,
                sections: convergence
                    .sections
                    .iter()
                    .map(|s| SectionState {
                        name: s.name.clone(),
                        content: String::new(),
                        converged: s.converged,
                        trend: s.trend,
                        agreement: vec![],
                        objection_history: vec![],
                    })
                    .collect(),
                cost: RoundCost {
                    tokens_in: self.cost.tokens_in,
                    tokens_out: self.cost.tokens_out,
                    usd: self.cost.estimate_usd(),
                },
                timestamp: chrono::Utc::now(),
            };
            self.observer
                .on_round_complete(current_round, round_state.cost.usd);

            // Persist round state to memory
            let round_json = serde_json::to_string(&round_state)?;
            self.persist(&Self::session_key(&session_id, &format!("round:{}", current_round)), &round_json, &session_id).await;
            // Persist current draft
            self.persist(&Self::session_key(&session_id, &format!("draft:{}", current_round)), &merged_draft, &session_id).await;

            self.rounds.push(round_state);

            // Check convergence
            if convergence.all_converged {
                eprintln!("All sections converged!");
                break;
            }

            if !self.config.do_loop {
                eprintln!("--no-loop: stopping after single pass");
                break;
            }

            // Cruise mode: checkpoint every N rounds
            if self.config.mode == Mode::Cruise
                && current_round.is_multiple_of(checkpoint_interval)
                && current_round < self.config.max_rounds
            {
                eprintln!("\n--- Cruise checkpoint (round {}) ---", current_round);
                eprintln!("  Cost so far: ${:.2}", self.cost.estimate_usd());
                eprintln!(
                    "  Converged: {}/{}",
                    convergence.sections.iter().filter(|s| s.converged).count(),
                    convergence.sections.len()
                );
                eprintln!("  Remaining rounds: {}", self.config.max_rounds - current_round);
                eprintln!("[c]ontinue  [q]uit and output best draft");
                if let Some(input) = self.read_user_input()
                    && input.trim().starts_with('q')
                {
                    eprintln!("  User quit at checkpoint");
                    break;
                }
            }

            last_draft = std::mem::take(&mut merged_draft);

            // Reset pipeline state for next round
            {
                let mut state = self.state.lock().unwrap();
                *state = PipelineState::new();
                state.advance(Stage::Brainstorm);
                state.advance(Stage::Merge1);
            }

            current_round += 1;
        }

        // --- FINALIZE ---
        let finalize_vars = self.build_vars(&[
            ("task", &self.task_description),
            ("draft", &merged_draft),
        ]);
        let finalize_prompt = self.render_prompt("finalize", &finalize_vars)?;
        self.cost.add_estimate(finalize_prompt.len(), true);
        let finalize_with_system = format!("{}\n\n{}", system_prompt, finalize_prompt);
        let final_result = agent.turn(&finalize_with_system).await?;
        self.cost.add_estimate(final_result.len(), false);

        let converged = self
            .rounds
            .last()
            .map(|r| r.sections.iter().all(|s| s.converged))
            .unwrap_or(false);
        self.observer
            .on_session_complete(current_round, self.cost.estimate_usd(), converged);

        // Persist final output
        self.persist(&Self::session_key(&session_id, "final_output"), &merged_draft, &session_id).await;

        eprintln!(
            "\nTotal cost estimate: ${:.2} ({:.0}K in / {:.0}K out tokens)",
            self.cost.estimate_usd(),
            self.cost.tokens_in as f64 / 1000.0,
            self.cost.tokens_out as f64 / 1000.0
        );

        if final_result.to_lowercase().contains("clean") {
            eprintln!("Final review: CLEAN");
            Ok(merged_draft)
        } else {
            eprintln!("Final review found issues:\n{}", final_result);
            Ok(merged_draft)
        }
    }

    fn session_key(session_id: &str, suffix: &str) -> String {
        format!("brainstorm:{}:{}", session_id, suffix)
    }

    async fn persist(&self, key: &str, value: &str, session_id: &str) {
        if let Err(e) = self.memory.store(
            key, value,
            zeroclaw::memory::MemoryCategory::Custom("brainstorm".into()),
            Some(session_id),
        ).await {
            eprintln!("Warning: failed to persist {}: {}", key, e);
        }
    }

    pub fn rounds_completed(&self) -> u32 {
        self.rounds.len() as u32
    }

    pub fn converged(&self) -> bool {
        self.rounds
            .last()
            .map(|r| r.sections.iter().all(|s| s.converged))
            .unwrap_or(false)
    }

    pub fn cost_usd(&self) -> f64 {
        self.cost.estimate_usd()
    }

    /// Prompt user for copilot action. Returns None in non-interactive modes.
    fn prompt_copilot(
        &self,
        stage: &str,
        output: &str,
    ) -> anyhow::Result<Option<CopilotAction>> {
        if self.config.mode != Mode::Copilot {
            return Ok(None);
        }

        let preview = if output.len() > 500 {
            format!("{}...\n[{} more chars]", &output[..500], output.len() - 500)
        } else {
            output.to_string()
        };
        eprintln!("\n--- {} output preview ---\n{}", stage, preview);
        eprintln!("[a]ccept  [r]eject+feedback  [e]dit");
        eprint!("> ");

        match self.read_user_input() {
            Some(input) => {
                let action = parse_copilot_input(&input);
                let feedback = format_copilot_feedback(&action, stage, "pipeline");
                eprintln!("  {}", feedback);
                Ok(Some(action))
            }
            None => Ok(Some(CopilotAction::Accept)),
        }
    }

    /// Read a line from stdin. Returns None if stdin is not a terminal.
    fn read_user_input(&self) -> Option<String> {
        use std::io::BufRead;
        // Only prompt if stdin is a terminal (not piped)
        if atty::is(atty::Stream::Stdin) {
            let mut input = String::new();
            std::io::stdin().lock().read_line(&mut input).ok()?;
            Some(input.trim().to_string())
        } else {
            None
        }
    }

    /// Track review objections per section for convergence guard.
    fn track_objections(
        &mut self,
        results: &[Result<(String, String), String>],
        sections: &[String],
        _round: u32,
    ) {
        for (model_id, output) in results.iter().flatten() {
            for section_name in sections {
                // Simple heuristic: if section name appears near "worse" or critique text
                let lower = output.to_lowercase();
                let section_lower = section_name.to_lowercase();
                if lower.contains(&section_lower) && lower.contains("worse") {
                    let history = self
                        .section_histories
                        .entry(section_name.clone())
                        .or_default();
                    // Extract the critique text near the section mention
                    if let Some(pos) = lower.find(&section_lower) {
                        let snippet_end = (pos + 200).min(output.len());
                        let snippet = &output[pos..snippet_end];
                        history
                            .objections
                            .push((model_id.clone(), snippet.to_string()));

                        // Check for irreconcilable: same model, similar objection
                        if history.objections.len() >= 2 {
                            let last = &history.objections[history.objections.len() - 1];
                            let prev = &history.objections[history.objections.len() - 2];
                            if last.0 == prev.0 {
                                // Same model repeated objection
                                self.observer.on_convergence_guard(
                                    section_name,
                                    &last.0,
                                    &last.1,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Evaluate convergence using review votes and LLM semantic diff results.
    ///
    /// For each section, uses the LLM semantic diff response if available,
    /// then falls back to review text signals, then to the length-ratio heuristic.
    fn evaluate_convergence_with_votes(
        &self,
        current: &str,
        previous: &str,
        section_names: &[String],
        _round: u32,
        review_results: &[Result<(String, String), String>],
        semantic_diffs: &[(String, String)],
    ) -> ConvergenceResult {
        // Length-ratio as final fallback
        let len_ratio = (current.len() as f64 - previous.len() as f64).abs()
            / previous.len().max(1) as f64;
        let fallback_delta_str = if len_ratio < 0.05 {
            "none"
        } else if len_ratio < 0.2 {
            "small"
        } else {
            "large"
        };

        let sections: Vec<SectionConvergence> = section_names
            .iter()
            .map(|name| {
                // For each review result, extract the score for THIS section only
                let votes: Vec<LlmVote> = review_results
                    .iter()
                    .filter_map(|r| r.as_ref().ok())
                    .filter_map(|(model_id, output)| {
                        let lower = output.to_lowercase();
                        let name_lower = name.to_lowercase();

                        // Find the section mention and extract nearby text (next 200 chars)
                        let section_pos = lower.find(&name_lower)?;
                        let section_text =
                            &lower[section_pos..lower.len().min(section_pos + 200)];

                        let score = if section_text.contains("better") {
                            RelativeScore::Better
                        } else if section_text.contains("worse") {
                            RelativeScore::Worse
                        } else {
                            RelativeScore::Same
                        };
                        Some(LlmVote {
                            model_id: model_id.clone(),
                            score,
                            comment: None,
                        })
                    })
                    .collect();

                // Per-section delta: use LLM semantic diff if available, else fallback
                let section_delta_str = semantic_diffs
                    .iter()
                    .find(|(s, _)| s == name)
                    .map(|(_, response)| {
                        let lower = response.to_lowercase();
                        if lower.contains("none") || lower.contains("no meaningful") {
                            "none"
                        } else if lower.contains("small") || lower.contains("minor") {
                            "small"
                        } else if lower.contains("large") || lower.contains("significant") {
                            "large"
                        } else {
                            fallback_delta_str
                        }
                    })
                    .unwrap_or(fallback_delta_str);
                let delta = parse_semantic_delta(section_delta_str);

                // Get objection history for this section
                let objection_history: Vec<String> = self
                    .section_histories
                    .get(name)
                    .map(|h| h.objections.iter().map(|(_, text)| text.clone()).collect())
                    .unwrap_or_default();

                evaluate_section(name, &votes, delta, &objection_history)
            })
            .collect();

        build_convergence_result(&sections)
    }

    fn build_vars(&self, pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn render_prompt(
        &self,
        name: &str,
        vars: &HashMap<String, String>,
    ) -> anyhow::Result<String> {
        let tmpl = template::load_prompt(name)?;
        Ok(template::interpolate(&tmpl, vars))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> SessionConfig {
        SessionConfig {
            task_type: "software".into(),
            mode: Mode::Autopilot,
            do_loop: true,
            brainstorm_models: vec![],
            review_models: vec![],
            max_rounds: 5,
            merge_llm: ModelRef {
                provider: "anthropic".into(),
                model: "claude-opus-4-6".into(),
            },
        }
    }

    fn test_pipeline(task: &str) -> Pipeline {
        let memory = Arc::new(zeroclaw::memory::NoneMemory);
        let observer = Arc::new(BrainstormObserver::new());
        let factory = crate::dry_run::dry_run_provider_factory();
        Pipeline::new(test_config(), observer, memory, task.into(), factory, String::new())
    }

    #[test]
    fn pipeline_build_vars() {
        let pipeline = test_pipeline("test task");
        let vars = pipeline.build_vars(&[("task", "Design a cache"), ("sections", "A, B")]);
        assert_eq!(vars["task"], "Design a cache");
        assert_eq!(vars["sections"], "A, B");
    }

    #[test]
    fn pipeline_render_prompt() {
        let pipeline = test_pipeline("test task");
        let mut vars = HashMap::new();
        vars.insert("task".into(), "Design a cache".into());
        vars.insert("task_type".into(), "software".into());
        vars.insert("sections".into(), "Architecture, API".into());

        let rendered = pipeline.render_prompt("brainstorm", &vars).unwrap();
        assert!(rendered.contains("Design a cache"));
        assert!(rendered.contains("Architecture, API"));
    }

    #[test]
    fn pipeline_evaluate_convergence_no_change() {
        let pipeline = test_pipeline("test");
        let current = "This is the current draft content for testing.";
        let previous = "This is the current draft content for testing.";
        let sections = vec!["Architecture".into(), "API".into()];

        let result =
            pipeline.evaluate_convergence_with_votes(current, previous, &sections, 2, &[], &[]);
        assert!(result.all_converged);
    }

    #[test]
    fn cost_tracker_estimates() {
        let mut cost = CostTracker::default();
        cost.add_estimate(4000, true); // ~1000 input tokens
        cost.add_estimate(4000, false); // ~1000 output tokens
        assert_eq!(cost.tokens_in, 1000);
        assert_eq!(cost.tokens_out, 1000);
        assert!(cost.estimate_usd() > 0.0);
    }

    #[test]
    fn section_history_tracks_objections() {
        let mut pipeline = test_pipeline("test");
        let results = vec![
            Ok(("openai/gpt-5.4".to_string(), "### Architecture\n**Score**: worse\nMissing error handling".to_string())),
        ];
        let sections = vec!["Architecture".into()];
        pipeline.track_objections(&results, &sections, 1);

        assert!(pipeline.section_histories.contains_key("Architecture"));
        assert_eq!(pipeline.section_histories["Architecture"].objections.len(), 1);
    }
}
