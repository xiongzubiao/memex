//! Chunking via pre-scanned break points + backward-only window with
//! squared-distance decay. Replaces the per-byte scoring walk in `embed.rs`.
//!
//! Score scale: H1=100, H2=90, H3=80, code fence edge=80, H4=70, H5=60,
//! H6=50, paragraph=20, list item=5, plain newline=1. `find_best_cutoff`
//! decays by squared distance, so a strong break a bit further back can
//! beat a weak break next to the target.

#[derive(Debug, Clone, Copy)]
pub struct BreakPoint {
    pub pos: usize,
    pub score: u32,
}

pub fn scan_break_points(body: &str) -> Vec<BreakPoint> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut inside_fence = false;
    let mut i = 0;
    while i < bytes.len() {
        let at_line_start = i == 0 || bytes[i - 1] == b'\n';
        if at_line_start {
            let line_end = bytes[i..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|p| i + p)
                .unwrap_or(bytes.len());
            let line = std::str::from_utf8(&bytes[i..line_end]).unwrap_or("");
            let trim = line.trim_start();
            if trim.starts_with("```") || trim.starts_with("~~~") {
                out.push(BreakPoint { pos: i, score: 80 });
                inside_fence = !inside_fence;
            } else if !inside_fence {
                let score = if trim.starts_with("# ") {
                    Some(100)
                } else if trim.starts_with("## ") {
                    Some(90)
                } else if trim.starts_with("### ") {
                    Some(80)
                } else if trim.starts_with("#### ") {
                    Some(70)
                } else if trim.starts_with("##### ") {
                    Some(60)
                } else if trim.starts_with("###### ") {
                    Some(50)
                } else if trim.starts_with("- ") || trim.starts_with("* ") {
                    Some(5)
                } else {
                    None
                };
                if let Some(s) = score {
                    out.push(BreakPoint { pos: i, score: s });
                }
            }
        }
        if !inside_fence && i + 1 < bytes.len() && bytes[i] == b'\n' && bytes[i + 1] == b'\n' {
            out.push(BreakPoint {
                pos: i + 2,
                score: 20,
            });
        }
        if !inside_fence && bytes[i] == b'\n' && i + 1 < bytes.len() {
            out.push(BreakPoint {
                pos: i + 1,
                score: 1,
            });
        }
        i += 1;
    }
    out.sort_by_key(|b| b.pos);
    out
}

pub fn find_best_cutoff(points: &[BreakPoint], target: usize, window: usize) -> Option<BreakPoint> {
    let lower = target.saturating_sub(window);
    let upper = target;
    let mut best: Option<(f64, BreakPoint)> = None;
    let decay = 0.7_f64;
    let w = window as f64;
    for p in points {
        if p.pos < lower || p.pos > upper {
            continue;
        }
        let dist = (target - p.pos) as f64;
        let multiplier = 1.0 - (dist / w).powi(2) * decay;
        let final_score = p.score as f64 * multiplier;
        if best.as_ref().map(|(s, _)| final_score > *s).unwrap_or(true) {
            best = Some((final_score, *p));
        }
    }
    best.map(|(_, p)| p)
}

#[derive(Debug, Clone, Copy)]
pub struct Chunk {
    pub pos: usize,
    pub len: usize,
}

