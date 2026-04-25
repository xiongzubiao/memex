//! End-to-end document ingest via the daemon, mocking the worker pool.
mod common;

use common::DaemonHarness;
use memex_cli::daemon::protocol::{Event, IngestSource, Request};

#[tokio::test(flavor = "multi_thread")]
async fn document_ingest_short_content_runs_one_chunk() {
    let h = DaemonHarness::start_with_mock_extract(|_prompt| {
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
        memex_root: h.memex_root().to_string_lossy().to_string(),
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
    let h = DaemonHarness::start_with_mock_extract(|_| {
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
        memex_root: h.memex_root().to_string_lossy().to_string(),
    };
    h.send(&req).await.unwrap();
    // Second call with identical content → Done with no Stored event.
    let events = h.send(&req).await.unwrap();
    let has_stored = events.iter().any(|e| matches!(e, Event::Stored { .. }));
    assert!(!has_stored, "second ingest should be deduped: {events:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn document_ingest_rejects_oversize() {
    let h = DaemonHarness::start().await;
    let big = "a".repeat(6 * 1024 * 1024);
    let req = Request::Ingest {
        source: IngestSource::Document {
            source_path: "x".into(),
            content: big,
        },
        collections: vec![],
        memex_root: h.memex_root().to_string_lossy().to_string(),
    };
    let events = h.send(&req).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Error { code, .. } if code == "bad_request"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn document_ingest_multi_chunk_runs_extract_per_chunk() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = calls.clone();
    let h = DaemonHarness::start_with_mock_extract(move |_prompt| {
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
        memex_root: h.memex_root().to_string_lossy().to_string(),
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
    let h = DaemonHarness::start_with_mock_extract_and_merge(
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
        memex_root: h.memex_root().to_string_lossy().to_string(),
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
