//! Daemon config schema + loader.
//!
//! Loads from `~/.memex/config.toml` (override via `MEMEX_CONFIG`), layered
//! with env vars (`MEMEX_<SECTION>__<FIELD>`, double-underscore separator).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub query: QueryConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub idle_timeout_min: u64,
    pub log_file: PathBuf,
    pub worker: WorkerConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_timeout_min: 15,
            log_file: expand_tilde("~/.memex/daemon.log"),
            worker: WorkerConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WorkerConfig {
    pub agent: Agent,
    pub model: Option<String>,
    pub max_count: usize,
    pub idle_reap_sec: u64,
    pub restart_after_jobs: u32,
    pub timeout_sec: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        // `available_parallelism` respects process CPU affinity
        // (Kubernetes static CPU manager, taskset, etc.). CFS-quota-only
        // cgroup limits are not automatically reflected — set
        // `max_count` explicitly in that case.
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Self {
            agent: Agent::ClaudeCode,
            model: None,
            max_count: cpus,
            idle_reap_sec: 600,
            restart_after_jobs: 100,
            timeout_sec: 60,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    ClaudeCode,
    Codex,
    GeminiCli,
}

impl Agent {
    /// Default model name for the agent. Used both as the `--model` arg
    /// passed to the subprocess and as the lookup key for
    /// `memex_core::model::lookup_model`, which resolves
    /// `max_input_tokens` for the context-restart threshold. All three
    /// provider CLIs accept the canonical catalog name.
    pub fn default_model(&self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude-sonnet-4-6",
            Agent::Codex => "gpt-5.4-mini",
            Agent::GeminiCli => "gemini-3-flash-preview",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QueryConfig {
    pub top_k: usize,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self { top_k: 5 }
    }
}

impl Config {
    /// Load daemon config from the given file path with env var overrides.
    /// Missing file → all defaults. Malformed → error. Out-of-range → error.
    /// Env vars use `MEMEX__<SECTION>__<FIELD>` (double-underscore separator
    /// between every path element, including the prefix).
    pub fn load(config_path: &Path) -> Result<Self> {
        let mut builder = ::config::Config::builder();
        if config_path.exists() {
            builder = builder.add_source(::config::File::from(config_path));
        }
        builder = builder.add_source(
            ::config::Environment::with_prefix("MEMEX")
                .prefix_separator("__")
                .separator("__")
                .try_parsing(true),
        );
        let cfg: Config = builder
            .build()
            .context("building config")?
            .try_deserialize()
            .context("deserializing config")?;
        cfg.validate().context("config validation")?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        let d = &self.daemon;
        if !(1..=1440).contains(&d.idle_timeout_min) {
            bail!(
                "daemon.idle_timeout_min = {} out of range 1..=1440",
                d.idle_timeout_min
            );
        }
        let w = &d.worker;
        if !(1..=64).contains(&w.max_count) {
            bail!(
                "daemon.worker.max_count = {} out of range 1..=64",
                w.max_count
            );
        }
        if !(30..=3600).contains(&w.idle_reap_sec) {
            bail!(
                "daemon.worker.idle_reap_sec = {} out of range 30..=3600",
                w.idle_reap_sec
            );
        }
        if !(1..=1000).contains(&w.restart_after_jobs) {
            bail!(
                "daemon.worker.restart_after_jobs = {} out of range 1..=1000",
                w.restart_after_jobs
            );
        }
        if !(1..=3600).contains(&w.timeout_sec) {
            bail!(
                "daemon.worker.timeout_sec = {} out of range 1..=3600",
                w.timeout_sec
            );
        }
        let q = &self.query;
        if !(1..=20).contains(&q.top_k) {
            bail!("query.top_k = {} out of range 1..=20", q.top_k);
        }
        Ok(())
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_config(dir: &TempDir, contents: &str) -> std::path::PathBuf {
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn default_values_match_spec() {
        let c = Config::default();
        assert_eq!(c.daemon.idle_timeout_min, 15);
        assert_eq!(c.daemon.worker.agent, Agent::ClaudeCode);
        assert_eq!(c.daemon.worker.model, None);
        assert!(c.daemon.worker.max_count >= 1);
        assert_eq!(c.daemon.worker.idle_reap_sec, 600);
        assert_eq!(c.daemon.worker.restart_after_jobs, 100);
        assert_eq!(c.daemon.worker.timeout_sec, 60);
        assert_eq!(c.query.top_k, 5);
    }

    #[test]
    fn idle_reap_sec_default_is_600() {
        assert_eq!(WorkerConfig::default().idle_reap_sec, 600);
    }

    #[test]
    fn loads_defaults_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist.toml");
        let c = Config::load(&missing).unwrap();
        assert_eq!(c.daemon.idle_timeout_min, 15);
    }

    #[test]
    fn file_values_override_defaults() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            &tmp,
            r#"
[daemon]
idle_timeout_min = 30

[daemon.worker]
agent = "codex"
model = "gpt-5.4-mini"
max_count = 4
"#,
        );
        let c = Config::load(&path).unwrap();
        assert_eq!(c.daemon.idle_timeout_min, 30);
        assert_eq!(c.daemon.worker.agent, Agent::Codex);
        assert_eq!(c.daemon.worker.model, Some("gpt-5.4-mini".to_string()));
        assert_eq!(c.daemon.worker.max_count, 4);
    }

    #[test]
    fn rejects_out_of_range_values() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            &tmp,
            r#"
[query]
top_k = 999
"#,
        );
        let err = Config::load(&path).unwrap_err();
        let full_error = format!("{:?}", err);
        assert!(
            full_error.contains("top_k"),
            "expected 'top_k' in error chain, got: {full_error}"
        );
    }
}