pub fn chunk_text(text: &str, max_tokens: usize, overlap_frac: f64) -> Vec<Chunk> {
    let max_chars = max_tokens.max(1) * 4;
    if text.len() <= max_chars {
        return vec![Chunk {
            pos: 0,
            len: text.len(),
        }];
    }
    let overlap_chars = (max_chars as f64 * overlap_frac) as usize;
    let pts = scan_break_points(text);
    // Backward-only window of 800 bytes from target — matches QMD's
    // CHUNK_WINDOW_CHARS constant (~200 tokens).
    // Phase 7's original fix shipped `window = max_chars` which caused
    // find_best_cutoff to pick break points at or near `start`, emitting
    // thousands of 1-byte chunks (6,158 chunks for a 144KB body). Cap
    // at min(800, max_chars-1) so very small max_chars still leaves
    // a non-empty window range.
    let window: usize = 800.min(max_chars.saturating_sub(1));
    // Hard floor on chunk length: never emit a chunk shorter than
    // (max_chars - window) AND never shorter than overlap+1 (so each
    // iteration makes net forward progress).
    let min_chunk_len = max_chars
        .saturating_sub(window)
        .max(overlap_chars + 1)
        .max(1);

    let mut chunks = Vec::new();
    let mut start = 0_usize;
    while start < text.len() {
        let target = (start + max_chars).min(text.len());
        if target == text.len() {
            chunks.push(Chunk {
                pos: start,
                len: text.len() - start,
            });
            break;
        }
        let cutoff = find_best_cutoff(&pts, target, window)
            .map(|p| p.pos)
            .filter(|&p| p >= start + min_chunk_len)
            .unwrap_or(target);
        let cutoff = floor_char_boundary(text, cutoff).max(start + min_chunk_len);
        chunks.push(Chunk {
            pos: start,
            len: cutoff - start,
        });
        let next_start = cutoff.saturating_sub(overlap_chars).max(start + 1);
        let next_start = floor_char_boundary(text, next_start);
        start = next_start;
    }
    chunks
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- scan_break_points

    #[test]
    fn scan_break_points_finds_h1_h2_paragraph_and_list() {
        let body = "intro\n\n# Title\n\n## Section\n\n- item\n\nlast line\n";
        let pts = scan_break_points(body);
        // H1=100, H2=90, ListItem=5, Paragraph=20.
        assert!(pts.iter().any(|b| b.score == 100), "expected H1: {pts:?}");
        assert!(pts.iter().any(|b| b.score == 90));
        assert!(pts.iter().any(|b| b.score == 5));
        assert!(pts.iter().any(|b| b.score == 20));
        let positions: Vec<usize> = pts.iter().map(|b| b.pos).collect();
        let mut sorted = positions.clone();
        sorted.sort();
        assert_eq!(positions, sorted, "break points must be sorted by position");
    }

    #[test]
    fn scan_break_points_skips_inside_code_fence() {
        let body = "before\n\n```rust\n# not a heading\n## also not\n```\n\nafter\n";
        let pts = scan_break_points(body);
        // Inside ``` neither H1 (100) nor H2 (90) should appear.
        assert!(
            !pts.iter().any(|b| b.score == 100 || b.score == 90),
            "headings inside ``` must not be break points: {pts:?}"
        );
    }

    // --- find_best_cutoff

    #[test]
    fn find_best_cutoff_picks_high_score_break_within_window() {
        let body = "a".repeat(1200) + "\n## Section\n" + &"b".repeat(1200);
        let pts = scan_break_points(&body);
        let cutoff = find_best_cutoff(&pts, 1300, 800);
        assert!(cutoff.is_some());
        let pos = cutoff.unwrap().pos;
        assert!(
            body[..pos].ends_with("a\n") || body[pos..].starts_with("## Section"),
            "expected cutoff at the heading, got pos {pos}"
        );
    }

    #[test]
    fn find_best_cutoff_decay_prefers_strong_break_far_back_over_weak_near_target() {
        let h2_score = 90.0_f64;
        let weak_score = 1.0_f64;
        let dist_h2 = 600.0_f64;
        let dist_weak = 10.0_f64;
        let window = 800.0_f64;
        let s_h2 = h2_score * (1.0 - (dist_h2 / window).powi(2) * 0.7);
        let s_weak = weak_score * (1.0 - (dist_weak / window).powi(2) * 0.7);
        assert!(
            s_h2 > s_weak,
            "decay must still let strong break win: h2={s_h2} weak={s_weak}"
        );
    }

    // --- chunk_text

    #[test]
    fn chunk_text_upper_bound_max_chars() {
        let text = "x ".repeat(5000);
        let chunks = chunk_text(&text, 900, 0.15);
        let max_chars = 900 * 4;
        for c in &chunks {
            assert!(c.len <= max_chars, "chunk len {} > {}", c.len, max_chars);
        }
    }

    #[test]
    fn chunk_text_overlap_when_split() {
        let text = "x ".repeat(5000);
        let chunks = chunk_text(&text, 900, 0.15);
        if chunks.len() >= 2 {
            assert!(
                chunks[1].pos < chunks[0].pos + chunks[0].len,
                "expected overlap"
            );
        }
    }

    #[test]
    fn chunk_text_byte_safe_with_multibyte() {
        let text = "前段中文\n# 标题\n中文正文\n".repeat(400);
        let chunks = chunk_text(&text, 100, 0.15);
        for c in &chunks {
            assert!(text.is_char_boundary(c.pos));
            assert!(text.is_char_boundary(c.pos + c.len));
        }
    }

    #[test]
    fn chunk_text_short_input_one_chunk() {
        let chunks = chunk_text("hello world", 900, 0.15);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].pos, 0);
        assert_eq!(chunks[0].len, "hello world".len());
    }
}
