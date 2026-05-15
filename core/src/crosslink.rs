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
    sorted_pages.sort_by_key(|p| std::cmp::Reverse(p.1.len()));

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

/// Auto-link eligibility: a stem is eligible for forward/backward
/// auto-linking if it is multi-token (contains `-`).
///
/// Single-token stems are always skipped, even longer ones like
/// `caching` or `kubernetes` — they double as everyday English and
/// the case-insensitive title match can't tell a navigation cue from
/// generic prose. Multi-token names (`auth-tokens`, `rest-patterns`,
/// `oauth-migration`) are distinctive enough that a body mention is
/// almost always a deliberate reference.
///
/// For single-token entities (`bob`, `alice`, `kubernetes`,
/// `performance`) the LLM is responsible for typing `[[stem]]`
/// explicitly when it intends a navigation reference. The
/// `/memex-ingest` skill prompt covers this path.
pub fn auto_link_eligible(stem: &str) -> bool {
    stem.contains('-')
}

/// Walk the wiki dir, add `[[new_stem]]` to every existing page whose
/// body mentions `new_title` or `new_stem` (and isn't already linked),
/// rewrite the file, and reindex (including embeddings). Returns
/// stems of pages that got a new backlink.
///
/// Caller is expected to gate on `auto_link_eligible(new_stem)` —
/// when ineligible, this function should not be called at all.
///
/// Body-only rewrite: frontmatter (including `updated_at`) is
/// preserved verbatim, since adding a backlink doesn't change the
/// page's knowledge content.
///
/// Re-embed each rewritten page. The body's content hash changes
/// when `[[link]]` text is inserted; `commit_doc` detects that
/// change and drops the old chunks. If we don't re-embed in the same
/// pass, the page would survive in FTS5 but disappear from vector
/// search until a later reconcile or `lint --fix` re-embedded it.
/// Embedding cost (~50 ms per page) is the price of correctness.
pub fn maintain_backlinks(
    memex: &crate::Memex,
    new_stem: &str,
    new_title: &str,
    embedder: &mut dyn crate::embed::Embedder,
) -> crate::error::Result<Vec<String>> {
    maintain_backlinks_batch(memex, &[(new_stem, new_title)], embedder)
}

/// Walk every wiki page once, applying every `(new_stem, new_title)`
/// entry against each, and re-embed each touched page exactly once.
///
/// Single-page callers go through `maintain_backlinks`. The ingest
/// pipeline uses this directly: a 10-page batch over a 10k-page wiki
/// went from 100k `read_to_string` calls to 10k under the previous
/// shape. Each existing page is also re-embedded at most once even
/// when several new entries link into it, instead of once per new
/// entry.
pub fn maintain_backlinks_batch(
    memex: &crate::Memex,
    new_entries: &[(&str, &str)],
    embedder: &mut dyn crate::embed::Embedder,
) -> crate::error::Result<Vec<String>> {
    let wiki_dir = memex.wiki_dir();
    let mut backlinked: Vec<String> = Vec::new();
    if new_entries.is_empty() {
        return Ok(backlinked);
    }
    let iter = match std::fs::read_dir(&wiki_dir) {
        Ok(it) => it,
        Err(_) => return Ok(backlinked),
    };
    for entry in iter.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let other_stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let Ok(other_content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok((_, other_body)) = crate::validate::parse_frontmatter(&other_content) else {
            continue;
        };
        // Apply every entry to this page's body in one pass. `body`
        // accumulates each rewrite so a page can be backlinked to
        // multiple new pages in one ingest. `touched` short-circuits
        // the atomic_write + re-embed if no entry matched.
        let mut body = other_body.to_string();
        let mut touched = false;
        for (new_stem, new_title) in new_entries {
            if other_stem == *new_stem {
                continue;
            }
            let (next, was_linked) = backward_link_page(&body, new_stem, new_title, &other_stem);
            if was_linked {
                body = next;
                touched = true;
            }
        }
        if !touched {
            continue;
        }
        let updated_content = replace_body_preserving_frontmatter(&other_content, &body);
        if crate::storage::atomic_write(&path, updated_content.as_bytes()).is_ok() {
            // Re-embed under the same writer pass to keep chunks
            // consistent with the new body hash.
            let _ = crate::index_wiki::index_wiki_file(memex, &path, Some(&mut *embedder));
            backlinked.push(other_stem);
        }
    }
    Ok(backlinked)
}

