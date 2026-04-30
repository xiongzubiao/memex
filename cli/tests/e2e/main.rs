//! E2E test crate: spawns the real `memex` binary and (when needed)
//! a real `memex daemon` subprocess. Each `#[test]` lives in its own
//! module; shared utilities live in `common`, and `e2e_harness`
//! provides explicit daemon lifecycle bracketing for tests that want
//! it.

#![cfg(any(test, feature = "test-harness"))]

mod common;
mod e2e_harness;

// CLI-binary tests, split by sub-command.
mod cli_delete;
mod cli_lint;
mod cli_misc;
mod cli_read;
mod cli_search;
mod cli_smoke;
mod cli_write;

// Daemon subprocess + CLI lifecycle tests.
mod daemon_autoscale;
mod daemon_codex;
mod daemon_concurrency;
mod daemon_expansion;
mod daemon_gemini_cli;
mod daemon_lifecycle;
mod daemon_synthesis;
mod daemon_worker_failures;

mod hook_shim;
