use crate::error::{MemexError, Result};
use crate::search::WikiSearch;
use crate::types::{IngestReport, PageAction, ProposedPage, Source, SourceFormat};
use crate::{Memex, index, llm_output, log, source_storage, storage, validate};
use std::path::{Path, PathBuf};
use tracing::{error, info, warn};

const SYSTEM_PROMPT: &str = "You are a knowledge synthesis assistant. \
    Your task is to read source material and produce structured wiki pages \
    in the specified format. Always include valid YAML frontmatter with \
    title, tags, created, last_updated, and sources fields.";

const INGEST_PROMPT_TEMPLATE: &str = r#"You are updating a personal wiki knowledge base.

Current index:
{index}

Source content to ingest (each section marked with [Source: path]):
{content}

Available source paths for citation:
{source_ref}

Produce wiki pages in this exact format:

<<< PAGE: wiki/page-name.md >>>
<<< ACTION: create >>>
---
title: Page Title
summary: One-sentence summary of the page (under 120 characters).
tags:
  - entity
created: {timestamp}
last_updated: {timestamp}
sources:
  - source/path/here
---

Page body content here.
<<< END PAGE >>>

Tags: free-form labels describing page content (e.g., entity, concept). Reserved tags: contradiction, brainstorm.
Summary: a concise one-sentence description of the page, under 120 characters. Used in the wiki index.

Rules:
- Create entity/concept pages for important topics. Do NOT create source-summary pages.
- Use kebab-case filenames with wiki/ prefix
- In the sources field, list ONLY the specific source paths that contributed to each page.
  Each source is marked with [Source: path] in the content above. Cite the exact paths.
- Write knowledge, not conversation summaries. State facts directly. Do NOT write "The
  conversation covered..." or "Flows discussed in the source..." — just state what is true.
- IMPORTANT: Use [[wiki links]] to cross-reference between pages. For example, if you create
  a page at wiki/oauth2.md and another at wiki/authentik.md, the authentik page should contain
  [[oauth2]] where it references OAuth2 concepts. Use the filename without .md extension.
  Every page should have at least one [[link]] to a related page when relevant.
"#;

const IMAGE_ENRICHMENT_PROMPT: &str = r#"These wiki pages were just created from conversations:

{pages}

These images were shared in those conversations:
{image_markers}

For each image:
- Describe what it shows
- If it adds context to an existing page above, update that page with the visual information
- If it represents something not yet covered, create a new page

Use the same <<< PAGE: >>> format for updates or new pages."#;

/// Maximum images per LLM call (zeroclaw default max is 16, leave headroom).
const IMAGES_PER_BATCH: usize = 8;

fn make_source_ref(root: &Path, stored_path: &Path) -> String {
    let rel = stored_path
        .strip_prefix(root)
        .unwrap_or(stored_path)
        .to_string_lossy();
    format!("  - {rel}")
}

impl Memex {
    /// Dispatch to ingest_file, ingest_url, or ingest_directory based on source type.
    pub async fn ingest(&self, source: &Source) -> Result<IngestReport> {
        match source {
            Source::File { path } => self.ingest_file(path).await,
            Source::Url { url } => self.ingest_url(url).await,
            Source::Directory { path } => self.ingest_directory(path).await,
        }
    }

    /// Main file ingest flow:
    /// 1. Store original via source_storage (dedup by hash)
    /// 2. Read content as text
    /// 3. Build prompt with current index
    /// 4. Call LLM for wiki synthesis
    /// 5. Parse, validate, write pages
    async fn ingest_file(&self, path: &Path) -> Result<IngestReport> {
        let format = source_storage::detect_format(path);

        // Archives: two-phase ingest (store then batch-synthesize)
        if matches!(format, SourceFormat::Zip | SourceFormat::Tgz) {
            return self.ingest_archive(path, &format).await;
        }

        // Session files: detect platform and store under sources/{platform}/sessions/
        if matches!(format, SourceFormat::Jsonl | SourceFormat::Json)
            && let Some((platform, content)) = self.detect_session(path)
        {
            let Some(stored_path) =
                source_storage::store_session_source(&self.root, path, &platform, None)?
            else {
                self.report_progress("Session already ingested, skipping.");
                return Ok(IngestReport::default());
            };
            let source_ref = make_source_ref(&self.root, &stored_path);
            let proposed = self.synthesize_wiki(&content, &source_ref, None).await?;
            return self.write_proposed_pages(&proposed).await;
        }

        // Normal files: store original in sources/documents/
        self.report_progress("Storing source...");
        let Some(stored_path) = source_storage::store_document_source(&self.root, path, None)?
        else {
            self.report_progress("Already ingested, skipping.");
            return Ok(IngestReport::default());
        };

        let content = self.read_source_content(path, &format)?;

        let source_ref = make_source_ref(&self.root, &stored_path);

        // Build and execute LLM prompt
        self.report_progress("Synthesizing wiki pages...");
        let proposed = self.synthesize_wiki(&content, &source_ref, None).await?;

        self.report_progress(&format!("Writing {} wiki page(s)...", proposed.len()));
        self.write_proposed_pages(&proposed).await
    }

