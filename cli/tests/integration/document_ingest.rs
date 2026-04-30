//! End-to-end document ingest via the daemon, mocking the worker pool.

use crate::integration_harness::IntegrationHarness;
use memex_cli::daemon::protocol::{Event, IngestSource, Request};


#[tokio::test(flavor = "multi_thread")]
async fn document_ingest_short_content_runs_one_chunk() {
    let h = IntegrationHarness::start_with_mock_extract(|_prompt| {
        // Mock returns a single page extracted from any document.
        // Bare YAML list — `parse_ingest` accepts via the `\n- slug:` recovery path.
        r#"
- slug: example-page
  title: Example Page
  tags: [test]
  body: |
    Page extracted from a short document.
"#
        .to_string()
    })
    .await;

    let req = Request::Ingest {
        source: IngestSource::Document {
            source_path: "https://example.com/post".into(),
            content: "# Test Article\n\nA short document that fits in one chunk.\n".into(),
        },
        collections: vec![],
    };
    let events = h.send(&req).await.unwrap();

    let stored = events.iter().find(|e| matches!(e, Event::Stored { .. }));
    assert!(stored.is_some(), "expected Stored event, got: {events:?}");
    if let Some(Event::Stored { wiki_pages, .. }) = stored {
        assert!(wiki_pages.iter().any(|s| s == "example-page"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn document_ingest_dedups_on_repeat() {
    let h = IntegrationHarness::start_with_mock_extract(|_| {
        r#"
- slug: only-page
  title: Only Page
  tags: []
  body: body
"#
        .to_string()
    })
    .await;

    let req = Request::Ingest {
        source: IngestSource::Document {
            source_path: "https://example.com/x".into(),
            content: "# X\n\nbody.\n".into(),
        },
        collections: vec![],
    };
    h.send(&req).await.unwrap();
    // Second call with identical content → Done with no Stored event.
    let events = h.send(&req).await.unwrap();
    let has_stored = events.iter().any(|e| matches!(e, Event::Stored { .. }));
    assert!(!has_stored, "second ingest should be deduped: {events:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn document_ingest_multi_chunk_runs_extract_per_chunk() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();
    let h = IntegrationHarness::start_with_mock_extract(move |_prompt| {
        let n = calls_clone.fetch_add(1, Ordering::Relaxed);
        format!(
            r#"
- slug: page-{n}
  title: Page {n}
  tags: []
  body: |
    Body extracted from chunk {n}.
"#
        )
    })
    .await;

    // Build a document with H1 sections sized to force chunking with the
    // default chunk_target_tokens=30000. Each section ~12K chars (~4K tokens);
    // 12 sections → ~48K tokens → 2-3 chunks expected.
    let mut content = String::with_capacity(180_000);
    for i in 0..12 {
        content.push_str(&format!("# Section {i}\n\n"));
        content.push_str(&"x".repeat(12_000));
        content.push_str("\n\n");
    }
    let req = Request::Ingest {
        source: IngestSource::Document {
            source_path: "https://example.com/long-doc".into(),
            content,
        },
        collections: vec![],
    };
    let events = h.send(&req).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(e, Event::Stored { .. })),
        "expected Stored event after multi-chunk ingest, got: {events:?}"
    );
    let n_extract_calls = calls.load(Ordering::Relaxed);
    assert!(
        n_extract_calls >= 2,
        "expected >=2 Extract calls (one per chunk), got {n_extract_calls}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn document_ingest_cross_chunk_same_slug_runs_merge() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    // Mock returns the SAME slug for every chunk to force fragment-merge.
    let extract_calls = Arc::new(AtomicUsize::new(0));
    let extract_clone = extract_calls.clone();
    let merge_calls = Arc::new(AtomicUsize::new(0));
    let merge_clone = merge_calls.clone();
    let h = IntegrationHarness::start_with_mock_extract_and_merge(
        move |_prompt| {
            extract_clone.fetch_add(1, Ordering::Relaxed);
            r#"
- slug: shared-subject
  title: Shared Subject
  tags: []
  body: A fragment about the shared subject.
"#
            .to_string()
        },
        move |_prompt| {
            merge_clone.fetch_add(1, Ordering::Relaxed);
            r#"
- slug: shared-subject
  title: Shared Subject
  tags: []
  body: A merged page combining all fragments.
"#
            .to_string()
        },
    )
    .await;

    // Force chunking the same way as the prior test.
    let mut content = String::with_capacity(120_000);
    for i in 0..8 {
        content.push_str(&format!("# Section {i}\n\n"));
        content.push_str(&"y".repeat(12_000));
        content.push_str("\n\n");
    }
    let req = Request::Ingest {
        source: IngestSource::Document {
            source_path: "https://example.com/dup-slug".into(),
            content,
        },
        collections: vec![],
    };
    h.send(&req).await.unwrap();
    let n_extracts = extract_calls.load(Ordering::Relaxed);
    let n_merges = merge_calls.load(Ordering::Relaxed);
    assert!(
        n_extracts >= 2,
        "expected >=2 Extract calls, got {n_extracts}"
    );
    // The merge count is N-1 chained fragment-merges plus possibly 1 wiki-side
    // merge if the same slug already exists on disk. Allow >= N-1.
    assert!(
        n_merges >= n_extracts - 1,
        "expected >={} Merge calls (fragment-merge), got {n_merges}",
        n_extracts - 1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn document_ingest_writes_raw_artifact_to_filesystem() {
    let harness = IntegrationHarness::start_with_mock_extract(|_prompt| {
        r#"
- slug: topic-article
  title: Topic Article
  tags: []
  body: |
    A page about authentication and bearer tokens.
"#
        .to_string()
    })
    .await;
    let body = "# Topic Article\n\nbody body body about authentication and bearer tokens.";
    let evt = harness
        .ingest_document("https://example.com/topic", body)
        .await
        .unwrap();

    let body_hash = memex_core::storage::content_hash(body.as_bytes());
    let raw_path = memex_core::raw::raw_path_for_hash(&harness.raw_dir(), &body_hash);
    assert!(
        raw_path.exists(),
        "raw artifact must exist on disk at {}",
        raw_path.display()
    );

    let file = std::fs::read_to_string(&raw_path).unwrap();
    assert!(file.starts_with("---\n"));
    assert!(file.contains("source: https://example.com/topic"));
    assert!(
        file.contains(body),
        "raw file must contain the original body verbatim"
    );

    // documents.path matches the relative raw path
    let conn_path = harness.root().join("index.db");
    let conn = rusqlite::Connection::open(&conn_path).unwrap();
    let row_path: String = conn
        .query_row(
            "SELECT path FROM documents WHERE doc_type='raw' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let raw_rel = raw_path
        .strip_prefix(harness.root())
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    assert_eq!(row_path, raw_rel);

    // The Stored event should report a 7-char docid that matches hash[..7]
    assert_eq!(evt.source_docid, &body_hash[..7]);
}

/// Regression test: when MERGE fails, the existing wiki page must keep its
/// accumulated content. Earlier behavior wrote the proposed pages as new with
/// the same slug, which routed through `store_ingest_batch`'s
/// `ON CONFLICT(doc_type, path) DO UPDATE` and destructively overwrote the
/// existing page with just the latest session's proposal.
#[tokio::test(flavor = "multi_thread")]
async fn merge_failure_preserves_existing_wiki_page() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let extract_calls = Arc::new(AtomicUsize::new(0));
    let extract_clone = extract_calls.clone();
    let merge_calls = Arc::new(AtomicUsize::new(0));
    let merge_clone = merge_calls.clone();

    // Extract: returns slug `alice` with body that embeds the call index, so
    //          successive calls produce distinguishable proposed content.
    // Merge:   always returns garbage to trigger a parse failure
    //          (-> WorkerError::Backend -> Err branch in store_extracted_pages).
    // Real-retrieval variant required: the dedup search that routes the
    // second ingest into MERGE relies on the embed model.
    let h = match IntegrationHarness::start_with_mock_extract_and_merge_with_embed(
        move |_prompt| {
            let n = extract_clone.fetch_add(1, Ordering::Relaxed);
            format!(
                "- slug: alice\n  title: Alice\n  tags: []\n  body: |\n    extract-call-{n}\n",
            )
        },
        move |_prompt| {
            merge_clone.fetch_add(1, Ordering::Relaxed);
            "this is not yaml or json — must fail to parse".to_string()
        },
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            eprintln!("SKIP: ONNX embed model unavailable: {e}");
            return;
        }
    };

    let alice_path = h.memex_root().join("wiki").join("alice.md");

    // First ingest: creates alice.md with body "extract-call-0".
    let req1 = Request::Ingest {
        source: IngestSource::Document {
            source_path: "https://example.com/doc1".into(),
            content: "# Doc 1\n\nFirst document body.\n".into(),
        },
        collections: vec![],
    };
    let events1 = h.send(&req1).await.unwrap();
    assert!(
        events1.iter().any(|e| matches!(e, Event::Stored { .. })),
        "first ingest should Store; got: {events1:?}"
    );

    let after_first =
        std::fs::read_to_string(&alice_path).expect("alice.md should exist after first ingest");
    assert!(
        after_first.contains("extract-call-0"),
        "expected extract-call-0 body, got:\n{after_first}"
    );
    assert_eq!(merge_calls.load(Ordering::Relaxed), 0, "no merge on first ingest");

    // Second ingest: extract proposes alice again, MERGE fails. The patched
    // fallback must NOT overwrite alice.md.
    let req2 = Request::Ingest {
        source: IngestSource::Document {
            source_path: "https://example.com/doc2".into(),
            content: "# Doc 2\n\nDifferent body so source dedup doesn't skip.\n".into(),
        },
        collections: vec![],
    };
    let events2 = h.send(&req2).await.unwrap();
    assert!(
        !events2.iter().any(|e| matches!(e, Event::Error { .. })),
        "second ingest should not surface an error event: {events2:?}"
    );
    assert!(
        merge_calls.load(Ordering::Relaxed) >= 1,
        "merge job should have been attempted on the second ingest"
    );

    // The critical invariant: alice.md is byte-identical to its post-first-ingest
    // state. The merge fallback must not silently replace the body with just
    // this session's proposal.
    let after_second = std::fs::read_to_string(&alice_path)
        .expect("alice.md should still exist after failed-merge ingest");
    assert_eq!(
        after_first, after_second,
        "merge fallback overwrote the wiki page (regression).\nbefore:\n{after_first}\nafter:\n{after_second}"
    );
    assert!(
        !after_second.contains("extract-call-1"),
        "alice.md must NOT contain the second-ingest proposal:\n{after_second}"
    );
}
