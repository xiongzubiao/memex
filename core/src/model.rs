//! Model context window catalog — vendored from litellm.
//!
//! The catalog JSON is generated at build time by `build.rs` from
//! `data/litellm_raw.json`. It contains only direct (non-provider-prefixed)
//! chat-mode entries with `max_input_tokens` and `max_output_tokens`.
//!
//! # Usage
//!
//! ```rust
//! use memex_core::model::{lookup_model, compute_batch_budget};
//!
//! let info = lookup_model("claude-sonnet-4-6");
//! let budget = compute_batch_budget("claude-sonnet-4-6", 500, 2_000);
//! ```

use std::sync::OnceLock;

/// Rough bytes-per-token estimate used for budget calculations.
pub const BYTES_PER_TOKEN: usize = 4;

static CATALOG_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/model_catalog.json"));

/// Context window information for a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// Maximum number of input tokens the model accepts.
    pub max_input_tokens: usize,
    /// Maximum number of output tokens the model can produce.
    pub max_output_tokens: usize,
}

/// Fallback returned for unknown models: 128K input, 16K output.
pub const DEFAULT_MODEL_INFO: ModelInfo = ModelInfo {
    max_input_tokens: 128_000,
    max_output_tokens: 16_000,
};

/// Parsed catalog: map from model name to (max_input, max_output).
fn catalog() -> &'static std::collections::HashMap<String, (usize, usize)> {
    static CATALOG: OnceLock<std::collections::HashMap<String, (usize, usize)>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let parsed: serde_json::Value = serde_json::from_str(CATALOG_JSON)
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let mut map = std::collections::HashMap::new();
        if let Some(obj) = parsed.as_object() {
            for (key, val) in obj {
                let max_in = val
                    .get("max_input_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                let max_out = val
                    .get("max_output_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                if let (Some(i), Some(o)) = (max_in, max_out) {
                    map.insert(key.clone(), (i, o));
                }
            }
        }
        map
    })
}

/// Look up a model by name, returning its context window info.
///
/// Resolution order:
/// 1. Exact match (e.g. `openai/gpt-5.4-mini` or `gpt-5.4-mini`).
/// 2. Strip provider prefix and retry (e.g. `openai/gpt-5.4-mini` → `gpt-5.4-mini`).
/// 3. Longest prefix match (catalog key is a prefix of the model name).
/// 4. Fall back to [`DEFAULT_MODEL_INFO`].
///
/// Provider-prefixed entries are preferred over bare names because different
/// providers may have different context window limits for the same base model
/// (e.g. `azure/gpt-5.4-mini` has 1.05M input vs. bare `gpt-5.4-mini` at 272K).
///
/// After raw catalog lookup, [`apply_known_overrides`] derates entries whose
/// upstream value reflects an opt-in capability that isn't accessible by
/// default in memex's typical usage path.
pub fn lookup_model(model: &str) -> ModelInfo {
    let cat = catalog();

    // 1. Exact match (e.g. "azure/gpt-5.4-mini" with provider-specific limits)
    if let Some(&(max_input_tokens, max_output_tokens)) = cat.get(model) {
        return apply_known_overrides(
            model,
            ModelInfo {
                max_input_tokens,
                max_output_tokens,
            },
        );
    }

    // 2. Strip provider prefix (e.g. "openai/gpt-5.4-mini" → "gpt-5.4-mini").
    //    LiteLLM stores OpenAI models under bare names, not "openai/" prefixed.
    if let Some(slash_idx) = model.find('/') {
        let bare = &model[slash_idx + 1..];
        if let Some(&(max_input_tokens, max_output_tokens)) = cat.get(bare) {
            return apply_known_overrides(
                model,
                ModelInfo {
                    max_input_tokens,
                    max_output_tokens,
                },
            );
        }
    }

    // 3. Longest prefix match (catalog key is a prefix of the requested model name)
    let best = cat
        .iter()
        .filter(|(k, _)| model.starts_with(k.as_str()))
        .max_by_key(|(k, _)| k.len());

    if let Some((_, &(max_input_tokens, max_output_tokens))) = best {
        return apply_known_overrides(
            model,
            ModelInfo {
                max_input_tokens,
                max_output_tokens,
            },
        );
    }

    // 4. Default
    apply_known_overrides(model, DEFAULT_MODEL_INFO)
}