/// Replace the body after frontmatter with `new_body`, preserving the
/// frontmatter block verbatim. Used to rewrite backlinks without
/// bumping `updated_at`.
pub fn replace_body_preserving_frontmatter(original: &str, new_body: &str) -> String {
    let trimmed = original.trim_start();
    if !trimmed.starts_with("---") {
        return original.to_string();
    }
    let after_open = &trimmed[3..];
    let Some(close_idx) = after_open.find("---") else {
        return original.to_string();
    };
    let trim_offset = original.len() - trimmed.len();
    let body_region_start = trim_offset + 3 + close_idx + 3;

    if let Ok((_, old_body)) = crate::validate::parse_frontmatter(original)
        && !old_body.is_empty()
        && let Some(rel) = original[body_region_start..].find(&old_body)
    {
        let abs = body_region_start + rel;
        return format!("{}{}", &original[..abs], new_body);
    }
    let prefix = original[..body_region_start].trim_end();
    format!("{prefix}\n\n{new_body}")
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

    #[test]
    fn auto_link_eligible_skips_all_single_token() {
        // Every single-token stem is skipped — short generic words
        // and longer common-noun names alike. The case-insensitive
        // title match can't tell a navigation cue from generic prose
        // for single-token names; the LLM types `[[stem]]` manually
        // when it means a reference.
        assert!(!auto_link_eligible("api"));
        assert!(!auto_link_eligible("auth"));
        assert!(!auto_link_eligible("cache"));
        assert!(!auto_link_eligible("oauth"));
        assert!(!auto_link_eligible("caching"));
        assert!(!auto_link_eligible("kubernetes"));
        assert!(!auto_link_eligible("performance"));
        assert!(!auto_link_eligible("database"));
        assert!(!auto_link_eligible("bob"));
        assert!(!auto_link_eligible("alice"));
    }

    #[test]
    fn auto_link_eligible_allows_multi_token() {
        // Multi-token stems are eligible — distinctive enough that a
        // body mention is almost always a deliberate reference.
        assert!(auto_link_eligible("auth-tokens"));
        assert!(auto_link_eligible("rest-patterns"));
        assert!(auto_link_eligible("oauth-migration"));
        assert!(auto_link_eligible("performance-tuning"));
        assert!(auto_link_eligible("kubernetes-deployment"));
    }

    /// Batched form must apply EVERY new entry to each existing page in
    /// one walk and re-embed the page exactly once even when several
    /// entries match. The previous per-entry loop did N walks and
    /// re-embedded once per entry, so two new pages backlinking into
    /// the same existing page meant two re-embeds.
    #[test]
    fn maintain_backlinks_batch_applies_all_entries_in_one_pass() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let wiki_dir = memex.wiki_dir();

        // Existing page mentions BOTH "Auth Tokens" and "REST Patterns"
        // verbatim, neither linked yet.
        let existing = "---\ntitle: API Design
sources: []\n\
            created_at: 2026-04-30T00:00:00Z\nupdated_at: 2026-04-30T00:00:00Z\n\
            ---\n\nWe use Auth Tokens for clients and REST Patterns for resources.\n";
        std::fs::write(wiki_dir.join("api-design.md"), existing).unwrap();

        // Two new entries; both should bracket their mention in the
        // existing page in one walk.
        let mut model = crate::embed::MockEmbedder;
        let backlinked = maintain_backlinks_batch(
            &memex,
            &[
                ("auth-tokens", "Auth Tokens"),
                ("rest-patterns", "REST Patterns"),
            ],
            &mut model,
        )
        .unwrap();
        assert_eq!(
            backlinked,
            vec!["api-design".to_string()],
            "page should be reported once even though two entries matched"
        );

        let after = std::fs::read_to_string(wiki_dir.join("api-design.md")).unwrap();
        assert!(
            after.contains("[[auth-tokens]]"),
            "first entry should be bracketed, got: {after}"
        );
        assert!(
            after.contains("[[rest-patterns]]"),
            "second entry should also be bracketed (single-pass batched), got: {after}"
        );
    }

    /// A new entry whose stem equals an existing page's stem must NOT
    /// rewrite that page (the page would be backlinking to itself).
    #[test]
    fn maintain_backlinks_batch_skips_self() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let wiki_dir = memex.wiki_dir();

        let body = "---\ntitle: Auth Tokens
sources: []\n\
            created_at: 2026-04-30T00:00:00Z\nupdated_at: 2026-04-30T00:00:00Z\n\
            ---\n\nThe Auth Tokens system manages session lifetimes.\n";
        std::fs::write(wiki_dir.join("auth-tokens.md"), body).unwrap();

        let mut model = crate::embed::MockEmbedder;
        let backlinked =
            maintain_backlinks_batch(&memex, &[("auth-tokens", "Auth Tokens")], &mut model)
                .unwrap();
        assert!(
            backlinked.is_empty(),
            "self-page must not be rewritten, got: {backlinked:?}"
        );
    }
}
