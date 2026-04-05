use brainstormer::agent_setup;
use brainstormer::dry_run::{DryRunProvider, dry_run_provider_factory};
use brainstormer::pipeline::Pipeline;
use brainstormer::types::*;
use std::sync::Arc;

fn build_test_config(do_loop: bool) -> SessionConfig {
    let model = ModelRef {
        provider: "dry-run".into(),
        model: "dry-run".into(),
    };
    SessionConfig {
        task_type: "software".into(),
        mode: Mode::Autopilot,
        do_loop,
        brainstorm_models: vec![model.clone(), model.clone()],
        review_models: vec![model.clone(), model.clone()],
        max_rounds: 3,
        merge_llm: model,
    }
}

fn build_test_agent() -> zeroclaw::agent::Agent {
    let memory: Arc<dyn zeroclaw::memory::Memory> = Arc::new(zeroclaw::memory::NoneMemory);
    let observer: Arc<dyn zeroclaw::observability::Observer> =
        Arc::new(zeroclaw::observability::NoopObserver);

    zeroclaw::agent::Agent::builder()
        .provider(Box::new(DryRunProvider) as Box<dyn zeroclaw::providers::Provider>)
        .tools(vec![])
        .memory(memory)
        .observer(observer)
        .tool_dispatcher(Box::new(zeroclaw::agent::dispatcher::NativeToolDispatcher))
        .model_name("dry-run".into())
        .temperature(0.7)
        .workspace_dir(std::path::PathBuf::from("/tmp"))
        .build()
        .expect("agent should build")
}

#[tokio::test]
async fn dry_run_single_pass_produces_output() {
    let config = build_test_config(false);
    let memory: Arc<dyn zeroclaw::memory::Memory> = Arc::new(zeroclaw::memory::NoneMemory);
    let observer = agent_setup::build_observer();
    let mut agent = build_test_agent();
    let factory = dry_run_provider_factory();

    let mut pipeline = Pipeline::new(
        config,
        observer,
        memory,
        "Design a distributed cache for social media feeds".into(),
        factory,
        String::new(),
    );

    let result = pipeline.run(&mut agent).await.unwrap();

    // Should contain merged content from dry-run responses
    assert!(!result.is_empty());
    assert!(result.contains("Cache") || result.contains("cache") || result.contains("feed"));
}

#[tokio::test]
async fn dry_run_multi_round_converges() {
    let config = build_test_config(true); // looping enabled, max 3 rounds
    let memory: Arc<dyn zeroclaw::memory::Memory> = Arc::new(zeroclaw::memory::NoneMemory);
    let observer = agent_setup::build_observer();
    let mut agent = build_test_agent();
    let factory = dry_run_provider_factory();

    let mut pipeline = Pipeline::new(
        config,
        observer,
        memory,
        "Design an API gateway".into(),
        factory,
        String::new(),
    );

    let result = pipeline.run(&mut agent).await.unwrap();
    assert!(!result.is_empty());
}

#[tokio::test]
async fn dry_run_observer_records_events() {
    let config = build_test_config(false);
    let memory: Arc<dyn zeroclaw::memory::Memory> = Arc::new(zeroclaw::memory::NoneMemory);
    let observer = agent_setup::build_observer();
    let observer_ref = observer.clone();
    let mut agent = build_test_agent();
    let factory = dry_run_provider_factory();

    let mut pipeline = Pipeline::new(
        config,
        observer,
        memory,
        "Design a search engine".into(),
        factory,
        String::new(),
    );

    let _ = pipeline.run(&mut agent).await.unwrap();

    let events = observer_ref.events();
    // Should have at least: round_start + round_complete + session_complete
    assert!(events.len() >= 3, "Expected at least 3 events, got {}", events.len());
}

#[tokio::test]
async fn dry_run_loop_convergence_evaluation() {
    let config = build_test_config(true); // looping enabled, max 3 rounds
    let memory: Arc<dyn zeroclaw::memory::Memory> = Arc::new(zeroclaw::memory::NoneMemory);
    let observer = agent_setup::build_observer();
    let mut agent = build_test_agent();
    let factory = dry_run_provider_factory();

    let mut pipeline = Pipeline::new(
        config,
        observer,
        memory,
        "Design a message queue system".into(),
        factory,
        String::new(),
    );

    let result = pipeline.run(&mut agent).await.unwrap();
    assert!(!result.is_empty());
    // With looping enabled, the pipeline should complete at least 1 round
    // and exercise the convergence evaluation path.
    assert!(
        pipeline.rounds_completed() >= 1,
        "Expected at least 1 round completed, got {}",
        pipeline.rounds_completed()
    );
}

#[tokio::test]
async fn dry_run_produces_exportable_output() {
    let config = build_test_config(false);
    let memory: Arc<dyn zeroclaw::memory::Memory> = Arc::new(zeroclaw::memory::NoneMemory);
    let observer = agent_setup::build_observer();
    let mut agent = build_test_agent();
    let factory = dry_run_provider_factory();

    let mut pipeline = Pipeline::new(
        config.clone(),
        observer,
        memory,
        "Design a distributed cache".into(),
        factory,
        String::new(),
    );

    let result = pipeline.run(&mut agent).await.unwrap();

    let exported = brainstormer::export::format_export(
        "Design a distributed cache",
        &config,
        &result,
        pipeline.rounds_completed(),
        pipeline.converged(),
        pipeline.cost_usd(),
    );
    assert!(exported.starts_with("---"));
    assert!(exported.contains("task: Design a distributed cache"));
    assert!(exported.contains("type: software"));
}
