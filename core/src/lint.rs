use crate::error::Result;
use crate::types::{LintIssue, LintIssueKind, LintReport, ProposedFix, WikiOperation};
use crate::{Memex, log, validate};
use std::path::PathBuf;
use tracing::info;

impl Memex {
    pub async fn lint(&self) -> Result<LintReport> {
        let index_content = self.read_index().unwrap_or_default();
        let wiki_dir = self.wiki_dir();

        // Empty wiki: nothing to lint
        if !wiki_dir.exists() || crate::index::is_empty_index(&index_content) {
            return Ok(LintReport {
                issues: vec![],
                suggested_questions: vec![
                    "Nothing to lint. Try ingesting some sources first.".to_string(),
                ],
                suggested_sources: vec![],
            });
        }

        // Collect all wiki page paths and contents
        let mut pages: Vec<(PathBuf, String)> = Vec::new();
        for entry in std::fs::read_dir(&wiki_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md")
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                let rel = format!("wiki/{}", path.file_name().unwrap().to_string_lossy());
                pages.push((PathBuf::from(rel), content));
            }
        }

        let mut issues = Vec::new();

        // Deterministic: dangling wiki links
        for (rel_path, content) in &pages {
            let dangling = validate::find_dangling_links(content, &wiki_dir);
            let now = chrono::Utc::now();
            let ts = now.format("%Y-%m-%dT%H:%M:%SZ");
            for link in dangling {
                issues.push(LintIssue {
                    kind: LintIssueKind::MissingLink,
                    description: format!(
                        "{} links to [[{}]] which doesn't exist",
                        rel_path.display(),
                        link
                    ),
                    affected_pages: vec![rel_path.clone()],
                    proposed_fix: Some(ProposedFix {
                        description: format!("Create stub page for {link}"),
                        operations: vec![WikiOperation::CreatePage {
                            path: PathBuf::from(format!("wiki/{link}.md")),
                            content: format!(
                                "---\ntitle: {}\ntags:\n  - entity\ncreated: {ts}\nlast_updated: {ts}\nsources: []\n---\n\nStub page. Needs content.\n",
                                link.replace('-', " "),
                            ),
                        }],
                    }),
                });
            }
        }

        // Deterministic: orphan pages (no incoming cross-refs, only when >1 pages)
        if pages.len() > 1 {
            // Build set of all referenced page names in a single pass (O(N*M) total)
            let all_referenced: std::collections::HashSet<String> = pages
                .iter()
                .flat_map(|(_, content)| validate::extract_wiki_links(content))
                .collect();
            for (path, _) in &pages {
                let page_name = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if !all_referenced.contains(&page_name) {
                    issues.push(LintIssue {
                        kind: LintIssueKind::Orphan,
                        description: format!("{} has no incoming cross-references", path.display()),
                        affected_pages: vec![path.clone()],
                        proposed_fix: None,
                    });
                }
            }
        }

        // LLM-powered checks + suggestions (batched by token budget)
        let mut suggested_questions = Vec::new();
        let mut suggested_sources = Vec::new();

        let system_prompt_tokens = 200; // ~800 chars of system + format instructions
        let index_tokens = self.index_token_count().unwrap_or(0);
        let budget_bytes = crate::ingest::compute_batch_budget_bytes(
            self.model(),
            system_prompt_tokens,
            index_tokens,
        );

        // Pack pages into batches that fit the context budget
        let mut batches: Vec<String> = Vec::new();
        let mut current_batch = String::new();
        for (rel_path, content) in &pages {
            let entry = format!("### {}\n{}\n\n", rel_path.display(), content);
            if !current_batch.is_empty() && current_batch.len() + entry.len() > budget_bytes {
                batches.push(std::mem::take(&mut current_batch));
            }
            current_batch.push_str(&entry);
        }
        if !current_batch.is_empty() {
            batches.push(current_batch);
        }

