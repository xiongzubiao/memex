use std::io;
use std::path::Path;

// ---------------------------------------------------------------------------
// Embedded fallback constants
// ---------------------------------------------------------------------------

pub const EMBEDDED_IDENTITY: &str = "Memex agent, a knowledge assistant inside Memex.";

pub const EMBEDDED_AGENTS_MD: &str = r#"# Wiki Conventions

version: 1

## Page Format
- Every page has YAML frontmatter: title, tags, created, last_updated, sources
- Tags: free-form labels (e.g., entity, concept). Reserved: brainstorm, contradiction
- Page names: lowercase-kebab-case, descriptive

## Cross-References
- Use [[links]] for cross-references (Obsidian-compatible)
- Link related concepts when they share context
- Contradictions get dedicated pages linking both sides

## When to Create vs Update
- New concept/entity not covered: create
- Existing page covers it: update with new information
- Conflicting information: create contradiction page

## Citations
- Link back to source file in sources/
- Every claim should be traceable to a source
"#;

pub const EMBEDDED_CLAUDE_MD: &str = "@AGENTS.md\n";

pub const EMBEDDED_GEMINI_MD: &str = "@AGENTS.md\n";

pub const EMBEDDED_SOUL: &str = r#"# Memex Agent

You are a knowledge assistant managing a personal knowledge base (memex).

## Rhythm
Follow this pattern for most tasks:
1. GATHER: Context is auto-loaded each turn. Read files or
   search the web if you need more information.
2. ACT: Execute the task. Read sources, synthesize knowledge,
   store results, answer questions, or fix issues.
3. REFLECT: Evaluate the result. Store new knowledge via memory_store.

## Tool guidance
- memory_store: save knowledge (key = topic name, e.g. "circuit-breaker")
- memory_recall: search for relevant knowledge
- file_read: read source files or local files
- ask_user: interact with the user when you need input or approval

## Knowledge conventions
- Every page has YAML frontmatter: title, tags, created, last_updated, sources
- Tags: free-form labels (e.g., entity, concept). Reserved: brainstorm, contradiction
- Use [[links]] for cross-references between pages
- Write knowledge, not conversation summaries
- Cite specific source files in the sources: frontmatter field

## Mode behavior
- single-shot: decide everything yourself, no ask_user calls
- interactive: present findings, explain reasoning, ask before writing
"#;

pub const EMBEDDED_PROPOSER_PROMPT: &str = r#"You are a proposer in a multi-model brainstorming session. Your task is to generate a structured proposal.

For each section defined in the preset, provide:
1. A clear, specific approach
2. Concrete details and examples
3. Trade-offs and alternatives considered
4. How this section relates to other sections

Structure your response with clear section headers matching the preset sections. Be thorough but concise. Focus on substance over filler.
"#;

pub const EMBEDDED_REVIEWER_PROMPT: &str = r#"You are a reviewer in a multi-model brainstorming session. Your task is to evaluate a merged draft.

For each section:
1. Rate as BETTER, WORSE, or SAME compared to the previous round (if applicable)
2. State specific strengths (what works well)
3. State specific weaknesses (what needs improvement)
4. Suggest concrete improvements
5. Flag any contradictions with other sections

Be constructive but honest. Point to specific text when critiquing. If a section is strong, say so briefly and move on.
"#;

pub const EMBEDDED_MERGE_TEMPLATE: &str = r#"Synthesize the following outputs into a single coherent draft.

## Task
{{task}}

## Outputs to Merge
{{outputs}}

## Review Critiques to Incorporate
{{critiques}}

Rules:
- Keep the best ideas from each output
- Resolve contradictions with reasoned judgment
- Maintain consistent style and terminology
- Every section must be present and complete
- Do not add placeholder text like "TBD" or "TODO"
- If no critiques are provided above, focus on synthesizing the outputs
"#;

// ---------------------------------------------------------------------------
// Loader helpers
// ---------------------------------------------------------------------------

fn read_or_warn(path: &std::path::PathBuf, label: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            tracing::info!("Loaded {} from {}", label, path.display());
            Some(content)
        }
        Err(e) => {
            tracing::warn!(
                "Could not read {} at {}: {}. Using embedded fallback.",
                label,
                path.display(),
                e
            );
            None
        }
    }
}