    /// Detect if a file is a known session format. Reads file once, tries all parsers.
    /// Returns (platform, parsed_content).
    fn detect_session(&self, path: &Path) -> Option<(String, String)> {
        let content = std::fs::read_to_string(path).ok()?;
        let session_id = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        if let Ok(conv) =
            crate::parsers::claude_code::parse_claude_code_content(&content, session_id.clone())
            && !conv.messages.is_empty()
        {
            return Some(("claude-code".to_string(), conv.to_json()));
        }
        if let Ok(conv) = crate::parsers::codex::parse_codex_content(&content, session_id.clone())
            && !conv.messages.is_empty()
        {
            return Some(("codex".to_string(), conv.to_json()));
        }
        if let Ok(conv) = crate::parsers::gemini_cli::parse_gemini_cli_content(&content, session_id)
            && !conv.messages.is_empty()
        {
            return Some(("gemini-cli".to_string(), conv.to_json()));
        }
        None
    }

    /// Two-phase archive ingest:
    /// Phase 1: Extract and store individual conversations + images + support files
    /// Phase 2: Batch-synthesize new conversations into wiki pages
    async fn ingest_archive(&self, path: &Path, format: &SourceFormat) -> Result<IngestReport> {
        // Phase 1: Store conversations
        let (new_conv_paths, platform) = self.store_archive_conversations(path, format)?;

        if new_conv_paths.is_empty() {
            info!("All conversations already ingested, nothing new");
            self.report_progress("All conversations already ingested.");
            return Ok(IngestReport::default());
        }

        info!(count = new_conv_paths.len(), platform = %platform, "stored new conversations");
        self.report_progress(&format!(
            "Stored {} new {} conversation(s).",
            new_conv_paths.len(),
            platform
        ));

        // Phase 2: Batch synthesize
        self.batch_synthesize_conversations(&new_conv_paths, &platform)
            .await
    }

    /// Phase 1: Extract archive and store conversations individually.
    /// Returns (newly_stored_paths, platform_name).
    fn store_archive_conversations(
        &self,
        path: &Path,
        format: &SourceFormat,
    ) -> Result<(Vec<std::path::PathBuf>, String)> {
        // Try ChatGPT (ZIP only)
        if *format == SourceFormat::Zip
            && let Ok(convs) = crate::parsers::chatgpt::parse_chatgpt_zip(path)
            && !convs.is_empty()
        {
            // Extract ZIP to temp dir for images/support files
            let temp_dir =
                std::env::temp_dir().join(format!("memex-chatgpt-{}", std::process::id()));
            std::fs::create_dir_all(&temp_dir)?;
            let file = std::fs::File::open(path)?;
            if let Ok(mut archive) = zip::ZipArchive::new(file) {
                let _ = archive.extract(&temp_dir);
            }
            let stored = crate::parsers::chatgpt::store_chatgpt_conversations(
                &self.root,
                &convs,
                Some(&temp_dir),
            )?;
            let _ = std::fs::remove_dir_all(&temp_dir);
            let skipped = convs.len() - stored.len();
            info!(
                parsed = convs.len(),
                new = stored.len(),
                "ChatGPT conversations stored"
            );
            if skipped > 0 {
                self.report_progress(&format!(
                    "Skipped {} already-ingested ChatGPT conversation(s).",
                    skipped
                ));
            }
            return Ok((stored, "chatgpt".to_string()));
        }

        // Try Gemini Takeout (ZIP or TGZ)
        if let Ok((convs, image_filenames, temp_dir)) =
            crate::parsers::gemini::extract_and_parse(path)
            && !convs.is_empty()
        {
            let stored = crate::parsers::gemini::store_conversations(
                &self.root,
                &convs,
                Some(&temp_dir),
                &image_filenames,
            )?;
            let _ = std::fs::remove_dir_all(&temp_dir);
            let skipped = convs.len() - stored.len();
            info!(
                parsed = convs.len(),
                new = stored.len(),
                "Gemini conversations stored"
            );
            if skipped > 0 {
                self.report_progress(&format!(
                    "Skipped {} already-ingested Gemini conversation(s).",
                    skipped
                ));
            }
            return Ok((stored, "gemini".to_string()));
        }

        Err(MemexError::ValidationFailure {
            details: format!(
                "Archive at {} contains no parseable conversations",
                path.display()
            ),
        })
    }

