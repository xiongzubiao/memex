//! Integration test crate: in-process daemon, no subprocess spawn.
//! Each `#[test]` lives in its own module; helpers and the harness
//! are sibling modules accessible via `crate::common::*` and
//! `crate::integration_harness::*`.

#![cfg(any(test, feature = "test-harness"))]

mod common;
mod integration_harness;

mod daemon_query;
mod daemon_search;
mod document_ingest;
mod harness_smoke;
mod multi_step;
mod query_intent;
mod read_ranged;
mod source_add;
mod watcher;
mod wiki_delete;
mod wiki_write;