/// Derate catalog values whose upstream number reflects an opt-in capability
/// that isn't accessible to memex's default usage path.
///
/// Currently: `claude-sonnet-4-6` is listed at `max_input_tokens=1_000_000`
/// in the upstream LiteLLM catalog. That 1M context tier is real but requires
/// the caller to have "usage credits" enabled at
/// claude.ai/settings/usage. The standard claude-code OAuth path reports
/// `contextWindow: 200000` in its `modelUsage` field — that's what memex
/// actually sees in practice. Requests above 200k tokens trigger a
/// compaction attempt that fails with "Usage credits required for 1M
/// context" and surfaces as `[invalid_request] Prompt is too long`. Capping
/// at 200k keeps the chunker honest about the default-accessible cap.
///
/// Users who have usage credits enabled will get smaller-than-necessary
/// chunks (suboptimal cost, not failure). If/when memex gains per-account
/// context-window probing, this override can be removed.
fn apply_known_overrides(model: &str, info: ModelInfo) -> ModelInfo {
    // Match the bare model name regardless of provider prefix
    // ("anthropic/claude-sonnet-4-6", "vertex_ai/claude-sonnet-4-6", etc.)
    // so the derate fires uniformly across the resolution paths in
    // `lookup_model` above.
    let bare = model.rsplit('/').next().unwrap_or(model);
    if bare == "claude-sonnet-4-6" {
        return ModelInfo {
            max_input_tokens: info.max_input_tokens.min(200_000),
            ..info
        };
    }
    info
}

