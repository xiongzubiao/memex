//! `Request::LintFix` handler — apply auto-fixes for filesystem-sync
//! issues (stale-index, untracked, missing-file, raw hash-mismatch) and
//! outdated-embedding drift. Routed through the daemon so the daemon
//! remains the single writer; per-fix `apply_fix_locked` already takes
//! the writer lock per issue, but routing through the daemon serializes
//! the lint pass against any concurrent ingest/write requests. Link
//! issues stay report-only — fixing them needs LLM judgment.

use crate::daemon::error::DaemonError;
use crate::daemon::handler::{HandlerState, error_events, get_or_open_memex};
use crate::daemon::protocol::Event;

pub(super) async fn handle_lint_fix(state: &HandlerState) -> Vec<Event> {
    let root = state.writer.bound_root().to_path_buf();
    let memex = match get_or_open_memex(state.writer.memex_handle(), &root) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };

    let report = match memex.lint() {
        Ok(r) => r,
        Err(e) => return error_events(DaemonError::Storage(e.to_string())),
    };

    let mut events: Vec<Event> = Vec::new();
    let mut fixed: u32 = 0;

    for issue in &report.issues {
        match issue.kind {
            memex_core::types::LintIssueKind::StaleIndex
            | memex_core::types::LintIssueKind::OutdatedEmbedding
            | memex_core::types::LintIssueKind::RawHashMismatch
            | memex_core::types::LintIssueKind::UntrackedFile
            | memex_core::types::LintIssueKind::MissingFile => {
                match memex.apply_fix_locked(issue) {
                    Ok(memex_core::FixOutcome::Applied) => {
                        events.push(Event::LintFixed {
                            page: issue.page.clone(),
                            kind: kind_str(&issue.kind).to_string(),
                        });
                        fixed += 1;
                    }
                    Ok(memex_core::FixOutcome::Stale) => {
                        events.push(Event::LintAlreadyFixed {
                            page: issue.page.clone(),
                        });
                    }
                    Err(e) => {
                        return error_events(DaemonError::Storage(format!(
                            "failed to fix {}: {e}",
                            issue.page
                        )));
                    }
                }
            }
            _ => {
                events.push(Event::LintRemaining {
                    page: issue.page.clone(),
                    kind: kind_str(&issue.kind).to_string(),
                    target: issue.target.clone(),
                });
            }
        }
    }

    let remaining = events
        .iter()
        .filter(|e| matches!(e, Event::LintRemaining { .. }))
        .count() as u32;
    events.push(Event::LintResult { fixed, remaining });
    events.push(Event::Done { status: 0 });
    events
}

fn kind_str(k: &memex_core::types::LintIssueKind) -> &'static str {
    use memex_core::types::LintIssueKind::*;
    match k {
        StaleIndex => "stale_index",
        DanglingLink => "dangling_link",
        MissingLink => "missing_link",
        UntrackedFile => "untracked_file",
        MissingFile => "missing_file",
        OutdatedEmbedding => "outdated_embedding",
        RawHashMismatch => "raw_hash_mismatch",
    }
}
