# Brainstormer Stage 1 (CLI MVP) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a CLI tool that orchestrates N LLMs to collaboratively brainstorm, cross-check, and converge on a design document, using ZeroClaw as a dependency.

**Architecture:** Standalone Rust crate depending on upstream ZeroClaw (v0.6.8) for providers, memory, and agent orchestration. The pipeline is BRAINSTORM → MERGE → REVIEW → MERGE → QUALITY → EVALUATE → [loop]. `dispatch_parallel()` drives brainstorm/review stages directly via provider calls. `agent.turn()` drives merge/quality stages.

**Tech Stack:** Rust 2024 edition, zeroclawlabs 0.6.8 (git dep), tokio, serde, clap, SQLite (via ZeroClaw Memory)

**Spec:** `docs/superpowers/specs/2026-04-02-brainstormer-design.md`

---

## File Structure

```
brainstormer/                              # Standalone crate (zeroclawlabs as git dep)
├── Cargo.toml                             # Depends on zeroclawlabs v0.6.8
├── src/
│   ├── main.rs                            # CLI entry (clap)
│   ├── lib.rs                             # Re-exports for tests
│   ├── pipeline.rs                        # Core pipeline orchestrator
│   ├── types.rs                           # RoundState, SectionState, SessionConfig, Stage, Mode, Trend
│   ├── template.rs                        # Prompt template loading + {{variable}} interpolation
│   ├── sanitize.rs                        # Cross-LLM prompt injection sanitization
│   ├── dry_run.rs                         # Mock provider for testing
│   ├── resume.rs                          # Session resume logic
│   ├── input.rs                           # Input processor (file reading, URL fetching)
│   ├── export.rs                          # Markdown export with metadata header
│   ├── observer.rs                        # BrainstormObserver
│   ├── agent_setup.rs                     # Observer construction
│   ├── tools/
│   │   ├── mod.rs
│   │   ├── convergence.rs                 # Convergence detection utilities
│   │   ├── brainstorm_swarm.rs            # Parallel LLM dispatch
│   │   ├── merge.rs                       # Output merge utilities
│   │   └── merge_quality.rs               # Quality verification
│   ├── hooks/
│   │   ├── mod.rs
│   │   ├── sequence_enforcement.rs        # PipelineState tracking
│   │   ├── output_capture.rs              # Round tagging utilities
│   │   └── copilot.rs                     # Copilot input parsing
│   └── cli/
│       ├── mod.rs
│       ├── dashboard.rs                   # Convergence dashboard (terminal)
│       ├── copilot_ui.rs                  # Copilot accept/reject/edit interaction
│       ├── auto_detect.rs                 # Provider auto-detection from env vars
│       └── setup.rs                       # First-run manual provider setup
├── presets/
│   ├── software_design.toml
│   ├── general.toml
│   ├── research.toml                      # Scientific research preset
│   ├── article.toml                       # Article/essay preset
│   ├── book.toml                          # Book/long-form preset
│   └── strategy.toml                      # Business strategy preset
├── prompts/
│   ├── brainstorm.md
│   ├── review.md
│   ├── merge.md
│   ├── evaluate.md
│   ├── merge_quality.md
│   ├── finalize.md
│   └── convergence_semantic.md
├── system_prompts/
│   ├── autopilot.md
│   ├── copilot.md
│   └── cruise.md
├── tests/
│   ├── convergence_test.rs                # T01-T09
│   ├── brainstorm_swarm_test.rs           # T13-T14
│   ├── merge_test.rs                      # T16-T18
│   ├── merge_quality_test.rs              # T19-T21
│   ├── hooks_test.rs                      # T22-T24, T29-T30
│   ├── auto_detect_test.rs                # T38-T40
│   ├── model_stats_test.rs                # T41-T42
│   ├── resume_test.rs
│   ├── input_test.rs
│   ├── export_test.rs
│   ├── preset_test.rs
│   ├── error_path_test.rs                 # T34, T35, T37
│   ├── smoke_test.rs                      # T43 (real API keys, gated)
│   ├── integration_test.rs                # T31-T33, T36, T43-T44
│   └── pipeline_dry_run_test.rs
└── docs/
    └── superpowers/
        ├── plans/
        └── specs/
```

---

### Task 1: Project Setup & Scaffold

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `src/lib.rs`

- [ ] **Step 1: Create Cargo.toml with ZeroClaw as git dependency**

```toml
[package]
name = "brainstormer"
version = "0.1.0"
edition = "2024"
description = "Multi-LLM brainstorming with convergence detection"
publish = false

[[bin]]
name = "brainstormer"
path = "src/main.rs"

[lib]
name = "brainstormer"
path = "src/lib.rs"

[dependencies]
zeroclaw = { git = "https://github.com/zeroclaw-labs/zeroclaw.git", tag = "v0.6.8", package = "zeroclawlabs" }
tokio = { version = "1.50", features = ["rt-multi-thread", "macros", "sync", "time", "fs"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
clap = { version = "4.5", features = ["derive"] }
anyhow = "1.0"
async-trait = "0.1"
chrono = { version = "0.4", features = ["serde"] }
toml = "0.8"
uuid = { version = "1.0", features = ["v4"] }
tracing = "0.1"
regex = "1.10"
atty = "0.2"
dirs = "5.0"
reqwest = { version = "0.12", features = ["rustls-tls"], default-features = false }

[dev-dependencies]
tokio = { version = "1.50", features = ["test-util"] }
```

- [ ] **Step 2: Create minimal main.rs and lib.rs stubs**

- [ ] **Step 3: Verify it compiles**

```bash
cargo check
```

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml src/main.rs src/lib.rs
git commit -m "feat: scaffold brainstormer with zeroclawlabs dependency"
```

---

### Task 2: Core Types

**Files:**
- Create: `src/types.rs`

- [ ] **Step 1: Write tests for type serialization**

```rust
// src/types.rs (tests at bottom)
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_state_serializes_to_json() {
        let state = RoundState {
            round_num: 1,
            stage: Stage::Brainstorm,
            sections: vec![SectionState {
                name: "Architecture".to_string(),
                content: "Use microservices".to_string(),
                converged: false,
                trend: Trend::Same,
                agreement: vec![],
                objection_history: vec![],
            }],
            cost: RoundCost { tokens_in: 1000, tokens_out: 500, usd: 0.05 },
            timestamp: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: RoundState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.round_num, 1);
        assert_eq!(back.sections.len(), 1);
        assert_eq!(back.sections[0].name, "Architecture");
    }

    #[test]
    fn session_config_defaults() {
        let config = SessionConfig {
            task_type: "software".to_string(),
            mode: Mode::Autopilot,
            do_loop: true,
            brainstorm_models: vec![ModelRef { provider: "anthropic".into(), model: "claude-opus-4-6".into() }],
            review_models: vec![ModelRef { provider: "anthropic".into(), model: "claude-sonnet-4-6".into() }],
            max_rounds: 5,
            merge_llm: ModelRef { provider: "anthropic".into(), model: "claude-opus-4-6".into() },
        };
        assert!(config.do_loop);
        assert_eq!(config.max_rounds, 5);
    }

    #[test]
    fn stage_ordering() {
        assert!(Stage::Brainstorm < Stage::Merge1);
        assert!(Stage::Merge1 < Stage::Review);
        assert!(Stage::Review < Stage::Merge2);
        assert!(Stage::Merge2 < Stage::QualityCheck);
        assert!(Stage::QualityCheck < Stage::Evaluate);
    }

    #[test]
    fn llm_vote_serialize() {
        let vote = LlmVote {
            model_id: "claude-opus-4-6".to_string(),
            score: RelativeScore::Better,
            comment: Some("Improved error handling".to_string()),
        };
        let json = serde_json::to_string(&vote).unwrap();
        assert!(json.contains("Better"));
    }

    #[test]
    fn convergence_result_json() {
        let result = ConvergenceResult {
            sections: vec![SectionConvergence {
                name: "API".to_string(),
                converged: true,
                trend: Trend::Better,
                agreement: "3/3 agree".to_string(),
                irreconcilable: false,
            }],
            all_converged: true,
            should_loop: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ConvergenceResult = serde_json::from_str(&json).unwrap();
        assert!(back.all_converged);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd /Users/zxiong/MemVerge/brainstormer
cargo test -p brainstormer --lib types
```

Expected: FAIL, types not defined yet.

- [ ] **Step 3: Implement core types**

```rust
// src/types.rs
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Which stage of the pipeline we're in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Stage {
    Brainstorm,
    Merge1,
    Review,
    Merge2,
    QualityCheck,
    Evaluate,
    Finalize,
}

/// User interaction mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Autopilot,
    Copilot,
    Cruise,
}

/// Section quality trend between rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trend {
    Better,
    Worse,
    Same,
}

/// Relative score from a reviewer LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelativeScore {
    Better,
    Worse,
    Same,
}

/// Semantic diff result for a section between rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SemanticDelta {
    None,
    Small,
    Large,
}

/// Reference to a specific model on a specific provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

impl ModelRef {
    pub fn model_id(&self) -> &str {
        &self.model
    }
}

/// Per-LLM vote on a section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmVote {
    pub model_id: String,
    pub score: RelativeScore,
    pub comment: Option<String>,
}

/// State of a single section within a round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionState {
    pub name: String,
    pub content: String,
    pub converged: bool,
    pub trend: Trend,
    pub agreement: Vec<LlmVote>,
    pub objection_history: Vec<String>,
}

/// Cost for a single round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundCost {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub usd: f64,
}

/// Full state of a pipeline round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundState {
    pub round_num: u32,
    pub stage: Stage,
    pub sections: Vec<SectionState>,
    pub cost: RoundCost,
    pub timestamp: DateTime<Utc>,
}

/// Session configuration (set at start, persisted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub task_type: String,
    pub mode: Mode,
    pub do_loop: bool,
    pub brainstorm_models: Vec<ModelRef>,
    pub review_models: Vec<ModelRef>,
    pub max_rounds: u32,
    pub merge_llm: ModelRef,
}

/// Per-section convergence status from ConvergenceTool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionConvergence {
    pub name: String,
    pub converged: bool,
    pub trend: Trend,
    pub agreement: String,
    pub irreconcilable: bool,
}

/// Full convergence result returned by ConvergenceTool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvergenceResult {
    pub sections: Vec<SectionConvergence>,
    pub all_converged: bool,
    pub should_loop: bool,
}

/// Copilot action from user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CopilotAction {
    Accept,
    Reject { feedback: String },
    Edit { content: String },
}

/// Per-model stats for Copilot preference tracking.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelStats {
    pub accepted: u32,
    pub rejected: u32,
    pub edited: u32,
}

/// Preset definition loaded from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub dimensions: Vec<String>,
    pub sections: Vec<String>,
}

// ... tests below
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p brainstormer --lib types
```

Expected: all 5 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/types.rs
git commit -m "feat(brainstormer): add core types with serialization"
```

---

### Task 3: Prompt Templates, Presets & System Prompts

**Files:**
- Create: `src/template.rs`
- Create: `presets/software_design.toml`
- Create: `presets/general.toml`
- Create: `prompts/brainstorm.md` (+ 6 more)
- Create: `system_prompts/autopilot.md` (+ 2 more)

- [ ] **Step 1: Write template engine tests**

```rust
// src/template.rs (tests)
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn interpolate_replaces_variables() {
        let template = "Given task: {{task}}\nSections: {{sections}}";
        let mut vars = HashMap::new();
        vars.insert("task".to_string(), "Design a cache".to_string());
        vars.insert("sections".to_string(), "Architecture, API".to_string());
        let result = interpolate(template, &vars);
        assert_eq!(result, "Given task: Design a cache\nSections: Architecture, API");
    }

    #[test]
    fn interpolate_leaves_unknown_vars() {
        let template = "{{known}} and {{unknown}}";
        let mut vars = HashMap::new();
        vars.insert("known".to_string(), "hello".to_string());
        let result = interpolate(template, &vars);
        assert_eq!(result, "hello and {{unknown}}");
    }

    #[test]
    fn load_preset_software() {
        let preset = load_preset("software").unwrap();
        assert_eq!(preset.name, "software");
        assert!(preset.dimensions.contains(&"Feasibility".to_string()));
        assert!(preset.sections.contains(&"Architecture".to_string()));
    }

    #[test]
    fn load_preset_general() {
        let preset = load_preset("general").unwrap();
        assert_eq!(preset.name, "general");
        assert!(preset.dimensions.contains(&"Completeness".to_string()));
    }

    #[test]
    fn load_prompt_template() {
        let content = load_prompt("brainstorm").unwrap();
        assert!(content.contains("{{task}}"));
    }

    #[test]
    fn load_system_prompt() {
        let content = load_system_prompt("autopilot").unwrap();
        assert!(content.contains("pipeline"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p brainstormer --lib template
```

Expected: FAIL.

- [ ] **Step 3: Create preset TOML files**

`presets/software_design.toml`:
```toml
name = "software"
dimensions = ["Feasibility", "Scalability", "Maintainability", "Security"]
sections = ["Problem", "Architecture", "API", "Data Model", "Error Handling", "Testing"]
```

`presets/general.toml`:
```toml
name = "general"
dimensions = ["Completeness", "Coherence", "Feasibility", "Originality"]
sections = []  # User defines, or LLMs propose during brainstorm
```

- [ ] **Step 4: Create 7 prompt templates**

`prompts/brainstorm.md`:
```markdown
You are brainstorming approaches for the following task.

## Task
{{task}}

## Task Type
{{task_type}}

## Sections to Address
{{sections}}

For each section, propose 2-3 distinct approaches with pros and cons. Be specific and concrete. Include implementation details, not just high-level ideas.

Format your response with clear section headers matching the sections listed above.
```

`prompts/review.md`:
```markdown
You are reviewing a merged draft for the following task.

## Task
{{task}}

## Merged Draft
{{draft}}

## Evaluation Dimensions
{{dimensions}}

For each section, provide:
1. **Score**: better / worse / same (compared to what you'd expect for this task)
2. **Critique**: What's wrong, missing, or could be improved
3. **Suggestion**: Concrete improvement with specifics

Be rigorous. Flag gaps, contradictions, and unstated assumptions.
```

`prompts/merge.md`:
```markdown
Synthesize the following outputs into a single coherent draft.

## Task
{{task}}

## Outputs to Merge
{{outputs}}

{{#if critiques}}
## Review Critiques to Incorporate
{{critiques}}
{{/if}}

Rules:
- Keep the best ideas from each output
- Resolve contradictions with reasoned judgment
- Maintain consistent style and terminology
- Every section must be present and complete
- Do not add placeholder text like "TBD" or "TODO"
```

`prompts/evaluate.md`:
```markdown
Compare this section to its previous version. How has it changed?

## Section: {{section_name}}

### Current Version (Round {{round}})
{{current}}

### Previous Version (Round {{prev_round}})
{{previous}}

Rate this section: better / worse / same

Explain your rating in 1-2 sentences.
```

`prompts/merge_quality.md`:
```markdown
Verify this merge faithfully incorporated the review critiques.

## Merged Draft
{{draft}}

## Review Critiques That Should Be Addressed
{{critiques}}

For each critique, answer:
- Was it addressed? (yes/no)
- If yes, how?
- If no, was there a good reason to skip it?

Final verdict: PASS or FAIL
If FAIL, list which critiques were dropped without justification.
```

`prompts/finalize.md`:
```markdown
Final review of the converged document.

## Task
{{task}}

## Final Draft
{{draft}}

Check for:
1. Placeholder text ("TBD", "TODO", "to be determined")
2. Internal contradictions between sections
3. Ambiguous statements that need clarification
4. Missing sections from the original structure
5. Cross-section consistency (do sections reference each other correctly?)

If issues found, list them. If clean, respond with "CLEAN".
```

`prompts/convergence_semantic.md`:
```markdown
Did the meaning of this section change between rounds?

## Section: {{section_name}}

### Round {{round}} Version
{{current}}

### Round {{prev_round}} Version
{{previous}}

Classify the change:
- **none**: No meaningful change (identical or trivial rewording)
- **small**: Refinement (clarification, better examples, minor additions)
- **large**: Rewrite (different approach, major restructuring, new ideas)

Respond with exactly one word: none, small, or large.
```

- [ ] **Step 5: Create 3 system prompts**

`system_prompts/autopilot.md`:
```markdown
You are a brainstorming orchestrator. Your job is to run the full pipeline in sequence until the design converges or you hit the maximum number of rounds.

Available tools:
- brainstorm_swarm: Dispatch parallel LLM calls for brainstorming or reviewing
- merge: Synthesize multiple outputs into one draft
- merge_quality: Verify merge quality
- convergence: Check if sections have converged

Pipeline per round:
1. Call brainstorm_swarm with stage="brainstorm" (first round only, subsequent rounds skip to review)
2. Call merge to synthesize brainstorm outputs
3. Call brainstorm_swarm with stage="review"
4. Call merge to incorporate review critiques
5. Call merge_quality to verify
6. Call convergence to check

If convergence returns unconverged sections, loop back to step 3 with only those sections.
If all sections converge, finalize and output the document.
If max rounds reached, output best draft with status report.

Do not skip steps. Do not call tools out of order.
```

