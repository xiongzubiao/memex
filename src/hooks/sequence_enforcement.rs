use crate::types::Stage;

pub const TOOL_BRAINSTORM: &str = "brainstorm_swarm";
pub const TOOL_MERGE: &str = "merge";
pub const TOOL_QUALITY: &str = "merge_quality";
pub const TOOL_CONVERGENCE: &str = "convergence";

/// Tracks pipeline state for sequence enforcement.
#[derive(Debug, Clone)]
pub struct PipelineState {
    current_stage: Option<Stage>,
    quality_passed: bool,
}

impl Default for PipelineState {
    fn default() -> Self {
        Self::new()
    }
}

impl PipelineState {
    pub fn new() -> Self {
        Self {
            current_stage: None,
            quality_passed: false,
        }
    }

    pub fn advance(&mut self, stage: Stage) {
        self.current_stage = Some(stage);
        if stage == Stage::QualityCheck {
            self.quality_passed = true;
        }
    }

    pub fn validate_tool_call(&self, tool_name: &str) -> Result<(), String> {
        match tool_name {
            TOOL_BRAINSTORM => match self.current_stage {
                None => Ok(()),
                Some(Stage::Merge1) => Ok(()),
                Some(Stage::Evaluate) => Ok(()),
                _ => Err(format!(
                    "{} not allowed after {:?}",
                    TOOL_BRAINSTORM, self.current_stage
                )),
            },
            TOOL_MERGE => match self.current_stage {
                Some(Stage::Brainstorm) => Ok(()),
                Some(Stage::Review) => Ok(()),
                _ => Err(format!("{} not allowed after {:?}", TOOL_MERGE, self.current_stage)),
            },
            TOOL_QUALITY => match self.current_stage {
                Some(Stage::Merge2) => Ok(()),
                _ => Err(format!(
                    "{} not allowed after {:?}",
                    TOOL_QUALITY, self.current_stage
                )),
            },
            TOOL_CONVERGENCE => {
                if !self.quality_passed {
                    Err(format!("{} not allowed until quality check passes", TOOL_CONVERGENCE))
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }
}
