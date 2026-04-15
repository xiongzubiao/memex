use regex::Regex;
use std::sync::LazyLock;

static SYSTEM_PROMPT_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r"(?im)^system:\s*.+$").unwrap(),
        Regex::new(r"(?is)<\|im_start\|>system.*?<\|im_end\|>").unwrap(),
        Regex::new(r"(?is)\[INST\].*?\[/INST\]").unwrap(),
        // Only strip role prefixes that look like chat turn markers (followed by newline or short content)
        Regex::new(r"(?im)^(Human|Assistant|User):\s*$").unwrap(),
    ]
});

/// Strip system prompt injection patterns from LLM output.
pub fn sanitize_llm_output(output: &str, _model_id: &str) -> String {
    let mut cleaned = output.to_string();
    for pattern in SYSTEM_PROMPT_PATTERNS.iter() {
        cleaned = pattern.replace_all(&cleaned, "").to_string();
    }
    cleaned
        .lines()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t11_strips_system_prompt_patterns() {
        let output =
            "Here is my response.\n\nSystem: You are a helpful assistant.\n\nMore content.";
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