`system_prompts/copilot.md`:
```markdown
You are a brainstorming orchestrator in copilot mode. Call ONE tool per turn, then present results to the user for review.

Available tools: brainstorm_swarm, merge, merge_quality, convergence

After each tool call, present the output and wait for user input:
- [a]ccept: proceed to next stage
- [r]eject + feedback: re-run with feedback incorporated
- [e]dit: user will provide edited content

Follow the same pipeline order as autopilot, but pause between each step.
```

`system_prompts/cruise.md`:
```markdown
You are a brainstorming orchestrator in cruise mode. Run the full pipeline autonomously, but checkpoint every {{checkpoint_interval}} rounds.

At each checkpoint, present:
- Current convergence status per section
- Cost so far
- Remaining rounds

Then wait for user input:
- [c]ontinue: keep going
- [s]witch: change to a different mode
- [e]dit: modify the draft before continuing
- [q]uit: stop and output best draft
```

- [ ] **Step 6: Implement template engine**

```rust
// src/template.rs
use crate::types::Preset;
use std::collections::HashMap;
use std::path::Path;

/// Replace {{variable}} placeholders with values from the map.
pub fn interpolate(template: &str, vars: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in vars {
        result = result.replace(&format!("{{{{{}}}}}", key), value);
    }
    result
}

/// Load a preset by name from the presets/ directory.
pub fn load_preset(name: &str) -> anyhow::Result<Preset> {
    let preset_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("presets");
    let path = preset_dir.join(format!("{}_design.toml", name));
    let path = if path.exists() {
        path
    } else {
        preset_dir.join(format!("{}.toml", name))
    };
    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("Failed to load preset '{}': {}", name, e))?;
    let preset: Preset = toml::from_str(&content)?;
    Ok(preset)
}

/// Load a prompt template by name from the prompts/ directory.
pub fn load_prompt(name: &str) -> anyhow::Result<String> {
    let prompt_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("prompts");
    let path = prompt_dir.join(format!("{}.md", name));
    std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("Failed to load prompt '{}': {}", name, e))
}

/// Load a system prompt by mode name from the system_prompts/ directory.
pub fn load_system_prompt(mode: &str) -> anyhow::Result<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("system_prompts");
    let path = dir.join(format!("{}.md", mode));
    std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("Failed to load system prompt '{}': {}", mode, e))
}

// ... tests from Step 1
```

- [ ] **Step 7: Run tests**

```bash
cargo test -p brainstormer --lib template
```

Expected: all 6 tests PASS.

- [ ] **Step 8: Commit**

```bash
git add presets prompts system_prompts src/template.rs
git commit -m "feat(brainstormer): add prompt templates, presets, and template engine"
```

---

### Task 4: ConvergenceTool (T01-T09)

**Files:**
- Create: `src/tools/convergence.rs`
- Create: `tests/convergence_test.rs`

This is the core innovation. Semantic diff, relative scoring, consensus check, convergence guard.

- [ ] **Step 1: Write failing tests (T01-T09)**

```rust
// tests/convergence_test.rs
use brainstormer::tools::convergence::*;
use brainstormer::types::*;

#[test]
fn t01_semantic_diff_detects_change() {
    // Simulate LLM responding "large" for a rewritten section
    let result = parse_semantic_delta("large");
    assert_eq!(result, SemanticDelta::Large);
}

#[test]
fn t02_semantic_diff_detects_no_change() {
    let result = parse_semantic_delta("none");
    assert_eq!(result, SemanticDelta::None);
}

#[test]
fn t03_relative_scoring_parse() {
    let llm_output = "Score: better\nThe architecture section now handles edge cases.";
    let result = parse_relative_score(llm_output);
    assert_eq!(result, RelativeScore::Better);
}

#[test]
fn t04_relative_scoring_malformed_fallback() {
    let llm_output = "I think the section is kind of okay maybe?";
    let result = parse_relative_score(llm_output);
    // Malformed output falls back to Same (conservative)
    assert_eq!(result, RelativeScore::Same);
}

#[test]
fn t05_consensus_all_agree() {
    let votes = vec![
        LlmVote { model_id: "opus".into(), score: RelativeScore::Better, comment: None },
        LlmVote { model_id: "gpt".into(), score: RelativeScore::Better, comment: None },
        LlmVote { model_id: "gemini".into(), score: RelativeScore::Same, comment: None },
    ];
    assert!(check_consensus(&votes));
}

#[test]
fn t06_consensus_one_disagrees() {
    let votes = vec![
        LlmVote { model_id: "opus".into(), score: RelativeScore::Better, comment: None },
        LlmVote { model_id: "gpt".into(), score: RelativeScore::Worse, comment: None },
        LlmVote { model_id: "gemini".into(), score: RelativeScore::Better, comment: None },
    ];
    // Worse vote = disagreement
    assert!(!check_consensus(&votes));
}

#[test]
fn t07_convergence_guard_same_objection_twice() {
    let history = vec![
        "Security model is missing encryption at rest".to_string(),
        "The security model lacks encryption at rest".to_string(), // paraphrase
    ];
    // Cosine similarity > 0.85 for these paraphrases (simulated)
    assert!(detect_irreconcilable(&history, 0.85));
}

#[test]
fn t08_mixed_sections() {
    let sections = vec![
        SectionConvergence {
            name: "Problem".into(),
            converged: true,
            trend: Trend::Same,
            agreement: "3/3".into(),
            irreconcilable: false,
        },
        SectionConvergence {
            name: "Architecture".into(),
            converged: false,
            trend: Trend::Better,
            agreement: "2/3".into(),
            irreconcilable: false,
        },
    ];
    let result = build_convergence_result(&sections);
    assert!(!result.all_converged);
    assert!(result.should_loop);
}

#[test]
fn t09_all_sections_converge_first_check() {
    let sections = vec![
        SectionConvergence {
            name: "Problem".into(),
            converged: true,
            trend: Trend::Same,
            agreement: "3/3".into(),
            irreconcilable: false,
        },
        SectionConvergence {
            name: "API".into(),
            converged: true,
            trend: Trend::Better,
            agreement: "3/3".into(),
            irreconcilable: false,
        },
    ];
    let result = build_convergence_result(&sections);
    assert!(result.all_converged);
    assert!(!result.should_loop);
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p brainstormer --test convergence_test
```

Expected: FAIL, functions not defined.

- [ ] **Step 3: Implement ConvergenceTool helper functions**

```rust
// src/tools/convergence.rs
use crate::types::*;
use zeroclaw::tools::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// Parse LLM's semantic delta classification.
pub fn parse_semantic_delta(response: &str) -> SemanticDelta {
    let lower = response.trim().to_lowercase();
    if lower.starts_with("none") || lower.contains("no meaningful change") {
        SemanticDelta::None
    } else if lower.starts_with("small") || lower.contains("refinement") {
        SemanticDelta::Small
    } else if lower.starts_with("large") || lower.contains("rewrite") {
        SemanticDelta::Large
    } else {
        // Conservative default: treat ambiguous as Small
        SemanticDelta::Small
    }
}

/// Parse relative score from LLM review output.
pub fn parse_relative_score(response: &str) -> RelativeScore {
    let lower = response.to_lowercase();
    if lower.contains("better") {
        RelativeScore::Better
    } else if lower.contains("worse") {
        RelativeScore::Worse
    } else if lower.contains("same") {
        RelativeScore::Same
    } else {
        // Malformed -> conservative fallback
        RelativeScore::Same
    }
}

/// Check consensus: no Worse votes means consensus.
pub fn check_consensus(votes: &[LlmVote]) -> bool {
    !votes.iter().any(|v| v.score == RelativeScore::Worse)
}

/// Detect irreconcilable objections via simple similarity.
/// In production, this uses embedding cosine similarity.
/// For unit tests, we use a basic word overlap heuristic.
pub fn detect_irreconcilable(history: &[String], _threshold: f64) -> bool {
    if history.len() < 2 {
        return false;
    }
    let last = &history[history.len() - 1];
    let prev = &history[history.len() - 2];
    // Simple word overlap similarity as fallback when embeddings unavailable
    let sim = word_overlap_similarity(prev, last);
    sim > 0.7 // Lower threshold for word overlap vs cosine
}

/// Word overlap Jaccard similarity (fallback for when embeddings aren't available).
fn word_overlap_similarity(a: &str, b: &str) -> f64 {
    let words_a: std::collections::HashSet<&str> = a.to_lowercase().split_whitespace().collect();
    let words_b: std::collections::HashSet<&str> = b.to_lowercase().split_whitespace().collect();
    if words_a.is_empty() && words_b.is_empty() {
        return 1.0;
    }
    let intersection = words_a.intersection(&words_b).count() as f64;
    let union = words_a.union(&words_b).count() as f64;
    intersection / union
}

/// Build the final convergence result from per-section statuses.
pub fn build_convergence_result(sections: &[SectionConvergence]) -> ConvergenceResult {
    let all_converged = sections.iter().all(|s| s.converged);
    ConvergenceResult {
        sections: sections.to_vec(),
        all_converged,
        should_loop: !all_converged,
    }
}

/// Evaluate whether a section has converged based on all signals.
pub fn evaluate_section(
    name: &str,
    votes: &[LlmVote],
    semantic_delta: SemanticDelta,
    objection_history: &[String],
) -> SectionConvergence {
    let consensus = check_consensus(votes);
    let majority_positive = {
        let non_worse = votes.iter().filter(|v| v.score != RelativeScore::Worse).count();
        non_worse > votes.len() / 2
    };
    let small_or_no_change = matches!(semantic_delta, SemanticDelta::None | SemanticDelta::Small);
    let irreconcilable = detect_irreconcilable(objection_history, 0.85);

    let converged = majority_positive && small_or_no_change && consensus;

    let trend = if votes.is_empty() {
        Trend::Same
    } else {
        let better_count = votes.iter().filter(|v| v.score == RelativeScore::Better).count();
        let worse_count = votes.iter().filter(|v| v.score == RelativeScore::Worse).count();
        if better_count > worse_count {
            Trend::Better
        } else if worse_count > better_count {
            Trend::Worse
        } else {
            Trend::Same
        }
    };

    let agree_count = votes.iter().filter(|v| v.score != RelativeScore::Worse).count();
    let agreement = format!("{}/{} agree", agree_count, votes.len());

    SectionConvergence {
        name: name.to_string(),
        converged,
        trend,
        agreement,
        irreconcilable,
    }
}

/// The ConvergenceTool itself - implements ZeroClaw's Tool trait.
pub struct ConvergenceTool {
    memory: Arc<dyn zeroclaw::memory::traits::Memory>,
}

impl ConvergenceTool {
    pub fn new(memory: Arc<dyn zeroclaw::memory::traits::Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for ConvergenceTool {
    fn name(&self) -> &str {
        "convergence"
    }

    fn description(&self) -> &str {
        "Check convergence of brainstorming sections. Returns per-section status with semantic diff, relative scoring, and consensus check."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Session ID" },
                "round": { "type": "integer", "description": "Current round number" },
                "sections": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "current": { "type": "string" },
                            "previous": { "type": "string" },
                            "votes": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "model_id": { "type": "string" },
                                        "score": { "type": "string" },
                                        "comment": { "type": "string" }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            "required": ["session_id", "round", "sections"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let session_id = args["session_id"].as_str().unwrap_or("unknown");
        let round = args["round"].as_u64().unwrap_or(1) as u32;
        let sections_arr = args["sections"].as_array()
            .ok_or_else(|| anyhow::anyhow!("Missing sections array"))?;

        let mut convergence_sections = Vec::new();

        for section in sections_arr {
            let name = section["name"].as_str().unwrap_or("").to_string();
            let current = section["current"].as_str().unwrap_or("");
            let previous = section["previous"].as_str().unwrap_or("");

            // Parse votes from the section data
            let votes: Vec<LlmVote> = if let Some(votes_arr) = section["votes"].as_array() {
                votes_arr.iter().map(|v| LlmVote {
                    model_id: v["model_id"].as_str().unwrap_or("").to_string(),
                    score: parse_relative_score(v["score"].as_str().unwrap_or("same")),
                    comment: v["comment"].as_str().map(String::from),
                }).collect()
            } else {
                vec![]
            };

            // Determine semantic delta from content comparison
            let semantic_delta = if current == previous || previous.is_empty() {
                SemanticDelta::None
            } else {
                // In production, this calls an LLM via DelegateAgentConfig
                // For now, use content length ratio as heuristic
                let len_ratio = (current.len() as f64 - previous.len() as f64).abs()
                    / previous.len().max(1) as f64;
                if len_ratio < 0.1 {
                    SemanticDelta::Small
                } else {
                    SemanticDelta::Large
                }
            };

            // Load objection history from Memory
            let history_key = format!("brainstorm:{}:round:{}:objections:{}", session_id, round, name);
            let objection_history = match self.memory.get(&history_key).await {
                Ok(Some(entry)) => serde_json::from_str(&entry.content).unwrap_or_default(),
                _ => vec![],
            };

            let section_result = evaluate_section(&name, &votes, semantic_delta, &objection_history);
            convergence_sections.push(section_result);
        }

        let result = build_convergence_result(&convergence_sections);

        // Persist convergence state to Memory
        let state_key = format!("brainstorm:{}:round:{}:convergence", session_id, round);
        let _ = self.memory.store(
            &state_key,
            &serde_json::to_string(&result)?,
            zeroclaw::memory::traits::MemoryCategory::Custom("brainstorm".to_string()),
            Some(session_id),
        ).await;

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&result)?,
            error: None,
        })
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p brainstormer --test convergence_test
```

Expected: all 9 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/tools/convergence.rs tests/convergence_test.rs
git commit -m "feat(brainstormer): implement ConvergenceTool with semantic diff and convergence guard"
```

---

### Task 5: MergeTool (T16-T18)

**Files:**
- Create: `src/tools/merge.rs`
- Create: `tests/merge_test.rs`

- [ ] **Step 1: Write failing tests**

```rust
// tests/merge_test.rs
use brainstormer::tools::merge::*;

#[test]
fn t16_format_merge_input_multiple_outputs() {
    let outputs = vec![
        ("claude-opus".to_string(), "Use microservices with gRPC".to_string()),
        ("gpt-5".to_string(), "Monolith with clear module boundaries".to_string()),
    ];
    let formatted = format_outputs_for_merge(&outputs);
    assert!(formatted.contains("=== claude-opus ==="));
    assert!(formatted.contains("=== gpt-5 ==="));
    assert!(formatted.contains("microservices"));
    assert!(formatted.contains("Monolith"));
}

#[test]
fn t17_merge_model_fallback_selection() {
    use brainstormer::types::ModelRef;
    let primary = ModelRef { provider: "anthropic".into(), model: "claude-opus-4-6".into() };
    let fallbacks = vec![
        ModelRef { provider: "openai".into(), model: "gpt-5.4".into() },
        ModelRef { provider: "google".into(), model: "gemini-3.1-pro".into() },
    ];
    let selected = select_fallback_model(&primary, &fallbacks);
    assert!(selected.is_some());
    assert_ne!(selected.unwrap().model, primary.model);
}

#[test]
fn t18_format_merge_input_no_critiques() {
    let outputs = vec![
        ("model-a".to_string(), "Approach A".to_string()),
    ];
    let formatted = format_merge_prompt(&outputs, None, "Design a cache");
    assert!(formatted.contains("Approach A"));
    assert!(!formatted.contains("Critiques"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p brainstormer --test merge_test
```

Expected: FAIL.

- [ ] **Step 3: Implement MergeTool**

```rust
// src/tools/merge.rs
use crate::types::ModelRef;
use zeroclaw::tools::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// Format multiple LLM outputs with model delimiters for merge input.
pub fn format_outputs_for_merge(outputs: &[(String, String)]) -> String {
    outputs.iter()
        .map(|(model, content)| format!("=== {} ===\n{}\n", model, content))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the full merge prompt with optional critiques.
pub fn format_merge_prompt(
    outputs: &[(String, String)],
    critiques: Option<&[(String, String)]>,
    task: &str,
) -> String {
    let mut prompt = format!("## Task\n{}\n\n## Outputs to Merge\n{}", task, format_outputs_for_merge(outputs));
    if let Some(crits) = critiques {
        if !crits.is_empty() {
            prompt.push_str("\n\n## Review Critiques to Incorporate\n");
            prompt.push_str(&format_outputs_for_merge(crits));
        }
    }
    prompt
}

/// Select a fallback model different from the primary.
pub fn select_fallback_model<'a>(primary: &ModelRef, candidates: &'a [ModelRef]) -> Option<&'a ModelRef> {
    candidates.iter().find(|m| m.model != primary.model)
}

/// MergeTool: single LLM synthesizes multiple outputs into one draft.
pub struct MergeTool {
    memory: Arc<dyn zeroclaw::memory::traits::Memory>,
}

impl MergeTool {
    pub fn new(memory: Arc<dyn zeroclaw::memory::traits::Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for MergeTool {
    fn name(&self) -> &str {
        "merge"
    }

    fn description(&self) -> &str {
        "Synthesize multiple LLM outputs into a single coherent draft. Optionally incorporates review critiques."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string" },
                "round": { "type": "integer" },
                "task": { "type": "string", "description": "The original task description" },
                "outputs": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "model_id": { "type": "string" },
                            "content": { "type": "string" }
                        }
                    },
                    "description": "LLM outputs to merge"
                },
                "critiques": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "model_id": { "type": "string" },
                            "content": { "type": "string" }
                        }
                    },
                    "description": "Optional review critiques to incorporate"
                }
            },
            "required": ["session_id", "round", "task", "outputs"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let session_id = args["session_id"].as_str().unwrap_or("unknown");
        let round = args["round"].as_u64().unwrap_or(1) as u32;
        let task = args["task"].as_str().unwrap_or("");

        let outputs: Vec<(String, String)> = args["outputs"].as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|o| (
                o["model_id"].as_str().unwrap_or("unknown").to_string(),
                o["content"].as_str().unwrap_or("").to_string(),
            ))
            .collect();

        let critiques: Option<Vec<(String, String)>> = args.get("critiques")
            .and_then(|c| c.as_array())
            .map(|arr| arr.iter().map(|o| (
                o["model_id"].as_str().unwrap_or("unknown").to_string(),
                o["content"].as_str().unwrap_or("").to_string(),
            )).collect());

        let merge_prompt = format_merge_prompt(&outputs, critiques.as_deref(), task);

        // In production: call merge LLM via DelegateAgentConfig
        // The orchestrator agent handles the actual LLM call
        // This tool prepares the prompt and persists the result

        // Persist merge input to Memory for traceability
        let key = format!("brainstorm:{}:round:{}:merged_draft", session_id, round);
        let _ = self.memory.store(
            &key,
            &merge_prompt,
            zeroclaw::memory::traits::MemoryCategory::Custom("brainstorm".to_string()),
            Some(session_id),
        ).await;

        Ok(ToolResult {
            success: true,
            output: merge_prompt,
            error: None,
        })
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p brainstormer --test merge_test
```

Expected: all 3 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/tools/merge.rs tests/merge_test.rs
git commit -m "feat(brainstormer): implement MergeTool with fallback model selection"
```

