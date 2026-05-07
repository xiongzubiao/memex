//! Faithful Rust port of LlamaIndex's `MarkdownNodeParser`.
//!
//! Source:
//! https://github.com/run-llama/llama_index/blob/main/llama-index-core/llama_index/core/node_parser/file/markdown.py
//!
//! Algorithm (verbatim):
//!   - Split text by `\n` into lines.
//!   - Toggle `code_block` when a line's `lstrip()` starts with ` ``` `.
//!   - Outside code blocks, match `^(#+)\s(.*)`; on match:
//!       1. Emit the prior `current_section` (if non-empty after `strip()`)
//!          with `header_path = separator.join(stack[:-1] texts)`.
//!       2. While `stack[-1].level >= new_level`: pop.
//!       3. Push `(new_level, new_text)` and reset `current_section` to
//!          the header line.
//!   - Otherwise append `line + "\n"` to `current_section`.
//!   - After the loop, emit the final non-empty section.
//!   - `header_path` formatting: `"/" + path + "/"` if path else `"/"`.

/// Default header-path separator. Mirrors LlamaIndex's
/// `header_path_separator: str = Field(default="/")`.
pub const DEFAULT_HEADER_PATH_SEPARATOR: &str = "/";

/// One markdown section (a "node" in LlamaIndex terminology). `pos` and
/// `len` are byte offsets into the original text — added so callers can
/// extract the section without copying. LlamaIndex returns `TextNode`
/// objects with copied text plus `header_path` metadata.
#[derive(Debug, Clone)]
pub struct MarkdownNode {
    pub pos: usize,
    pub len: usize,
    pub header_path: String,
}

/// Faithful port of `MarkdownNodeParser.get_nodes_from_node`.
pub fn get_nodes_from_text(text: &str, header_path_separator: &str) -> Vec<MarkdownNode> {
    let mut nodes: Vec<MarkdownNode> = Vec::new();
    let mut header_stack: Vec<(usize, String)> = Vec::new();
    let mut in_code_block = false;
    let mut current_start: usize = 0;
    let mut current_has_content = false;
    let bytes = text.as_bytes();

    let mut line_start: usize = 0;
    loop {
        let line_end = match bytes[line_start..].iter().position(|&b| b == b'\n') {
            Some(p) => line_start + p,
            None => bytes.len(),
        };
        let line = &text[line_start..line_end];
        let trimmed = line.trim_start();

        // `if line.lstrip().startswith("```"): code_block = not code_block`
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            if !line.trim().is_empty() {
                current_has_content = true;
            }
        } else if !in_code_block {
            // `header_match = re.match(r"^(#+)\s(.*)", line)`
            if let Some((level, hdr_text)) = parse_header_line(line) {
                if current_has_content {
                    nodes.push(MarkdownNode {
                        pos: current_start,
                        len: line_start - current_start,
                        header_path: build_header_path(&header_stack, header_path_separator),
                    });
                }
                while header_stack
                    .last()
                    .map(|(l, _)| *l >= level)
                    .unwrap_or(false)
                {
                    header_stack.pop();
                }
                header_stack.push((level, hdr_text));
                current_start = line_start;
                current_has_content = true;

                if line_end == bytes.len() {
                    break;
                }
                line_start = line_end + 1;
                continue;
            }
        }

        if !line.trim().is_empty() {
            current_has_content = true;
        }

        if line_end == bytes.len() {
            break;
        }
        line_start = line_end + 1;
    }

    if current_has_content {
        nodes.push(MarkdownNode {
            pos: current_start,
            len: text.len() - current_start,
            header_path: build_header_path(&header_stack, header_path_separator),
        });
    }

    nodes
}

/// Match Python's `re.match(r"^(#+)\s(.*)", line)`. Returns
/// `(level, text)` where `level` is the count of leading `#` and `text`
/// is everything after the single whitespace separator.
fn parse_header_line(line: &str) -> Option<(usize, String)> {
    let bytes = line.as_bytes();
    if bytes.is_empty() || bytes[0] != b'#' {
        return None;
    }
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b'#' {
        i += 1;
    }
    let level = i;
    if i >= bytes.len() {
        return None;
    }
    let c = bytes[i];
    // Python's \s: [ \t\n\r\f\v]. \n cannot occur in a single line.
    if !(c == b' ' || c == b'\t' || c == b'\r' || c == 0x0b || c == 0x0c) {
        return None;
    }
    i += 1;
    Some((level, line[i..].to_string()))
}