/// Load agent identity from `IDENTITY.md` inside `memex_root`, falling back
/// to [`EMBEDDED_IDENTITY`].
pub fn load_identity(memex_root: &Path) -> String {
    let path = memex_root.join("IDENTITY.md");
    read_or_warn(&path, "IDENTITY.md").unwrap_or_else(|| EMBEDDED_IDENTITY.to_string())
}

/// Load agent soul from `SOUL.md` inside `memex_root`, falling back to
/// [`EMBEDDED_SOUL`].
pub fn load_soul(memex_root: &Path) -> String {
    let path = memex_root.join("SOUL.md");
    read_or_warn(&path, "SOUL.md").unwrap_or_else(|| EMBEDDED_SOUL.to_string())
}

/// Load a prompt file from `prompts/{name}.md` inside `memex_root`.
///
/// Falls back to the appropriate embedded constant when the file is absent.
/// Recognised names: `"proposer"`, `"reviewer"`. Any other name falls back to
/// an empty string with a warning.
pub fn load_prompt_file(memex_root: &Path, name: &str) -> String {
    let path = memex_root.join("prompts").join(format!("{name}.md"));
    read_or_warn(&path, &format!("prompts/{name}.md")).unwrap_or_else(|| match name {
        "proposer" => EMBEDDED_PROPOSER_PROMPT.to_string(),
        "reviewer" => EMBEDDED_REVIEWER_PROMPT.to_string(),
        other => {
            tracing::warn!("No embedded fallback for system prompt '{}'", other);
            String::new()
        }
    })
}

/// Load the merge prompt template from `prompts/merge.md` inside `memex_root`,
/// falling back to [`EMBEDDED_MERGE_TEMPLATE`].
pub fn load_merge_template(memex_root: &Path) -> String {
    let path = memex_root.join("prompts").join("merge.md");
    read_or_warn(&path, "prompts/merge.md").unwrap_or_else(|| EMBEDDED_MERGE_TEMPLATE.to_string())
}

// ---------------------------------------------------------------------------
// Scaffold
// ---------------------------------------------------------------------------

/// Create identity/soul/prompt files inside `memex_root` if they do not already
/// exist. This is a one-time setup step; existing files are never overwritten.
///
/// Creates:
/// - `AGENTS.md`  — wiki conventions (Karpathy's schema layer)
/// - `CLAUDE.md`  — contains `@AGENTS.md`
/// - `GEMINI.md`  — contains `@AGENTS.md`
/// - `IDENTITY.md`
/// - `SOUL.md`
/// - `prompts/proposer.md`
/// - `prompts/reviewer.md`
/// - `prompts/merge.md`
pub fn scaffold_identity_files(memex_root: &Path) -> io::Result<()> {
    // Ensure sub-directories exist.
    let prompts_dir = memex_root.join("prompts");
    std::fs::create_dir_all(&prompts_dir)?;

    let files: Vec<(std::path::PathBuf, &str)> = vec![
        (memex_root.join("AGENTS.md"), EMBEDDED_AGENTS_MD),
        (memex_root.join("CLAUDE.md"), EMBEDDED_CLAUDE_MD),
        (memex_root.join("GEMINI.md"), EMBEDDED_GEMINI_MD),
        (memex_root.join("IDENTITY.md"), EMBEDDED_IDENTITY),
        (memex_root.join("SOUL.md"), EMBEDDED_SOUL),
        (prompts_dir.join("proposer.md"), EMBEDDED_PROPOSER_PROMPT),
        (prompts_dir.join("reviewer.md"), EMBEDDED_REVIEWER_PROMPT),
        (prompts_dir.join("merge.md"), EMBEDDED_MERGE_TEMPLATE),
    ];

    for (path, content) in &files {
        if path.exists() {
            tracing::info!("scaffold: {} already exists, skipping", path.display());
        } else {
            std::fs::write(path, content)?;
            tracing::info!("scaffold: created {}", path.display());
        }
    }

    Ok(())
}