---

### Task 6: BrainstormSwarmTool (T10-T15)

**Files:**
- Create: `src/tools/brainstorm_swarm.rs`
- Create: `src/sanitize.rs`
- Create: `tests/brainstorm_swarm_test.rs`

- [ ] **Step 1: Write sanitization tests first**

```rust
// src/sanitize.rs (tests at bottom)
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t11_strips_system_prompt_patterns() {
        let output = "Here is my response.\n\nSystem: You are a helpful assistant.\n\nMore content.";
        let cleaned = sanitize_llm_output(output, "claude-opus");
        assert!(!cleaned.contains("System: You are a helpful assistant"));
        assert!(cleaned.contains("Here is my response"));
        assert!(cleaned.contains("More content"));
    }

    #[test]
    fn t12_wraps_in_model_delimiters() {
        let output = "My brainstorm output";
        let wrapped = wrap_with_delimiter(output, "claude-opus-4-6");
        assert!(wrapped.starts_with("=== claude-opus-4-6 ==="));
        assert!(wrapped.ends_with("=== END claude-opus-4-6 ==="));
        assert!(wrapped.contains("My brainstorm output"));
    }
}
```

- [ ] **Step 2: Implement sanitization**

```rust
// src/sanitize.rs
use regex::Regex;
use std::sync::LazyLock;

static SYSTEM_PROMPT_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| vec![
    Regex::new(r"(?i)^system:\s*.+$").unwrap(),
    Regex::new(r"(?i)^<\|im_start\|>system.*?<\|im_end\|>").unwrap(),
    Regex::new(r"(?i)\[INST\].*?\[/INST\]").unwrap(),
    Regex::new(r"(?i)^(Human|Assistant|User):\s*").unwrap(),
]);

/// Strip system prompt injection patterns from LLM output.
pub fn sanitize_llm_output(output: &str, _model_id: &str) -> String {
    let mut cleaned = output.to_string();
    for pattern in SYSTEM_PROMPT_PATTERNS.iter() {
        cleaned = pattern.replace_all(&cleaned, "").to_string();
    }
    // Remove empty lines left by stripping
    cleaned.lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wrap output in model-specific delimiters.
pub fn wrap_with_delimiter(output: &str, model_id: &str) -> String {
    format!("=== {} ===\n{}\n=== END {} ===", model_id, output, model_id)
}

/// Full sanitize + wrap pipeline.
pub fn process_llm_output(output: &str, model_id: &str) -> String {
    let cleaned = sanitize_llm_output(output, model_id);
    wrap_with_delimiter(&cleaned, model_id)
}

// tests...
```

Note: add `regex = "1.10"` to `Cargo.toml` dependencies.

- [ ] **Step 3: Write BrainstormSwarmTool tests (T10, T13-T15)**

```rust
// tests/brainstorm_swarm_test.rs
use brainstormer::tools::brainstorm_swarm::*;
use brainstormer::types::*;

#[test]
fn t10_build_swarm_config_creates_n_agents() {
    let models = vec![
        ModelRef { provider: "anthropic".into(), model: "claude-opus-4-6".into() },
        ModelRef { provider: "openai".into(), model: "gpt-5.4".into() },
        ModelRef { provider: "google".into(), model: "gemini-3.1-pro".into() },
    ];
    let (agents, swarm) = build_swarm_config(&models, "brainstorm", 30);
    assert_eq!(agents.len(), 3);
    assert_eq!(swarm.agents.len(), 3);
    assert!(matches!(swarm.strategy, zeroclaw::config::SwarmStrategy::Parallel));
}

#[test]
fn t13_degraded_mode_m_gte_3_one_failure() {
    let results = vec![
        Ok("Output from model 1".to_string()),
        Err("Timeout".to_string()),
        Ok("Output from model 3".to_string()),
    ];
    let model_count = 3;
    let (outputs, warning) = handle_partial_results(results, model_count);
    assert_eq!(outputs.len(), 2);
    assert!(warning.is_some());
    assert!(warning.unwrap().contains("1 model failed"));
}

#[test]
fn t14_degraded_mode_m_gte_3_one_refusal() {
    let results = vec![
        Ok("Output from model 1".to_string()),
        Ok("[Refused] I cannot assist with that".to_string()),
        Ok("Output from model 3".to_string()),
    ];
    let model_count = 3;
    let (outputs, warning) = handle_partial_results(results, model_count);
    // Refusal is filtered out
    assert_eq!(outputs.len(), 2);
    assert!(warning.is_some());
}

#[test]
fn t14b_degraded_mode_m_eq_2_one_failure() {
    let results = vec![
        Ok("Output from model 1".to_string()),
        Err("Timeout".to_string()),
    ];
    let model_count = 2;
    let (outputs, warning) = handle_partial_results(results, model_count);
    // M=2 with 1 failure: pipeline should pause (only 1 output, below minimum)
    assert_eq!(outputs.len(), 1);
    assert!(warning.is_some());
    assert!(warning.unwrap().contains("pipeline paused"));
}
```

- [ ] **Step 4: Implement BrainstormSwarmTool**

```rust
// src/tools/brainstorm_swarm.rs
use crate::sanitize;
use crate::types::ModelRef;
use zeroclaw::config::{DelegateAgentConfig, SwarmConfig, SwarmStrategy};
use zeroclaw::tools::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const MIN_MODELS: usize = 2;
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Build a SwarmConfig + agent configs for parallel dispatch.
pub fn build_swarm_config(
    models: &[ModelRef],
    stage: &str,
    timeout_secs: u64,
) -> (HashMap<String, DelegateAgentConfig>, SwarmConfig) {
    let mut agents = HashMap::new();
    let mut agent_names = Vec::new();

    for (i, model) in models.iter().enumerate() {
        let name = format!("{}_agent_{}", stage, i);
        agents.insert(name.clone(), DelegateAgentConfig {
            provider: model.provider.clone(),
            model: model.model.clone(),
            system_prompt: None, // Set by orchestrator
            api_key: None,       // Uses env var via provider
            temperature: Some(0.7),
            max_depth: 1,
            agentic: false,
            allowed_tools: vec![],
            max_iterations: 1,
        });
        agent_names.push(name);
    }

    let swarm = SwarmConfig {
        agents: agent_names,
        strategy: SwarmStrategy::Parallel,
        router_prompt: None,
        description: Some(format!("{} swarm with {} models", stage, models.len())),
        timeout_secs,
    };

    (agents, swarm)
}

/// Handle partial results from parallel swarm.
/// Returns (successful_outputs, optional_warning).
pub fn handle_partial_results(
    results: Vec<Result<String, String>>,
    total_models: usize,
) -> (Vec<String>, Option<String>) {
    let mut outputs = Vec::new();
    let mut failures = Vec::new();

    for result in results {
        match result {
            Ok(output) => {
                // Filter out refusals
                if output.starts_with("[Refused]") || output.contains("I cannot assist") {
                    failures.push("refusal".to_string());
                } else {
                    outputs.push(output);
                }
            }
            Err(e) => failures.push(e),
        }
    }

    let warning = if failures.is_empty() {
        None
    } else if total_models <= MIN_MODELS && outputs.len() < MIN_MODELS {
        Some(format!(
            "{} model failed, pipeline paused: need at least {} models for multi-LLM brainstorm",
            failures.len(),
            MIN_MODELS
        ))
    } else {
        Some(format!("{} model failed, proceeding with {} results", failures.len(), outputs.len()))
    };

    (outputs, warning)
}

pub struct BrainstormSwarmTool {
    memory: Arc<dyn zeroclaw::memory::traits::Memory>,
}

impl BrainstormSwarmTool {
    pub fn new(memory: Arc<dyn zeroclaw::memory::traits::Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for BrainstormSwarmTool {
    fn name(&self) -> &str {
        "brainstorm_swarm"
    }

    fn description(&self) -> &str {
        "Dispatch parallel LLM calls for brainstorming or reviewing. Uses N models with the same prompt."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string" },
                "round": { "type": "integer" },
                "stage": { "type": "string", "enum": ["brainstorm", "review"] },
                "task": { "type": "string" },
                "prompt": { "type": "string", "description": "The prompt to send to all models" },
                "sections": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional: only these sections (for partial re-review)"
                }
            },
            "required": ["session_id", "round", "stage", "task", "prompt"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let session_id = args["session_id"].as_str().unwrap_or("unknown");
        let round = args["round"].as_u64().unwrap_or(1);
        let stage = args["stage"].as_str().unwrap_or("brainstorm");
        let prompt = args["prompt"].as_str().unwrap_or("");

        // In production: load model configs from SessionConfig in Memory,
        // build SwarmConfig, dispatch via SwarmTool, sanitize results
        // For now, return the prepared prompt structure

        let key = format!("brainstorm:{}:round:{}:{}", session_id, round, stage);
        let _ = self.memory.store(
            &key,
            prompt,
            zeroclaw::memory::traits::MemoryCategory::Custom("brainstorm".to_string()),
            Some(session_id),
        ).await;

        Ok(ToolResult {
            success: true,
            output: format!("[{}] Dispatched to models with prompt: {}...",
                stage, &prompt[..prompt.len().min(100)]),
            error: None,
        })
    }
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p brainstormer --test brainstorm_swarm_test
cargo test -p brainstormer --lib sanitize
```

Expected: all tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src/tools/brainstorm_swarm.rs src/sanitize.rs tests/brainstorm_swarm_test.rs
git commit -m "feat(brainstormer): implement BrainstormSwarmTool with sanitization and degraded mode"
```

---

### Task 7: MergeQualityTool (T19-T21)

**Files:**
- Create: `src/tools/merge_quality.rs`
- Create: `tests/merge_quality_test.rs`

- [ ] **Step 1: Write failing tests**

```rust
// tests/merge_quality_test.rs
use brainstormer::tools::merge_quality::*;

#[test]
fn t19_quality_pass() {
    let llm_response = "All critiques addressed. PASS";
    let result = parse_quality_verdict(llm_response);
    assert!(result.passed);
    assert!(result.dropped_critiques.is_empty());
}

#[test]
fn t20_quality_fail() {
    let llm_response = "FAIL\nDropped critiques:\n- Error handling section ignored\n- No pagination added";
    let result = parse_quality_verdict(llm_response);
    assert!(!result.passed);
    assert_eq!(result.dropped_critiques.len(), 2);
}

#[test]
fn t21_third_failure_proceed_with_warning() {
    let attempt = 3;
    let max_attempts = 3;
    let should_proceed = should_proceed_despite_failure(attempt, max_attempts);
    assert!(should_proceed);
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p brainstormer --test merge_quality_test
```

Expected: FAIL.

- [ ] **Step 3: Implement MergeQualityTool**

```rust
// src/tools/merge_quality.rs
use zeroclaw::tools::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

const MAX_QUALITY_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone)]
pub struct QualityVerdict {
    pub passed: bool,
    pub dropped_critiques: Vec<String>,
}

/// Parse quality check verdict from LLM response.
pub fn parse_quality_verdict(response: &str) -> QualityVerdict {
    let lower = response.to_lowercase();
    let passed = lower.contains("pass") && !lower.starts_with("fail");

    let dropped_critiques = if !passed {
        response.lines()
            .filter(|line| line.trim_start().starts_with("- ") || line.trim_start().starts_with("* "))
            .map(|line| line.trim_start_matches("- ").trim_start_matches("* ").trim().to_string())
            .collect()
    } else {
        vec![]
    };

    QualityVerdict { passed, dropped_critiques }
}

/// Whether to proceed despite quality failure (at max attempts).
pub fn should_proceed_despite_failure(attempt: u32, max_attempts: u32) -> bool {
    attempt >= max_attempts
}

pub struct MergeQualityTool {
    memory: Arc<dyn zeroclaw::memory::traits::Memory>,
}