    /// Phase 2: Read stored conversation files in batches, synthesize wiki pages.
    /// Each batch sees the current index.md, so later batches build on earlier wiki pages.
    /// Batches by token budget (~100K tokens = ~400KB), not by fixed count.
    async fn batch_synthesize_conversations(
        &self,
        conv_paths: &[std::path::PathBuf],
        platform: &str,
    ) -> Result<IngestReport> {
        let mut combined = IngestReport::default();
        let total = conv_paths.len();

        // Dynamic budget from model context window
        let system_prompt_tokens = (SYSTEM_PROMPT.len() + INGEST_PROMPT_TEMPLATE.len())
            / crate::model_catalog::BYTES_PER_TOKEN;
        let index_tokens = self.index_token_count().unwrap_or(0);
        let max_batch_bytes =
            compute_batch_budget_bytes(self.model(), system_prompt_tokens, index_tokens);
        info!(
            budget_bytes = max_batch_bytes,
            model = self.model(),
            index_tokens,
            "computed batch budget"
        );
        // Per-conversation wrapper: "[Source: {path}]\n{content}\n\n---\n\n"
        const WRAPPER_OVERHEAD: usize = 32; // "[Source: " + "]\n" + "\n\n---\n\n"

        let mut batches: Vec<Vec<(std::path::PathBuf, usize)>> = Vec::new();
        let mut current_batch: Vec<(std::path::PathBuf, usize)> = Vec::new();
        let mut current_bytes: usize = 0;

        for conv_path in conv_paths {
            let full_path = self.root.join(conv_path);
            let size = std::fs::metadata(&full_path)
                .map(|m| m.len() as usize)
                .unwrap_or(0);
            let path_len = conv_path.to_string_lossy().len();
            let wrapped_size = size + WRAPPER_OVERHEAD + path_len;

            if wrapped_size > max_batch_bytes {
                // Single conversation exceeds entire budget — skip with warning
                let msg = format!(
                    "skipping {} ({}KB exceeds {}KB batch budget). Use a model with a larger context window or split the conversation.",
                    conv_path.display(),
                    wrapped_size / 1024,
                    max_batch_bytes / 1024,
                );
                warn!("{msg}");
                combined.warnings.push(msg);
                continue;
            }

            if !current_batch.is_empty() && current_bytes + wrapped_size > max_batch_bytes {
                batches.push(std::mem::take(&mut current_batch));
                current_bytes = 0;
            }
            current_batch.push((conv_path.clone(), wrapped_size));
            current_bytes += wrapped_size;
        }
        if !current_batch.is_empty() {
            batches.push(current_batch);
        }

        // Guard: if budget is zero (model lookup failed or overhead exceeds window), bail early
        if max_batch_bytes == 0 {
            let msg = format!(
                "batch budget is 0 bytes for model '{}' — check model name in config.toml",
                self.model()
            );
            warn!("{msg}");
            combined.warnings.push(msg);
            return Ok(combined);
        }

        let total_batches = batches.len();
        let mut conv_index = 0;

        for (batch_num, batch) in batches.iter().enumerate() {
            let batch_start = conv_index + 1;
            let batch_end = conv_index + batch.len();
            let batch_kb: usize = batch.iter().map(|(_, s)| s).sum::<usize>() / 1024;
            conv_index = batch_end;
            info!(
                batch = batch_num + 1,
                total_batches,
                conv_start = batch_start,
                conv_end = batch_end,
                total,
                size_kb = batch_kb,
                "synthesizing batch"
            );
            self.report_progress(&format!(
                "Synthesizing batch {}/{} (conversations {}-{}/{}, {}KB)...",
                batch_num + 1,
                total_batches,
                batch_start,
                batch_end,
                total,
                batch_kb
            ));

            let mut batch_content = String::new();
            let mut batch_source_paths: Vec<String> = Vec::new();
            for (conv_path, _) in batch {
                let full_path = self.root.join(conv_path);
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    let conv_ref = conv_path.to_string_lossy();
                    batch_content.push_str(&format!("[Source: {conv_ref}]\n{content}\n\n---\n\n"));
                    batch_source_paths.push(conv_ref.to_string());
                }
            }

            if batch_content.is_empty() {
                continue;
            }

            let source_ref = batch_source_paths
                .iter()
                .map(|p| format!("  - {p}"))
                .collect::<Vec<_>>()
                .join("\n");
            match self
                .synthesize_wiki(&batch_content, &source_ref, None)
                .await
            {
                Ok(proposed) => match self.write_proposed_pages(&proposed).await {
                    Ok(report) => {
                        // Image enrichment pass 2 (before partial moves out of report)
                        match self.enrich_with_images(&report, batch, platform).await {
                            Ok(img_report) => combined.merge(img_report),
                            Err(e) => warn!(error = %e, "image enrichment failed for batch"),
                        }

                        combined.merge(report);
                    }
                    Err(e) => {
                        let msg =
                            format!("batch {}/{total_batches} write failed: {e}", batch_num + 1);
                        warn!("{msg}");
                        combined.warnings.push(msg);
                    }
                },
                Err(e) => {
                    let msg = format!(
                        "batch {}/{total_batches} synthesis failed: {e}",
                        batch_num + 1
                    );
                    warn!("{msg}");
                    combined.warnings.push(msg);
                }
            }
        }

