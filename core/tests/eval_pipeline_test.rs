//! Full pipeline evaluation with mock LLM (automated, runs in CI).
//!
//! Tests the three-tier retrieval pipeline (BM25 → expansion → LLM fallback)
//! using a mock LLM that returns canned expansion terms. This verifies tier
//! routing, per-term search merging, and fallback logic.
//!
//! Uses qmd's eval dataset from `tests/eval-docs/`.
//!
//! Expected results with mock expansion:
//! - Easy: ≥80% Hit@3 (BM25 handles directly at tier 1)
//! - Medium: ≥50% Hit@3 (mock expansion rescues most semantic queries)
//! - Hard: ≥80% Hit@5 (BM25 handles directly at tier 1)
//! - Overall: ≥60% Hit@3

use async_trait::async_trait;
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Mock LLM that provides canned query expansion and synthesis
// ---------------------------------------------------------------------------

struct EvalMockProvider;

#[async_trait]
impl memex_core::LlmProvider for EvalMockProvider {
    async fn chat(
        &self,
        _system: Option<&str>,
        prompt: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let lower = prompt.to_lowercase();

        if lower.contains("expand this search query") {
            // Tier 2: query expansion — return reasonable search terms
            let terms = expand_query_mock(&lower);
            Ok(terms)
        } else if lower.contains("which wiki pages") {
            // Tier 3: LLM page selection from index — return all pages
            // (in real usage, the LLM would select relevant ones)
            Ok("wiki/api-design-principles.md\n\
                wiki/startup-fundraising-memo.md\n\
                wiki/distributed-systems-overview.md\n\
                wiki/machine-learning-primer.md\n\
                wiki/remote-work-policy.md\n\
                wiki/product-launch-retrospective.md\n"
                .to_string())
        } else if lower.contains("based on these wiki pages") {
            // Synthesis — just echo back which pages were provided
            Ok("Answer synthesized from provided wiki pages.".to_string())
        } else {
            Ok(String::new())
        }
    }
}

/// Mock query expansion: return reasonable search terms for known eval queries.
fn expand_query_mock(prompt_lower: &str) -> String {
    if prompt_lower.contains("structure rest endpoints") {
        "REST API design\nendpoint structure\nresource URL patterns\nHTTP methods\nAPI versioning"
    } else if prompt_lower.contains("raising money") || prompt_lower.contains("startup") {
        "fundraising\nSeries A\nventure capital\ninvestor pitch\nstartup funding"
    } else if prompt_lower.contains("consistency") && prompt_lower.contains("availability") {
        "CAP theorem\nconsistency availability partition\ndistributed tradeoffs"
    } else if prompt_lower.contains("memorizing data") || prompt_lower.contains("prevent models") {
        "overfitting\nregularization\nmachine learning\ntrain test split\ncross validation"
    } else if prompt_lower.contains("working from home") || prompt_lower.contains("guidelines") {
        "remote work policy\nwork from home\nhybrid office\nVPN security"
    } else if prompt_lower.contains("went wrong") || prompt_lower.contains("launch") {
        "product launch retrospective\npost-mortem\nProject Phoenix\nbeta bugs"
    } else {
        // Generic fallback
        "relevant search terms\nalternative keywords"
    }
    .to_string()
}

// ---------------------------------------------------------------------------
// Queries — from qmd's eval-bm25.test.ts
// ---------------------------------------------------------------------------

