# AGENTS.md — Brainstormer

Cross-tool agent instructions for any AI coding assistant working on this repository.

## Commands

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Project Snapshot

Brainstormer is a Rust CLI that orchestrates multiple LLMs to collaboratively brainstorm, cross-check, and converge on a design document. It uses ZeroClaw (zeroclawlabs) as a dependency for provider abstraction, memory persistence, and agent orchestration.

Pipeline stages: BRAINSTORM → MERGE → REVIEW → MERGE → QUALITY → EVALUATE → [loop until converged].

Key extension points:
- `src/tools/` — Utility functions for convergence, merge, and parallel dispatch
- `src/hooks/` — Pipeline state tracking and copilot interaction utilities
- `presets/` — Task type presets (software, general, research, article, book, strategy)
- `prompts/` — Stage-specific prompt templates
- `system_prompts/` — Mode-specific system prompts (autopilot, copilot, cruise)

## Repository Map

- `src/main.rs` — CLI entrypoint (clap)
- `src/lib.rs` — Module exports
- `src/pipeline.rs` — Core pipeline orchestrator
- `src/types.rs` — Domain types (Stage, Mode, SessionConfig, etc.)
- `src/template.rs` — Prompt template loading + interpolation
- `src/sanitize.rs` — LLM output sanitization
- `src/dry_run.rs` — Mock provider for testing
- `src/input.rs` — File and URL context loading
- `src/export.rs` — Markdown export with metadata
- `src/resume.rs` — Session resume logic
- `src/observer.rs` — BrainstormObserver
- `src/agent_setup.rs` — Observer construction
- `src/tools/` — Parallel dispatch, convergence, merge utilities
- `src/hooks/` — Pipeline state, output tagging, copilot parsing
- `src/cli/` — Provider auto-detect, dashboard, copilot UI
- `tests/` — Unit and integration tests
- `docs/` — Specs and implementation plans

## Workflow

1. **Read before write** — inspect existing module and adjacent tests before editing.
2. **One concern per PR** — avoid mixed feature+refactor+infra patches.
3. **Implement minimal patch** — no speculative abstractions.
4. **Run tests** — `cargo test` before committing.

## Anti-Patterns

- Do not add heavy dependencies for minor convenience.
- Do not mix formatting-only changes with functional changes.
- Do not modify unrelated modules "while here".