        let batch_count = batches.len();
        for (i, batch_content) in batches.iter().enumerate() {
            self.report_progress(&format!("Linting batch {}/{batch_count}...", i + 1));
            let lint_prompt = format!(
                "You are auditing a wiki. Check for:\n1. Contradictions between pages\n2. Duplicate coverage\n3. Stale or incomplete pages\n\nThen suggest:\n- Questions the wiki cannot answer well\n- Sources to look for\n\nWiki pages:\n{batch_content}\n\nRespond in this format:\nISSUES:\n- [contradiction|duplicate|stale|incomplete] description\n\nSUGGESTED_QUESTIONS:\n- question\n\nSUGGESTED_SOURCES:\n- source"
            );

            if let Ok(llm_result) = self
                .provider
                .chat(
                    Some("You are a wiki auditor."),
                    &lint_prompt,
                    self.model(),
                    0.3,
                )
                .await
            {
                parse_lint_response(
                    &llm_result,
                    &mut issues,
                    &mut suggested_questions,
                    &mut suggested_sources,
                );
            }
        }

        // Cross-batch check: if multiple batches, send page summaries + per-batch
        // findings to the LLM to catch contradictions across batches.
        if batch_count > 1 {
            self.report_progress("Cross-checking findings across batches...");
            let mut summary = String::new();
            for (rel_path, content) in &pages {
                let page_summary = crate::index::extract_summary(content, 200);
                summary.push_str(&format!("- {} -- {}\n", rel_path.display(), page_summary));
            }

            let batch_findings: String = issues
                .iter()
                .filter(|i| {
                    matches!(
                        i.kind,
                        LintIssueKind::Contradiction
                            | LintIssueKind::DuplicateCoverage
                            | LintIssueKind::Stale
                            | LintIssueKind::IncompletePage
                    )
                })
                .map(|i| format!("- [{:?}] {}\n", i.kind, i.description))
                .collect();

            let cross_prompt = format!(
                "A wiki was audited in {batch_count} batches. Below are all page titles and the issues found per batch.\n\n\
                 Page index:\n{summary}\n\
                 Issues found so far:\n{batch_findings}\n\
                 Check for contradictions or duplicate coverage ACROSS pages that were in different batches. \
                 Only report NEW issues not already listed above.\n\n\
                 Respond in this format:\nISSUES:\n- [contradiction|duplicate] description\n\nSUGGESTED_QUESTIONS:\n- question"
            );

            if let Ok(cross_result) = self
                .provider
                .chat(
                    Some("You are a wiki auditor doing a cross-batch review."),
                    &cross_prompt,
                    self.model(),
                    0.3,
                )
                .await
            {
                let mut extra_questions = Vec::new();
                let mut extra_sources = Vec::new();
                parse_lint_response(
                    &cross_result,
                    &mut issues,
                    &mut extra_questions,
                    &mut extra_sources,
                );
                suggested_questions.extend(extra_questions);
                suggested_sources.extend(extra_sources);
            }
        }

        let fixable_count = issues.iter().filter(|i| i.proposed_fix.is_some()).count();
        log::append_log(
            self.root(),
            "lint",
            &format!("{} issues", issues.len()),
            &format!("{fixable_count} fixable"),
        )?;
        Ok(LintReport {
            issues,
            suggested_questions,
            suggested_sources,
        })
    }

    /// Apply a proposed fix from lint (create/update/delete pages).
    pub async fn apply_fix(&self, fix: &crate::types::ProposedFix) -> crate::error::Result<()> {
        let lock_file = crate::storage::try_acquire_lock_async(&self.lock_path(), 30)
            .await
            .map_err(|_| crate::error::MemexError::StaleLock {
                lock_path: self.lock_path(),
            })?;

        let wiki_dir = self.wiki_dir();
        for op in &fix.operations {
            let op_path = match op {
                crate::types::WikiOperation::CreatePage { path, .. }
                | crate::types::WikiOperation::UpdatePage { path, .. }
                | crate::types::WikiOperation::DeletePage { path } => path,
            };
            // Path traversal guard: ensure operation stays inside wiki/
            let normalized =
                crate::storage::normalize_path(self.root(), &op_path.to_string_lossy());
            if !normalized.starts_with(&wiki_dir) {
                tracing::warn!(
                    path = %op_path.display(),
                    "apply_fix: path traversal rejected"
                );
                continue;
            }
            match op {
                crate::types::WikiOperation::CreatePage { content, .. } => {
                    let abs_path = self.root().join(op_path);
                    if let Some(parent) = abs_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    crate::storage::atomic_write(&abs_path, content.as_bytes())?;
                    info!(path = %op_path.display(), "created page via lint fix");
                }
                crate::types::WikiOperation::UpdatePage { content, .. } => {
                    let abs_path = self.root().join(op_path);
                    crate::storage::atomic_write(&abs_path, content.as_bytes())?;
                    info!(path = %op_path.display(), "updated page via lint fix");
                }
                crate::types::WikiOperation::DeletePage { .. } => {
                    let abs_path = self.root().join(op_path);
                    if abs_path.exists() {
                        std::fs::remove_file(&abs_path)?;
                        info!(path = %op_path.display(), "deleted page via lint fix");
                    }
                }
            }
        }

        // Rebuild index after fixes
        let index_content = crate::index::rebuild_index(self.root())?;
        crate::storage::atomic_write(&self.root().join("index.md"), index_content.as_bytes())?;

        crate::storage::release_lock(lock_file);

        log::append_log(
            self.root(),
            "lint-fix",
            &fix.description,
            &format!("{} operations", fix.operations.len()),
        )?;

        Ok(())
    }
}