struct EvalQuery {
    query: &'static str,
    expected_doc: &'static str,
    difficulty: Difficulty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Difficulty {
    Easy,
    Medium,
    Hard,
}

const EVAL_QUERIES: &[EvalQuery] = &[
    // EASY
    EvalQuery {
        query: "API versioning",
        expected_doc: "api-design",
        difficulty: Difficulty::Easy,
    },
    EvalQuery {
        query: "Series A fundraising",
        expected_doc: "fundraising",
        difficulty: Difficulty::Easy,
    },
    EvalQuery {
        query: "CAP theorem",
        expected_doc: "distributed-systems",
        difficulty: Difficulty::Easy,
    },
    EvalQuery {
        query: "overfitting machine learning",
        expected_doc: "machine-learning",
        difficulty: Difficulty::Easy,
    },
    EvalQuery {
        query: "remote work VPN",
        expected_doc: "remote-work",
        difficulty: Difficulty::Easy,
    },
    EvalQuery {
        query: "Project Phoenix retrospective",
        expected_doc: "product-launch",
        difficulty: Difficulty::Easy,
    },
    // MEDIUM
    EvalQuery {
        query: "how to structure REST endpoints",
        expected_doc: "api-design",
        difficulty: Difficulty::Medium,
    },
    EvalQuery {
        query: "raising money for startup",
        expected_doc: "fundraising",
        difficulty: Difficulty::Medium,
    },
    EvalQuery {
        query: "consistency vs availability tradeoffs",
        expected_doc: "distributed-systems",
        difficulty: Difficulty::Medium,
    },
    EvalQuery {
        query: "how to prevent models from memorizing data",
        expected_doc: "machine-learning",
        difficulty: Difficulty::Medium,
    },
    EvalQuery {
        query: "working from home guidelines",
        expected_doc: "remote-work",
        difficulty: Difficulty::Medium,
    },
    EvalQuery {
        query: "what went wrong with the launch",
        expected_doc: "product-launch",
        difficulty: Difficulty::Medium,
    },
    // HARD
    EvalQuery {
        query: "nouns not verbs",
        expected_doc: "api-design",
        difficulty: Difficulty::Hard,
    },
    EvalQuery {
        query: "Sequoia investor pitch",
        expected_doc: "fundraising",
        difficulty: Difficulty::Hard,
    },
    EvalQuery {
        query: "Raft algorithm leader election",
        expected_doc: "distributed-systems",
        difficulty: Difficulty::Hard,
    },
    EvalQuery {
        query: "F1 score precision recall",
        expected_doc: "machine-learning",
        difficulty: Difficulty::Hard,
    },
    EvalQuery {
        query: "quarterly team gathering travel",
        expected_doc: "remote-work",
        difficulty: Difficulty::Hard,
    },
    EvalQuery {
        query: "beta program 47 bugs",
        expected_doc: "product-launch",
        difficulty: Difficulty::Hard,
    },
];

// ---------------------------------------------------------------------------
// Setup — create a full Memex instance with eval docs as wiki pages
// ---------------------------------------------------------------------------

fn setup() -> (TempDir, memex_core::Memex) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // Create wiki dir and write eval docs as wiki pages with frontmatter
    let wiki_dir = root.join("wiki");
    std::fs::create_dir_all(&wiki_dir).unwrap();

    let eval_docs_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/eval-docs");
    for entry in std::fs::read_dir(&eval_docs_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "md") {
            let content = std::fs::read_to_string(&path).unwrap();
            let filename = path.file_name().unwrap().to_string_lossy();
            let stem = filename.trim_end_matches(".md");
            let tags = stem.replace('-', " ");

            let title = content
                .lines()
                .find(|l| l.starts_with("# "))
                .map(|l| l.trim_start_matches("# "))
                .unwrap_or(stem);

            let wiki_page = format!(
                "---\ntitle: {title}\ntags:\n  - {tags}\ncreated: 2026-01-01T00:00:00Z\n\
                 last_updated: 2026-01-01T00:00:00Z\nsources: []\n---\n\n{content}"
            );
            std::fs::write(wiki_dir.join(&*filename), wiki_page).unwrap();
        }
    }

    // Create index.md
    let index = memex_core::index::rebuild_index(root).unwrap();
    std::fs::write(root.join("index.md"), index).unwrap();

    // Open Memex with mock provider
    let provider = Box::new(EvalMockProvider);
    let memex = memex_core::Memex::open(root.to_path_buf(), provider, "mock-model").unwrap();

    (dir, memex)
}

// ---------------------------------------------------------------------------
// Hit rate scoring via full query pipeline
// ---------------------------------------------------------------------------

