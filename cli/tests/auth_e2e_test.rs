use memex_cli::auth::{MemexAuthStore, TokenSet};
use memex_cli::config_file;
use std::path::Path;
use std::process::Command;

fn memex_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_memex"))
}

fn run_memex(root: &Path, args: &[&str]) -> (String, String, bool) {
    run_memex_with_env(root, args, &[])
}

fn run_memex_with_env(root: &Path, args: &[&str], envs: &[(&str, &str)]) -> (String, String, bool) {
    let mut command = memex_bin();
    let output = command
        .args(args)
        .env("MEMEX_ROOT", root.to_str().unwrap())
        .envs(envs.iter().copied())
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("GEMINI_OAUTH_CLIENT_ID")
        .env_remove("GEMINI_OAUTH_CLIENT_SECRET")
        .output()
        .expect("failed to run memex binary");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (stdout, stderr, output.status.success())
}

fn seed_oauth(root: &Path, provider: &str) {
    std::fs::create_dir_all(root).unwrap();
    let store = MemexAuthStore::new(root);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        store
            .upsert_oauth(
                provider,
                TokenSet {
                    access_token: format!("{provider}-access"),
                    refresh_token: Some(format!("{provider}-refresh")),
                    id_token: Some(format!("{provider}-id")),
                    expires_at: None,
                    token_type: Some("Bearer".to_string()),
                    scope: None,
                },
                Some("acct_test".to_string()),
            )
            .await
            .unwrap();
    });
}

fn seed_codex_cli_auth_cache(home: &Path) {
    let cache = home.join(".codex");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        cache.join("auth.json"),
        r#"{"tokens":{"access_token":"codex-access","refresh_token":"codex-refresh","id_token":"codex-id","account_id":"acct_codex"}}"#,
    )
    .unwrap();
}

fn seed_gemini_cli_auth_cache(home: &Path) {
    let cache = home.join(".gemini");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        cache.join("oauth_creds.json"),
        r#"{"access_token":"gemini-access","refresh_token":"gemini-refresh","id_token":"gemini-id","token_type":"Bearer","expiry_date":4102444800000}"#,
    )
    .unwrap();
}

#[test]
fn e2e_auth_status_reports_memex_managed_providers() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("memex");
    seed_oauth(&root, "openai-codex");
    seed_oauth(&root, "gemini");

    let (stdout, stderr, ok) = run_memex(&root, &["auth", "status"]);
    assert!(ok, "status failed: stdout={stdout}\nstderr={stderr}");
    assert!(
        stdout.contains("openai-codex"),
        "missing codex status: {stdout}"
    );
    assert!(stdout.contains("gemini"), "missing gemini status: {stdout}");
}

#[test]
fn e2e_auth_logout_removes_provider_record() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("memex");
    seed_oauth(&root, "openai-codex");

    let (stdout, stderr, ok) = run_memex(&root, &["auth", "logout", "--provider", "codex"]);
    assert!(ok, "logout failed: stdout={stdout}\nstderr={stderr}");
    assert!(
        stdout.contains("Removed"),
        "expected removed output: {stdout}"
    );

    let store = MemexAuthStore::new(&root);
    let loaded = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async { store.load().await.unwrap() });
    assert!(
        !loaded.providers.contains_key("openai-codex"),
        "provider should be removed: {:?}",
        loaded.providers.keys()
    );
}

#[test]
fn e2e_auth_login_updates_task_defaults_without_overwriting_global() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    seed_codex_cli_auth_cache(&home);

    std::fs::write(
        root.join("config.toml"),
        r#"
model = "anthropic/claude-sonnet-4-6"

[query]
model = "anthropic/claude-opus-4-6"
"#,
    )
    .unwrap();

    let (stdout, stderr, ok) = run_memex_with_env(
        &root,
        &["auth", "login", "--provider", "codex"],
        &[("HOME", home.to_str().unwrap())],
    );
    assert!(ok, "login failed: stdout={stdout}\nstderr={stderr}");

    let raw = std::fs::read_to_string(root.join("config.toml")).unwrap();
    let cfg = config_file::load_config_str(&raw).unwrap();
    assert_eq!(cfg.model.as_deref(), Some("anthropic/claude-sonnet-4-6"));
    assert_eq!(
        cfg.query.model.as_deref(),
        Some("anthropic/claude-opus-4-6")
    );
    assert_eq!(
        cfg.ingest.model.as_deref(),
        Some("openai-codex/gpt-5.4-mini")
    );
    assert_eq!(cfg.lint.model.as_deref(), Some("openai-codex/gpt-5.4-mini"));
}

#[test]
fn e2e_auth_login_make_default_overrides_global_model() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    seed_codex_cli_auth_cache(&home);

    std::fs::write(
        root.join("config.toml"),
        r#"
model = "anthropic/claude-sonnet-4-6"
"#,
    )
    .unwrap();

    let (stdout, stderr, ok) = run_memex_with_env(
        &root,
        &["auth", "login", "--provider", "codex", "--make-default"],
        &[("HOME", home.to_str().unwrap())],
    );
    assert!(ok, "login failed: stdout={stdout}\nstderr={stderr}");

    let raw = std::fs::read_to_string(root.join("config.toml")).unwrap();
    let cfg = config_file::load_config_str(&raw).unwrap();
    assert_eq!(cfg.model.as_deref(), Some("openai-codex/gpt-5.4"));
    assert_eq!(cfg.query.model.as_deref(), Some("openai-codex/gpt-5.4"));
}

#[test]
fn e2e_auth_login_device_code_skips_import_cache_for_gemini() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    seed_gemini_cli_auth_cache(&home);

    let (stdout, stderr, ok) = run_memex_with_env(
        &root,
        &["auth", "login", "--provider", "gemini", "--device-code"],
        &[("HOME", home.to_str().unwrap())],
    );
    assert!(
        !ok,
        "login unexpectedly succeeded: stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stderr.contains("GEMINI_OAUTH_CLIENT_ID is required"),
        "expected device-code path failure, got stderr={stderr}"
    );

    let store = MemexAuthStore::new(&root);
    let loaded = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async { store.load().await.unwrap() });
    assert!(
        !loaded.providers.contains_key("gemini"),
        "gemini auth should not be imported when --device-code is set"
    );
}
