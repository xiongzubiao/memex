use crate::error::{MemexError, Result};
use std::path::Path;
use std::time::Duration;

const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const TIMEOUT_RANGE_SECS: std::ops::RangeInclusive<u64> = 1..=3600;

/// Memex configuration. v1 has one knob; add more with backwards-compatible
/// additions to `TomlConfig`. Kept narrow by spec — YAGNI on speculative keys.
#[derive(Debug, Clone)]
pub struct Config {
    pub lock_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            lock_timeout: Duration::from_secs(120),
        }
    }
}

impl Config {
    /// Load config from `{memex_root}/config.toml` with env overrides.
    /// Precedence: MEMEX_LOCK_TIMEOUT_SECONDS env > config file > compile default.
    pub fn load(memex_root: &Path) -> Result<Self> {
        let mut cfg = Config::default();
        let path = memex_root.join("config.toml");
        if path.exists() {
            let metadata = std::fs::metadata(&path)?;
            if metadata.len() > MAX_CONFIG_BYTES {
                return Err(MemexError::MalformedConfig {
                    path: path.clone(),
                    reason: format!(
                        "config file is {} bytes; max allowed is {}",
                        metadata.len(),
                        MAX_CONFIG_BYTES
                    ),
                });
            }
            let raw = std::fs::read_to_string(&path)?;
            let parsed: TomlConfig = toml::from_str(&raw)
                .map_err(|e| MemexError::MalformedConfig {
                    path: path.clone(),
                    reason: e.to_string(),
                })?;
            if let Some(secs) = parsed.locking.and_then(|l| l.timeout_seconds) {
                if !TIMEOUT_RANGE_SECS.contains(&secs) {
                    return Err(MemexError::MalformedConfig {
                        path: path.clone(),
                        reason: format!(
                            "[locking] timeout_seconds = {secs} is outside allowed range {:?}",
                            TIMEOUT_RANGE_SECS
                        ),
                    });
                }
                cfg.lock_timeout = Duration::from_secs(secs);
            }
        }

        if let Ok(v) = std::env::var("MEMEX_LOCK_TIMEOUT_SECONDS") {
            let secs: u64 = v.parse().map_err(|_| MemexError::InvalidEnvVar {
                var: "MEMEX_LOCK_TIMEOUT_SECONDS",
                value: v.clone(),
                reason: "not a non-negative integer".into(),
            })?;
            if !TIMEOUT_RANGE_SECS.contains(&secs) {
                return Err(MemexError::InvalidEnvVar {
                    var: "MEMEX_LOCK_TIMEOUT_SECONDS",
                    value: v,
                    reason: format!("outside allowed range {:?}", TIMEOUT_RANGE_SECS),
                });
            }
            cfg.lock_timeout = Duration::from_secs(secs);
        }
        Ok(cfg)
    }
}

#[derive(serde::Deserialize)]
struct TomlConfig {
    locking: Option<TomlLocking>,
}