fn parse_lint_response(
    llm_result: &str,
    issues: &mut Vec<LintIssue>,
    suggested_questions: &mut Vec<String>,
    suggested_sources: &mut Vec<String>,
) {
    #[derive(PartialEq)]
    enum Section {
        None,
        Issues,
        Questions,
        Sources,
    }
    let mut section = Section::None;
    for line in llm_result.lines() {
        let line = line.trim();
        if line.starts_with("ISSUES:") {
            section = Section::Issues;
            continue;
        }
        if line.starts_with("SUGGESTED_QUESTIONS:") {
            section = Section::Questions;
            continue;
        }
        if line.starts_with("SUGGESTED_SOURCES:") {
            section = Section::Sources;
            continue;
        }

        if let Some(item) = line.strip_prefix("- ") {
            match section {
                Section::Issues => {
                    if let Some((kind_str, desc)) = item.split_once(']') {
                        let kind_str = kind_str.trim_start_matches('[');
                        let kind = match kind_str {
                            "contradiction" => LintIssueKind::Contradiction,
                            "duplicate" => LintIssueKind::DuplicateCoverage,
                            "stale" => LintIssueKind::Stale,
                            "incomplete" => LintIssueKind::IncompletePage,
                            _ => continue,
                        };
                        issues.push(LintIssue {
                            kind,
                            description: desc.trim().to_string(),
                            affected_pages: vec![],
                            proposed_fix: None,
                        });
                    }
                }
                Section::Questions => suggested_questions.push(item.to_string()),
                Section::Sources => suggested_sources.push(item.to_string()),
                Section::None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn apply_fix_creates_page() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("memex");

        struct StubProvider;
        #[async_trait::async_trait]
        impl crate::LlmProvider for StubProvider {
            async fn chat(
                &self,
                _: Option<&str>,
                _: &str,
                _: &str,
                _: f64,
            ) -> anyhow::Result<String> {
                Ok("stub".to_string())
            }
        }

        let memex = crate::Memex::open(root.clone(), Box::new(StubProvider), "test").unwrap();

        let fix = crate::types::ProposedFix {
            description: "Create stub page".to_string(),
            operations: vec![crate::types::WikiOperation::CreatePage {
                path: std::path::PathBuf::from("wiki/test-stub.md"),
                content: "---\ntitle: Test Stub\ntags:\n  - entity\ncreated: 2026-04-06T00:00:00Z\nlast_updated: 2026-04-06T00:00:00Z\nsources: []\n---\n\nStub page.\n".to_string(),
            }],
        };

        memex.apply_fix(&fix).await.unwrap();
        assert!(root.join("wiki/test-stub.md").exists());
        // Index should be rebuilt
        let index = std::fs::read_to_string(root.join("index.md")).unwrap();
        assert!(index.contains("Test Stub"), "index should contain new page");
    }
}