/// Scaffold brainstorm presets inside `memex_root/presets/` if they don't exist.
///
/// Presets define dimensions and sections for brainstorm task types.
/// Shipped presets: software, research, article, book, strategy, general.
pub fn scaffold_presets(memex_root: &Path) -> io::Result<()> {
    let presets_dir = memex_root.join("presets");
    std::fs::create_dir_all(&presets_dir)?;

    let presets: &[(&str, &str)] = &[
        ("software.toml", include_str!("../presets/software.toml")),
        ("research.toml", include_str!("../presets/research.toml")),
        ("article.toml", include_str!("../presets/article.toml")),
        ("book.toml", include_str!("../presets/book.toml")),
        ("strategy.toml", include_str!("../presets/strategy.toml")),
        ("general.toml", include_str!("../presets/general.toml")),
    ];

    for (name, content) in presets {
        let path = presets_dir.join(name);
        if path.exists() {
            tracing::info!("scaffold: {} already exists, skipping", path.display());
        } else {
            std::fs::write(&path, content)?;
            tracing::info!("scaffold: created {}", path.display());
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    // ------------------------------------------------------------------
    // load_identity
    // ------------------------------------------------------------------

    #[test]
    fn load_identity_uses_fallback() {
        let dir = tmp();
        // No IDENTITY.md written — should fall back to embedded constant.
        let result = load_identity(dir.path());
        assert_eq!(result, EMBEDDED_IDENTITY);
    }

    #[test]
    fn load_identity_prefers_disk() {
        let dir = tmp();
        let identity_path = dir.path().join("IDENTITY.md");
        std::fs::write(&identity_path, "Custom identity on disk").unwrap();

        let result = load_identity(dir.path());
        assert_eq!(result, "Custom identity on disk");
    }

    // ------------------------------------------------------------------
    // load_soul
    // ------------------------------------------------------------------

    #[test]
    fn load_soul_uses_fallback() {
        let dir = tmp();
        let result = load_soul(dir.path());
        assert_eq!(result, EMBEDDED_SOUL);
    }

    #[test]
    fn load_soul_prefers_disk() {
        let dir = tmp();
        std::fs::write(dir.path().join("SOUL.md"), "Custom soul").unwrap();
        assert_eq!(load_soul(dir.path()), "Custom soul");
    }

    // ------------------------------------------------------------------
    // load_prompt_file
    // ------------------------------------------------------------------

    #[test]
    fn load_prompt_file_proposer_fallback() {
        let dir = tmp();
        let result = load_prompt_file(dir.path(), "proposer");
        assert_eq!(result, EMBEDDED_PROPOSER_PROMPT);
    }

    #[test]
    fn load_prompt_file_reviewer_fallback() {
        let dir = tmp();
        let result = load_prompt_file(dir.path(), "reviewer");
        assert_eq!(result, EMBEDDED_REVIEWER_PROMPT);
    }

    #[test]
    fn load_prompt_file_unknown_returns_empty() {
        let dir = tmp();
        let result = load_prompt_file(dir.path(), "unknown_role");
        assert!(result.is_empty());
    }

    #[test]
    fn load_prompt_file_prefers_disk() {
        let dir = tmp();
        let p_dir = dir.path().join("prompts");
        std::fs::create_dir_all(&p_dir).unwrap();
        std::fs::write(p_dir.join("proposer.md"), "Disk proposer prompt").unwrap();

        let result = load_prompt_file(dir.path(), "proposer");
        assert_eq!(result, "Disk proposer prompt");
    }

    // ------------------------------------------------------------------
    // load_merge_template
    // ------------------------------------------------------------------

    #[test]
    fn load_merge_template_uses_fallback() {
        let dir = tmp();
        let result = load_merge_template(dir.path());
        assert_eq!(result, EMBEDDED_MERGE_TEMPLATE);
    }

    #[test]
    fn load_merge_template_prefers_disk() {
        let dir = tmp();
        let p_dir = dir.path().join("prompts");
        std::fs::create_dir_all(&p_dir).unwrap();
        std::fs::write(p_dir.join("merge.md"), "Custom merge template {{task}}").unwrap();

        let result = load_merge_template(dir.path());
        assert_eq!(result, "Custom merge template {{task}}");
    }

    // ------------------------------------------------------------------
    // scaffold_creates_files
    // ------------------------------------------------------------------

    #[test]
    fn scaffold_creates_files() {
        let dir = tmp();
        scaffold_identity_files(dir.path()).expect("scaffold should succeed");

        assert!(dir.path().join("AGENTS.md").exists(), "AGENTS.md missing");
        assert!(dir.path().join("CLAUDE.md").exists(), "CLAUDE.md missing");
        assert!(dir.path().join("GEMINI.md").exists(), "GEMINI.md missing");
        assert!(
            dir.path().join("IDENTITY.md").exists(),
            "IDENTITY.md missing"
        );
        assert!(dir.path().join("SOUL.md").exists(), "SOUL.md missing");
        assert!(
            dir.path().join("prompts/proposer.md").exists(),
            "prompts/proposer.md missing"
        );
        assert!(
            dir.path().join("prompts/reviewer.md").exists(),
            "prompts/reviewer.md missing"
        );
        assert!(
            dir.path().join("prompts/merge.md").exists(),
            "prompts/merge.md missing"
        );
    }

    #[test]
    fn scaffold_does_not_overwrite_existing() {
        let dir = tmp();
        let identity_path = dir.path().join("IDENTITY.md");
        std::fs::write(&identity_path, "Preserved content").unwrap();

        scaffold_identity_files(dir.path()).expect("scaffold should succeed");

        let content = std::fs::read_to_string(&identity_path).unwrap();
        assert_eq!(
            content, "Preserved content",
            "existing file was overwritten"
        );
    }

    #[test]
    fn scaffold_content_matches_embedded() {
        let dir = tmp();
        scaffold_identity_files(dir.path()).unwrap();

        let agents = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert_eq!(agents, EMBEDDED_AGENTS_MD);

        let claude = std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();
        assert_eq!(claude, EMBEDDED_CLAUDE_MD);

        let gemini = std::fs::read_to_string(dir.path().join("GEMINI.md")).unwrap();
        assert_eq!(gemini, EMBEDDED_GEMINI_MD);

        let identity = std::fs::read_to_string(dir.path().join("IDENTITY.md")).unwrap();
        assert_eq!(identity, EMBEDDED_IDENTITY);

        let soul = std::fs::read_to_string(dir.path().join("SOUL.md")).unwrap();
        assert_eq!(soul, EMBEDDED_SOUL);

        let proposer = std::fs::read_to_string(dir.path().join("prompts/proposer.md")).unwrap();
        assert_eq!(proposer, EMBEDDED_PROPOSER_PROMPT);

        let reviewer = std::fs::read_to_string(dir.path().join("prompts/reviewer.md")).unwrap();
        assert_eq!(reviewer, EMBEDDED_REVIEWER_PROMPT);

        let merge = std::fs::read_to_string(dir.path().join("prompts/merge.md")).unwrap();
        assert_eq!(merge, EMBEDDED_MERGE_TEMPLATE);
    }

    // ------------------------------------------------------------------
    // scaffold_presets
    // ------------------------------------------------------------------

    #[test]
    fn scaffold_presets_creates_all_files() {
        let dir = tmp();
        scaffold_presets(dir.path()).expect("scaffold_presets should succeed");

        for name in &[
            "software.toml",
            "research.toml",
            "article.toml",
            "book.toml",
            "strategy.toml",
            "general.toml",
        ] {
            assert!(
                dir.path().join("presets").join(name).exists(),
                "presets/{name} missing"
            );
        }
    }

    #[test]
    fn scaffold_presets_does_not_overwrite() {
        let dir = tmp();
        let presets_dir = dir.path().join("presets");
        std::fs::create_dir_all(&presets_dir).unwrap();
        std::fs::write(presets_dir.join("software.toml"), "custom").unwrap();

        scaffold_presets(dir.path()).unwrap();

        let content = std::fs::read_to_string(presets_dir.join("software.toml")).unwrap();
        assert_eq!(content, "custom", "existing preset was overwritten");
    }
}