/// Mirror `separator + separator.join(h[1] for h in stack[:-1]) + separator`
/// with the `"/"` fallback when the join is empty.
fn build_header_path(stack: &[(usize, String)], separator: &str) -> String {
    if stack.len() <= 1 {
        return separator.to_string();
    }
    let parts: Vec<&str> = stack[..stack.len() - 1]
        .iter()
        .map(|(_, t)| t.as_str())
        .collect();
    let joined = parts.join(separator);
    if joined.is_empty() {
        separator.to_string()
    } else {
        format!("{separator}{joined}{separator}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_all_header_levels() {
        let body = "intro\n\n# H1 Title\n\nbody1\n\n## H2 Sub\n\nbody2\n\n### H3 Deeper\n\nbody3\n";
        let nodes = get_nodes_from_text(body, "/");
        assert_eq!(nodes.len(), 4, "intro + 3 headers expected: {nodes:?}");
        assert_eq!(nodes[0].header_path, "/");
        assert_eq!(nodes[1].header_path, "/");
        assert_eq!(nodes[2].header_path, "/H1 Title/");
        assert_eq!(nodes[3].header_path, "/H1 Title/H2 Sub/");
    }

    #[test]
    fn skips_headers_inside_code_blocks() {
        let body =
            "# Real H1\n\n```\n## fake H2 inside code\n# fake H1 inside code\n```\n\nafter\n";
        let nodes = get_nodes_from_text(body, "/");
        assert_eq!(nodes.len(), 1);
        let body_text = &body[nodes[0].pos..nodes[0].pos + nodes[0].len];
        assert!(body_text.contains("## fake H2 inside code"));
        assert!(body_text.contains("after"));
    }

    #[test]
    fn pops_equal_or_higher_level_headers() {
        let body = "# A\n\nbody-a\n\n## B\n\nbody-b\n\n# C\n\nbody-c\n";
        let nodes = get_nodes_from_text(body, "/");
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0].header_path, "/");
        assert_eq!(nodes[1].header_path, "/A/");
        assert_eq!(nodes[2].header_path, "/");
    }

    #[test]
    fn handles_jump_from_h1_to_h3() {
        let body = "# A\n\n### C\n\nbody\n";
        let nodes = get_nodes_from_text(body, "/");
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[1].header_path, "/A/");
    }

    #[test]
    fn byte_offsets_cover_body_with_no_gaps() {
        let body = "# A\nbody-a\n## B\nbody-b\n";
        let nodes = get_nodes_from_text(body, "/");
        let total: usize = nodes.iter().map(|n| n.len).sum();
        assert_eq!(total, body.len());
        for n in &nodes {
            assert!(body.is_char_boundary(n.pos));
            assert!(body.is_char_boundary(n.pos + n.len));
        }
    }

    #[test]
    fn parse_header_line_matches_python_regex() {
        assert_eq!(parse_header_line("# Title").unwrap(), (1, "Title".into()));
        assert_eq!(parse_header_line("### Triple").unwrap(), (3, "Triple".into()));
        assert_eq!(parse_header_line("###### Six").unwrap(), (6, "Six".into()));
        assert!(parse_header_line("#NoSpace").is_none());
        assert!(parse_header_line("#").is_none());
        assert_eq!(parse_header_line("#\tWithTab").unwrap(), (1, "WithTab".into()));
        assert!(parse_header_line("text").is_none());
    }

    #[test]
    fn custom_separator() {
        let body = "# A\n\n## B\n\nbody\n";
        let nodes = get_nodes_from_text(body, " > ");
        assert_eq!(nodes[1].header_path, " > A > ");
    }

    #[test]
    fn no_headers_returns_single_node() {
        let body = "Just body text\nwith no headers.\n";
        let nodes = get_nodes_from_text(body, "/");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].pos, 0);
        assert_eq!(nodes[0].len, body.len());
        assert_eq!(nodes[0].header_path, "/");
    }

    #[test]
    fn empty_body_returns_no_nodes() {
        let nodes = get_nodes_from_text("", "/");
        assert_eq!(nodes.len(), 0);
    }
}
