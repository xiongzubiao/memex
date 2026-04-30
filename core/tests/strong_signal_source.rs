//! Verify the strong-signal probe in `hybrid_retrieve_expanded` fires
//! when *either* wiki or source has a clean BM25 winner. Pre-fix, the
//! probe only looked at wiki, so source-heavy corpora missed the
//! expansion-bypass even with obvious BM25 hits.

use memex_core::Memex;
use memex_core::retrieval::{hybrid_retrieve_expanded, Expansion, Signal};
use memex_core::search::UpsertDocument;
use tempfile::TempDir;

fn upsert(memex: &Memex, doc: UpsertDocument) {
    memex.search().upsert_document(&doc).unwrap();
}

/// Insert 8 unrelated decoy wiki pages so BM25 idf is meaningful. With
/// only 2 corpus docs, idf collapses to ~0 and even strong term hits
/// score below the strong-signal threshold. This matches the "real
/// corpus" setup the comparison test used.
fn insert_decoys(memex: &Memex) {
    let decoys = [
        ("decoy-pasta", "Pasta Cooking", "Boil pasta in salted water until al dente."),
        ("decoy-coffee", "Coffee Brewing", "Pour over methods give clean cups; espresso uses pressure."),
        ("decoy-dna", "DNA Replication", "Helicase unwinds the helix; polymerase synthesizes new strands."),
        ("decoy-photo", "Photosynthesis", "Chlorophyll absorbs light and drives the Calvin cycle."),
        ("decoy-rust", "Rust Ownership", "Each value has one owner; references must be shared XOR mutable."),
        ("decoy-git", "Git Rebase", "Rebase rewrites history by replaying commits onto a new base."),
        ("decoy-sql", "SQL Indexing", "B-tree indexes let queries find rows without scanning the table."),
        ("decoy-tcp", "TCP Handshake", "SYN, SYN-ACK, ACK establishes a reliable connection."),
    ];
    for (i, (slug, title, body)) in decoys.iter().enumerate() {
        let hash = format!("{:0>64}", format!("d{i:0>2}"));
        upsert(
            memex,
            UpsertDocument {
                doc_type: "wiki",
                path: &format!("wiki/{slug}.md"),
                title,
                hash: &hash,
                tags: "entity",
                source: None,
                body,
                mtime: "2026-04-27T00:00:00Z",
                size: body.len() as i64,
            },
        );
    }
}

#[test]
fn strong_signal_fires_on_source_only_match() {
    let dir = TempDir::new().unwrap();
    let memex = Memex::open_writer(dir.path().to_path_buf()).unwrap();
    insert_decoys(&memex);

    // One wiki page on an unrelated topic; one source doc that strongly
    // matches the query. Pre-fix this was signal=weak; now it should
    // be signal=strong because the source list has a clean winner.
    upsert(
        &memex,
        UpsertDocument {
            doc_type: "wiki",
            path: "wiki/cooking.md",
            title: "Pasta Cooking",
            hash: "0000000000000000000000000000000000000000000000000000000000000001",
            tags: "entity",
            source: None,
            body: "Boil pasta in salted water until al dente.",
            mtime: "2026-04-27T00:00:00Z",
            size: 42,
        },
    );
    upsert(
        &memex,
        UpsertDocument {
            doc_type: "raw",
            path: "raw/aa/transcript.md",
            title: "Bearer token discussion",
            hash: "0000000000000000000000000000000000000000000000000000000000000002",
            tags: "",
            source: Some("transcript"),
            body: "Bearer tokens are short-lived credentials. The bearer header carries the token. \
                   Bearer authentication is a stateless scheme.",
            mtime: "2026-04-27T00:00:00Z",
            size: 200,
        },
    );

    let result = hybrid_retrieve_expanded(
        memex.search(),
        "bearer tokens",
        &[], // empty embedding — skip vector search path
        &Expansion::default(),
        &[],
        None,
        memex.root(),
    )
    .unwrap();

    assert_eq!(result.signal, Signal::Strong, "expected strong signal from clean source-only BM25 hit");
}

#[test]
fn weak_signal_when_no_clean_winner() {
    let dir = TempDir::new().unwrap();
    let memex = Memex::open_writer(dir.path().to_path_buf()).unwrap();
    insert_decoys(&memex);

    // Two source docs both matching "bearer" — no clean gap between them.
    upsert(
        &memex,
        UpsertDocument {
            doc_type: "raw",
            path: "raw/aa/transcript1.md",
            title: "Bearer token discussion",
            hash: "0000000000000000000000000000000000000000000000000000000000000003",
            tags: "",
            source: Some("transcript"),
            body: "Bearer tokens are short-lived credentials.",
            mtime: "2026-04-27T00:00:00Z",
            size: 50,
        },
    );
    upsert(
        &memex,
        UpsertDocument {
            doc_type: "raw",
            path: "raw/bb/transcript2.md",
            title: "Bearer Auth Flow",
            hash: "0000000000000000000000000000000000000000000000000000000000000004",
            tags: "",
            source: Some("transcript"),
            body: "Bearer tokens flow through HTTP headers.",
            mtime: "2026-04-27T00:00:00Z",
            size: 50,
        },
    );

    let result = hybrid_retrieve_expanded(
        memex.search(),
        "bearer tokens",
        &[],
        &Expansion::default(),
        &[],
        None,
        memex.root(),
    )
    .unwrap();

    assert_eq!(
        result.signal,
        Signal::Weak,
        "expected weak signal when two source docs tie on BM25"
    );
}

