// Integration tests for agent components.

/// Test 1: session_lifecycle
/// Verifies session creation, tool call logging, completion, and listing.
#[test]
fn session_lifecycle() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("sources/brainstorms")).unwrap();

    // Create
    let (id, path) = memex_agent::session::create_session(root, "Test task").unwrap();
    assert!(path.exists());

    // Log tool call
    memex_agent::session::log_tool_call(
        &path,
        "consult_panelists",
        &serde_json::json!({"task": "test"}),
        "response",
    )
    .unwrap();

    // Complete
    memex_agent::session::complete_session(&path, "# Final output").unwrap();

    // List
    let sessions = memex_agent::session::list_sessions(root);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].0, id);
    assert_eq!(sessions[0].1["status"], "completed");
}

/// Test 2: brainstorm_config_defaults
/// Verifies that the default BrainstormConfig has expected values.
#[test]
fn brainstorm_config_defaults() {
    let config = memex_agent::config::BrainstormConfig::default();
    assert_eq!(config.orchestrator, "dry-run/default");
    assert_eq!(config.cost_budget, 5.0);
    assert_eq!(config.small_memex_threshold, 15_000);
    assert!(config.proposer_models.is_empty());
    assert!(config.reviewer_models.is_empty());
}

/// Test 3: identity_scaffold_and_load
/// Verifies that scaffolded identity files can be loaded with expected content.
#[test]
fn identity_scaffold_and_load() {
    let dir = tempfile::TempDir::new().unwrap();
    memex_agent::identity::scaffold_identity_files(dir.path()).unwrap();

    let identity = memex_agent::identity::load_identity(dir.path());
    assert!(identity.contains("Memex agent"));

    let soul = memex_agent::identity::load_soul(dir.path());
    assert!(soul.contains("GATHER"));
    assert!(soul.contains("REFLECT"));
}

/// Test 4: agent_build_with_dry_run
/// Verifies that the agent can be built with a dry-run provider.
#[tokio::test]
async fn agent_build_with_dry_run() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let provider: Box<dyn memex_core::LlmProvider> = Box::new(memex_agent::dry_run::DryRunProvider);
    let memex =
        std::sync::Arc::new(memex_core::Memex::open(root.clone(), provider, "dry-run").unwrap());

    memex_agent::identity::scaffold_identity_files(&root).unwrap();

    let config = memex_agent::config::BrainstormConfig::default();

    let orchestrator_provider =
        Box::new(memex_agent::dry_run::DryRunProvider) as Box<dyn zeroclaw::providers::Provider>;

    let result =
        memex_agent::builder::MemexAgentBuilder::new(memex, config, orchestrator_provider).build();

    // Should build successfully; if zeroclaw internals prevent it, report but don't fail hard.
    if let Err(ref e) = result {
        eprintln!(
            "NOTE: agent build returned error (may be zeroclaw internal issue): {:?}",
            e
        );
    }
    assert!(result.is_ok(), "Agent build failed: {:?}", result.err());
}