        Ok(combined)
    }

    /// Ingest a Claude.ai export folder (contains conversations.json).
    async fn ingest_claude_folder(&self, folder: &Path) -> Result<IngestReport> {
        let convs = crate::parsers::claude::parse_claude_export(folder)?;
        if convs.is_empty() {
            return Ok(IngestReport::default());
        }
        let stored = crate::parsers::claude::store_claude_export(&self.root, folder, &convs)?;
        info!(
            parsed = convs.len(),
            new = stored.len(),
            "Claude conversations stored"
        );

        let mut combined = IngestReport::default();

        // Synthesize memories.json if present (user profile data from Claude)
        let memories_path = self.root.join("sources/claude/memories.json");
        if let Ok(content) = std::fs::read_to_string(&memories_path) {
            let hash = storage::content_hash(content.as_bytes());
            let marker = self.root.join("sources/claude/.memories_hash");
            let already_done = std::fs::read_to_string(&marker)
                .ok()
                .is_some_and(|h| h.trim() == hash);
            if !already_done {
                info!("synthesizing Claude memories.json into wiki");
                let source_ref = "  - sources/claude/memories.json".to_string();
                match self.synthesize_wiki(&content, &source_ref, None).await {
                    Ok(proposed) => {
                        if let Ok(report) = self.write_proposed_pages(&proposed).await {
                            combined.merge(report);
                        }
                    }
                    Err(e) => warn!(error = %e, "memories.json synthesis failed"),
                }
                let _ = std::fs::write(&marker, &hash);
            }
        }

        if stored.is_empty() {
            return Ok(combined);
        }
        let conv_report = self
            .batch_synthesize_conversations(&stored, "claude")
            .await?;
        combined.merge(conv_report);
        Ok(combined)
    }

    /// Fetch URL content, store as source, then synthesize wiki pages.
    async fn ingest_url(&self, url: &str) -> Result<IngestReport> {
        // Fetch URL content
        let response = reqwest::get(url)
            .await
            .map_err(|e| MemexError::Other(e.into()))?;
        let body = response
            .text()
            .await
            .map_err(|e| MemexError::Other(e.into()))?;

        // Check dedup by hash before writing anything
        let safe_name = url_to_filename(url);
        let hash = storage::content_hash(body.as_bytes());
        let prefix = &hash[..12];
        let docs_dir = self.root.join("sources/documents");
        std::fs::create_dir_all(&docs_dir)?;

        let already_stored = std::fs::read_dir(&docs_dir)
            .ok()
            .map(|entries| {
                entries.filter_map(|e| e.ok()).any(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.starts_with(prefix) && !name.ends_with(".meta.json")
                })
            })
            .unwrap_or(false);

        if already_stored {
            self.report_progress("URL content already ingested, skipping.");
            return Ok(IngestReport::default());
        }

        // Write directly to final hash-prefixed name
        let stored_name = format!("{}-{}", prefix, safe_name);
        let stored_path = docs_dir.join(&stored_name);
        std::fs::write(&stored_path, &body)?;

        // Write meta
        let meta = crate::types::SourceMeta {
            original_path: url.to_string(),
            format: SourceFormat::Html,
            hash,
            ingested_at: chrono::Utc::now(),
        };
        let meta_json =
            serde_json::to_string_pretty(&meta).map_err(|e| MemexError::Other(e.into()))?;
        std::fs::write(docs_dir.join(format!("{stored_name}.meta.json")), meta_json)?;

        let source_ref = make_source_ref(&self.root, &stored_path);

        let proposed = self.synthesize_wiki(&body, &source_ref, None).await?;
        self.write_proposed_pages(&proposed).await
    }

    /// Walk directory, skip already-ingested files, call ingest_file for each.
    async fn ingest_directory(&self, dir: &Path) -> Result<IngestReport> {
        // Check if this directory is a Claude.ai export (has conversations.json)
        if dir.join("conversations.json").exists() {
            return self.ingest_claude_folder(dir).await;
        }

        // Detect platform root directories and collect session/memory files
        if let Some((platform, platform_files)) = detect_platform_files(dir) {
            info!(
                platform = %platform,
                files = platform_files.len(),
                "detected platform data"
            );

            self.report_progress(&format!(
                "Detected {} {} file(s).",
                platform_files.len(),
                platform
            ));

            // Split into sessions and memory, store each under sources/{platform}/
            let mut session_stored_paths: Vec<PathBuf> = Vec::new();
            let mut combined = IngestReport::default();
            let mut skipped = 0usize;

            for file_path in &platform_files {
                let is_memory = file_path.components().any(|c| c.as_os_str() == "memory");
                let subdir = if is_memory { "memory" } else { "sessions" };
                let store_dir = format!("{platform}/{subdir}");
                let Some(stored_path) =
                    source_storage::store_session_source(&self.root, file_path, &store_dir, None)?
                else {
                    skipped += 1;
                    continue; // dedup
                };

                if is_memory {
                    // Memory files: synthesize individually (few and small)
                    let content = std::fs::read_to_string(file_path).unwrap_or_default();
                    if content.is_empty() {
                        continue;
                    }
                    let source_ref = make_source_ref(&self.root, &stored_path);
                    match self.synthesize_wiki(&content, &source_ref, None).await {
                        Ok(proposed) => {
                            if let Ok(report) = self.write_proposed_pages(&proposed).await {
                                combined.merge(report);
                            }
                        }
                        Err(e) => {
                            warn!(path = %file_path.display(), error = %e, "memory synthesis failed")
                        }
                    }
                } else {
                    // Sessions: collect stored paths for batch synthesis
                    let rel_path = stored_path
                        .strip_prefix(&self.root)
                        .unwrap_or(&stored_path)
                        .to_path_buf();
                    session_stored_paths.push(rel_path);
                }
            }

            if skipped > 0 {
                self.report_progress(&format!("Skipped {} already-ingested file(s).", skipped));
            }

            // Batch synthesize sessions (same infrastructure as conversation archives)
            if !session_stored_paths.is_empty() {
                let batch_report = self
                    .batch_synthesize_conversations(&session_stored_paths, &platform)
                    .await?;
                combined.merge(batch_report);
            }

            return Ok(combined);
        }

        // Generic directory walk: ingest all recognized files
        let mut combined = IngestReport::default();
        for entry in walkdir::WalkDir::new(dir)
            .min_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let file_path = entry.path().to_path_buf();

            // Skip metadata sidecars and dotfiles
            let filename = file_path.file_name().unwrap_or_default().to_string_lossy();
            if filename.ends_with(".meta.json") || filename.starts_with('.') {
                continue;
            }

            // Skip unknown formats
            let format = source_storage::detect_format(&file_path);
            if format == SourceFormat::Unknown {
                continue;
            }

            // ingest_file handles dedup internally via store_document_source
            match self.ingest_file(&file_path).await {
                Ok(report) => combined.merge(report),
                Err(e) => {
                    let msg = format!("failed to ingest {}: {e}", file_path.display());
                    warn!("{msg}");
                    combined.warnings.push(msg);
                }
            }
        }

        Ok(combined)
    }

    /// Read file content as text based on format.
    fn read_source_content(&self, path: &Path, format: &SourceFormat) -> Result<String> {
        match format {
            SourceFormat::Markdown | SourceFormat::Text | SourceFormat::Html => {
                Ok(std::fs::read_to_string(path)?)
            }
            SourceFormat::Code => {
                let code = std::fs::read_to_string(path)?;
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                Ok(format!("```{ext}\n{code}\n```"))
            }
            SourceFormat::Jsonl => {
                // Read once, try each parser with the in-memory content
                let content = std::fs::read_to_string(path)?;
                let session_id = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if let Ok(conv) = crate::parsers::claude_code::parse_claude_code_content(
                    &content,
                    session_id.clone(),
                ) && !conv.messages.is_empty()
                {
                    return Ok(conv.to_json());
                }
                if let Ok(conv) =
                    crate::parsers::codex::parse_codex_content(&content, session_id.clone())
                    && !conv.messages.is_empty()
                {
                    return Ok(conv.to_json());
                }
                if let Ok(conv) =
                    crate::parsers::gemini_cli::parse_gemini_cli_content(&content, session_id)
                    && !conv.messages.is_empty()
                {
                    return Ok(conv.to_json());
                }
                Ok(content)
            }
            SourceFormat::Json => {
                // Try as Gemini Takeout single conversation file (array of role/parts objects)
                let raw = std::fs::read_to_string(path)?;
                if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&raw)
                    && arr.iter().any(|v| v.get("parts").is_some())
                {
                    // Looks like Gemini format: convert to markdown
                    let mut md = String::new();
                    for entry in &arr {
                        let role = entry
                            .get("role")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let text = entry
                            .get("parts")
                            .and_then(|v| v.as_array())
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default();
                        if !text.is_empty() {
                            md.push_str(&format!("**{role}**: {text}\n\n"));
                        }
                    }
                    if !md.is_empty() {
                        return Ok(md);
                    }
                }
                // Not a conversation file, pass raw JSON to LLM
                Ok(raw)
            }
            SourceFormat::Toml => Ok(std::fs::read_to_string(path)?),
            SourceFormat::Zip | SourceFormat::Tgz => {
                // Archives are handled by ingest_archive(), not read_source_content()
                Err(MemexError::ValidationFailure {
                    details: "Archives should be ingested via ingest_archive()".to_string(),
                })
            }
            SourceFormat::Pdf => Err(MemexError::MultimodalUnsupported {
                format: "pdf".to_string(),
            }),
            SourceFormat::Image => Err(MemexError::MultimodalUnsupported {
                format: "image".to_string(),
            }),
            SourceFormat::Unknown => Ok(std::fs::read_to_string(path)?),
        }
    }

    /// Call LLM to synthesize wiki pages from source content.
    async fn synthesize_wiki(
        &self,
        content: &str,
        source_ref: &str,
        guidance: Option<&str>,
    ) -> Result<Vec<ProposedPage>> {
        let index_content = self.read_index().unwrap_or_else(|_| String::new());
        let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        let mut prompt = INGEST_PROMPT_TEMPLATE
            .replace("{index}", &index_content)
            .replace("{content}", content)
            .replace("{timestamp}", &timestamp)
            .replace("{source_ref}", source_ref);

        if let Some(guide) = guidance {
            prompt.push_str(&format!(
                "\n\nUser guidance: {guide}\nFocus on what the user asked for."
            ));
        }

        let llm_output = self
            .provider
            .chat(Some(SYSTEM_PROMPT), &prompt, self.model(), 0.3)
            .await
            .map_err(|e| {
                error!(error = %e, "LLM provider error");
                MemexError::LlmCallFailed {
                    details: e.to_string(),
                }
            })?;

        llm_output::parse_llm_wiki_output(&llm_output)
    }

    /// Analyze a source and return key takeaways (building block for interactive ingest).
    /// One LLM call. Does not write anything.
    pub async fn analyze_source(&self, content: &str) -> Result<crate::types::SourceAnalysis> {
        let prompt = format!(
            "Analyze this source material. Return:\n\
            TAKEAWAYS:\n- key point 1\n- key point 2\n...\n\n\
            EMPHASIS:\n- what's most important\n...\n\n\
            Source:\n{content}"
        );

        let response = self
            .provider
            .chat(Some(SYSTEM_PROMPT), &prompt, self.model(), 0.3)
            .await
            .map_err(|e| {
                error!(error = %e, "analyze_source LLM call failed");
                MemexError::LlmCallFailed {
                    details: e.to_string(),
                }
            })?;

        let mut takeaways = Vec::new();
        let mut emphasis = Vec::new();
        let mut section = "";

        for line in response.lines() {
            let line = line.trim();
            if line.starts_with("TAKEAWAYS:") {
                section = "takeaways";
                continue;
            }
            if line.starts_with("EMPHASIS:") {
                section = "emphasis";
                continue;
            }
            if let Some(item) = line.strip_prefix("- ") {
                match section {
                    "takeaways" => takeaways.push(item.to_string()),
                    "emphasis" => emphasis.push(item.to_string()),
                    _ => {}
                }
            }
        }

        Ok(crate::types::SourceAnalysis {
            takeaways,
            suggested_emphasis: emphasis,
            image_count: 0,
        })
    }

    /// Synthesize wiki pages with optional user guidance (building block for interactive ingest).
    /// Returns proposed pages without writing them.
    pub async fn synthesize_with_guidance(
        &self,
        content: &str,
        source_ref: &str,
        guidance: Option<&str>,
    ) -> Result<Vec<ProposedPage>> {
        self.synthesize_wiki(content, source_ref, guidance).await
    }

    /// Image enrichment pass: after text synthesis, send images to LLM to enrich wiki pages.
    async fn enrich_with_images(
        &self,
        ingest_report: &IngestReport,
        conv_paths: &[(std::path::PathBuf, usize)],
        platform: &str,
    ) -> Result<IngestReport> {
        let created_pages = &ingest_report.pages_created;
        let updated_pages = &ingest_report.pages_updated;

        // Collect all image paths from batch's conversations
        let mut all_images: Vec<PathBuf> = Vec::new();
        for (conv_path, _) in conv_paths {
            let meta_path = self.root.join(format!("{}.meta.json", conv_path.display()));
            let images = read_images_from_meta(&meta_path);
            for img in images {
                let full = self.root.join(format!("sources/{platform}/{img}"));
                if full.exists() {
                    all_images.push(full);
                }
            }
        }

        if all_images.is_empty() {
            return Ok(IngestReport::default());
        }

        // Read wiki pages just created/updated
        let mut pages_text = String::new();
        for page_path in created_pages.iter().chain(updated_pages.iter()) {
            let abs = self.root.join(page_path);
            if let Ok(content) = std::fs::read_to_string(&abs) {
                pages_text.push_str(&format!("=== {} ===\n{}\n\n", page_path.display(), content));
            }
        }

        if pages_text.is_empty() {
            return Ok(IngestReport::default());
        }

        let mut combined = IngestReport::default();
        let total_image_batches = all_images.len().div_ceil(IMAGES_PER_BATCH);

        // Sub-batch images
        for (i, chunk) in all_images.chunks(IMAGES_PER_BATCH).enumerate() {
            let image_markers: String = chunk
                .iter()
                .map(|p| format!("[IMAGE:{}]", p.display()))
                .collect::<Vec<_>>()
                .join("\n");

            let prompt = IMAGE_ENRICHMENT_PROMPT
                .replace("{pages}", &pages_text)
                .replace("{image_markers}", &image_markers);

            info!(
                image_batch = i + 1,
                total_image_batches,
                images = chunk.len(),
                "enriching wiki pages with images"
            );

            match self
                .provider
                .chat(Some(SYSTEM_PROMPT), &prompt, self.model(), 0.3)
                .await
            {
                Ok(output) => match llm_output::parse_llm_wiki_output(&output) {
                    Ok(proposed) => {
                        if !proposed.is_empty() {
                            match self.write_proposed_pages(&proposed).await {
                                Ok(report) => combined.merge(report),
                                Err(e) => warn!(error = %e, "image enrichment write failed"),
                            }
                        }
                    }
                    Err(e) => warn!(error = %e, "image enrichment parse failed"),
                },
                Err(e) => warn!(error = %e, "image enrichment LLM call failed"),
            }
        }

        Ok(combined)
    }

    /// Validate proposed pages, acquire lock, write pages, rebuild index, log, release lock.
    /// Public for agent use.
    pub async fn write_proposed_pages(&self, proposed: &[ProposedPage]) -> Result<IngestReport> {
        // Validate all pages and parse frontmatter once per page.
        // Carry (page, frontmatter, body) through the rest of the function
        // to avoid re-parsing YAML for index entries and search indexing.
        let mut valid_pages: Vec<(&ProposedPage, crate::types::PageFrontmatter, String)> =
            Vec::new();
        for page in proposed {
            match validate::parse_frontmatter(&page.content) {
                Ok((fm, body)) => {
                    if fm.title.trim().is_empty() {
                        warn!(page = %page.path.display(), "skipping page with empty title");
                        continue;
                    }
                    valid_pages.push((page, fm, body));
                }
                Err(e) => {
                    warn!(page = %page.path.display(), error = %e, "skipping invalid page");
                }
            }
        }

        if valid_pages.is_empty() {
            return Ok(IngestReport::default());
        }

        // Check for dangling links (warn only)
        for (page, _, _) in &valid_pages {
            let dangling = validate::find_dangling_links(&page.content, &self.wiki_dir());
            for link in dangling {
                warn!(page = %page.path.display(), link = %link, "dangling wiki link");
            }
        }

        // Acquire lock (async-safe: runs in blocking thread pool)
        let lock_file = storage::try_acquire_lock_async(&self.lock_path(), 30)
            .await
            .map_err(|_| MemexError::StaleLock {
                lock_path: self.lock_path(),
            })?;

        let mut report = IngestReport::default();
        let mut index_entries: Vec<(String, String, String)> = Vec::new();

        let wiki_dir = self.wiki_dir();

        // Write pages
        for (page, fm, body) in &valid_pages {
            // Ensure page path starts with "wiki/" (LLM may omit the prefix)
            let page_key = page.path.to_string_lossy();
            let canonical_key = if page_key.starts_with("wiki/") {
                page_key.to_string()
            } else {
                format!("wiki/{page_key}")
            };

            // Path traversal guard: normalize lexically and verify path stays inside wiki_dir
            let normalized = storage::normalize_path(&self.root, &canonical_key);
            if !normalized.starts_with(&wiki_dir) {
                warn!(
                    page = %page.path.display(),
                    "write_proposed_pages: path traversal rejected"
                );
                continue;
            }

            let abs_path = self.root.join(&canonical_key);

            // Ensure parent directory exists
            if let Some(parent) = abs_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            storage::atomic_write(&abs_path, page.content.as_bytes())?;

            // Index entry: use pre-parsed frontmatter (no re-parse)
            let summary = fm
                .summary
                .clone()
                .unwrap_or_else(|| index::extract_summary(body, 120));
            index_entries.push((canonical_key.clone(), fm.title.clone(), summary));

            // Search index: use pre-parsed frontmatter + body (no re-parse)
            let canonical_path = PathBuf::from(&canonical_key);
            let tags = fm.tags.join(", ");
            let _ = self
                .search
                .index_page(&canonical_path, &fm.title, body, &tags);

            match page.action {
                PageAction::Create => {
                    report.pages_created.push(canonical_path);
                }
                PageAction::Update => {
                    report.pages_updated.push(canonical_path);
                }
            }
        }

        // Batch update index.md (one read + one write instead of N each)
        if !index_entries.is_empty() {
            let index_path = self.root.join("index.md");
            let batch: Vec<(&str, &str, &str)> = index_entries
                .iter()
                .map(|(path, title, summary)| (path.as_str(), title.as_str(), summary.as_str()))
                .collect();
            index::update_index_entries_batch(&index_path, &batch)?;
        }

        // Append log
        let pages_created = report.pages_created.len();
        let pages_updated = report.pages_updated.len();
        log::append_log(
            &self.root,
            "ingest",
            "batch",
            &format!("{pages_created} created, {pages_updated} updated"),
        )?;

        // Release lock
        storage::release_lock(lock_file);

        Ok(report)
    }
}

