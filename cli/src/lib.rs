pub mod daemon;

#[cfg(feature = "test-harness")]
pub mod test_utils;

/// Initialize the ONNX Runtime. Wraps `memex_core::embed::init_runtime`
/// in `catch_unwind_silent` because some malformed dylibs panic from
/// inside `ort` rather than returning Err.
pub fn init_ort_runtime() -> Result<(), String> {
    match memex_core::embed::catch_unwind_silent(memex_core::embed::init_runtime) {
        Some(Ok(())) => Ok(()),
        Some(Err(e)) => Err(e.to_string()),
        None => Err("ONNX Runtime init panicked".to_string()),
    }
}

use std::path::PathBuf;

/// Slice a body into a 1-indexed line range and return `(sliced_text, (start, end))`.
///
/// - `from_line=None`, `max_lines=None`: returns the whole body unchanged.
/// - `from_line` is 1-indexed; if it is beyond EOF returns `("", (0, 0))`.
/// - `max_lines` is a count (mirrors the `@@ -N,M @@` second field).
/// - The returned `(start, end)` is `(first, last)` inclusive line numbers;
///   both 0 when the slice is empty.
pub fn slice_body(
    body: &str,
    from_line: Option<usize>,
    max_lines: Option<usize>,
) -> (String, (usize, usize)) {
    // Whole-body fast path – avoids any line counting/splitting.
    if from_line.is_none() && max_lines.is_none() {
        let total = body.lines().count();
        let (s, e) = if total == 0 { (0, 0) } else { (1, total) };
        return (body.to_string(), (s, e));
    }

    let lines: Vec<&str> = body.lines().collect();
    let total = lines.len();

    let from = from_line.unwrap_or(1).max(1);
    if from > total {
        return (String::new(), (0, 0));
    }

    let count = max_lines.unwrap_or(total - from + 1);
    let end = (from + count - 1).min(total);

    let slice = &lines[from - 1..end];
    let mut out = slice.join("\n");
    // Preserve trailing newline if the original body has one and we didn't
    // just emit an empty string.
    if body.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    (out, (from, end))
}

pub fn memex_root() -> PathBuf {
    if let Ok(root) = std::env::var("MEMEX_ROOT") {
        return PathBuf::from(root);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".memex")
}

/// Normalize a title to a kebab-case slug for wiki page filenames.
pub fn slugify(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    slug.split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