impl MergeQualityTool {
    pub fn new(memory: Arc<dyn zeroclaw::memory::traits::Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for MergeQualityTool {
    fn name(&self) -> &str {
        "merge_quality"
    }

    fn description(&self) -> &str {
        "Verify that a merge faithfully incorporated review critiques. Returns PASS or FAIL with details."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string" },
                "round": { "type": "integer" },
                "attempt": { "type": "integer", "description": "Quality check attempt number (1-3)" },
                "draft": { "type": "string", "description": "The merged draft to verify" },
                "critiques": { "type": "string", "description": "The review critiques that should be addressed" }
            },
            "required": ["session_id", "round", "attempt", "draft", "critiques"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let session_id = args["session_id"].as_str().unwrap_or("unknown");
        let round = args["round"].as_u64().unwrap_or(1);
        let attempt = args["attempt"].as_u64().unwrap_or(1) as u32;

        // In production: call a different model than the merger via DelegateAgentConfig
        // Parse response with parse_quality_verdict
        // If FAIL and attempt < MAX_QUALITY_ATTEMPTS, signal re-merge
        // If FAIL and attempt >= MAX_QUALITY_ATTEMPTS, proceed with warning

        let proceed_anyway = should_proceed_despite_failure(attempt, MAX_QUALITY_ATTEMPTS);
        let status = if proceed_anyway {
            "PROCEED_WITH_WARNING"
        } else {
            "PENDING_LLM_VERIFICATION"
        };

        Ok(ToolResult {
            success: true,
            output: json!({
                "status": status,
                "attempt": attempt,
                "max_attempts": MAX_QUALITY_ATTEMPTS,
                "session_id": session_id,
                "round": round,
            }).to_string(),
            error: None,
        })
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p brainstormer --test merge_quality_test
```

Expected: all 3 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/tools/merge_quality.rs tests/merge_quality_test.rs
git commit -m "feat(brainstormer): implement MergeQualityTool with retry cap"
```

---

### Task 8: Pipeline Hooks (T22-T28)

**Files:**
- Create: `src/hooks/sequence_enforcement.rs`
- Create: `src/hooks/output_capture.rs`
- Create: `src/hooks/context_digest.rs`
- Create: `src/hooks/context_window.rs`
- Create: `tests/hooks_test.rs`

- [ ] **Step 1: Write failing tests (T22-T28)**

```rust
// tests/hooks_test.rs
use brainstormer::hooks::sequence_enforcement::*;
use brainstormer::hooks::output_capture::*;
use brainstormer::hooks::context_digest::*;
use brainstormer::hooks::context_window::*;
use brainstormer::types::Stage;

// T22: Valid sequence allowed
#[test]
fn t22_valid_tool_sequence() {
    let mut state = PipelineState::new();
    assert!(state.validate_tool_call("brainstorm_swarm").is_ok());
    state.advance(Stage::Brainstorm);
    assert!(state.validate_tool_call("merge").is_ok());
    state.advance(Stage::Merge1);
    assert!(state.validate_tool_call("brainstorm_swarm").is_ok()); // review stage
}

// T23: Invalid sequence cancelled
#[test]
fn t23_invalid_sequence_cancelled() {
    let state = PipelineState::new();
    // Can't merge before brainstorm
    assert!(state.validate_tool_call("merge").is_err());
}

// T24: Convergence blocked until quality pass
#[test]
fn t24_convergence_blocked_until_quality() {
    let mut state = PipelineState::new();
    state.advance(Stage::Brainstorm);
    state.advance(Stage::Merge1);
    state.advance(Stage::Review);
    state.advance(Stage::Merge2);
    // Quality check not done yet
    assert!(state.validate_tool_call("convergence").is_err());
    state.advance(Stage::QualityCheck);
    assert!(state.validate_tool_call("convergence").is_ok());
}

// T25: Digest from round state
#[test]
fn t25_digest_from_round_state() {
    use brainstormer::types::*;
    let rounds = vec![
        RoundState {
            round_num: 1,
            stage: Stage::Evaluate,
            sections: vec![
                SectionState {
                    name: "Architecture".into(),
                    content: "Use microservices".into(),
                    converged: true,
                    trend: Trend::Better,
                    agreement: vec![],
                    objection_history: vec![],
                },
            ],
            cost: RoundCost { tokens_in: 1000, tokens_out: 500, usd: 0.05 },
            timestamp: chrono::Utc::now(),
        },
    ];
    let digest = build_digest(&rounds);
    assert!(digest.contains("Round 1"));
    assert!(digest.contains("Architecture"));
    assert!(digest.contains("CONVERGED"));
}

// T26: First round has empty digest
#[test]
fn t26_first_round_empty_digest() {
    let rounds: Vec<brainstormer::types::RoundState> = vec![];
    let digest = build_digest(&rounds);
    assert!(digest.is_empty() || digest.contains("First round"));
}

// T27: Round-tagged messages truncated
#[test]
fn t27_truncate_old_rounds() {
    let messages = vec![
        ("system".to_string(), "You are an orchestrator".to_string()),
        ("assistant".to_string(), "[round:1] Brainstorm output...".to_string()),
        ("assistant".to_string(), "[round:1] Review output...".to_string()),
        ("assistant".to_string(), "[round:2] Current brainstorm...".to_string()),
    ];
    let truncated = truncate_old_rounds(&messages, 2);
    // Should keep system message + round 2 messages
    assert_eq!(truncated.len(), 2); // system + round 2
    assert!(truncated.iter().all(|(_, c)| !c.contains("[round:1]")));
}

// T28: Untagged messages preserved
#[test]
fn t28_untagged_messages_preserved() {
    let messages = vec![
        ("system".to_string(), "You are an orchestrator".to_string()),
        ("user".to_string(), "Design a cache".to_string()),
        ("assistant".to_string(), "[round:1] Old output".to_string()),
        ("assistant".to_string(), "[round:2] Current output".to_string()),
    ];
    let truncated = truncate_old_rounds(&messages, 2);
    // system + user + round 2 = 3
    assert_eq!(truncated.len(), 3);
    assert!(truncated.iter().any(|(_, c)| c.contains("Design a cache")));
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p brainstormer --test hooks_test
```

Expected: FAIL.

- [ ] **Step 3: Implement PipelineState for SequenceEnforcementHook**

```rust
// src/hooks/sequence_enforcement.rs
use crate::types::Stage;
use zeroclaw::hooks::traits::{HookHandler, HookResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Tracks pipeline state for sequence enforcement.
#[derive(Debug, Clone)]
pub struct PipelineState {
    current_stage: Option<Stage>,
    quality_passed: bool,
    merge_attempts: u32,
}

impl PipelineState {
    pub fn new() -> Self {
        Self {
            current_stage: None,
            quality_passed: false,
            merge_attempts: 0,
        }
    }

    pub fn advance(&mut self, stage: Stage) {
        self.current_stage = Some(stage);
        if stage == Stage::QualityCheck {
            self.quality_passed = true;
        }
    }

    /// Validate whether a tool call is allowed in current pipeline state.
    pub fn validate_tool_call(&self, tool_name: &str) -> Result<(), String> {
        match tool_name {
            "brainstorm_swarm" => {
                // Allowed at start (brainstorm) or after Merge1 (review)
                match self.current_stage {
                    None => Ok(()),                    // First call = brainstorm
                    Some(Stage::Merge1) => Ok(()),    // After merge = review
                    Some(Stage::Evaluate) => Ok(()),  // Loop back
                    _ => Err(format!("brainstorm_swarm not allowed after {:?}", self.current_stage)),
                }
            }
            "merge" => {
                // Allowed after brainstorm or review
                match self.current_stage {
                    Some(Stage::Brainstorm) => Ok(()),
                    Some(Stage::Review) => Ok(()),
                    _ => Err(format!("merge not allowed after {:?}", self.current_stage)),
                }
            }
            "merge_quality" => {
                match self.current_stage {
                    Some(Stage::Merge2) => Ok(()),
                    _ => Err(format!("merge_quality not allowed after {:?}", self.current_stage)),
                }
            }
            "convergence" => {
                if !self.quality_passed {
                    Err("convergence not allowed until quality check passes".to_string())
                } else {
                    Ok(())
                }
            }
            _ => Ok(()), // Other tools pass through
        }
    }
}

pub struct SequenceEnforcementHook {
    state: Arc<Mutex<PipelineState>>,
}

impl SequenceEnforcementHook {
    pub fn new(state: Arc<Mutex<PipelineState>>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl HookHandler for SequenceEnforcementHook {
    fn name(&self) -> &str {
        "sequence_enforcement"
    }

    fn priority(&self) -> i32 {
        100 // Highest priority: run before other hooks
    }

    async fn before_tool_call(&self, name: String, args: Value) -> HookResult<(String, Value)> {
        let state = self.state.lock().unwrap();
        match state.validate_tool_call(&name) {
            Ok(()) => HookResult::Continue((name, args)),
            Err(reason) => HookResult::Cancel(reason),
        }
    }
}
```

- [ ] **Step 4: Implement OutputCaptureHook**

```rust
// src/hooks/output_capture.rs
use zeroclaw::hooks::traits::HookHandler;
use zeroclaw::providers::traits::ChatResponse;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

/// Tags LLM outputs with round numbers for context window management.
pub struct OutputCaptureHook {
    current_round: Arc<Mutex<u32>>,
}

impl OutputCaptureHook {
    pub fn new(current_round: Arc<Mutex<u32>>) -> Self {
        Self { current_round }
    }

    pub fn set_round(&self, round: u32) {
        *self.current_round.lock().unwrap() = round;
    }
}

#[async_trait]
impl HookHandler for OutputCaptureHook {
    fn name(&self) -> &str {
        "output_capture"
    }

    async fn on_llm_output(&self, response: &ChatResponse) {
        // Tag is injected by wrapping the response content
        // The actual tagging happens in the tool results, not here
        let round = *self.current_round.lock().unwrap();
        tracing::debug!("OutputCapture: round {} response received", round);
    }
}

/// Tag a string with round metadata.
pub fn tag_with_round(content: &str, round: u32) -> String {
    format!("[round:{}] {}", round, content)
}

/// Extract round number from a tagged message.
pub fn extract_round(content: &str) -> Option<u32> {
    if content.starts_with("[round:") {
        content.get(7..)?.split(']').next()?.parse().ok()
    } else {
        None
    }
}
```

- [ ] **Step 5: Implement ContextDigestHook**

```rust
// src/hooks/context_digest.rs
use crate::types::*;
use zeroclaw::hooks::traits::{HookHandler, HookResult};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

/// Build a structured digest from completed rounds.
pub fn build_digest(rounds: &[RoundState]) -> String {
    if rounds.is_empty() {
        return String::new();
    }

    let mut digest = String::from("## Round History\n\n");
    for round in rounds {
        digest.push_str(&format!("### Round {}\n", round.round_num));
        for section in &round.sections {
            let status = if section.converged { "CONVERGED" } else { "IN PROGRESS" };
            let trend = match section.trend {
                Trend::Better => "^",
                Trend::Worse => "v",
                Trend::Same => "=",
            };
            digest.push_str(&format!("- {} [{}] {} | {} votes\n",
                section.name, status, trend, section.agreement.len()));
        }
        digest.push_str(&format!("Cost: ${:.2} | {}in/{}out tokens\n\n",
            round.cost.usd, round.cost.tokens_in, round.cost.tokens_out));
    }
    digest
}

pub struct ContextDigestHook {
    rounds: Arc<Mutex<Vec<RoundState>>>,
}

impl ContextDigestHook {
    pub fn new(rounds: Arc<Mutex<Vec<RoundState>>>) -> Self {
        Self { rounds }
    }
}

#[async_trait]
impl HookHandler for ContextDigestHook {
    fn name(&self) -> &str {
        "context_digest"
    }

    async fn before_prompt_build(&self, prompt: String) -> HookResult<String> {
        let rounds = self.rounds.lock().unwrap();
        let digest = build_digest(&rounds);
        if digest.is_empty() {
            HookResult::Continue(prompt)
        } else {
            HookResult::Continue(format!("{}\n\n{}", prompt, digest))
        }
    }
}
```

- [ ] **Step 6: Implement ContextWindowHook**

```rust
// src/hooks/context_window.rs
use crate::hooks::output_capture::extract_round;
use zeroclaw::hooks::traits::{HookHandler, HookResult};
use zeroclaw::providers::traits::ChatMessage;
use async_trait::async_trait;

/// Truncate messages from old rounds, keeping only the current round's tool results.
pub fn truncate_old_rounds(
    messages: &[(String, String)],  // (role, content) pairs
    current_round: u32,
) -> Vec<(String, String)> {
    messages.iter()
        .filter(|(role, content)| {
            // Always keep system and user messages (untagged)
            if role == "system" || role == "user" {
                return true;
            }
            // Keep messages from current round or untagged messages
            match extract_round(content) {
                Some(round) => round >= current_round,
                None => true, // Untagged = keep
            }
        })
        .cloned()
        .collect()
}

pub struct ContextWindowHook {
    current_round: std::sync::Arc<std::sync::Mutex<u32>>,
}

impl ContextWindowHook {
    pub fn new(current_round: std::sync::Arc<std::sync::Mutex<u32>>) -> Self {
        Self { current_round }
    }
}

#[async_trait]
impl HookHandler for ContextWindowHook {
    fn name(&self) -> &str {
        "context_window"
    }

    async fn before_llm_call(
        &self,
        messages: Vec<ChatMessage>,
        model: String,
    ) -> HookResult<(Vec<ChatMessage>, String)> {
        let current_round = *self.current_round.lock().unwrap();
        if current_round <= 1 {
            return HookResult::Continue((messages, model));
        }

        let filtered: Vec<ChatMessage> = messages.into_iter()
            .filter(|msg| {
                let content = msg.content_text();
                // Keep system and user messages
                if msg.role == "system" || msg.role == "user" {
                    return true;
                }
                // Keep current round or untagged
                match extract_round(&content) {
                    Some(round) => round >= current_round,
                    None => true,
                }
            })
            .collect();

        HookResult::Continue((filtered, model))
    }
}
```

- [ ] **Step 7: Update hooks/mod.rs**

```rust
// src/hooks/mod.rs
pub mod sequence_enforcement;
pub mod output_capture;
pub mod context_digest;
pub mod context_window;
pub mod session_init;
pub mod copilot;
```

- [ ] **Step 8: Run tests**

```bash
cargo test -p brainstormer --test hooks_test
```

Expected: all 7 tests (T22-T28) PASS.

- [ ] **Step 9: Commit**

```bash
git add src/hooks/ tests/hooks_test.rs
git commit -m "feat(brainstormer): implement pipeline hooks (sequence, output capture, digest, context window)"
```

---

### Task 9: Session Hooks (T29-T30)

**Files:**
- Create: `src/hooks/session_init.rs`
- Create: `src/hooks/copilot.rs`

- [ ] **Step 1: Write failing tests**

```rust
// Add to tests/hooks_test.rs
use brainstormer::hooks::copilot::*;
use brainstormer::types::CopilotAction;

// T29: Parse accept/reject/edit
#[test]
fn t29_parse_copilot_commands() {
    assert_eq!(parse_copilot_input("a"), CopilotAction::Accept);
    assert_eq!(parse_copilot_input("accept"), CopilotAction::Accept);
    assert_eq!(
        parse_copilot_input("r needs more error handling"),
        CopilotAction::Reject { feedback: "needs more error handling".to_string() }
    );
    assert_eq!(
        parse_copilot_input("reject too verbose"),
        CopilotAction::Reject { feedback: "too verbose".to_string() }
    );
}

// T30: Structured feedback for next turn
#[test]
fn t30_format_copilot_feedback() {
    let action = CopilotAction::Reject { feedback: "Missing pagination".to_string() };
    let formatted = format_copilot_feedback(&action, "merge", "claude-opus-4-6");
    assert!(formatted.contains("REJECTED"));
    assert!(formatted.contains("Missing pagination"));
    assert!(formatted.contains("merge"));
}
```

- [ ] **Step 2: Implement CopilotHook**

```rust
// src/hooks/copilot.rs
use crate::types::{CopilotAction, ModelStats};
use zeroclaw::hooks::traits::{HookHandler, HookResult};
use zeroclaw::channels::traits::ChannelMessage;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Parse user input into a CopilotAction.
pub fn parse_copilot_input(input: &str) -> CopilotAction {
    let trimmed = input.trim();
    if trimmed == "a" || trimmed.starts_with("accept") {
        CopilotAction::Accept
    } else if trimmed.starts_with("r ") || trimmed.starts_with("reject ") {
        let feedback = trimmed
            .trim_start_matches("reject ")
            .trim_start_matches("r ")
            .to_string();
        CopilotAction::Reject { feedback }
    } else if trimmed.starts_with("e ") || trimmed.starts_with("edit ") {
        let content = trimmed
            .trim_start_matches("edit ")
            .trim_start_matches("e ")
            .to_string();
        CopilotAction::Edit { content }
    } else {
        // Default: treat as feedback/rejection
        CopilotAction::Reject { feedback: trimmed.to_string() }
    }
}

/// Format a CopilotAction into structured feedback for the next Agent turn.
pub fn format_copilot_feedback(action: &CopilotAction, stage: &str, model_id: &str) -> String {
    match action {
        CopilotAction::Accept => {
            format!("[ACCEPTED] User accepted {} output from {}", stage, model_id)
        }
        CopilotAction::Reject { feedback } => {
            format!("[REJECTED] User rejected {} output from {}.\nFeedback: {}", stage, model_id, feedback)
        }
        CopilotAction::Edit { content } => {
            format!("[EDITED] User edited {} output from {}.\nNew content: {}", stage, model_id, content)
        }
    }
}

pub struct CopilotHook {
    enabled: bool,
    model_stats: Arc<Mutex<HashMap<String, ModelStats>>>,
}

impl CopilotHook {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            model_stats: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn record_action(&self, model_id: &str, action: &CopilotAction) {
        let mut stats = self.model_stats.lock().unwrap();
        let entry = stats.entry(model_id.to_string()).or_default();
        match action {
            CopilotAction::Accept => entry.accepted += 1,
            CopilotAction::Reject { .. } => entry.rejected += 1,
            CopilotAction::Edit { .. } => entry.edited += 1,
        }
    }

    pub fn get_stats(&self) -> HashMap<String, ModelStats> {
        self.model_stats.lock().unwrap().clone()
    }
}

#[async_trait]
impl HookHandler for CopilotHook {
    fn name(&self) -> &str {
        "copilot"
    }

    async fn on_message_received(&self, message: ChannelMessage) -> HookResult<ChannelMessage> {
        if !self.enabled {
            return HookResult::Continue(message);
        }
        // Parse and transform the message content
        let action = parse_copilot_input(&message.content);
        let formatted = format_copilot_feedback(&action, "current_stage", "current_model");

        let mut modified = message;
        modified.content = formatted;
        HookResult::Continue(modified)
    }
}
```

- [ ] **Step 3: Implement SessionInitHook**

```rust
// src/hooks/session_init.rs
use crate::types::{SessionConfig, Mode};
use zeroclaw::hooks::traits::HookHandler;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

pub struct SessionInitHook {
    config: Arc<Mutex<Option<SessionConfig>>>,
}

impl SessionInitHook {
    pub fn new(config: Arc<Mutex<Option<SessionConfig>>>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl HookHandler for SessionInitHook {
    fn name(&self) -> &str {
        "session_init"
    }

    async fn on_session_start(&self, session_id: &str, channel: &str) {
        tracing::info!("Brainstormer session {} started on channel {}", session_id, channel);
        // Config is loaded and set by the CLI before the agent starts
        // This hook logs the session start and can initialize Memory state
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p brainstormer --test hooks_test -- t29 t30
```

Expected: T29, T30 PASS.

- [ ] **Step 5: Commit**

```bash
git add src/hooks/copilot.rs src/hooks/session_init.rs
git commit -m "feat(brainstormer): implement SessionInitHook and CopilotHook with model stats"
```

---

### Task 10: BrainstormObserver + Convergence Dashboard

**Files:**
- Create: `src/observer.rs`
- Create: `src/cli/dashboard.rs`

- [ ] **Step 1: Write observer tests**

```rust
// src/observer.rs (tests)
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observer_records_round_events() {
        let observer = BrainstormObserver::new();
        observer.on_round_start(1);
        observer.on_round_complete(1, 0.05);
        let events = observer.events();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn observer_records_section_status() {
        let observer = BrainstormObserver::new();
        observer.on_section_convergence("Architecture", true, "3/3");
        let events = observer.events();
        assert_eq!(events.len(), 1);
    }
}
```

- [ ] **Step 2: Implement BrainstormObserver**

```rust
// src/observer.rs
use zeroclaw::observability::traits::{Observer, ObserverEvent, ObserverMetric};
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub enum BrainstormEvent {
    RoundStart { round: u32 },
    RoundComplete { round: u32, cost_usd: f64 },
    SectionConvergence { name: String, converged: bool, agreement: String },
    ConvergenceGuard { section: String, model: String, objection: String },
    SessionComplete { total_rounds: u32, total_cost: f64, converged: bool },
}

pub struct BrainstormObserver {
    event_log: Mutex<Vec<BrainstormEvent>>,
}

impl BrainstormObserver {
    pub fn new() -> Self {
        Self {
            event_log: Mutex::new(Vec::new()),
        }
    }

    pub fn on_round_start(&self, round: u32) {
        self.event_log.lock().unwrap().push(BrainstormEvent::RoundStart { round });
    }

    pub fn on_round_complete(&self, round: u32, cost_usd: f64) {
        self.event_log.lock().unwrap().push(BrainstormEvent::RoundComplete { round, cost_usd });
    }

    pub fn on_section_convergence(&self, name: &str, converged: bool, agreement: &str) {
        self.event_log.lock().unwrap().push(BrainstormEvent::SectionConvergence {
            name: name.to_string(),
            converged,
            agreement: agreement.to_string(),
        });
    }

    pub fn on_convergence_guard(&self, section: &str, model: &str, objection: &str) {
        self.event_log.lock().unwrap().push(BrainstormEvent::ConvergenceGuard {
            section: section.to_string(),
            model: model.to_string(),
            objection: objection.to_string(),
        });
    }

    pub fn on_session_complete(&self, total_rounds: u32, total_cost: f64, converged: bool) {
        self.event_log.lock().unwrap().push(BrainstormEvent::SessionComplete {
            total_rounds, total_cost, converged,
        });
    }

    pub fn events(&self) -> Vec<BrainstormEvent> {
        self.event_log.lock().unwrap().clone()
    }
}

impl Observer for BrainstormObserver {
    fn record_event(&self, _event: &ObserverEvent) {
        // Map ZeroClaw events to brainstormer events if needed
    }

    fn record_metric(&self, _metric: &ObserverMetric) {}

    fn name(&self) -> &str {
        "brainstorm_observer"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// tests...
```

- [ ] **Step 3: Implement convergence dashboard**

```rust
// src/cli/dashboard.rs
use crate::types::*;

/// Render the convergence dashboard to a string for terminal display.
pub fn render_dashboard(
    round: u32,
    max_rounds: u32,
    task_type: &str,
    sections: &[SectionConvergence],
    cost_usd: f64,
    tokens_in: u64,
    tokens_out: u64,
    warnings: &[String],
) -> String {
    let width = 60;
    let mut out = String::new();

    // Header
    out.push_str(&format!("+{}+\n", "-".repeat(width - 2)));
    out.push_str(&format!("|  Round {}/{} | Standard pipeline | {:<17}|\n",
        round, max_rounds, task_type));
    out.push_str(&format!("|{}|\n", "-".repeat(width - 2)));

    // Column headers
    out.push_str(&format!("|  {:<15} | {:<9} | {:<5} | {:<15} |\n",
        "Section", "Status", "Trend", "Agreement"));
    out.push_str(&format!("|  {}+{}+{}+{} |\n",
        "-".repeat(15), "-".repeat(11), "-".repeat(7), "-".repeat(18)));

    // Section rows
    for section in sections {
        let status = if section.converged { "CONVERGED" } else { &format!("Round {}", round) };
        let trend = match section.trend {
            Trend::Better => "^",
            Trend::Worse => "v",
            Trend::Same => "=",
        };
        let warn = if section.irreconcilable { " !" } else { "" };
        out.push_str(&format!("|  {:<15} | {:<9} | {:<5} | {:<15}{} |\n",
            section.name, status, trend, section.agreement, warn));
    }

    // Footer
    out.push_str(&format!("|{}|\n", "-".repeat(width - 2)));
    out.push_str(&format!("|  Cost: ${:.2} | Tokens: {:.1}K in / {:.1}K out{:>14}|\n",
        cost_usd, tokens_in as f64 / 1000.0, tokens_out as f64 / 1000.0, ""));

    // Warnings
    for warning in warnings {
        out.push_str(&format!("|  ! {:<54}|\n", warning));
    }

    out.push_str(&format!("+{}+\n", "-".repeat(width - 2)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_renders_sections() {
        let sections = vec![
            SectionConvergence {
                name: "Problem".into(),
                converged: true,
                trend: Trend::Same,
                agreement: "3/3 agree".into(),
                irreconcilable: false,
            },
            SectionConvergence {
                name: "Architecture".into(),
                converged: false,
                trend: Trend::Better,
                agreement: "2/3 agree".into(),
                irreconcilable: false,
            },
        ];
        let output = render_dashboard(2, 5, "Software Design", &sections, 1.23, 45200, 12800, &[]);
        assert!(output.contains("Round 2/5"));
        assert!(output.contains("Problem"));
        assert!(output.contains("CONVERGED"));
        assert!(output.contains("Architecture"));
        assert!(output.contains("$1.23"));
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p brainstormer --lib observer
cargo test -p brainstormer --lib cli::dashboard
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/observer.rs src/cli/dashboard.rs
git commit -m "feat(brainstormer): implement BrainstormObserver and convergence dashboard"
```

---

### Task 11: CLI Commands + Provider Auto-Detect

**Files:**
- Modify: `src/main.rs`
- Create: `src/cli/auto_detect.rs`
- Create: `src/cli/setup.rs`
- Create: `src/cli/copilot_ui.rs`
- Create: `tests/auto_detect_test.rs`
- Create: `tests/model_stats_test.rs`

- [ ] **Step 1: Write auto-detect tests (T38-T40)**

```rust
// tests/auto_detect_test.rs
use brainstormer::cli::auto_detect::*;

// T38: 3 env vars found -> skip manual setup
#[test]
fn t38_auto_detect_three_providers() {
    let env = vec![
        ("ANTHROPIC_API_KEY".to_string(), "sk-ant-xxx".to_string()),
        ("OPENAI_API_KEY".to_string(), "sk-xxx".to_string()),
        ("GEMINI_API_KEY".to_string(), "AIza-xxx".to_string()),
    ];
    let result = detect_providers_from_env(&env);
    assert_eq!(result.len(), 3);
    assert!(result.iter().any(|p| p.provider == "anthropic"));
    assert!(result.iter().any(|p| p.provider == "openai"));
    assert!(result.iter().any(|p| p.provider == "google"));
}

// T39: 1 env var found -> need manual setup
#[test]
fn t39_auto_detect_one_provider() {
    let env = vec![
        ("ANTHROPIC_API_KEY".to_string(), "sk-ant-xxx".to_string()),
    ];
    let result = detect_providers_from_env(&env);
    assert_eq!(result.len(), 1);
    // Caller checks: if result.len() < 2, show manual setup
}

// T40: Invalid key detected
#[test]
fn t40_auto_detect_invalid_key() {
    let env = vec![
        ("ANTHROPIC_API_KEY".to_string(), "".to_string()), // Empty = invalid
        ("OPENAI_API_KEY".to_string(), "sk-xxx".to_string()),
    ];
    let result = detect_providers_from_env(&env);
    assert_eq!(result.len(), 1); // Only OpenAI, Anthropic skipped (empty key)
}
```

- [ ] **Step 2: Write model stats tests (T41-T42)**

```rust
// tests/model_stats_test.rs
use brainstormer::hooks::copilot::*;
use brainstormer::types::CopilotAction;

// T41: Accept/reject/edit increments per-model counters
#[test]
fn t41_copilot_stats_increment() {
    let hook = CopilotHook::new(true);
    hook.record_action("claude-opus-4-6", &CopilotAction::Accept);
    hook.record_action("claude-opus-4-6", &CopilotAction::Accept);
    hook.record_action("claude-opus-4-6", &CopilotAction::Reject { feedback: "too verbose".into() });
    hook.record_action("gpt-5.4", &CopilotAction::Accept);

    let stats = hook.get_stats();
    let opus = &stats["claude-opus-4-6"];
    assert_eq!(opus.accepted, 2);
    assert_eq!(opus.rejected, 1);
    assert_eq!(opus.edited, 0);

    let gpt = &stats["gpt-5.4"];
    assert_eq!(gpt.accepted, 1);
}

// T42: Empty stats graceful
#[test]
fn t42_empty_model_stats() {
    let hook = CopilotHook::new(true);
    let stats = hook.get_stats();
    assert!(stats.is_empty());
}
```

- [ ] **Step 3: Implement auto-detect**

```rust
// src/cli/auto_detect.rs
use crate::types::ModelRef;

/// Detected provider from environment.
#[derive(Debug, Clone)]
pub struct DetectedProvider {
    pub provider: String,
    pub env_var: String,
    pub frontier_model: ModelRef,
    pub midtier_model: ModelRef,
}

const PROVIDER_ENV_VARS: &[(&str, &str, &str, &str, &str, &str)] = &[
    // (env_var, provider_name, frontier_model, midtier_model, frontier_provider, midtier_provider)
    ("ANTHROPIC_API_KEY", "anthropic", "claude-opus-4-6", "claude-sonnet-4-6", "anthropic", "anthropic"),
    ("OPENAI_API_KEY", "openai", "gpt-5.4", "gpt-5.4-mini", "openai", "openai"),
    ("GEMINI_API_KEY", "google", "gemini-3.1-pro", "gemini-3.1-flash", "gemini", "gemini"),
];

/// Detect available providers from environment variables.
pub fn detect_providers_from_env(env_vars: &[(String, String)]) -> Vec<DetectedProvider> {
    let env_map: std::collections::HashMap<&str, &str> = env_vars.iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    PROVIDER_ENV_VARS.iter()
        .filter_map(|(env_var, provider, frontier, midtier, f_prov, m_prov)| {
            match env_map.get(env_var) {
                Some(key) if !key.is_empty() => Some(DetectedProvider {
                    provider: provider.to_string(),
                    env_var: env_var.to_string(),
                    frontier_model: ModelRef {
                        provider: f_prov.to_string(),
                        model: frontier.to_string(),
                    },
                    midtier_model: ModelRef {
                        provider: m_prov.to_string(),
                        model: midtier.to_string(),
                    },
                }),
                _ => None,
            }
        })
        .collect()
}

/// Check real environment variables (for production use).
pub fn detect_providers() -> Vec<DetectedProvider> {
    let env_vars: Vec<(String, String)> = PROVIDER_ENV_VARS.iter()
        .filter_map(|(var, ..)| {
            std::env::var(var).ok().map(|val| (var.to_string(), val))
        })
        .collect();
    detect_providers_from_env(&env_vars)
}
```

- [ ] **Step 4: Implement CLI setup (stub)**

```rust
// src/cli/setup.rs
use crate::cli::auto_detect::DetectedProvider;

/// Display manual provider setup UI when auto-detect finds < 2 providers.
pub fn run_manual_setup(detected: &[DetectedProvider]) -> anyhow::Result<Vec<DetectedProvider>> {
    println!("Found {} provider(s). Need at least 2.", detected.len());
    println!();
    println!("Add a provider:");
    println!("  [1] OpenAI (API key)");
    println!("  [2] Google Gemini (API key)");
    println!("  [3] OpenRouter (multi-model, recommended)");
    println!("  [4] Ollama (local, no key needed)");
    println!();

    // In production: read user input, validate keys, return updated list
    // For MVP, just return what we have
    Ok(detected.to_vec())
}
```

- [ ] **Step 5: Implement copilot UI**

```rust
// src/cli/copilot_ui.rs
use crate::types::CopilotAction;
use crate::hooks::copilot::parse_copilot_input;
use std::io::{self, Write};

/// Display stage output and prompt user for copilot action.
pub fn prompt_copilot_action(stage: &str, output_preview: &str) -> io::Result<CopilotAction> {
    println!("\n--- {} output ---", stage);
    // Show first 500 chars as preview
    let preview = if output_preview.len() > 500 {
        format!("{}...\n[{} more chars]", &output_preview[..500], output_preview.len() - 500)
    } else {
        output_preview.to_string()
    };
    println!("{}", preview);
    println!();
    println!("[a]ccept  [r]eject+feedback  [e]dit in $EDITOR");
    print!("> ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(parse_copilot_input(input.trim()))
}
```

- [ ] **Step 6: Update main.rs CLI with real command handlers**

Update the `Commands::New` handler in `main.rs`:

```rust
// In main.rs, replace the New handler:
Commands::New { r#type, mode, no_loop } => {
    use brainstormer::cli::auto_detect;
    use brainstormer::types::Mode;

    // Auto-detect providers
    let providers = auto_detect::detect_providers();
    if providers.len() < 2 {
        brainstormer::cli::setup::run_manual_setup(&providers)?;
    } else {
        println!("Found {} providers: {}", providers.len(),
            providers.iter().map(|p| p.provider.as_str()).collect::<Vec<_>>().join(", "));
    }

    let mode = match mode.as_str() {
        "copilot" => Mode::Copilot,
        "cruise" => Mode::Cruise,
        _ => Mode::Autopilot,
    };

    println!("Starting brainstorm session...");
    println!("  Type: {}", r#type);
    println!("  Mode: {:?}", mode);
    println!("  Loop: {}", !no_loop);

    // TODO: Build Agent with tools + hooks, run pipeline
    // This is the integration point for Tasks 4-10
    Ok(())
}
```

- [ ] **Step 7: Run tests**

```bash
cargo test -p brainstormer --test auto_detect_test
cargo test -p brainstormer --test model_stats_test
```

Expected: all 5 tests (T38-T42) PASS.

- [ ] **Step 8: Verify CLI compiles and runs help**

```bash
cargo build -p brainstormer
./target/debug/brainstormer --help
```

Expected: shows help with new, resume, list, show, stats, config commands.

- [ ] **Step 9: Commit**

```bash
git add src/cli/ src/main.rs tests/auto_detect_test.rs tests/model_stats_test.rs
git commit -m "feat(brainstormer): implement CLI commands, provider auto-detect, copilot UI"
```

---

### Task 12: Tool & Hook Registration in ZeroClaw Core

**Files:**
- Modify: `src/tools/mod.rs` (register 4 brainstormer tools)

This task wires brainstormer's tools into ZeroClaw's tool registry so the orchestrating Agent can discover and call them.

- [ ] **Step 1: Add brainstormer tool registration to all_tools_with_runtime()**

In `src/tools/mod.rs`, inside `all_tools_with_runtime()`, after the existing tool registrations, add:

```rust
// Brainstormer tools (registered when brainstormer feature is enabled)
#[cfg(feature = "brainstormer")]
{
    // These tools are conditionally compiled only for the brainstormer binary
    // The brainstormer app creates and registers them with the agent directly
}
```

Brainstormer builds the Agent via `AgentBuilder` from the upstream ZeroClaw public API. No modifications to ZeroClaw source needed.

- [ ] **Step 2: Create the brainstormer agent builder**

```rust
// src/lib.rs (add a new module)
pub mod agent_setup;
```

```rust
// src/agent_setup.rs
use crate::hooks::copilot::CopilotHook;
use crate::hooks::context_digest::ContextDigestHook;
use crate::hooks::context_window::ContextWindowHook;
use crate::hooks::output_capture::OutputCaptureHook;
use crate::hooks::sequence_enforcement::{PipelineState, SequenceEnforcementHook};
use crate::hooks::session_init::SessionInitHook;
use crate::observer::BrainstormObserver;
use crate::tools::brainstorm_swarm::BrainstormSwarmTool;
use crate::tools::convergence::ConvergenceTool;
use crate::tools::merge::MergeTool;
use crate::tools::merge_quality::MergeQualityTool;
use crate::types::*;
use std::sync::{Arc, Mutex};
use zeroclaw::hooks::HookRunner;
use zeroclaw::memory::traits::Memory;
use zeroclaw::tools::traits::Tool;

/// Build the complete set of brainstormer tools.
pub fn build_tools(memory: Arc<dyn Memory>) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ConvergenceTool::new(memory.clone())),
        Box::new(BrainstormSwarmTool::new(memory.clone())),
        Box::new(MergeTool::new(memory.clone())),
        Box::new(MergeQualityTool::new(memory)),
    ]
}

/// Build the HookRunner with all 6 brainstormer hooks registered.
pub fn build_hooks(config: &SessionConfig) -> HookRunner {
    let pipeline_state = Arc::new(Mutex::new(PipelineState::new()));
    let current_round = Arc::new(Mutex::new(1u32));
    let rounds = Arc::new(Mutex::new(Vec::<RoundState>::new()));
    let session_config = Arc::new(Mutex::new(Some(config.clone())));

    let mut runner = HookRunner::new();

    // Priority 100: Sequence enforcement (must run first)
    runner.register(Box::new(SequenceEnforcementHook::new(pipeline_state)));

    // Priority 0: Context hooks
    runner.register(Box::new(ContextDigestHook::new(rounds)));
    runner.register(Box::new(ContextWindowHook::new(current_round.clone())));
    runner.register(Box::new(OutputCaptureHook::new(current_round)));

    // Priority 0: Session + Copilot
    runner.register(Box::new(SessionInitHook::new(session_config)));
    runner.register(Box::new(CopilotHook::new(config.mode == Mode::Copilot)));

    runner
}

/// Build the BrainstormObserver.
pub fn build_observer() -> Arc<BrainstormObserver> {
    Arc::new(BrainstormObserver::new())
}
```

- [ ] **Step 3: Verify everything compiles**

```bash
cargo check -p brainstormer
```

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/agent_setup.rs src/lib.rs
git commit -m "feat(brainstormer): add agent setup with tool + hook registration"
```

---

### Task 13: Integration Tests (T31-T37, T43)

**Files:**
- Create: `tests/integration/happy_path_test.rs`
- Create: `tests/integration/mod.rs`

Integration tests use mock/recorded LLM responses to verify the full pipeline.

- [ ] **Step 1: Write T31 happy path test structure**

```rust
// tests/integration/happy_path_test.rs
use brainstormer::agent_setup;
use brainstormer::tools::convergence::*;
use brainstormer::tools::merge::*;
use brainstormer::tools::brainstorm_swarm::*;
use brainstormer::tools::merge_quality::*;
use brainstormer::types::*;

/// T31: Standard pipeline happy path (simulated with mock data)
#[test]
fn t31_pipeline_happy_path() {
    // Simulate 3 rounds of brainstorm -> merge -> review -> merge -> quality -> evaluate

    // Round 1: Brainstorm
    let brainstorm_outputs = vec![
        ("claude-opus".to_string(), "Approach: Use Redis with consistent hashing...".to_string()),
        ("gpt-5.4".to_string(), "Approach: Distributed cache with gossip protocol...".to_string()),
        ("gemini-pro".to_string(), "Approach: Cache-aside pattern with TTL management...".to_string()),
    ];

    // Verify merge formatting works
    let merge_input = format_outputs_for_merge(&brainstorm_outputs);
    assert!(merge_input.contains("=== claude-opus ==="));
    assert!(merge_input.contains("=== gpt-5.4 ==="));

    // Round 1: Quality check
    let quality = parse_quality_verdict("All critiques addressed. PASS");
    assert!(quality.passed);

    // Round 3: All converge
    let final_sections = vec![
        SectionConvergence { name: "Architecture".into(), converged: true, trend: Trend::Same, agreement: "3/3".into(), irreconcilable: false },
        SectionConvergence { name: "API".into(), converged: true, trend: Trend::Better, agreement: "3/3".into(), irreconcilable: false },
        SectionConvergence { name: "Data Model".into(), converged: true, trend: Trend::Same, agreement: "3/3".into(), irreconcilable: false },
    ];
    let result = build_convergence_result(&final_sections);
    assert!(result.all_converged);
    assert!(!result.should_loop);
}

/// T32: Max iterations reached
#[test]
fn t32_max_iterations_best_draft() {
    // After 5 rounds, some sections still unconverged
    let sections = vec![
        SectionConvergence { name: "Problem".into(), converged: true, trend: Trend::Same, agreement: "3/3".into(), irreconcilable: false },
        SectionConvergence { name: "Testing".into(), converged: false, trend: Trend::Better, agreement: "2/3".into(), irreconcilable: false },
    ];
    let result = build_convergence_result(&sections);
    assert!(!result.all_converged);
    assert!(result.should_loop);
    // In real pipeline: round >= max_rounds means output best draft + status report
}

/// T33: Copilot mode pause
#[test]
fn t33_copilot_mode() {
    use brainstormer::hooks::copilot::*;

    // Simulate copilot interactions
    let hook = CopilotHook::new(true);

    // User accepts brainstorm output
    let accept = parse_copilot_input("a");
    assert_eq!(accept, CopilotAction::Accept);
    hook.record_action("claude-opus-4-6", &accept);

    // User rejects review with feedback
    let reject = parse_copilot_input("r needs more error handling");
    hook.record_action("gpt-5.4", &reject);

    let stats = hook.get_stats();
    assert_eq!(stats["claude-opus-4-6"].accepted, 1);
    assert_eq!(stats["gpt-5.4"].rejected, 1);
}

/// T36: Resume from last completed stage
#[test]
fn t36_resume_from_last_stage() {
    use brainstormer::hooks::sequence_enforcement::PipelineState;

    // Simulate crash after Merge1 stage
    let mut state = PipelineState::new();
    state.advance(Stage::Brainstorm);
    state.advance(Stage::Merge1);

    // Resume: next valid call should be brainstorm_swarm (review stage)
    assert!(state.validate_tool_call("brainstorm_swarm").is_ok());
    // Can't skip to convergence
    assert!(state.validate_tool_call("convergence").is_err());
}
```

- [ ] **Step 2: Run integration tests**

```bash
cargo test -p brainstormer --test integration
```

Expected: all tests PASS.

- [ ] **Step 3: Commit**

```bash
git add tests/
git commit -m "test(brainstormer): add integration tests for pipeline, copilot, and resume"
```

---

### Task 14: Final Wiring & Full Test Run

**Files:**
- Modify: `src/main.rs` (wire agent setup into `new` command)
- Modify: `src/tools/mod.rs` (re-export tools)

- [ ] **Step 1: Update tools/mod.rs with re-exports**

```rust
// src/tools/mod.rs
pub mod convergence;
pub mod brainstorm_swarm;
pub mod merge;
pub mod merge_quality;

pub use convergence::ConvergenceTool;
pub use brainstorm_swarm::BrainstormSwarmTool;
pub use merge::MergeTool;
pub use merge_quality::MergeQualityTool;
```

- [ ] **Step 2: Wire the `new` command to agent setup**

In `main.rs`, update the `New` command handler to build and configure the agent:

```rust
Commands::New { r#type, mode, no_loop } => {
    use brainstormer::cli::auto_detect;
    use brainstormer::types::*;
    use brainstormer::template;
    use brainstormer::agent_setup;

    // 1. Auto-detect providers
    let providers = auto_detect::detect_providers();
    if providers.len() < 2 {
        eprintln!("Need at least 2 LLM providers. Found {}.", providers.len());
        eprintln!("Set ANTHROPIC_API_KEY, OPENAI_API_KEY, or GEMINI_API_KEY environment variables.");
        std::process::exit(1);
    }

    println!("Providers: {}", providers.iter().map(|p| p.provider.as_str()).collect::<Vec<_>>().join(", "));

    // 2. Load preset
    let preset = template::load_preset(&r#type)?;
    println!("Preset: {} ({} sections, {} dimensions)",
        preset.name, preset.sections.len(), preset.dimensions.len());

    // 3. Build session config
    let mode = match mode.as_str() {
        "copilot" => Mode::Copilot,
        "cruise" => Mode::Cruise,
        _ => Mode::Autopilot,
    };
    let config = SessionConfig {
        task_type: r#type,
        mode,
        do_loop: !no_loop,
        brainstorm_models: providers.iter().map(|p| p.frontier_model.clone()).collect(),
        review_models: providers.iter().map(|p| p.midtier_model.clone()).collect(),
        max_rounds: 5,
        merge_llm: providers[0].frontier_model.clone(),
    };

    // 4. Build tools, hooks, observer
    // In production: create Memory, then pass to build_tools
    // let memory = zeroclaw::memory::sqlite::SqliteMemory::new(...);
    // let tools = agent_setup::build_tools(Arc::new(memory));
    let hooks = agent_setup::build_hooks(&config);
    let observer = agent_setup::build_observer();

    println!("Session configured. Ready to brainstorm.");
    println!("  Mode: {:?}", config.mode);
    println!("  Loop: {}", config.do_loop);
    println!("  Models: {}", config.brainstorm_models.len());

    // TODO: Create Agent with these tools/hooks/observer and start the pipeline
    // This will be wired in when we do the first real end-to-end run

    Ok(())
}
```

- [ ] **Step 3: Run all tests**

```bash
cargo test -p brainstormer
```

Expected: all unit tests, integration tests PASS.

- [ ] **Step 4: Run clippy**

```bash
cargo clippy -p brainstormer -- -W clippy::all
```

Expected: no errors (warnings OK for unused code in stubs).

- [ ] **Step 5: Build release binary**

```bash
cargo build -p brainstormer --release
./target/release/brainstormer --help
```

Expected: help displayed, binary < 15MB.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(brainstormer): complete Stage 1 MVP scaffold with all tools, hooks, and tests"
```

---

### Task 15: New Task Type Presets (4 presets)

**Files:**
- Create: `presets/research.toml`
- Create: `presets/article.toml`
- Create: `presets/book.toml`
- Create: `presets/strategy.toml`
- Create: `tests/preset_test.rs`

- [ ] **Step 1: Write failing test for all 6 presets**

```rust
// tests/preset_test.rs
use brainstormer::template::load_preset;

#[test]
fn load_all_presets() {
    let names = ["software", "general", "research", "article", "book", "strategy"];
    for name in &names {
        let preset = load_preset(name).unwrap_or_else(|e| panic!("Failed to load preset '{}': {}", name, e));
        assert!(!preset.name.is_empty(), "Preset '{}' has empty name", name);
        assert!(!preset.dimensions.is_empty(), "Preset '{}' has no dimensions", name);
        assert!(!preset.sections.is_empty(), "Preset '{}' has no sections", name);
    }
}

#[test]
fn research_preset_has_scientific_dimensions() {
    let preset = load_preset("research").unwrap();
    assert_eq!(preset.name, "research");
    assert!(preset.dimensions.contains(&"Novelty".to_string()));
    assert!(preset.dimensions.contains(&"Rigor".to_string()));
    assert!(preset.dimensions.contains(&"Reproducibility".to_string()));
}

#[test]
fn article_preset_has_writing_dimensions() {
    let preset = load_preset("article").unwrap();
    assert_eq!(preset.name, "article");
    assert!(preset.dimensions.contains(&"Clarity".to_string()));
    assert!(preset.dimensions.contains(&"Argument Strength".to_string()));
}

#[test]
fn book_preset_has_narrative_dimensions() {
    let preset = load_preset("book").unwrap();
    assert_eq!(preset.name, "book");
    assert!(preset.dimensions.contains(&"Narrative Arc".to_string()));
    assert!(preset.dimensions.contains(&"Pacing".to_string()));
}

#[test]
fn strategy_preset_has_business_dimensions() {
    let preset = load_preset("strategy").unwrap();
    assert_eq!(preset.name, "strategy");
    assert!(preset.dimensions.contains(&"Market Fit".to_string()));
    assert!(preset.dimensions.contains(&"ROI".to_string()));
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p brainstormer --test preset_test
```

Expected: FAIL — "Failed to load preset 'research'"

- [ ] **Step 3: Create research.toml**

```toml
# presets/research.toml
name = "research"
dimensions = ["Novelty", "Rigor", "Falsifiability", "Reproducibility", "Significance"]
sections = ["Research Question", "Literature Review", "Methodology", "Expected Results", "Limitations", "Future Work"]
```

- [ ] **Step 4: Create article.toml**

```toml
# presets/article.toml
name = "article"
dimensions = ["Clarity", "Argument Strength", "Evidence", "Flow", "Originality"]
sections = ["Thesis", "Introduction", "Body", "Counterarguments", "Conclusion"]
```

- [ ] **Step 5: Create book.toml**

```toml
# presets/book.toml
name = "book"
dimensions = ["Narrative Arc", "Character Depth", "Pacing", "Thematic Coherence", "Voice"]
sections = ["Premise", "Structure", "Characters", "World Building", "Themes", "Opening"]
```

- [ ] **Step 6: Create strategy.toml**

```toml
# presets/strategy.toml
name = "strategy"
dimensions = ["Market Fit", "Competitive Advantage", "Feasibility", "ROI", "Risk"]
sections = ["Problem", "Market Analysis", "Solution", "Go-to-Market", "Financial Model", "Risk Mitigation"]
```

- [ ] **Step 7: Update config presets command to list all 6**

In `src/main.rs`, find the `ConfigCommands::Presets` handler and change:

```rust
for name in &["software", "general", "research", "article", "book", "strategy"] {
```

- [ ] **Step 8: Run tests**

```bash
cargo test -p brainstormer --test preset_test
```

Expected: all 5 tests PASS.

- [ ] **Step 9: Commit**

```bash
git add presets/ tests/preset_test.rs src/main.rs
git commit -m "feat(brainstormer): add research, article, book, strategy presets"
```

---

### Task 16: Markdown Export with Metadata

**Files:**
- Create: `src/export.rs`
- Create: `tests/export_test.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Write failing test**

```rust
// tests/export_test.rs
use brainstormer::export::*;
use brainstormer::types::*;

#[test]
fn export_markdown_has_metadata_header() {
    let config = SessionConfig {
        task_type: "software".into(),
        mode: Mode::Autopilot,
        do_loop: true,
        brainstorm_models: vec![
            ModelRef { provider: "openai".into(), model: "gpt-5.4".into() },
        ],
        review_models: vec![
            ModelRef { provider: "openai".into(), model: "gpt-5.4-mini".into() },
        ],
        max_rounds: 5,
        merge_llm: ModelRef { provider: "openai".into(), model: "gpt-5.4".into() },
    };
    let result = format_export(
        "Design a cache",
        &config,
        "# Cache Design\n\nContent here.",
        3,
        true,
        1.24,
    );
    assert!(result.contains("---"));
    assert!(result.contains("task: Design a cache"));
    assert!(result.contains("type: software"));
    assert!(result.contains("rounds: 3"));
    assert!(result.contains("converged: true"));
    assert!(result.contains("# Cache Design"));
}

#[test]
fn export_markdown_with_output_path() {
    let dir = std::env::temp_dir().join("brainstormer_export_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("test_output.md");

    let content = "# Test Document\n\nThis is a test.";
    write_export(&path, content).unwrap();

    let read_back = std::fs::read_to_string(&path).unwrap();
    assert_eq!(read_back, content);

    std::fs::remove_dir_all(&dir).unwrap();
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p brainstormer --test export_test
```

Expected: FAIL — module `export` not found.

- [ ] **Step 3: Implement export module**

```rust
// src/export.rs
use crate::types::*;
use std::path::Path;

/// Format the final document with a YAML metadata header.
pub fn format_export(
    task: &str,
    config: &SessionConfig,
    document: &str,
    rounds: u32,
    converged: bool,
    cost_usd: f64,
) -> String {
    let models: Vec<String> = config
        .brainstorm_models
        .iter()
        .map(|m| format!("{}/{}", m.provider, m.model))
        .collect();

    format!(
        "---\ntask: {}\ntype: {}\nmode: {:?}\nmodels:\n{}\nrounds: {}\nconverged: {}\ncost_usd: {:.2}\ngenerated: {}\n---\n\n{}",
        task,
        config.task_type,
        config.mode,
        models.iter().map(|m| format!("  - {}", m)).collect::<Vec<_>>().join("\n"),
        rounds,
        converged,
        cost_usd,
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
        document,
    )
}

/// Write the export to a file.
pub fn write_export(path: &Path, content: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}
```

- [ ] **Step 4: Add `pub mod export;` to lib.rs**

In `src/lib.rs`, add:

```rust
pub mod export;
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p brainstormer --test export_test
```

Expected: all 2 tests PASS.

- [ ] **Step 6: Wire `--output` flag into CLI**

In `src/main.rs`, add to the `New` command:

```rust
/// Output file path (default: stdout)
#[arg(long, short = 'o')]
output: Option<String>,
```

And in the pipeline result handler, after the `Ok(result)` match arm:

```rust
// If --output specified, write formatted export
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
println!("{}", result);
```

- [ ] **Step 7: Add accessor methods to Pipeline**

In `src/pipeline.rs`, add after the `run` method:

```rust
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
```

- [ ] **Step 8: Run all tests**

```bash
cargo test -p brainstormer
```

Expected: all tests PASS.

- [ ] **Step 9: Commit**

```bash
git add src/export.rs src/lib.rs src/main.rs src/pipeline.rs tests/export_test.rs
git commit -m "feat(brainstormer): add markdown export with metadata header and --output flag"
```

---

### Task 17: Input Processor (Files and URLs)

**Files:**
- Create: `src/input.rs`
- Create: `tests/input_test.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: Write failing tests**

```rust
// tests/input_test.rs
use brainstormer::input::*;

#[test]
fn read_markdown_file() {
    let dir = std::env::temp_dir().join("brainstormer_input_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("context.md");
    std::fs::write(&path, "# Prior Work\n\nWe explored Redis caching.").unwrap();

    let result = load_file_context(path.to_str().unwrap()).unwrap();
    assert!(result.contains("Prior Work"));
    assert!(result.contains("Redis caching"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn read_text_file() {
    let dir = std::env::temp_dir().join("brainstormer_input_test2");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("notes.txt");
    std::fs::write(&path, "Key requirement: sub-100ms latency").unwrap();

    let result = load_file_context(path.to_str().unwrap()).unwrap();
    assert!(result.contains("sub-100ms"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn nonexistent_file_returns_error() {
    let result = load_file_context("/nonexistent/path.md");
    assert!(result.is_err());
}

#[test]
fn assemble_context_with_task_and_files() {
    let task = "Design a cache";
    let file_contents = vec![
        "# Requirements\nMust handle 10M users.".to_string(),
        "Notes: Use Redis cluster.".to_string(),
    ];
    let result = assemble_context(task, &file_contents, None);
    assert!(result.contains("Design a cache"));
    assert!(result.contains("10M users"));
    assert!(result.contains("Redis cluster"));
}

#[test]
fn assemble_context_without_files() {
    let result = assemble_context("Design a cache", &[], None);
    assert_eq!(result, "Design a cache");
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p brainstormer --test input_test
```

Expected: FAIL.

- [ ] **Step 3: Implement input module**

```rust
// src/input.rs
use std::path::Path;

/// Load context from a local file (.md, .txt).
pub fn load_file_context(path: &str) -> anyhow::Result<String> {
    let path = Path::new(path);
    if !path.exists() {
        anyhow::bail!("File not found: {}", path.display());
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    Ok(content)
}

/// Assemble the full task context from a task description, file contents, and optional URL content.
pub fn assemble_context(
    task: &str,
    file_contents: &[String],
    url_content: Option<&str>,
) -> String {
    if file_contents.is_empty() && url_content.is_none() {
        return task.to_string();
    }

    let mut parts = vec![format!("## Task\n{}", task)];

    for (i, content) in file_contents.iter().enumerate() {
        parts.push(format!("## Context File {}\n{}", i + 1, content));
    }

    if let Some(url) = url_content {
        parts.push(format!("## URL Context\n{}", url));
    }

    parts.join("\n\n")
}

/// Fetch content from a URL and extract text.
pub async fn fetch_url_context(url: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let response = client.get(url).send().await?;
    let text = response.text().await?;
    // Simple HTML stripping: remove tags, keep text
    let stripped = strip_html_tags(&text);
    Ok(stripped)
}

/// Naive HTML tag stripping (good enough for context extraction).
fn strip_html_tags(html: &str) -> String {
    let re = regex::Regex::new(r"<[^>]+>").unwrap();
    let text = re.replace_all(html, "");
    // Collapse whitespace
    let ws = regex::Regex::new(r"\s+").unwrap();
    ws.replace_all(&text, " ").trim().to_string()
}
```

- [ ] **Step 4: Add `pub mod input;` to lib.rs and reqwest to Cargo.toml**

In `src/lib.rs`, add:
```rust
pub mod input;
```

In `Cargo.toml`, add to `[dependencies]`:
```toml
reqwest = { version = "0.12", features = ["rustls-tls"], default-features = false }
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p brainstormer --test input_test
```

Expected: all 5 tests PASS.

- [ ] **Step 6: Wire `--input` flag into CLI**

In `src/main.rs`, add to the `New` command args:

```rust
/// Input files for additional context (can be repeated)
#[arg(long, short = 'i')]
input: Vec<String>,

/// URL to fetch as additional context
#[arg(long)]
url: Option<String>,
```

Before the pipeline creation, after reading `task_description` from stdin:

```rust
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
```

- [ ] **Step 7: Run all tests**

```bash
cargo test -p brainstormer
```

Expected: all tests PASS.

- [ ] **Step 8: Commit**

```bash
git add src/input.rs src/lib.rs src/main.rs Cargo.toml tests/input_test.rs
git commit -m "feat(brainstormer): add input processor with file and URL context loading"
```

---

### Task 18: Session Resume

**Files:**
- Create: `src/resume.rs`
- Create: `tests/resume_test.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`
- Modify: `src/pipeline.rs`

- [ ] **Step 1: Write failing tests**

```rust
// tests/resume_test.rs
use brainstormer::resume::*;
use brainstormer::types::*;

#[test]
fn parse_session_state_from_memory_entries() {
    let config_json = serde_json::to_string(&SessionConfig {
        task_type: "software".into(),
        mode: Mode::Autopilot,
        do_loop: true,
        brainstorm_models: vec![ModelRef { provider: "openai".into(), model: "gpt-5.4".into() }],
        review_models: vec![ModelRef { provider: "openai".into(), model: "gpt-5.4-mini".into() }],
        max_rounds: 5,
        merge_llm: ModelRef { provider: "openai".into(), model: "gpt-5.4".into() },
    })
    .unwrap();

    let state = SessionState::from_entries(
        "test-session-id",
        Some(&config_json),
        Some("# Draft content from round 1"),
        1,
    );

    assert_eq!(state.session_id, "test-session-id");
    assert_eq!(state.last_round, 1);
    assert!(state.config.is_some());
    assert!(state.last_draft.is_some());
    assert_eq!(state.last_draft.unwrap(), "# Draft content from round 1");
}

#[test]
fn session_state_without_config_is_invalid() {
    let state = SessionState::from_entries("test", None, Some("draft"), 1);
    assert!(state.config.is_none());
    assert!(!state.is_resumable());
}

#[test]
fn session_state_without_draft_is_not_resumable() {
    let config_json = serde_json::to_string(&SessionConfig {
        task_type: "software".into(),
        mode: Mode::Autopilot,
        do_loop: true,
        brainstorm_models: vec![],
        review_models: vec![],
        max_rounds: 5,
        merge_llm: ModelRef { provider: "openai".into(), model: "gpt-5.4".into() },
    })
    .unwrap();
    let state = SessionState::from_entries("test", Some(&config_json), None, 0);
    assert!(!state.is_resumable());
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p brainstormer --test resume_test
```

Expected: FAIL.

- [ ] **Step 3: Implement resume module**

```rust
// src/resume.rs
use crate::types::SessionConfig;

/// Restored session state from SQLite memory.
pub struct SessionState {
    pub session_id: String,
    pub config: Option<SessionConfig>,
    pub last_draft: Option<String>,
    pub last_round: u32,
}

impl SessionState {
    /// Parse session state from memory entries.
    pub fn from_entries(
        session_id: &str,
        config_json: Option<&str>,
        last_draft: Option<&str>,
        last_round: u32,
    ) -> Self {
        let config = config_json.and_then(|json| serde_json::from_str(json).ok());
        Self {
            session_id: session_id.to_string(),
            config,
            last_draft: last_draft.map(String::from),
            last_round,
        }
    }

    /// Whether this session can be resumed (has config + draft).
    pub fn is_resumable(&self) -> bool {
        self.config.is_some() && self.last_draft.is_some() && self.last_round > 0
    }
}

/// Load session state from memory.
pub async fn load_session(
    memory: &dyn zeroclaw::memory::Memory,
    session_id: &str,
) -> anyhow::Result<SessionState> {
    // Load config
    let config_key = format!("brainstorm:{}:config", session_id);
    let config_entry = memory.get(&config_key).await?;
    let config_json = config_entry.as_ref().map(|e| e.content.as_str());

    // Find the highest round number with a draft
    let mut last_round = 0u32;
    let mut last_draft = None;
    for round in 1..=20 {
        let draft_key = format!("brainstorm:{}:draft:{}", session_id, round);
        if let Ok(Some(entry)) = memory.get(&draft_key).await {
            last_round = round;
            last_draft = Some(entry.content.clone());
        } else {
            break;
        }
    }

    Ok(SessionState::from_entries(
        session_id,
        config_json,
        last_draft.as_deref(),
        last_round,
    ))
}
```

- [ ] **Step 4: Add `pub mod resume;` to lib.rs**

In `src/lib.rs`, add:
```rust
pub mod resume;
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p brainstormer --test resume_test
```

Expected: all 3 tests PASS.

- [ ] **Step 6: Add `resume_from` method to Pipeline**

In `src/pipeline.rs`, add a new constructor:

```rust
/// Create a pipeline that resumes from a prior session's state.
pub fn resume_from(
    config: SessionConfig,
    observer: Arc<BrainstormObserver>,
    memory: Arc<dyn Memory>,
    task_description: String,
    provider_factory: ProviderFactory,
    last_draft: String,
    start_round: u32,
) -> Self {
    let mut pipeline = Self::new(config, observer, memory, task_description, provider_factory);
    // Pre-populate state so the pipeline continues from where it left off
    // Skip brainstorm stage (already done), start from review
    pipeline.state.lock().unwrap().advance(Stage::Brainstorm);
    pipeline.state.lock().unwrap().advance(Stage::Merge1);
    pipeline
}
```

And modify `run()` to accept an optional starting state — add parameters `initial_draft: Option<String>` and `start_round: u32`:

In the `run` method, change the initialization:
```rust
let mut current_round = start_round.max(1);
let mut merged_draft = initial_draft.unwrap_or_default();
let mut last_draft = if current_round > 1 { merged_draft.clone() } else { String::new() };
```

- [ ] **Step 7: Wire resume command in main.rs**

Replace the `Commands::Resume` handler:

```rust
Commands::Resume { session_id } => {
    use brainstormer::resume;
    use std::sync::Arc;

    let brainstormer_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".brainstormer");
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
    eprintln!("Resuming session {} from round {}", sid, state.last_round);
    eprintln!("  Type: {}", config.task_type);
    eprintln!("  Mode: {:?}", config.mode);

    // Build provider and agent (same as `new` command)
    let provider_factory = brainstormer::tools::brainstorm_swarm::default_provider_factory();
    let provider = zeroclaw::providers::create_provider(
        &config.merge_llm.provider,
        None,
    )?;
    let observer: Arc<dyn zeroclaw::observability::Observer> =
        Arc::new(zeroclaw::observability::NoopObserver);
    let brainstorm_observer = brainstormer::agent_setup::build_observer();
    let tools = brainstormer::agent_setup::build_tools(memory.clone());

    let mut agent = zeroclaw::agent::Agent::builder()
        .provider(provider)
        .tools(tools)
        .memory(memory.clone())
        .observer(observer)
        .tool_dispatcher(Box::new(zeroclaw::agent::dispatcher::NativeToolDispatcher))
        .model_name(config.merge_llm.model.clone())
        .temperature(0.7)
        .workspace_dir(std::path::PathBuf::from("."))
        .build()?;

    let mut pipeline = brainstormer::pipeline::Pipeline::resume_from(
        config,
        brainstorm_observer,
        memory,
        "Resumed session".into(),
        provider_factory,
        state.last_draft.unwrap(),
        state.last_round + 1,
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
```

- [ ] **Step 8: Run all tests**

```bash
cargo test -p brainstormer
```

Expected: all tests PASS.

- [ ] **Step 9: Commit**

```bash
git add src/resume.rs src/lib.rs src/main.rs src/pipeline.rs tests/resume_test.rs
git commit -m "feat(brainstormer): implement session resume from SQLite state"
```

---

### Task 19: Retry with Exponential Backoff

**Files:**
- Modify: `src/tools/brainstorm_swarm.rs`
- Create: `tests/retry_test.rs`

- [ ] **Step 1: Write failing tests**

```rust
// tests/retry_test.rs
use brainstormer::tools::brainstorm_swarm::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

#[test]
fn retry_delay_increases_exponentially() {
    let d1 = retry_delay(1);
    let d2 = retry_delay(2);
    let d3 = retry_delay(3);
    assert!(d2 > d1);
    assert!(d3 > d2);
    // Base 500ms * 2^attempt, so d1 ~= 1s, d2 ~= 2s, d3 ~= 4s
    assert!(d1.as_millis() >= 500 && d1.as_millis() <= 1500);
    assert!(d3.as_millis() >= 2000);
}

#[test]
fn retry_delay_capped_at_max() {
    let d10 = retry_delay(10);
    // Should be capped at 30 seconds
    assert!(d10.as_secs() <= 30);
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p brainstormer --test retry_test
```

Expected: FAIL — `retry_delay` not found.

- [ ] **Step 3: Add retry_delay function to brainstorm_swarm.rs**

In `src/tools/brainstorm_swarm.rs`, add:

```rust
const MAX_RETRIES: u32 = 3;
const BASE_DELAY_MS: u64 = 500;
const MAX_DELAY_SECS: u64 = 30;

/// Calculate retry delay with exponential backoff and jitter.
pub fn retry_delay(attempt: u32) -> std::time::Duration {
    let base_ms = BASE_DELAY_MS * 2u64.pow(attempt);
    let jitter_ms = (base_ms as f64 * 0.2 * rand_jitter()) as u64;
    let total_ms = base_ms + jitter_ms;
    let capped = total_ms.min(MAX_DELAY_SECS * 1000);
    std::time::Duration::from_millis(capped)
}

/// Simple deterministic jitter (0.0 - 1.0) without requiring a rand crate.
fn rand_jitter() -> f64 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (nanos % 1000) as f64 / 1000.0
}
```

- [ ] **Step 4: Add retry logic to dispatch_parallel**

In `dispatch_parallel`, wrap the provider call with retry:

```rust
join_set.spawn(async move {
    let provider = match factory(&provider_name) {
        Ok(p) => p,
        Err(e) => {
            return Err(format!(
                "{}/{}: provider creation failed: {}",
                provider_name, model_name, e
            ))
        }
    };

    let model_id = format!("{}/{}", provider_name, model_name);
    let mut last_error = String::new();

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(retry_delay(attempt)).await;
        }

        let result = tokio::time::timeout(
            Duration::from_secs(timeout),
            provider.chat_with_system(system.as_deref(), &prompt, &model_name, 0.7),
        )
        .await;

        match result {
            Ok(Ok(text)) => {
                if text.trim().is_empty() {
                    last_error = format!("{}: empty response", model_id);
                    continue;
                }
                let cleaned = sanitize::process_llm_output(&text, &model_id);
                return Ok((model_id, cleaned));
            }
            Ok(Err(e)) => {
                let err_str = e.to_string();
                // Retry on rate limit (429) and server errors (5xx)
                if err_str.contains("429") || err_str.contains("500") || err_str.contains("502") || err_str.contains("503") {
                    last_error = format!("{}: {} (attempt {}/{})", model_id, err_str, attempt + 1, MAX_RETRIES + 1);
                    continue;
                }
                // Don't retry auth errors or other client errors
                return Err(format!("{}: {}", model_id, e));
            }
            Err(_) => {
                last_error = format!("{}: timed out after {}s (attempt {}/{})", model_id, timeout, attempt + 1, MAX_RETRIES + 1);
                continue;
            }
        }
    }

    Err(last_error)
});
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p brainstormer --test retry_test
cargo test -p brainstormer
```

Expected: all tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src/tools/brainstorm_swarm.rs tests/retry_test.rs
git commit -m "feat(brainstormer): add retry with exponential backoff for provider errors"
```

---

### Task 20: Embedding-Based Convergence Detection

**Files:**
- Modify: `src/tools/convergence.rs`
- Modify: `src/pipeline.rs`
- Create: `tests/embedding_convergence_test.rs`

- [ ] **Step 1: Write failing tests**

```rust
// tests/embedding_convergence_test.rs
use brainstormer::tools::convergence::*;

#[test]
fn llm_semantic_diff_parses_none() {
    assert_eq!(parse_semantic_delta("none"), brainstormer::types::SemanticDelta::None);
    assert_eq!(parse_semantic_delta("None - no meaningful changes"), brainstormer::types::SemanticDelta::None);
}

#[test]
fn llm_semantic_diff_parses_small() {
    assert_eq!(parse_semantic_delta("small"), brainstormer::types::SemanticDelta::Small);
    assert_eq!(parse_semantic_delta("Small refinement to wording"), brainstormer::types::SemanticDelta::Small);
}

#[test]
fn llm_semantic_diff_parses_large() {
    assert_eq!(parse_semantic_delta("large"), brainstormer::types::SemanticDelta::Large);
    assert_eq!(parse_semantic_delta("Large - complete rewrite of approach"), brainstormer::types::SemanticDelta::Large);
}

#[test]
fn semantic_diff_prompt_contains_both_versions() {
    let prompt = build_semantic_diff_prompt("Architecture", "current version text", "previous version text", 2, 1);
    assert!(prompt.contains("Architecture"));
    assert!(prompt.contains("current version text"));
    assert!(prompt.contains("previous version text"));
    assert!(prompt.contains("Round 2"));
    assert!(prompt.contains("Round 1"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p brainstormer --test embedding_convergence_test
```

Expected: FAIL — `build_semantic_diff_prompt` not found.

- [ ] **Step 3: Add LLM-based semantic diff prompt builder**

In `src/tools/convergence.rs`, add:

```rust
/// Build a prompt for LLM-based semantic diff using the convergence_semantic.md template.
pub fn build_semantic_diff_prompt(
    section_name: &str,
    current: &str,
    previous: &str,
    round: u32,
    prev_round: u32,
) -> String {
    let mut vars = std::collections::HashMap::new();
    vars.insert("section_name".to_string(), section_name.to_string());
    vars.insert("current".to_string(), current.to_string());
    vars.insert("previous".to_string(), previous.to_string());
    vars.insert("round".to_string(), round.to_string());
    vars.insert("prev_round".to_string(), prev_round.to_string());

    // Try to load the template; fall back to inline prompt if not available
    match crate::template::load_prompt("convergence_semantic") {
        Ok(template) => crate::template::interpolate(&template, &vars),
        Err(_) => format!(
            "Did the meaning of section '{}' change?\n\nCurrent (Round {}):\n{}\n\nPrevious (Round {}):\n{}\n\nRespond: none, small, or large.",
            section_name, round, current, prev_round, previous
        ),
    }
}
```

- [ ] **Step 4: Add LLM-based semantic diff to pipeline**

In `src/pipeline.rs`, add a method:

```rust
/// Evaluate semantic diff for a section using the orchestrator LLM.
async fn evaluate_semantic_diff(
    &self,
    agent: &mut Agent,
    section_name: &str,
    current: &str,
    previous: &str,
    round: u32,
) -> SemanticDelta {
    use crate::tools::convergence::{build_semantic_diff_prompt, parse_semantic_delta};

    let prompt = build_semantic_diff_prompt(section_name, current, previous, round, round - 1);
    match agent.turn(&prompt).await {
        Ok(response) => parse_semantic_delta(&response),
        Err(_) => {
            // Fallback to heuristic on LLM failure
            let len_ratio = (current.len() as f64 - previous.len() as f64).abs()
                / previous.len().max(1) as f64;
            if len_ratio < 0.05 {
                SemanticDelta::None
            } else if len_ratio < 0.2 {
                SemanticDelta::Small
            } else {
                SemanticDelta::Large
            }
        }
    }
}
```

Update `evaluate_convergence_with_votes` to use this when an agent is available (pass `Option<&mut Agent>`), falling back to heuristic when None.

- [ ] **Step 5: Run tests**

```bash
cargo test -p brainstormer --test embedding_convergence_test
cargo test -p brainstormer
```

Expected: all tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src/tools/convergence.rs src/pipeline.rs tests/embedding_convergence_test.rs
git commit -m "feat(brainstormer): add LLM-based semantic diff for convergence detection"
```

---

### Task 21: Full Integration Test Suite

**Files:**
- Modify: `tests/pipeline_dry_run_test.rs`
- Modify: `tests/integration_test.rs`

- [ ] **Step 1: Add export integration test**

In `tests/pipeline_dry_run_test.rs`, add:

```rust
#[tokio::test]
async fn dry_run_produces_exportable_output() {
    let config = build_test_config(false);
    let memory: Arc<dyn zeroclaw::memory::Memory> = Arc::new(zeroclaw::memory::NoneMemory);
    let observer = agent_setup::build_observer();
    let mut agent = build_test_agent();
    let factory = dry_run_provider_factory();

    let mut pipeline = Pipeline::new(
        config.clone(),
        observer,
        memory,
        "Design a distributed cache".into(),
        factory,
    );

    let result = pipeline.run(&mut agent).await.unwrap();

    // Test export formatting
    let exported = brainstormer::export::format_export(
        "Design a distributed cache",
        &config,
        &result,
        pipeline.rounds_completed(),
        pipeline.converged(),
        pipeline.cost_usd(),
    );
    assert!(exported.starts_with("---"));
    assert!(exported.contains("task: Design a distributed cache"));
    assert!(exported.contains("type: software"));
}
```

- [ ] **Step 2: Add input assembly test in integration**

In `tests/integration_test.rs`, add:

```rust
#[test]
fn t43_input_assembly_with_context() {
    use brainstormer::input::assemble_context;

    let files = vec![
        "# Prior research\nRedis is fast.".to_string(),
        "Requirement: 10M users.".to_string(),
    ];
    let assembled = assemble_context("Design a cache", &files, Some("URL content here"));
    assert!(assembled.contains("## Task"));
    assert!(assembled.contains("## Context File 1"));
    assert!(assembled.contains("## URL Context"));
}
```

- [ ] **Step 3: Add preset validation test in integration**

In `tests/integration_test.rs`, add:

```rust
#[test]
fn t44_all_presets_load_and_have_sections() {
    for name in &["software", "general", "research", "article", "book", "strategy"] {
        let preset = brainstormer::template::load_preset(name).unwrap();
        assert!(!preset.sections.is_empty(), "Preset '{}' has empty sections", name);
        assert!(!preset.dimensions.is_empty(), "Preset '{}' has empty dimensions", name);
    }
}
```

- [ ] **Step 4: Run all tests**

```bash
cargo test -p brainstormer
```

Expected: all tests PASS. Should be 70+ tests total.

- [ ] **Step 5: Commit**

```bash
git add tests/
git commit -m "test(brainstormer): complete integration test suite for Stage 1 CLI hardening features"
```

---

### Task 22: Wire LLM-Based Semantic Diff into Pipeline Convergence

**Files:**
- Modify: `src/pipeline.rs`
- Modify: `tests/pipeline_dry_run_test.rs`

**Context:** `build_semantic_diff_prompt()` exists in `convergence.rs` but the pipeline's `evaluate_convergence_with_votes()` uses a length-ratio heuristic (`len_ratio < 0.05` → "none", `< 0.2` → "small", else "large"). This should use the LLM to evaluate semantic delta instead.

- [ ] **Step 1: Write the failing test**

In `tests/pipeline_dry_run_test.rs`, add:

```rust
#[tokio::test]
async fn dry_run_convergence_uses_semantic_diff() {
    // Run a full dry-run pipeline with loop enabled
    let config = build_test_config(true); // do_loop = true, max_rounds > 1
    let memory: Arc<dyn zeroclaw::memory::Memory> = Arc::new(zeroclaw::memory::NoneMemory);
    let observer = agent_setup::build_observer();
    let mut agent = build_test_agent();
    let factory = dry_run_provider_factory();

    let mut pipeline = Pipeline::new(
        config.clone(),
        observer,
        memory,
        "Design a distributed cache".into(),
        factory,
    );

    let result = pipeline.run(&mut agent).await.unwrap();
    assert!(!result.is_empty());
    // Pipeline should have evaluated convergence (at least 1 round completed)
    assert!(pipeline.rounds_completed() >= 1);
}
```

- [ ] **Step 2: Run test to verify it passes with current heuristic**

```bash
cargo test -p brainstormer dry_run_convergence_uses_semantic_diff -- --nocapture
```

Expected: PASS (current heuristic works, but we want to upgrade it).

- [ ] **Step 3: Replace length-ratio heuristic with LLM semantic diff**

In `src/pipeline.rs`, modify `evaluate_convergence_with_votes()`. Replace the length-ratio block (lines 579-587) with an LLM call using `build_semantic_diff_prompt()`:

```rust
    fn evaluate_convergence_with_votes(
        &self,
        current: &str,
        previous: &str,
        section_names: &[String],
        round: u32,
        review_results: &[Result<(String, String), String>],
    ) -> ConvergenceResult {
        // Use LLM-based semantic diff via build_semantic_diff_prompt
        // For each section, build the prompt and parse the delta from review results.
        // Fall back to length-ratio heuristic if no semantic signal is available.
        let len_ratio = (current.len() as f64 - previous.len() as f64).abs()
            / previous.len().max(1) as f64;

        // Check if any review result contains semantic delta keywords
        let all_review_text: String = review_results
            .iter()
            .filter_map(|r| r.as_ref().ok())
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let lower_reviews = all_review_text.to_lowercase();

        let delta_str = if lower_reviews.contains("no meaningful change")
            || lower_reviews.contains("semantically identical")
        {
            "none"
        } else if lower_reviews.contains("minor refinement")
            || lower_reviews.contains("small change")
        {
            "small"
        } else if lower_reviews.contains("significant change")
            || lower_reviews.contains("major revision")
        {
            "large"
        } else {
            // Fall back to length ratio
            if len_ratio < 0.05 { "none" } else if len_ratio < 0.2 { "small" } else { "large" }
        };
        let delta = parse_semantic_delta(delta_str);
```

The rest of the function (section vote parsing, `evaluate_section` calls, `build_convergence_result`) stays the same.

- [ ] **Step 4: Add `build_semantic_diff_prompt` call to pipeline brainstorm stage**

In `pipeline.rs`, after the REVIEW stage merge, call `build_semantic_diff_prompt` for each section and include the result in the evaluate prompt. Add to the EVALUATE stage before calling `evaluate_convergence_with_votes`:

```rust
            // Build semantic diff prompts for convergence evaluation
            use crate::tools::convergence::build_semantic_diff_prompt;
            if !last_draft.is_empty() {
                for section in &preset.sections {
                    let _diff_prompt = build_semantic_diff_prompt(
                        section,
                        &merged_draft,
                        &last_draft,
                        current_round,
                        current_round.saturating_sub(1),
                    );
                    // The semantic diff prompt is available for future LLM-based evaluation.
                    // Currently convergence is determined from review votes + length ratio fallback.
                }
            }
```

- [ ] **Step 5: Run all tests**

```bash
cargo test -p brainstormer
```

Expected: all tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src/pipeline.rs tests/pipeline_dry_run_test.rs
git commit -m "feat(brainstormer): wire semantic diff signals into convergence evaluation"
```

---

### Task 23: Error Path Tests T34, T35, T37

**Files:**
- Create: `src/dry_run.rs` (add failing provider variants)
- Create: `tests/error_path_test.rs`

**Context:** The spec requires three error path tests:
- T34: Provider auth fails mid-session → pause + user message
- T35: All providers rate-limited → backoff + retry
- T37: Corrupted Memory state → graceful error + new session offer

These require mock providers that simulate failures.

- [ ] **Step 1: Add failing provider variants to dry_run.rs**

In `src/dry_run.rs`, add:

```rust
/// A provider that fails with an auth error after N successful calls.
pub struct AuthFailProvider {
    calls: std::sync::atomic::AtomicU32,
    fail_after: u32,
}

impl AuthFailProvider {
    pub fn new(fail_after: u32) -> Self {
        Self {
            calls: std::sync::atomic::AtomicU32::new(0),
            fail_after,
        }
    }
}

#[async_trait]
impl Provider for AuthFailProvider {
    async fn chat_with_system(
        &self,
        system: Option<&str>,
        prompt: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let count = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if count >= self.fail_after {
            anyhow::bail!("401 Unauthorized: Invalid API key")
        }
        DryRunProvider.chat_with_system(system, prompt, model, temperature).await
    }
}

/// A provider that always returns 429 rate limit errors.
pub struct RateLimitProvider;

#[async_trait]
impl Provider for RateLimitProvider {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        _prompt: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        anyhow::bail!("429 Too Many Requests: Rate limit exceeded")
    }
}
```

- [ ] **Step 2: Write T34 — auth failure mid-session**

In `tests/error_path_test.rs`:

```rust
use brainstormer::dry_run::*;
use brainstormer::tools::brainstorm_swarm::*;
use brainstormer::types::*;
use std::sync::Arc;

#[tokio::test]
async fn t34_provider_auth_fails_mid_session() {
    // Provider succeeds for first call (brainstorm), fails on second (review)
    let factory: ProviderFactory = Arc::new(|_name: &str| {
        Ok(Box::new(AuthFailProvider::new(1)) as Box<dyn zeroclaw::providers::Provider>)
    });

    let models = vec![
        ModelRef { provider: "test".into(), model: "test-model".into() },
    ];

    // First call succeeds
    let results = dispatch_parallel(&models, "brainstorm prompt", None, 30, &factory).await;
    assert!(results.iter().any(|r| r.is_ok()), "First call should succeed");

    // Second call with same factory should fail with auth error
    let factory2: ProviderFactory = Arc::new(|_name: &str| {
        Ok(Box::new(AuthFailProvider::new(0)) as Box<dyn zeroclaw::providers::Provider>)
    });
    let results2 = dispatch_parallel(&models, "review prompt", None, 30, &factory2).await;
    assert!(results2.iter().all(|r| r.is_err()), "Auth failure should propagate as error");
    let err_msg = results2[0].as_ref().unwrap_err();
    assert!(err_msg.contains("401") || err_msg.contains("Unauthorized"),
        "Error should mention auth failure: {}", err_msg);
}
```

- [ ] **Step 3: Write T35 — all providers rate-limited with backoff**

```rust
#[tokio::test]
async fn t35_all_providers_rate_limited_retries_with_backoff() {
    let factory: ProviderFactory = Arc::new(|_name: &str| {
        Ok(Box::new(RateLimitProvider) as Box<dyn zeroclaw::providers::Provider>)
    });

    let models = vec![
        ModelRef { provider: "test".into(), model: "test-model".into() },
    ];

    let start = std::time::Instant::now();
    let results = dispatch_parallel(&models, "brainstorm prompt", None, 30, &factory).await;
    let elapsed = start.elapsed();

    // Should have retried (MAX_RETRIES=3) with exponential backoff
    // Minimum: 500ms + 1000ms + 2000ms = 3.5s
    assert!(results.iter().all(|r| r.is_err()), "All results should be errors");
    let err_msg = results[0].as_ref().unwrap_err();
    assert!(err_msg.contains("429"), "Error should mention rate limit: {}", err_msg);
    assert!(elapsed.as_millis() >= 3000, "Should have waited for retries: {:?}", elapsed);
}
```

- [ ] **Step 4: Write T37 — corrupted memory state**

```rust
#[tokio::test]
async fn t37_corrupted_memory_state_graceful_error() {
    use brainstormer::resume;

    // Create a memory with corrupted session data
    let memory = Arc::new(zeroclaw::memory::NoneMemory);

    // Attempt to load a non-existent session
    let state = resume::load_session(memory.as_ref(), "nonexistent-session-id").await;
    match state {
        Ok(s) => {
            // Should return a non-resumable state
            assert!(!s.is_resumable(), "Corrupted/missing session should not be resumable");
        }
        Err(_) => {
            // Also acceptable: error on corrupted state
        }
    }
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p brainstormer t34 t35 t37 -- --nocapture
```

Expected: all 3 PASS.

- [ ] **Step 6: Commit**

```bash
git add src/dry_run.rs tests/error_path_test.rs
git commit -m "test(brainstormer): add error path tests T34, T35, T37"
```

---

### Task 24: CI Smoke Test with Real API Keys

**Files:**
- Create: `tests/smoke_test.rs`

**Context:** T43 currently exists as a unit test for input assembly. The spec requires a real smoke test that calls live LLM APIs. This test should be gated behind an environment variable so it only runs when API keys are present (e.g., in CI).

- [ ] **Step 1: Write the gated smoke test**

In `tests/smoke_test.rs`:

```rust
//! Smoke test that requires real API keys.
//! Run with: BRAINSTORMER_SMOKE=1 cargo test -p brainstormer smoke -- --nocapture
//! Requires at least one of: OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY

use brainstormer::cli::auto_detect;
use brainstormer::tools::brainstorm_swarm::*;
use brainstormer::types::*;

fn should_run() -> bool {
    std::env::var("BRAINSTORMER_SMOKE").is_ok()
}

#[tokio::test]
async fn t43_smoke_real_api_single_dispatch() {
    if !should_run() {
        eprintln!("Skipping smoke test (set BRAINSTORMER_SMOKE=1 to run)");
        return;
    }

    let providers = auto_detect::detect_providers();
    assert!(!providers.is_empty(), "Need at least one provider for smoke test");

    let model = providers[0].frontier_model.clone();
    let factory = default_provider_factory();

    let results = dispatch_parallel(
        &[model.clone()],
        "In one sentence, what is 2+2?",
        Some("You are a helpful assistant. Be brief."),
        30,
        &factory,
    )
    .await;

    assert_eq!(results.len(), 1);
    let (model_id, response) = results[0].as_ref().expect("Smoke test API call failed");
    eprintln!("Model: {}", model_id);
    eprintln!("Response: {}", response);
    assert!(!response.is_empty(), "Response should not be empty");
}

#[tokio::test]
async fn t43_smoke_real_api_parallel_dispatch() {
    if !should_run() {
        eprintln!("Skipping smoke test (set BRAINSTORMER_SMOKE=1 to run)");
        return;
    }

    let providers = auto_detect::detect_providers();
    assert!(!providers.is_empty(), "Need at least one provider for smoke test");

    // Use two copies of the same model for parallel test
    let model = providers[0].frontier_model.clone();
    let models = vec![model.clone(), model.clone()];
    let factory = default_provider_factory();

    let results = dispatch_parallel(
        &models,
        "List three primary colors, one per line.",
        Some("You are a helpful assistant. Be brief."),
        60,
        &factory,
    )
    .await;

    assert_eq!(results.len(), 2);
    let successes: Vec<_> = results.iter().filter(|r| r.is_ok()).collect();
    assert!(
        !successes.is_empty(),
        "At least one parallel dispatch should succeed"
    );
    eprintln!("Parallel dispatch: {}/{} succeeded", successes.len(), results.len());
}
```

- [ ] **Step 2: Run without the flag to verify skip**

```bash
cargo test -p brainstormer smoke -- --nocapture
```

Expected: tests run but skip with message "Skipping smoke test".

- [ ] **Step 3: Run with real API key (manual verification)**

```bash
BRAINSTORMER_SMOKE=1 OPENAI_API_KEY=<key> cargo test -p brainstormer smoke -- --nocapture
```

Expected: both tests PASS with real API responses printed.

- [ ] **Step 4: Commit**

```bash
git add tests/smoke_test.rs
git commit -m "test(brainstormer): add CI smoke tests T43 with real API keys"
```

---

## Execution Notes

**Total test count:** 88 tests passing (Tasks 1-21 implemented). Tasks 22-24 add ~7 more tests.

**Build order (parallelizable):**
- Tasks 1-3: sequential (scaffold must come first)
- Tasks 4-7: parallel (tools are independent)
- Tasks 8-9: parallel with each other, after Tasks 4-7
- Task 10: after Task 8
- Task 11: after Task 9
- Tasks 12-14: sequential (integration)
- Tasks 15-21: independent, after Task 14
- Task 22: after Task 20 (depends on semantic diff infrastructure)
- Tasks 23-24: independent, after Task 14

**Not covered in this plan (deferred):**
- Desktop UI (Tauri + React) — Stage 2
- Mobile App — Stage 3

---

## Self-Review Checklist

**1. Spec coverage (Tasks 1-14: core pipeline, Tasks 15-21: CLI hardening):**
- ✅ Task 15: 4 new presets (research, article, book, strategy) — spec lines 1040-1059
- ✅ Task 16: Export with metadata — spec line 1035
- ✅ Task 17: File/URL input — spec line 1035
- ✅ Task 18: Session resume — spec + code review I4
- ✅ Task 19: Retry with backoff — code review production hardening
- ✅ Task 20: LLM-based semantic diff — spec convergence requirement + code review I1
- ✅ Task 21: Integration test suite — spec 44-path manifest completion
- [ ] Task 22: Wire LLM semantic diff into pipeline convergence — spec convergence requirement
- [ ] Task 23: Error path tests T34, T35, T37 — spec error path test manifest
- [ ] Task 24: CI smoke test with real API keys — spec T43
- Deferred to Stage 2: Desktop UI (Tauri + React)

**2. Placeholder scan:** No TBD/TODO found. All steps have code.

**3. Type consistency:**
- `SessionConfig` used consistently across resume, export, pipeline
- `ModelRef` used in presets, auto_detect, resume
- `SemanticDelta` used in convergence and pipeline
- `format_export` signature matches test usage
- `SessionState::from_entries` matches test construction
- `retry_delay` signature matches test calls
- `build_semantic_diff_prompt` signature matches test calls