/// Compute the available token budget for batch conversation content.
///
/// Formula:
/// ```text
/// budget = max_input - system_prompt_tokens - index_tokens - max_output - 10% safety margin
/// ```
///
/// Returns 0 if the result would be negative (safety floor).
///
/// # Arguments
///
/// * `model` — Model identifier (looked up in the catalog).
/// * `system_prompt_tokens` — Estimated tokens consumed by the system prompt.
/// * `index_tokens` — Estimated tokens consumed by the index/context sent with every call.
pub fn compute_batch_budget(
    model: &str,
    system_prompt_tokens: usize,
    index_tokens: usize,
) -> usize {
    let info = lookup_model(model);
    let overhead = system_prompt_tokens + index_tokens + info.max_output_tokens;
    // Apply 10% safety margin on max_input
    let safety = info.max_input_tokens / 10;
    let usable = info.max_input_tokens.saturating_sub(safety);
    usable.saturating_sub(overhead)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_model_returns_default() {
        let info = lookup_model("totally-unknown-model-xyz-99999");
        assert_eq!(info, DEFAULT_MODEL_INFO);
    }

    #[test]
    fn known_model_returns_catalog_values() {
        // claude-sonnet-4-6 is in the litellm catalog as a direct entry
        let info = lookup_model("claude-sonnet-4-6");
        // Should not be the default (catalog should have a real value)
        assert!(
            info.max_input_tokens > 0,
            "expected positive max_input_tokens"
        );
        assert!(
            info.max_output_tokens > 0,
            "expected positive max_output_tokens"
        );
        // claude-sonnet-4-6 has a very large context window
        assert!(
            info.max_input_tokens >= 128_000,
            "expected at least 128K input tokens for claude-sonnet-4-6, got {}",
            info.max_input_tokens
        );
    }

    #[test]
    fn claude_sonnet_4_6_is_capped_to_default_accessible_context() {
        // The upstream LiteLLM catalog lists claude-sonnet-4-6 at
        // max_input_tokens=1_000_000 (the 1M-context tier). That tier
        // requires usage credits enabled at claude.ai/settings/usage;
        // default claude-code OAuth reports `contextWindow: 200000`.
        // `apply_known_overrides` caps the lookup so chunkers don't
        // oversize requests. See apply_known_overrides docstring.
        let info = lookup_model("claude-sonnet-4-6");
        assert_eq!(
            info.max_input_tokens, 200_000,
            "claude-sonnet-4-6 must be derated to 200k (default-accessible context); got {}",
            info.max_input_tokens
        );
    }

    #[test]
    fn claude_sonnet_4_6_override_fires_through_provider_prefix() {
        // The override must apply regardless of provider prefix, because
        // `lookup_model` strips prefixes when resolving and could otherwise
        // bypass the derate via the provider-prefix branch.
        for name in [
            "claude-sonnet-4-6",
            "anthropic/claude-sonnet-4-6",
            "vertex_ai/claude-sonnet-4-6",
        ] {
            let info = lookup_model(name);
            assert_eq!(
                info.max_input_tokens, 200_000,
                "{name} should be derated to 200k; got {}",
                info.max_input_tokens
            );
        }
    }

    #[test]
    fn override_does_not_affect_other_models() {
        // Sanity: the override applies only to claude-sonnet-4-6.
        // claude-sonnet-4-20250514 has the same 1M upstream value but
        // we don't override it (not memex's default; if someone picks
        // it explicitly they presumably know what they're doing).
        let info = lookup_model("claude-sonnet-4-20250514");
        assert!(
            info.max_input_tokens >= 500_000,
            "claude-sonnet-4-20250514 should not be derated; got {}",
            info.max_input_tokens
        );
    }

    #[test]
    fn budget_is_positive_for_reasonable_inputs() {
        let budget = compute_batch_budget("claude-sonnet-4-6", 500, 2_000);
        assert!(
            budget > 0,
            "budget should be positive for reasonable prompt sizes, got {budget}"
        );
    }

    #[test]
    fn budget_decreases_with_larger_index() {
        let small_index = compute_batch_budget("claude-sonnet-4-6", 500, 1_000);
        let large_index = compute_batch_budget("claude-sonnet-4-6", 500, 50_000);
        assert!(
            small_index > large_index,
            "budget should decrease as index grows: small={small_index}, large={large_index}"
        );
    }

    #[test]
    fn budget_saturates_at_zero_for_huge_overhead() {
        // Pass an enormous index that exceeds even the largest context window
        let budget = compute_batch_budget("claude-sonnet-4-6", 0, usize::MAX / 2);
        assert_eq!(
            budget, 0,
            "budget should saturate at zero for huge overhead"
        );
    }

    #[test]
    fn default_model_info_constants_are_sane() {
        assert_eq!(DEFAULT_MODEL_INFO.max_input_tokens, 128_000);
        assert_eq!(DEFAULT_MODEL_INFO.max_output_tokens, 16_000);
    }

    #[test]
    fn provider_prefixed_model_resolves() {
        // "openai/gpt-5.4-mini" should resolve — not fall back to default
        let info = lookup_model("openai/gpt-5.4-mini");
        assert_ne!(
            info, DEFAULT_MODEL_INFO,
            "openai/gpt-5.4-mini should not fall back to default"
        );
        assert_eq!(
            info.max_input_tokens, 272_000,
            "openai/gpt-5.4-mini should have 272K input (OpenAI's limit)"
        );
    }

    #[test]
    fn provider_prefixed_uses_provider_specific_limits() {
        // azure/gpt-5.4-mini has different limits than bare gpt-5.4-mini
        let azure = lookup_model("azure/gpt-5.4-mini");
        let bare = lookup_model("gpt-5.4-mini");
        assert_ne!(
            azure.max_input_tokens, bare.max_input_tokens,
            "azure and openai should have different input limits for gpt-5.4-mini"
        );
        // Azure has a larger context window for this model
        assert!(
            azure.max_input_tokens > bare.max_input_tokens,
            "azure ({}) should have larger context than bare ({})",
            azure.max_input_tokens,
            bare.max_input_tokens,
        );
    }
}
