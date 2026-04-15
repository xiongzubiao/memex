use crate::error::{MemexError, Result};
use crate::storage::content_hash;
use crate::types::{SourceFormat, SourceMeta};
use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};

/// Default maximum file size for ingest: 100 MiB.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

pub fn detect_format(path: &Path) -> SourceFormat {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "md" | "markdown" => SourceFormat::Markdown,
        "txt" => SourceFormat::Text,
        "rs" | "py" | "js" | "ts" | "go" | "java" | "c" | "cpp" | "h" | "rb" | "sh" | "yaml"
        | "yml" => SourceFormat::Code,
        "toml" => SourceFormat::Toml,
        "pdf" => SourceFormat::Pdf,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" => SourceFormat::Image,
        "html" | "htm" => SourceFormat::Html,
        "json" => SourceFormat::Json,
        "jsonl" => SourceFormat::Jsonl,
        "zip" => SourceFormat::Zip,
        "tgz" => SourceFormat::Tgz,
        "gz" => {
            // Check for .tar.gz
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            if stem.ends_with(".tar") {
                SourceFormat::Tgz
            } else {
                SourceFormat::Unknown
            }
        }
        _ => SourceFormat::Unknown,
    }
}

pub fn store_document_source(
    root: &Path,
    original_path: &Path,
    max_file_size: Option<u64>,
) -> Result<Option<PathBuf>> {
    store_source(root, original_path, "documents", max_file_size)
}

pub fn store_session_source(
    root: &Path,
    original_path: &Path,
    platform: &str,
    max_file_size: Option<u64>,
) -> Result<Option<PathBuf>> {
    store_source(root, original_path, platform, max_file_size)
}

fn store_source(
    root: &Path,
    original_path: &Path,
    dest_subdir: &str,
    max_file_size: Option<u64>,
) -> Result<Option<PathBuf>> {
    let limit = max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE);
    let file_size = fs::metadata(original_path)?.len();
    if file_size > limit {
        return Err(MemexError::Other(anyhow::anyhow!(
            "File too large to ingest: {} bytes (limit is {} bytes). \
             Use a smaller file or increase the limit.",
            file_size,
            limit,
        )));
    }
    let content = fs::read(original_path)?;
    let hash = content_hash(&content);
    let filename = original_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let stored_name = format!("{}-{}", &hash[..12], filename);
    let dest_dir = root.join(format!("sources/{dest_subdir}"));
    let dest = dest_dir.join(&stored_name);

    if dest.exists() {
        return Ok(None); // dedup
    }

    fs::create_dir_all(&dest_dir)?;
    fs::write(&dest, &content)?;

    let meta = SourceMeta {
        original_path: original_path.to_string_lossy().to_string(),
        format: detect_format(original_path),
        hash,
        ingested_at: Utc::now(),
    };
    let meta_dest = dest_dir.join(format!("{stored_name}.meta.json"));
    let meta_json = serde_json::to_string_pretty(&meta)
        .map_err(|e| crate::error::MemexError::Other(e.into()))?;
    fs::write(&meta_dest, meta_json)?;

    Ok(Some(dest))
}

pub fn is_already_ingested(root: &Path, file_path: &Path) -> Result<bool> {
    let content = fs::read(file_path)?;
    let hash = content_hash(&content);
    let prefix = &hash[..12];
    let docs_dir = root.join("sources/documents");
    if !docs_dir.exists() {
        return Ok(false);
    }
    for entry in fs::read_dir(&docs_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(prefix) && !name.ends_with(".meta.json") {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn detect_format_markdown() {
        assert_eq!(detect_format(Path::new("notes.md")), SourceFormat::Markdown);
    }

    #[test]
    fn detect_format_code() {
        assert_eq!(detect_format(Path::new("main.rs")), SourceFormat::Code);
        assert_eq!(detect_format(Path::new("app.py")), SourceFormat::Code);
    }

    #[test]
    fn detect_format_unknown() {
        assert_eq!(detect_format(Path::new("file.xyz")), SourceFormat::Unknown);
    }

    #[test]
    fn store_document_source_creates_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        fs::create_dir_all(root.join("sources/documents")).unwrap();
        let source = dir.path().join("notes.md");
        fs::write(&source, "# My Notes\nSome content.").unwrap();
        let result = store_document_source(&root, &source, None).unwrap();
        assert!(result.is_some());
        let stored = result.unwrap();
        assert!(stored.exists());
        assert!(stored.to_string_lossy().contains("notes.md"));
        // Meta file should exist
        let meta = PathBuf::from(format!("{}.meta.json", stored.display()));
        assert!(meta.exists(), "meta file at {}", meta.display());
    }

    #[test]
    fn store_document_source_dedup() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        fs::create_dir_all(root.join("sources/documents")).unwrap();
        let source = dir.path().join("notes.md");
        fs::write(&source, "same content").unwrap();
        let r1 = store_document_source(&root, &source, None).unwrap();
        assert!(r1.is_some());
        let r2 = store_document_source(&root, &source, None).unwrap();
        assert!(r2.is_none());
    }

    #[test]
    fn is_already_ingested_detects_existing() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        fs::create_dir_all(root.join("sources/documents")).unwrap();
        let source = dir.path().join("notes.md");
        fs::write(&source, "content").unwrap();
        assert!(!is_already_ingested(&root, &source).unwrap());
        store_document_source(&root, &source, None).unwrap();
        assert!(is_already_ingested(&root, &source).unwrap());
    }

    /// A sparse file (200 MiB) should be rejected when the limit is 100 MiB.
    #[test]
    fn store_document_source_rejects_oversized_file() {
        use std::fs::File;

        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        fs::create_dir_all(root.join("sources/documents")).unwrap();

        // Create a sparse file that reports 200 MiB in metadata but occupies
        // no actual disk blocks.
        let large_path = dir.path().join("big.pdf");
        let file = File::create(&large_path).unwrap();
        file.set_len(200 * 1024 * 1024).unwrap(); // 200 MiB sparse file

        let limit = 100 * 1024 * 1024u64; // 100 MiB
        let err = store_document_source(&root, &large_path, Some(limit))
            .expect_err("expected error for oversized file");
        let msg = format!("{err}");
        assert!(
            msg.contains("209715200"),
            "error should include file size: {msg}"
        );
        assert!(
            msg.contains("104857600"),
            "error should include limit: {msg}"
        );
    }

    /// Files within the size limit should still be stored successfully.
    #[test]
    fn store_document_source_accepts_normal_sized_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        fs::create_dir_all(root.join("sources/documents")).unwrap();
        let source = dir.path().join("small.txt");
        fs::write(&source, b"hello world").unwrap();

        // Use a tight explicit limit (1 KiB) — the 11-byte file should pass.
        let result = store_document_source(&root, &source, Some(1024)).unwrap();
        assert!(result.is_some(), "small file should be stored");
    }
}
