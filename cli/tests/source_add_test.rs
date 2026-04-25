//! Integration tests for the `Request::SourceAdd` daemon path.
mod common;

use common::DaemonHarness;
use memex_cli::daemon::protocol::{Event, Request};

#[tokio::test(flavor = "multi_thread")]
async fn source_add_returns_docid_and_dedups_repeats() {
    let h = DaemonHarness::start().await;
    let req = Request::SourceAdd {
        source_path: "https://example.com/article".into(),
        content: "# Title\n\nbody paragraph.\n".into(),
        collections: vec![],
        memex_root: h.memex_root().to_string_lossy().to_string(),
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
    let h = DaemonHarness::start().await;
    let req = Request::SourceAdd {
        source_path: "x".into(),
        content: "   \n".into(),
        collections: vec![],
        memex_root: h.memex_root().to_string_lossy().to_string(),
    };
    let events = h.send(&req).await.unwrap();
    let has_err = events
        .iter()
        .any(|e| matches!(e, Event::Error { code, .. } if code == "bad_request"));
    assert!(has_err, "expected bad_request error, got: {events:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn source_delete_removes_unreferenced_source() {
    let h = DaemonHarness::start().await;
    // Add
    let docid = {
        let r = Request::SourceAdd {
            source_path: "https://example.com/x".into(),
            content: "# X\n\nbody\n".into(),
            collections: vec![],
            memex_root: h.memex_root().to_string_lossy().to_string(),
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
        memex_root: h.memex_root().to_string_lossy().to_string(),
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
    let h = DaemonHarness::start().await;
    // Add source
    let src_docid = {
        let r = Request::SourceAdd {
            source_path: "https://example.com/y".into(),
            content: "# Y\n\nbody\n".into(),
            collections: vec![],
            memex_root: h.memex_root().to_string_lossy().to_string(),
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
        tags: vec![],
        source: Some(src_docid.clone()),
        force: false,
        memex_root: h.memex_root().to_string_lossy().to_string(),
    };
    h.send(&r).await.unwrap();
    // Delete without --force should fail
    let r = Request::SourceDelete {
        ref_: src_docid.clone(),
        force: false,
        memex_root: h.memex_root().to_string_lossy().to_string(),
    };
    let events = h.send(&r).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(e, Event::Error { code, message, .. }
            if code == "bad_request" && message.contains("wiki pages reference"))),
        "expected reference-blocked error, got: {events:?}"
    );
    // Delete with --force succeeds
    let r = Request::SourceDelete {
        ref_: src_docid,
        force: true,
        memex_root: h.memex_root().to_string_lossy().to_string(),
    };
    let events = h.send(&r).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(e, Event::SourceDeleted { dangling_wiki_pages, .. } if !dangling_wiki_pages.is_empty())),
        "expected SourceDeleted with dangling list, got: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn source_delete_resolves_path_ref() {
    let h = DaemonHarness::start().await;
    let r = Request::SourceAdd {
        source_path: "https://example.com/z".into(),
        content: "# Z\n\nbody\n".into(),
        collections: vec![],
        memex_root: h.memex_root().to_string_lossy().to_string(),
    };
    h.send(&r).await.unwrap();
    let r = Request::SourceDelete {
        ref_: "path:https://example.com/z".into(),
        force: false,
        memex_root: h.memex_root().to_string_lossy().to_string(),
    };
    let events = h.send(&r).await.unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::SourceDeleted { .. })));
}

#[tokio::test(flavor = "multi_thread")]
async fn write_with_source_docid_succeeds() {
    let h = DaemonHarness::start().await;
    // Step 1: source add
    let src_docid = {
        let r = Request::SourceAdd {
            source_path: "https://example.com/post".into(),
            content: "# Article\n\nbody\n".into(),
            collections: vec![],
            memex_root: h.memex_root().to_string_lossy().to_string(),
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
        tags: vec![],
        source: Some(src_docid.clone()),
        force: false,
        memex_root: h.memex_root().to_string_lossy().to_string(),
    };
    let events = h.send(&r).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(e, Event::Written { .. })),
        "expected Written event, got: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn source_list_returns_added_sources() {
    let h = DaemonHarness::start().await;
    for path in &["https://x/1", "https://x/2"] {
        let r = Request::SourceAdd {
            source_path: (*path).to_string(),
            content: format!("# {path}\n\nbody"),
            collections: vec![],
            memex_root: h.memex_root().to_string_lossy().to_string(),
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
    let h = DaemonHarness::start().await;
    let r = Request::Write {
        title: "Test".into(),
        content: "---\ntitle: Test\n---\n\nbody.\n".into(),
        tags: vec![],
        source: Some("/path/to/file.md".into()), // looks like fspath, not a docid
        force: false,
        memex_root: h.memex_root().to_string_lossy().to_string(),
    };
    let events = h.send(&r).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Error { code, .. } if code == "bad_request")),
        "expected bad_request, got: {events:?}"
    );
}
