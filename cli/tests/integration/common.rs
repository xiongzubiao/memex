//! Shared utilities for the integration test crate.
//!
//! Sibling: `integration_harness` (the in-process daemon harness).
//! Tests reach for either via `crate::common::*` /
//! `crate::integration_harness::*` from main.rs.

#![allow(dead_code)]

pub use memex_cli::test_utils::ingest_page;
