//! Integration tests for the `Request::SourceAdd` daemon path.

use crate::integration_harness::IntegrationHarness;
use memex_cli::daemon::protocol::{Event, Request};

#[tokio::test(flavor = "multi_thread")]
async fn source_add_returns_docid_and_dedups_repeats() {
    let h = IntegrationHarness::start().await;
    let req = Request::SourceAdd {
        source_path: "https://example.com/article".into(),
        content: "# Title\n\nbody paragraph.\n".into(),
        collections: vec![],
    };
    let events = h.send(&req).await.unwrap();
    let docid1 = events
        .iter()
        .find_map(|e| {
            if let Event::SourceAdded { docid } = e {
                Some(docid.clone())
            } else {
                None
            }
        })
        .expect("first SourceAdd should return a docid");

    let events = h.send(&req).await.unwrap();
    let docid2 = events
        .iter()
        .find_map(|e| {
            if let Event::SourceAdded { docid } = e {
                Some(docid.clone())
            } else {
                None
            }
        })
        .expect("second SourceAdd should also return a docid");
    assert_eq!(docid1, docid2, "identical content should reuse docid");
}

#[tokio::test(flavor = "multi_thread")]
async fn source_add_rejects_empty_content() {
    let h = IntegrationHarness::start().await;
    let req = Request::SourceAdd {
        source_path: "x".into(),
        content: "   \n".into(),
        collections: vec![],
    };
    let events = h.send(&req).await.unwrap();
    let has_err = events
        .iter()
        .any(|e| matches!(e, Event::Error { code, .. } if code == "bad_request"));
    assert!(has_err, "expected bad_request error, got: {events:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn source_delete_removes_unreferenced_source() {
    let h = IntegrationHarness::start().await;
    // Add
    let docid = {
        let r = Request::SourceAdd {
            source_path: "https://example.com/x".into(),
            content: "# X\n\nbody\n".into(),
            collections: vec![],
        };
        h.send(&r)
            .await
            .unwrap()
            .into_iter()
            .find_map(|e| {
                if let Event::SourceAdded { docid } = e {
                    Some(docid)
                } else {
                    None
                }
            })
            .expect("SourceAdded")
    };
    // Delete
    let r = Request::SourceDelete {
        ref_: docid.clone(),
        force: false,
    };
    let events = h.send(&r).await.unwrap();
    let deleted = events.iter().find_map(|e| match e {
        Event::SourceDeleted {
            docid,
            dangling_wiki_pages,
            ..
        } => Some((docid.clone(), dangling_wiki_pages.clone())),
        _ => None,
    });
    assert!(deleted.is_some(), "expected SourceDeleted, got: {events:?}");
    let (returned_docid, dangling) = deleted.unwrap();
    assert_eq!(returned_docid, docid);
    assert!(dangling.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn source_delete_refuses_when_referenced_without_force() {
    let h = IntegrationHarness::start().await;
    // Add source
    let src_docid = {
        let r = Request::SourceAdd {
            source_path: "https://example.com/y".into(),
            content: "# Y\n\nbody\n".into(),
            collections: vec![],
        };
        h.send(&r)
            .await
            .unwrap()
            .into_iter()
            .find_map(|e| {
                if let Event::SourceAdded { docid } = e {
                    Some(docid)
                } else {
                    None
                }
            })
            .expect("SourceAdded")
    };
    // Write a wiki page referencing it
    let r = Request::Write {
        title: "Referencing".into(),
        content: "---\ntitle: Referencing\n---\n\nbody".to_string(),
        source: Some(src_docid.clone()),
        force: false,
    };
    h.send(&r).await.unwrap();
    // Delete without --force should fail
    let r = Request::SourceDelete {
        ref_: src_docid.clone(),
        force: false,
    };
    let events = h.send(&r).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Error { code, message, .. }
            if code == "bad_request" && message.contains("wiki pages reference"))),
        "expected reference-blocked error, got: {events:?}"
    );
    // Delete with --force succeeds
    let r = Request::SourceDelete {
        ref_: src_docid,
        force: true,
    };
    let events = h.send(&r).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(e, Event::SourceDeleted { dangling_wiki_pages, .. } if !dangling_wiki_pages.is_empty())),
        "expected SourceDeleted with dangling list, got: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn source_delete_resolves_path_ref() {
    let h = IntegrationHarness::start().await;
    let add_req = Request::SourceAdd {
        source_path: "https://example.com/z".into(),
        content: "# Z\n\nbody\n".into(),
        collections: vec![],
    };
    let add_events = h.send(&add_req).await.unwrap();
    let docid = add_events
        .into_iter()
        .find_map(|e| {
            if let Event::SourceAdded { docid } = e {
                Some(docid)
            } else {
                None
            }
        })
        .expect("SourceAdded");
    let r = Request::SourceDelete {
        ref_: docid,
        force: false,
    };
    let events = h.send(&r).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::SourceDeleted { .. }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn write_with_source_docid_succeeds() {
    let h = IntegrationHarness::start().await;
    // Step 1: source add
    let src_docid = {
        let r = Request::SourceAdd {
            source_path: "https://example.com/post".into(),
            content: "# Article\n\nbody\n".into(),
            collections: vec![],
        };
        h.send(&r)
            .await
            .unwrap()
            .into_iter()
            .find_map(|e| {
                if let Event::SourceAdded { docid } = e {
                    Some(docid)
                } else {
                    None
                }
            })
            .expect("source_add docid")
    };
    // Step 2: write with --source docid
    let r = Request::Write {
        title: "Test Page".into(),
        content: "---\ntitle: Test Page\n---\n\nBody.\n".into(),
        source: Some(src_docid.clone()),
        force: false,
    };
    let events = h.send(&r).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(e, Event::Written { .. })),
        "expected Written event, got: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn source_list_returns_added_sources() {
    let h = IntegrationHarness::start().await;
    for path in &["https://x/1", "https://x/2"] {
        let r = Request::SourceAdd {
            source_path: (*path).to_string(),
            content: format!("# {path}\n\nbody"),
            collections: vec![],
        };
        h.send(&r).await.unwrap();
    }
    let memex = memex_core::Memex::open(h.memex_root().to_path_buf()).unwrap();
    let rows = memex.search().list_sources(&[]).unwrap();
    let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&"https://x/1"));
    assert!(paths.contains(&"https://x/2"));
}

