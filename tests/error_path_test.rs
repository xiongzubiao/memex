use brainstormer::dry_run::{auth_fail_provider_factory, rate_limit_provider_factory};
use brainstormer::tools::brainstorm_swarm::dispatch_parallel;
use brainstormer::types::ModelRef;
use std::sync::Arc;

fn test_models(n: usize) -> Vec<ModelRef> {
    (0..n)
        .map(|i| ModelRef {
            provider: format!("test-provider-{}", i),
            model: format!("test-model-{}", i),
        })
        .collect()
}

#[tokio::test]
async fn t34_provider_auth_fails_mid_session() {
    // AuthFailProvider(0) fails immediately on every call
    let factory = auth_fail_provider_factory(0);
    let models = test_models(2);

    let results = dispatch_parallel(&models, "brainstorm something", None, 10, &factory).await;

    // All results should be errors (401 is not retried)
    assert!(!results.is_empty());
    for result in &results {
        assert!(result.is_err(), "Expected error, got: {:?}", result);
        let err = result.as_ref().unwrap_err();
        assert!(
            err.contains("401") || err.contains("Unauthorized"),
            "Error should mention 401/Unauthorized, got: {}",
            err
        );
    }
}

#[tokio::test]
async fn t35_all_providers_rate_limited_retries_with_backoff() {
    let factory = rate_limit_provider_factory();
    let models = test_models(1);

    let start = std::time::Instant::now();
    let results = dispatch_parallel(&models, "brainstorm something", None, 60, &factory).await;
    let elapsed = start.elapsed();

    // All results should be errors mentioning 429
    assert!(!results.is_empty());
    for result in &results {
        assert!(result.is_err(), "Expected error, got: {:?}", result);
        let err = result.as_ref().unwrap_err();
        assert!(
            err.contains("429"),
            "Error should mention 429, got: {}",
            err
        );
    }

    // Exponential backoff: attempt 1 (500ms) + attempt 2 (1000ms) + attempt 3 (2000ms) = 3500ms minimum
    // With jitter up to 20%, minimum is still ~3500ms * 0.8 baseline = ~2800ms, but base delays alone are 3500ms
    // Be conservative: at least 3 seconds total
    assert!(
        elapsed >= std::time::Duration::from_secs(3),
        "Expected >= 3s of backoff delay, got {:?}",
        elapsed
    );
}

#[tokio::test]
async fn t37_corrupted_memory_state_graceful_error() {
    let memory = Arc::new(zeroclaw::memory::NoneMemory::new());

    // Attempt to load a non-existent session
    let state = brainstormer::resume::load_session(memory.as_ref(), "nonexistent-session-xyz")
        .await
        .expect("load_session should not hard-error on missing session");

    // NoneMemory returns None for all gets, so session should not be resumable
    assert!(
        !state.is_resumable(),
        "Session from NoneMemory should not be resumable"
    );
    assert!(state.config.is_none(), "Config should be None");
    assert!(state.last_draft.is_none(), "Last draft should be None");
    assert_eq!(state.last_round, 0, "Last round should be 0");
}
