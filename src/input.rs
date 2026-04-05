use std::path::Path;
use std::sync::LazyLock;
use regex::Regex;

static HTML_TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
static WHITESPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

/// Load context from a local file (.md, .txt).
pub fn load_file_context(path: &str) -> anyhow::Result<String> {
    let path = Path::new(path);
    if !path.exists() {
        anyhow::bail!("File not found: {}", path.display());
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
    Ok(content)
}

/// Assemble the full task context from a task description, file contents, and optional URL content.
pub fn assemble_context(
    task: &str,
    file_contents: &[String],
    url_content: Option<&str>,
) -> String {
    if file_contents.is_empty() && url_content.is_none() {
        return task.to_string();
    }

    let mut parts = vec![format!("## Task\n{}", task)];

    for (i, content) in file_contents.iter().enumerate() {
        parts.push(format!("## Context File {}\n{}", i + 1, content));
    }

    if let Some(url) = url_content {
        parts.push(format!("## URL Context\n{}", url));
    }

    parts.join("\n\n")
}

/// Fetch content from a URL and extract text.
pub async fn fetch_url_context(url: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let response = client.get(url).send().await?.error_for_status()?;
    let text = response.text().await?;
    let stripped = strip_html_tags(&text);
    Ok(stripped)
}

/// Naive HTML tag stripping (public for testing).
pub fn strip_html_tags(html: &str) -> String {
    let text = HTML_TAG_RE.replace_all(html, "");
    WHITESPACE_RE.replace_all(&text, " ").trim().to_string()
}
