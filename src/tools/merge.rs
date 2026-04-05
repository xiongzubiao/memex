use crate::types::ModelRef;

/// Format multiple LLM outputs with model delimiters for merge input.
pub fn format_outputs_for_merge(outputs: &[(String, String)]) -> String {
    outputs
        .iter()
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
    let mut prompt = format!(
        "## Task\n{}\n\n## Outputs to Merge\n{}",
        task,
        format_outputs_for_merge(outputs)
    );
    if let Some(crits) = critiques
        && !crits.is_empty()
    {
        prompt.push_str("\n\n## Review Critiques to Incorporate\n");
        prompt.push_str(&format_outputs_for_merge(crits));
    }
    prompt
}

/// Select a fallback model different from the primary.
pub fn select_fallback_model<'a>(
    primary: &ModelRef,
    candidates: &'a [ModelRef],
) -> Option<&'a ModelRef> {
    candidates.iter().find(|m| m.model != primary.model)
}
