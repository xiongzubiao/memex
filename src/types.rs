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

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Autopilot => "autopilot",
            Mode::Copilot => "copilot",
            Mode::Cruise => "cruise",
        }
    }
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

/// Preset definition loaded from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub dimensions: Vec<String>,
    pub sections: Vec<String>,
}

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
            cost: RoundCost {
                tokens_in: 1000,
                tokens_out: 500,
                usd: 0.05,
            },
            timestamp: Utc::now(),
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
            brainstorm_models: vec![ModelRef {
                provider: "anthropic".into(),
                model: "claude-opus-4-6".into(),
            }],
            review_models: vec![ModelRef {
                provider: "anthropic".into(),
                model: "claude-sonnet-4-6".into(),
            }],
            max_rounds: 5,
            merge_llm: ModelRef {
                provider: "anthropic".into(),
                model: "claude-opus-4-6".into(),
            },
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
