//! Task 33: docid prefix ambiguity + resolution at 5-char prefix.

use memex_core::Memex;

#[test]
fn ambiguous_4char_prefix_resolves_at_5char() {
    let dir = tempfile::TempDir::new().unwrap();
    let memex = Memex::open_writer(dir.path().to_path_buf()).unwrap();

    // Synthesize two distinct wiki bodies whose hashes share a 4-char prefix.
    let (b1, b2) = forge_pair_with_prefix("abcd");

    std::fs::write(
        memex.wiki_dir().join("a.md"),
        format!(
            "---\ntitle: A\ntags: []\nsources: []\ncreated_at: 2026-04-26T00:00:00Z\nupdated_at: 2026-04-26T00:00:00Z\n---\n\n{b1}"
        ),
    )
    .unwrap();
    std::fs::write(
        memex.wiki_dir().join("b.md"),
        format!(
            "---\ntitle: B\ntags: []\nsources: []\ncreated_at: 2026-04-26T00:00:00Z\nupdated_at: 2026-04-26T00:00:00Z\n---\n\n{b2}"
        ),
    )
    .unwrap();
    memex_core::reconcile::reconcile(&memex, Default::default()).unwrap();

    let conn = memex.search().conn_for_test();
    let err = memex_core::docid::resolve_prefix(&conn, "abcd").unwrap_err();
    match err {
        memex_core::docid::ResolveError::Ambiguous { candidates, .. } => {
            assert_eq!(candidates.len(), 2)
        }
        e => panic!("expected Ambiguous, got {e:?}"),
    }

    // Use a 5-char prefix derived from b1 — should be unique.
    let h1 = memex_core::storage::content_hash(b1.as_bytes());
    let unique = &h1[..5];
    let r = memex_core::docid::resolve_prefix(&conn, unique).unwrap();
    assert_eq!(r.hash, h1);
}

/// Brute-force two distinct body strings whose `content_hash` shares the
/// given hex prefix. With prefix length 4 (16 bits), expected ~65k tries
/// per find; we cap at 1M iterations.
fn forge_pair_with_prefix(prefix: &str) -> (String, String) {
    let mut first: Option<String> = None;
    for n in 0_u64..1_000_000 {
        let body = format!("body {n}");
        let h = memex_core::storage::content_hash(body.as_bytes());
        if h.starts_with(prefix) {
            match &first {
                None => first = Some(body),
                Some(b1) => {
                    let h1 = memex_core::storage::content_hash(b1.as_bytes());
                    if h != h1 {
                        return (b1.clone(), body);
                    }
                }
            }
        }
    }
    panic!("could not forge prefix collision for {prefix:?} in 1M iterations");
}
