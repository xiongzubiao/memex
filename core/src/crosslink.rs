//! Auto cross-linking: insert `[[stem]]` wiki links into page bodies.

/// Scan `body` for mentions of existing page titles and stems, and replace the
/// first occurrence of each with a `[[stem]]` wiki link.
///
/// # Parameters
/// - `body` — page body text (after frontmatter)
/// - `existing_pages` — `(stem, title)` pairs for all existing wiki pages
/// - `self_stem` — stem of the page being written (to avoid self-linking)
///
/// # Returns
/// `(modified_body, linked_stems)` — the body with links inserted, and which
/// stems were linked.
///
/// # Rules
/// - Only the first occurrence per existing page is replaced.
/// - Text already inside `[[...]]` wiki links is skipped.
/// - Matching is case-insensitive against both the title and the stem
///   (hyphens treated as spaces).
/// - A page is never linked to itself.
/// - Title is matched first (longer, more specific), then stem-as-words.
pub fn forward_link(
    body: &str,
    existing_pages: &[(String, String)],
    self_stem: &str,
) -> (String, Vec<String>) {
    let mut result = body.to_string();
    let mut linked_stems: Vec<String> = Vec::new();

    // Sort by title length descending so longer (more specific) titles match
    // before their substrings (e.g. "Rust Borrow Checker" before "Rust").
    let mut sorted_pages: Vec<&(String, String)> = existing_pages.iter().collect();
    sorted_pages.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    for (stem, title) in sorted_pages {
        // Never self-link.
        if stem == self_stem {
            continue;
        }

        let stem_words = stem.replace('-', " ");

        // Try title first (longer / more specific), then stem-as-words.
        let candidates: &[&str] = if title.to_lowercase() == stem_words.to_lowercase() {
            // Both patterns are equivalent — search only once.
            &[title.as_str()]
        } else {
            &[title.as_str(), stem_words.as_str()]
        };

        let mut linked = false;
        for &pattern in candidates {
            if linked {
                break;
            }
            if let Some((start, end)) = find_first_unlinkified(&result, pattern) {
                // Build the replacement: `[[stem]]`.
                let replacement = format!("[[{stem}]]");
                result.replace_range(start..end, &replacement);
                linked_stems.push(stem.clone());
                linked = true;
            }
        }
    }

    (result, linked_stems)
}

