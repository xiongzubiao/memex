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
        response
            .lines()
            .filter(|line| {
                line.trim_start().starts_with("- ") || line.trim_start().starts_with("* ")
            })
            .map(|line| {
                line.trim_start_matches("- ")
                    .trim_start_matches("* ")
                    .trim()
                    .to_string()
            })
            .collect()
    } else {
        vec![]
    };

    QualityVerdict {
        passed,
        dropped_critiques,
    }
}

/// Whether to proceed despite quality failure (at max attempts).
pub fn should_proceed_despite_failure(attempt: u32, max_attempts: u32) -> bool {
    attempt >= max_attempts
}