/// Read image paths from a conversation's .meta.json sidecar.
fn read_images_from_meta(meta_path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(meta_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|val| val.get("images")?.as_array().cloned())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Detect if a directory is a known platform root and return session + memory file paths.
///
/// Supported platforms:
/// - Claude Code (`~/.claude`): sessions at `projects/*/*.jsonl`, memories at `projects/*/memory/*.md`
/// - Codex (`~/.codex`): sessions at `sessions/**/*.jsonl`
/// - Gemini CLI (`~/.gemini`): sessions at `tmp/*/chats/*.json`
fn detect_platform_files(dir: &Path) -> Option<(String, Vec<PathBuf>)> {
    let mut files = Vec::new();

    // Claude Code: ~/.claude/projects/{project}/*.jsonl + memory/*.md
    let projects_dir = dir.join("projects");
    if projects_dir.is_dir() {
        if let Ok(projects) = std::fs::read_dir(&projects_dir) {
            for project in projects.filter_map(|e| e.ok()) {
                if !project.path().is_dir() {
                    continue;
                }
                // Session files
                if let Ok(entries) = std::fs::read_dir(project.path()) {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let path = entry.path();
                        if path.extension().is_some_and(|e| e == "jsonl") && path.is_file() {
                            files.push(path);
                        }
                    }
                }
                // Memory files (project-level knowledge)
                let memory_dir = project.path().join("memory");
                if memory_dir.is_dir()
                    && let Ok(entries) = std::fs::read_dir(&memory_dir)
                {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let path = entry.path();
                        if path.extension().is_some_and(|e| e == "md")
                            && path.is_file()
                            && path.file_name().unwrap_or_default() != "MEMORY.md"
                        {
                            files.push(path);
                        }
                    }
                }
            }
        }
        if !files.is_empty() {
            return Some(("claude-code".to_string(), files));
        }
    }

    // Codex: ~/.codex/sessions/**/*.jsonl
    let sessions_dir = dir.join("sessions");
    if sessions_dir.is_dir() {
        for entry in walkdir::WalkDir::new(&sessions_dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path().to_path_buf();
            if path.extension().is_some_and(|e| e == "jsonl") {
                // Skip session_index.jsonl
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if !name.starts_with("session_index") {
                    files.push(path);
                }
            }
        }
        if !files.is_empty() {
            return Some(("codex".to_string(), files));
        }
    }

    // Gemini CLI: ~/.gemini/tmp/*/chats/*.json
    let tmp_dir = dir.join("tmp");
    if tmp_dir.is_dir() {
        if let Ok(projects) = std::fs::read_dir(&tmp_dir) {
            for project in projects.filter_map(|e| e.ok()) {
                let chats_dir = project.path().join("chats");
                if !chats_dir.is_dir() {
                    continue;
                }
                if let Ok(entries) = std::fs::read_dir(&chats_dir) {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let path = entry.path();
                        if path.extension().is_some_and(|e| e == "json") && path.is_file() {
                            files.push(path);
                        }
                    }
                }
            }
        }
        if !files.is_empty() {
            return Some(("gemini-cli".to_string(), files));
        }
    }

    None
}

