//! Faithful Rust ports of LlamaIndex's node parsers. Each submodule
//! mirrors a Python source file; see the per-module docs for the
//! upstream URL and any deliberate substitutions.
//!
//! Source layout mirrors LlamaIndex's
//! `llama-index-core/llama_index/core/node_parser/`.

pub mod markdown;
pub mod semantic_splitter;
pub mod sentence;