#[derive(serde::Deserialize)]
struct TomlLocking {
    timeout_seconds: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Scoped env override that restores the prior value on drop.
    /// Env vars are process-global; to avoid races between parallel cargo
    /// test threads, every test that touches MEMEX_LOCK_TIMEOUT_SECONDS
    /// must be `#[serial]` (see `serial_test` crate — add to
    /// core/Cargo.toml [dev-dependencies]: `serial_test = "3"`).
    struct ScopedEnv {
        key: &'static str,
        prior: Option<String>,
    }
    impl ScopedEnv {
        fn set(key: &'static str, value: &str) -> Self {
            let prior = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value); }
            Self { key, prior }
        }
    }
    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => unsafe { std::env::set_var(self.key, v); },
                None => unsafe { std::env::remove_var(self.key); },
            }
        }
    }

    // All Config::load tests are serialized on the "env" token because env vars
    // are process-global. Tests that set env vars pollute parallel non-serial
    // tests; serializing all of them eliminates the race.
    use serial_test::serial;

    #[test]
    #[serial(env)]
    fn missing_config_file_uses_defaults() {
        let dir = TempDir::new().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.lock_timeout, Duration::from_secs(120));
    }

    #[test]
    #[serial(env)]
    fn valid_config_overrides_default() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[locking]\ntimeout_seconds = 60\n",
        ).unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.lock_timeout, Duration::from_secs(60));
    }

    #[test]
    #[serial(env)]
    fn malformed_toml_returns_error() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("config.toml"), "not valid toml [\n").unwrap();
        match Config::load(dir.path()) {
            Err(MemexError::MalformedConfig { .. }) => {}
            other => panic!("expected MalformedConfig, got {other:?}"),
        }
    }

    #[test]
    #[serial(env)]
    fn oversize_config_file_rejected() {
        let dir = TempDir::new().unwrap();
        let big = "x".repeat(100_000);
        std::fs::write(dir.path().join("config.toml"), big).unwrap();
        match Config::load(dir.path()) {
            Err(MemexError::MalformedConfig { reason, .. }) => {
                assert!(reason.contains("max allowed"));
            }
            other => panic!("expected size-related MalformedConfig, got {other:?}"),
        }
    }

    #[test]
    #[serial(env)]
    fn timeout_out_of_range_rejected() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[locking]\ntimeout_seconds = 0\n",
        ).unwrap();
        match Config::load(dir.path()) {
            Err(MemexError::MalformedConfig { reason, .. }) => {
                assert!(reason.contains("outside allowed range"));
            }
            other => panic!("expected range MalformedConfig, got {other:?}"),
        }

        std::fs::write(
            dir.path().join("config.toml"),
            "[locking]\ntimeout_seconds = 99999999\n",
        ).unwrap();
        match Config::load(dir.path()) {
            Err(MemexError::MalformedConfig { .. }) => {}
            other => panic!("expected range MalformedConfig, got {other:?}"),
        }
    }

    #[test]
    #[serial(env)]
    fn unknown_keys_ignored() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[locking]\ntimeout_seconds = 30\nfuture_knob = \"ok\"\n\n[unknown_section]\nx = 1\n",
        ).unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.lock_timeout, Duration::from_secs(30));
    }

    // Env-touching tests must be `#[serial]` to avoid races with parallel
    // cargo test threads — env vars are process-global. Add serial_test to
    // core/Cargo.toml [dev-dependencies]: `serial_test = "3"`.

    #[test]
    #[serial(env)]
    fn env_var_overrides_config() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[locking]\ntimeout_seconds = 60\n",
        ).unwrap();
        let _env = ScopedEnv::set("MEMEX_LOCK_TIMEOUT_SECONDS", "30");
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.lock_timeout, Duration::from_secs(30));
    }

    #[test]
    #[serial(env)]
    fn env_var_non_numeric_rejected() {
        let dir = TempDir::new().unwrap();
        let _env = ScopedEnv::set("MEMEX_LOCK_TIMEOUT_SECONDS", "abc");
        match Config::load(dir.path()) {
            Err(MemexError::InvalidEnvVar { .. }) => {}
            other => panic!("expected InvalidEnvVar, got {other:?}"),
        }
    }

    #[test]
    #[serial(env)]
    fn env_var_out_of_range_rejected() {
        let dir = TempDir::new().unwrap();
        let _env = ScopedEnv::set("MEMEX_LOCK_TIMEOUT_SECONDS", "99999999");
        match Config::load(dir.path()) {
            Err(MemexError::InvalidEnvVar { reason, .. }) => {
                assert!(reason.contains("outside allowed range"));
            }
            other => panic!("expected range InvalidEnvVar, got {other:?}"),
        }
    }

    #[test]
    #[serial(env)]
    fn bom_prefixed_config_parses_or_rejects_cleanly() {
        // BOM = 0xEF 0xBB 0xBF. toml crate will either accept or reject; must not panic.
        let dir = TempDir::new().unwrap();
        let mut contents = vec![0xEF, 0xBB, 0xBF];
        contents.extend_from_slice(b"[locking]\ntimeout_seconds = 30\n");
        std::fs::write(dir.path().join("config.toml"), contents).unwrap();
        let _ = Config::load(dir.path());  // either Ok or Err MalformedConfig; no panic
    }
}
