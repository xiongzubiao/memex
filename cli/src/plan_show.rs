//! Format a `Plan` JSON document as human-readable text. Local CLI op
//! invoked by `memex plan show < plan.json`. No daemon involvement.

use crate::daemon::plan::Plan;

/// Format the plan as a table + per-merge diff blocks. Returns the
/// stdout content (trailing newline included).
pub fn format_plan(plan: &Plan) -> String {
    let mut out = String::new();
    let merge_count = plan
        .proposals
        .iter()
        .filter(|p| p.merge_target_slug.is_some())
        .count();
    out.push_str(&format!(
        "PLAN: {} ({} bytes → {} proposals, {} merge{})\n\n",
        plan.source.identifier,
        plan.source.size_bytes,
        plan.proposals.len(),
        merge_count,
        if merge_count == 1 { "" } else { "s" }
    ));
    out.push_str("# | slug | title | tags | status\n");
    out.push_str("--+------+-------+------+-------\n");
    for p in &plan.proposals {
        let mut status = match (&p.merge_target_slug, &p.error, p.dropped, p.committed) {
            (_, _, true, _) => "DROPPED".to_string(),
            (_, _, _, true) => "COMMITTED".to_string(),
            (_, Some(e), _, _) => format!("[ERROR] {e}"),
            (Some(t), None, _, _) => format!("merge → {t}"),
            (None, None, _, _) => "new".to_string(),
        };
        if p.slug != p.original_slug {
            status.push_str(&format!(" (edited from {})", p.original_slug));
        }
        out.push_str(&format!(
            "{} | {} | {} | {}\n",
            p.index, p.slug, p.title, status
        ));
    }
    for p in &plan.proposals {
        if let Some(diff) = &p.merge_diff {
            out.push_str(&format!(
                "\n--- diff: {} (proposal {} → existing wiki page) ---\n",
                p.slug, p.index
            ));
            out.push_str(diff);
        }
    }
    out.push_str("\nTo commit: pipe this plan to `memex plan apply`.\n");
    out.push_str("To edit:   open the plan file in your editor (slug, title, dropped fields), then re-pipe to apply.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::plan::{PlanSource, Proposal};

    fn sample_plan() -> Plan {
        Plan {
            version: 1,
            source: PlanSource {
                id: "src-abc".into(),
                identifier: "https://example.com/article".into(),
                content_hash: "a".repeat(64),
                size_bytes: 4823,
            },
            created_at: "2026-04-30T19:42:00Z".into(),
            proposals: vec![
                Proposal {
                    index: 0,
                    slug: "alpha".into(),
                    title: "Alpha".into(),
                    body: "...".into(),
                    merge_target_slug: Some("alpha".into()),
                    merge_target_hash: Some("f".repeat(64)),
                    merge_diff: Some("--- existing\n+++ merged\n@@\n+## New\n".into()),
                    dropped: false,
                    committed: false,
                    original_slug: "alpha".into(),
                    error: None,
                },
                Proposal {
                    index: 1,
                    slug: "gpu-checkpoint".into(),
                    title: "GPU Checkpoint".into(),
                    body: "...".into(),
                    merge_target_slug: None,
                    merge_target_hash: None,
                    merge_diff: None,
                    dropped: false,
                    committed: false,
                    original_slug: "gpu-checkpoint".into(),
                    error: None,
                },
            ],
        }
    }

    #[test]
    fn format_plan_includes_header_and_table() {
        let s = format_plan(&sample_plan());
        assert!(s.contains("PLAN: https://example.com/article"));
        assert!(s.contains("4823 bytes"));
        assert!(s.contains("2 proposals"));
        assert!(s.contains("1 merge"));
        assert!(s.contains("0 | alpha | Alpha | merge → alpha"));
        assert!(s.contains("1 | gpu-checkpoint"));
    }

    #[test]
    fn format_plan_includes_merge_diff_block() {
        let s = format_plan(&sample_plan());
        assert!(s.contains("--- diff: alpha (proposal 0 → existing wiki page) ---"));
        assert!(s.contains("+## New"));
    }

    #[test]
    fn format_plan_marks_error_status() {
        let mut p = sample_plan();
        p.proposals[0].error = Some("merge-dry-run failed: timeout".into());
        let s = format_plan(&p);
        assert!(s.contains("[ERROR] merge-dry-run failed: timeout"));
    }

    #[test]
    fn format_plan_marks_dropped_and_committed() {
        let mut p = sample_plan();
        p.proposals[0].dropped = true;
        p.proposals[1].committed = true;
        let s = format_plan(&p);
        assert!(s.contains("DROPPED"));
        assert!(s.contains("COMMITTED"));
    }

    #[test]
    fn format_plan_marks_user_edited_slug() {
        let mut p = sample_plan();
        p.proposals[1].slug = "gpu-checkpoint-renamed".into();
        // original_slug stays "gpu-checkpoint" → status gets the edited tag.
        let s = format_plan(&p);
        assert!(
            s.contains("(edited from gpu-checkpoint)"),
            "expected edited indicator, got: {s}"
        );
    }
}