/// Check whether an existing page's body should get a backward link to a newly
/// created page.  Convenience wrapper around [`forward_link`].
///
/// # Returns
/// `(updated_body, was_linked)` — the body with the link added (if applicable)
/// and whether a link was actually inserted.
pub fn backward_link_page(
    existing_body: &str,
    new_stem: &str,
    new_title: &str,
    existing_stem: &str,
) -> (String, bool) {
    let new_page = vec![(new_stem.to_string(), new_title.to_string())];
    let (updated, linked) = forward_link(existing_body, &new_page, existing_stem);
    let was_linked = !linked.is_empty();
    (updated, was_linked)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Find the byte range `[start, end)` of the first case-insensitive occurrence
/// of `pattern` in `text` that is NOT already inside a `[[...]]` wiki link.
///
/// Uses `regex` with case-insensitive matching so that byte offsets always refer
/// to positions in the original `text`.  This avoids the Unicode byte-length
/// mismatch that would occur if we lowercased `text` and then applied the
/// offsets to the original (e.g. Turkish `İ` (2 bytes) lowercases to `i`
/// (1 byte), shifting every subsequent offset).
///
/// Returns `None` if no such occurrence exists.
fn find_first_unlinkified(text: &str, pattern: &str) -> Option<(usize, usize)> {
    if pattern.is_empty() {
        return None;
    }

    let escaped = regex::escape(pattern);
    let re = match regex::RegexBuilder::new(&escaped)
        .case_insensitive(true)
        .build()
    {
        Ok(r) => r,
        Err(_) => return None,
    };

    for m in re.find_iter(text) {
        if !is_inside_wiki_link(text, m.start()) {
            return Some((m.start(), m.end()));
        }
    }

    None
}

/// Return `true` if the byte position `pos` in `text` falls inside a
/// `[[...]]` wiki-link span.
///
/// Detection heuristic: the most recent `[[` before `pos` must appear *after*
/// the most recent `]]` before `pos`.  In other words there is an unclosed
/// `[[` open at position `pos`.
fn is_inside_wiki_link(text: &str, pos: usize) -> bool {
    let prefix = &text[..pos];

    let last_open = prefix.rfind("[[");
    let last_close = prefix.rfind("]]");

    match (last_open, last_close) {
        (Some(open), Some(close)) => open > close,
        (Some(_open), None) => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pages(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(s, t)| (s.to_string(), t.to_string()))
            .collect()
    }

    // 1. Forward link inserts [[stem]] when body mentions a page title.
    #[test]
    fn forward_link_replaces_mention() {
        let body = "We follow REST patterns for API design.";
        let existing = pages(&[("rest-patterns", "REST Patterns")]);
        let (out, linked) = forward_link(body, &existing, "api-design");
        assert!(out.contains("[[rest-patterns]]"), "got: {out}");
        assert_eq!(linked, vec!["rest-patterns"]);
    }

    // 2. Matching is case-insensitive.
    #[test]
    fn forward_link_case_insensitive() {
        let body = "We follow rest patterns for API design.";
        let existing = pages(&[("rest-patterns", "REST Patterns")]);
        let (out, linked) = forward_link(body, &existing, "api-design");
        assert!(out.contains("[[rest-patterns]]"), "got: {out}");
        assert!(!linked.is_empty());
    }

    // 3. Skip text already inside [[...]].
    #[test]
    fn forward_link_skips_existing_links() {
        let body = "We follow [[rest-patterns]] for API design.";
        let existing = pages(&[("rest-patterns", "REST Patterns")]);
        let (out, linked) = forward_link(body, &existing, "api-design");
        // Body should be unchanged.
        assert_eq!(out, body);
        assert!(linked.is_empty());
    }

    // 4. A page is never linked to itself.
    #[test]
    fn forward_link_skips_self() {
        let body = "REST patterns are described here.";
        let existing = pages(&[("rest-patterns", "REST Patterns")]);
        let (out, linked) = forward_link(body, &existing, "rest-patterns");
        assert_eq!(out, body);
        assert!(linked.is_empty());
    }

    // 5. Only the first occurrence per page is replaced.
    #[test]
    fn forward_link_only_first_occurrence() {
        let body = "REST patterns are useful. We love REST patterns.";
        let existing = pages(&[("rest-patterns", "REST Patterns")]);
        let (out, linked) = forward_link(body, &existing, "api");
        // Exactly one [[rest-patterns]] should appear.
        let count = out.matches("[[rest-patterns]]").count();
        assert_eq!(count, 1, "expected exactly one link, got: {out}");
        assert_eq!(linked.len(), 1);
    }

    // 6. Multiple different pages are all linked.
    #[test]
    fn forward_link_multiple_pages() {
        let body = "Use REST patterns and OAuth migration together.";
        let existing = pages(&[
            ("rest-patterns", "REST Patterns"),
            ("oauth-migration", "OAuth Migration"),
        ]);
        let (out, linked) = forward_link(body, &existing, "guide");
        assert!(out.contains("[[rest-patterns]]"), "got: {out}");
        assert!(out.contains("[[oauth-migration]]"), "got: {out}");
        assert_eq!(linked.len(), 2);
    }

    // 7. Title match takes precedence over stem-as-words.
    #[test]
    fn forward_link_matches_title_over_stem() {
        // Title is "OAuth Migration" (with capital letters / specific spacing).
        // Stem-as-words would be "oauth migration".  The body uses the title
        // casing — either way, we only want one link.
        let body = "Follow the OAuth Migration guide.";
        let existing = pages(&[("oauth-migration", "OAuth Migration")]);
        let (out, linked) = forward_link(body, &existing, "guide");
        let count = out.matches("[[oauth-migration]]").count();
        assert_eq!(count, 1, "expected exactly one link, got: {out}");
        assert_eq!(linked, vec!["oauth-migration"]);
    }

    // 8. backward_link_page adds a link when the existing body mentions the new page.
    #[test]
    fn backward_link_adds_to_existing_page() {
        let existing_body = "We discuss new page features here.";
        let (out, was_linked) =
            backward_link_page(existing_body, "new-page", "New Page", "existing-page");
        assert!(was_linked, "expected a link to be added");
        assert!(out.contains("[[new-page]]"), "got: {out}");
    }

    // 9. backward_link_page leaves the body unchanged if link already present.
    #[test]
    fn backward_link_skips_if_already_linked() {
        let existing_body = "See [[new-page]] for details.";
        let (out, was_linked) =
            backward_link_page(existing_body, "new-page", "New Page", "existing-page");
        assert!(!was_linked, "should not re-link");
        assert_eq!(out, existing_body);
    }

    // 10. Longer titles match before their substrings.
    #[test]
    fn forward_link_prefers_longer_match() {
        let body = "The Rust Borrow Checker prevents data races.";
        let existing = pages(&[
            ("rust", "Rust"),
            ("rust-borrow-checker", "Rust Borrow Checker"),
        ]);
        let (out, linked) = forward_link(body, &existing, "guide");
        assert!(
            out.contains("[[rust-borrow-checker]]"),
            "longer title should match first, got: {out}"
        );
        assert!(
            !out.contains("[[rust]]"),
            "shorter title should not consume the match, got: {out}"
        );
        assert!(linked.contains(&"rust-borrow-checker".to_string()));
    }

    // 11. Unicode safety: case-insensitive matching must not corrupt byte offsets.
    //     Characters whose lowercase form has a different byte length (e.g.
    //     Turkish U+00DC -> U+00FC, or U+00C9 -> U+00E9) must still produce
    //     valid offsets into the original text.
    #[test]
    fn forward_link_unicode_case_insensitive() {
        // Page titled "Uber Design" (with U+00DC: capital U with diaeresis).
        let existing = pages(&[("uber-design", "\u{00dc}ber Design")]);
        // Body uses the lowercase form (U+00FC: small u with diaeresis).
        let body = "Exploring \u{00fc}ber design patterns in depth.";
        let (out, linked) = forward_link(body, &existing, "other-page");
        assert!(
            out.contains("[[uber-design]]"),
            "Unicode case-insensitive match should produce a link, got: {out}"
        );
        assert_eq!(linked, vec!["uber-design"]);
        // Verify no corruption: the rest of the sentence must survive intact.
        assert!(
            out.contains("patterns in depth"),
            "text after replacement must be preserved, got: {out}"
        );
    }
}