/// Compute batch content budget in bytes (1 token ≈ 4 bytes).
pub fn compute_batch_budget_bytes(
    model: &str,
    system_prompt_tokens: usize,
    index_tokens: usize,
) -> usize {
    let budget_tokens =
        crate::model_catalog::compute_batch_budget(model, system_prompt_tokens, index_tokens);
    budget_tokens * crate::model_catalog::BYTES_PER_TOKEN
}

/// Convert a URL to a safe filename.
fn url_to_filename(url: &str) -> String {
    let without_scheme = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let safe: String = without_scheme
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Trim trailing dashes and ensure it ends with .html
    let trimmed = safe.trim_end_matches('-');
    if trimmed.ends_with(".html") || trimmed.ends_with(".htm") {
        trimmed.to_string()
    } else {
        format!("{trimmed}.html")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_to_filename_basic() {
        let name = url_to_filename("https://example.com/page");
        assert!(name.contains("example"));
        assert!(!name.contains("://"));
    }

    #[test]
    fn batch_budget_bytes_from_model() {
        let budget = compute_batch_budget_bytes("gpt-4o", 500, 5000);
        assert!(budget > 0);
        let unknown = compute_batch_budget_bytes("unknown-xyz", 500, 5000);
        assert!(unknown > 0);
    }

    #[test]
    fn read_images_from_meta_parses_images() {
        let dir = tempfile::TempDir::new().unwrap();
        let meta = dir.path().join("conv.json.meta.json");
        std::fs::write(
            &meta,
            r#"{"id":"test","images":["images/a.png","images/b.jpg"]}"#,
        )
        .unwrap();
        let images = read_images_from_meta(&meta);
        assert_eq!(images, vec!["images/a.png", "images/b.jpg"]);
    }

    #[test]
    fn read_images_from_meta_missing_field() {
        let dir = tempfile::TempDir::new().unwrap();
        let meta = dir.path().join("conv.json.meta.json");
        std::fs::write(&meta, r#"{"id":"test"}"#).unwrap();
        let images = read_images_from_meta(&meta);
        assert!(images.is_empty());
    }

    #[test]
    fn read_images_from_meta_missing_file() {
        let images = read_images_from_meta(std::path::Path::new("/nonexistent/meta.json"));
        assert!(images.is_empty());
    }
}