#[tokio::test(flavor = "multi_thread")]
async fn write_rejects_non_docid_source() {
    let h = IntegrationHarness::start().await;
    let r = Request::Write {
        title: "Test".into(),
        content: "---\ntitle: Test\n---\n\nbody.\n".into(),
        source: Some("/path/to/file.md".into()), // looks like fspath, not a docid
        force: false,
    };
    let events = h.send(&r).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Error { code, .. } if code == "bad_request")),
        "expected bad_request, got: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_add_writes_raw_file_with_frontmatter_and_returns_short_docid() {
    let harness = IntegrationHarness::start().await;
    let body = "# Auth Tokens Explained\n\nArticle body...";
    let resp = harness
        .source_add("https://example.com/articles/auth-tokens-explained", body)
        .await
        .unwrap();
    let body_hash = memex_core::storage::content_hash(body.as_bytes());
    assert_eq!(resp.docid, &body_hash[..7]);

    let raw_path = memex_core::raw::raw_path_for_hash(&harness.raw_dir(), &body_hash);
    let file = std::fs::read_to_string(&raw_path).unwrap();
    assert!(file.starts_with("---\n"));
    assert!(file.contains("source: https://example.com/articles/auth-tokens-explained"));
    assert!(file.contains("source_kind: url"));
    assert!(file.contains("ingested_at:"));
    assert!(
        file.ends_with(body),
        "file should end with original body verbatim"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_add_idempotent_does_not_modify_existing_file() {
    let harness = IntegrationHarness::start().await;
    let body = "same body";
    let _ = harness.source_add("https://x", body).await.unwrap();
    let raw_path = memex_core::raw::raw_path_for_hash(
        &harness.raw_dir(),
        &memex_core::storage::content_hash(body.as_bytes()),
    );
    let mtime_before = std::fs::metadata(&raw_path).unwrap().modified().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let _ = harness.source_add("https://x", body).await.unwrap();
    let mtime_after = std::fs::metadata(&raw_path).unwrap().modified().unwrap();
    assert_eq!(
        mtime_before, mtime_after,
        "second add must not touch the file"
    );
}
