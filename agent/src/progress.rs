//! Brainstorm progress reporting via tool wrappers.
//!
//! The zeroclaw `Agent::turn()` code path does not emit `ToolCallStart` or
//! `LlmRequest` observer events, so we wrap the actual tools to print progress
//! before and after execution.

use async_trait::async_trait;
use std::time::Instant;
use zeroclaw::tools::{Tool, ToolResult};

/// A wrapper around any `Tool` that prints progress to stderr.
///
/// Before `execute()`, prints the step label (e.g., `[PROPOSE]`).
/// After `execute()`, prints completion with duration.
pub struct ProgressTool {
    inner: Box<dyn Tool>,
}

impl ProgressTool {
    pub fn wrap(tool: Box<dyn Tool>) -> Box<dyn Tool> {
        Box::new(Self { inner: tool })
    }

    /// Detect the brainstorm step from the tool name and arguments.
    fn print_start(&self, args: &serde_json::Value) {
        let name = self.inner.name();
        if name == "swarm" {
            if let Some(swarm) = args.get("swarm").and_then(|v| v.as_str()) {
                if swarm == "proposers" {
                    eprintln!("\n[PROPOSE] Dispatching to proposer swarm...");
                } else if swarm == "reviewers" {
                    eprintln!("\n[REVIEW] Dispatching to reviewer swarm...");
                } else {
                    eprintln!("\n[SWARM] Dispatching to {swarm} swarm...");
                }
            } else {
                eprintln!("\n[SWARM] Dispatching swarm...");
            }
        } else if name == "delegate" {
            let agent = args
                .get("agent")
                .and_then(|v| v.as_str())
                .unwrap_or("sub-agent");
            eprintln!("\n[DELEGATE] Delegating to {agent}...");
        } else if name == "memory_recall" {
            eprintln!("\n[GATHER] Recalling wiki knowledge...");
        } else if name == "memory_store" {
            eprintln!("\n[WRITE] Storing result to memex...");
        }
    }

    fn print_end(&self, duration_secs: f64, success: bool) {
        let name = self.inner.name();
        let label = match name {
            "swarm" => "Swarm",
            "delegate" => "Delegate",
            "memory_recall" => "Recall",
            "memory_store" => "Store",
            _ => return,
        };
        if success {
            eprintln!("[DONE] {label} complete ({duration_secs:.1}s)");
        } else {
            eprintln!("[FAIL] {label} failed ({duration_secs:.1}s)");
        }
    }
}

#[async_trait]
impl Tool for ProgressTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.print_start(&args);
        let start = Instant::now();
        let result = self.inner.execute(args).await;
        let secs = start.elapsed().as_secs_f64();
        match &result {
            Ok(r) => self.print_end(secs, r.success),
            Err(_) => self.print_end(secs, false),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyTool;

    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            "swarm"
        }
        fn description(&self) -> &str {
            "test swarm"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: "done".into(),
                error: None,
            })
        }
    }

    #[test]
    fn wrap_preserves_name() {
        let wrapped = ProgressTool::wrap(Box::new(DummyTool));
        assert_eq!(wrapped.name(), "swarm");
    }

    #[test]
    fn wrap_preserves_description() {
        let wrapped = ProgressTool::wrap(Box::new(DummyTool));
        assert_eq!(wrapped.description(), "test swarm");
    }

    #[tokio::test]
    async fn execute_delegates_and_succeeds() {
        let wrapped = ProgressTool::wrap(Box::new(DummyTool));
        let args = serde_json::json!({"swarm": "proposers", "task": "test"});
        let result = wrapped.execute(args).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "done");
    }
}
