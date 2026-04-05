//! Smoke test that requires real API keys.
//! Run with: BRAINSTORMER_SMOKE=1 cargo test -p brainstormer smoke -- --nocapture
//! Requires at least one of: OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY

use brainstormer::cli::auto_detect;
use brainstormer::tools::brainstorm_swarm::*;

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
    assert!(
        !providers.is_empty(),
        "Need at least one provider for smoke test"
    );

    let model = providers[0].frontier_model.clone();
    let factory = default_provider_factory();

    let results = dispatch_parallel(
        std::slice::from_ref(&model),
        "In one sentence, what is 2+2?",
        Some("You are a helpful assistant. Be brief."),
        30,
        &factory,
    )
    .await;

    assert_eq!(results.len(), 1);
    let (model_id, response) = results[0]
        .as_ref()
        .expect("Smoke test API call failed");
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
    assert!(
        !providers.is_empty(),
        "Need at least one provider for smoke test"
    );

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
    eprintln!(
        "Parallel dispatch: {}/{} succeeded",
        successes.len(),
        results.len()
    );
}
