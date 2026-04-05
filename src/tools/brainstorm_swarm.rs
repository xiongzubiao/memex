use crate::sanitize;
use crate::types::ModelRef;
use std::sync::Arc;
use std::time::Duration;

const MIN_MODELS: usize = 2;
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

/// Provider factory: creates a provider for a given name.
/// Override with a custom factory for dry-run/testing.
pub type ProviderFactory =
    Arc<dyn Fn(&str) -> anyhow::Result<Box<dyn zeroclaw::providers::Provider>> + Send + Sync>;

/// Default factory that uses ZeroClaw's provider registry.
pub fn default_provider_factory() -> ProviderFactory {
    Arc::new(|name: &str| zeroclaw::providers::create_provider(name, None))
}

/// Dispatch a prompt to multiple models in parallel, returning their outputs.
/// Each model gets its own provider instance. Results are sanitized and wrapped
/// with model delimiters.
pub async fn dispatch_parallel(
    models: &[ModelRef],
    prompt: &str,
    system_prompt: Option<&str>,
    timeout_secs: u64,
    provider_factory: &ProviderFactory,
) -> Vec<Result<(String, String), String>> {
    let mut join_set = tokio::task::JoinSet::new();

    for model in models {
        let provider_name = model.provider.clone();
        let model_name = model.model.clone();
        let prompt = prompt.to_string();
        let system = system_prompt.map(String::from);
        let timeout = timeout_secs;
        let factory = provider_factory.clone();

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
                        if err_str.contains("429")
                            || err_str.contains("500")
                            || err_str.contains("502")
                            || err_str.contains("503")
                        {
                            last_error = format!(
                                "{}: {} (attempt {}/{})",
                                model_id, err_str, attempt + 1, MAX_RETRIES + 1
                            );
                            continue;
                        }
                        return Err(format!("{}: {}", model_id, e));
                    }
                    Err(_) => {
                        last_error = format!(
                            "{}: timed out after {}s (attempt {}/{})",
                            model_id, timeout, attempt + 1, MAX_RETRIES + 1
                        );
                        continue;
                    }
                }
            }

            Err(last_error)
        });
    }

    let mut results = Vec::new();
    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok(result) => results.push(result),
            Err(e) => results.push(Err(format!("task join error: {}", e))),
        }
    }
    results
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
        Some(format!(
            "{} model failed, proceeding with {} results",
            failures.len(),
            outputs.len()
        ))
    };

    (outputs, warning)
}

/// Collect successful outputs from dispatch results into a merged string.
pub fn collect_outputs(results: &[Result<(String, String), String>]) -> (String, Vec<String>) {
    let mut combined = Vec::new();
    let mut warnings = Vec::new();

    for result in results {
        match result {
            Ok((model_id, output)) => {
                combined.push(format!("### {} response:\n{}", model_id, output));
            }
            Err(e) => {
                warnings.push(e.clone());
            }
        }
    }

    (combined.join("\n\n---\n\n"), warnings)
}
