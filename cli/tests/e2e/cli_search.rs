//! E2E tests for `memex search`. Uses `E2EHarness` so the daemon is
//! spawned once per test up front (explicit lifecycle, Drop = stop)
//! instead of relying on the binary's auto-spawn-on-first-call.

use crate::common::*;
use crate::e2e_harness::E2EHarness;

#[test]
fn search_empty_wiki() {
    let h = E2EHarness::start();
    let out = h.cli(&["search", "anything"]);
    assert!(
        out.status.success(),
        "search should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().is_empty(),
        "empty wiki should produce no output, got: {stdout}"
    );
}

#[test]
fn search_with_content() {
    let h = E2EHarness::start();
    let content = make_page(
        "Caching Strategies",
        "Content about caching and performance.",
    );
    let out = run_write(h.memex_root(), "caching", &content, &[]);
    assert!(out.status.success(), "write: {:?}", out);

    let stdout = h.cli_ok(&["search", "Caching Strategies"]);
    assert_eq!(stdout.trim(), "caching", "should print slug, got: {stdout}");
}

#[test]
fn search_query_sanitization_hyphens() {
    let h = E2EHarness::start();
    let content = make_page(
        "Multi-Agent Architecture",
        "A multi-agent system for distributed task coordination.",
    );
    let out = run_write(h.memex_root(), "multi-agent", &content, &[]);
    assert!(out.status.success(), "write: {:?}", out);

    let stdout = h.cli_ok(&["search", "Multi-Agent Architecture"]);
    assert_eq!(stdout.trim(), "multi-agent", "got: {stdout}");
}

#[test]
fn search_query_sanitization_phrases() {
    let h = E2EHarness::start();
    let content = make_page(
        "API Rate Limiting",
        "Token bucket and sliding window approaches to API rate limiting.",
    );
    let out = run_write(h.memex_root(), "api-rate-limiting", &content, &[]);
    assert!(out.status.success(), "write: {:?}", out);

    let stdout = h.cli_ok(&["search", "API Rate Limiting"]);
    assert_eq!(stdout.trim(), "api-rate-limiting", "got: {stdout}");
}

#[test]
fn search_probe_includes_vector() {
    let h = E2EHarness::start();
    let content = make_page(
        "Caching Strategies",
        "LRU and TTL-based eviction policies for in-memory caches.",
    );
    let out = run_write(h.memex_root(), "caching", &content, &[]);
    assert!(out.status.success(), "write: {:?}", out);

    let stdout = h.cli_ok(&["search", "Caching Strategies"]);
    assert_eq!(stdout.trim(), "caching", "got: {stdout}");
}