fn pipeline_hit_rate(memex: &memex_core::Memex, queries: &[&EvalQuery], top_k: usize) -> f64 {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut hits = 0;
    for q in queries {
        let result = rt.block_on(memex.query(q.query)).unwrap();
        let found = result
            .citations
            .iter()
            .take(top_k)
            .any(|c| c.page.to_string_lossy().contains(q.expected_doc));
        if found {
            hits += 1;
        }
    }
    hits as f64 / queries.len() as f64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn pipeline_easy_queries_hit_at_3() {
    let (_dir, memex) = setup();
    let easy: Vec<&EvalQuery> = EVAL_QUERIES
        .iter()
        .filter(|q| q.difficulty == Difficulty::Easy)
        .collect();
    let rate = pipeline_hit_rate(&memex, &easy, 3);
    eprintln!(
        "Pipeline Easy Hit@3: {:.0}% ({}/{})",
        rate * 100.0,
        (rate * easy.len() as f64) as usize,
        easy.len()
    );
    assert!(rate >= 0.80, "Easy ≥80% Hit@3, got {:.0}%", rate * 100.0);
}

#[test]
fn pipeline_medium_queries_hit_at_3() {
    let (_dir, memex) = setup();
    let medium: Vec<&EvalQuery> = EVAL_QUERIES
        .iter()
        .filter(|q| q.difficulty == Difficulty::Medium)
        .collect();
    let rate = pipeline_hit_rate(&memex, &medium, 3);
    eprintln!(
        "Pipeline Medium Hit@3: {:.0}% ({}/{})",
        rate * 100.0,
        (rate * medium.len() as f64) as usize,
        medium.len()
    );
    assert!(
        rate >= 0.50,
        "Medium ≥50% Hit@3 with expansion, got {:.0}%",
        rate * 100.0
    );
}

#[test]
fn pipeline_hard_queries_hit_at_5() {
    let (_dir, memex) = setup();
    let hard: Vec<&EvalQuery> = EVAL_QUERIES
        .iter()
        .filter(|q| q.difficulty == Difficulty::Hard)
        .collect();
    let rate = pipeline_hit_rate(&memex, &hard, 5);
    eprintln!(
        "Pipeline Hard Hit@5: {:.0}% ({}/{})",
        rate * 100.0,
        (rate * hard.len() as f64) as usize,
        hard.len()
    );
    assert!(rate >= 0.80, "Hard ≥80% Hit@5, got {:.0}%", rate * 100.0);
}

#[test]
fn pipeline_overall_hit_at_3() {
    let (_dir, memex) = setup();
    let all: Vec<&EvalQuery> = EVAL_QUERIES.iter().collect();
    let rate = pipeline_hit_rate(&memex, &all, 3);
    eprintln!(
        "Pipeline Overall Hit@3: {:.0}% ({}/{})",
        rate * 100.0,
        (rate * all.len() as f64) as usize,
        all.len()
    );
    assert!(rate >= 0.60, "Overall ≥60% Hit@3, got {:.0}%", rate * 100.0);
}

/// Detailed per-query report showing which tier was used.
#[test]
fn pipeline_eval_report() {
    let (_dir, memex) = setup();
    let rt = tokio::runtime::Runtime::new().unwrap();

    eprintln!("\n=== Full Pipeline Eval Report (qmd dataset, mock LLM) ===\n");
    for q in EVAL_QUERIES {
        let result = rt.block_on(memex.query(q.query)).unwrap();
        let hit_rank = result
            .citations
            .iter()
            .position(|c| c.page.to_string_lossy().contains(q.expected_doc))
            .map(|i| i + 1);

        let status = match hit_rank {
            Some(1) => "✓".to_string(),
            Some(r) => format!("@{r}"),
            None => "✗".to_string(),
        };

        let cited_pages: Vec<String> = result
            .citations
            .iter()
            .map(|c| {
                c.page
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();

        eprintln!(
            "[{:6}] {} {:50} → {} (cited: {})",
            format!("{:?}", q.difficulty),
            status,
            q.query,
            q.expected_doc,
            if cited_pages.is_empty() {
                "none".to_string()
            } else {
                cited_pages.join(", ")
            },
        );
    }
}
