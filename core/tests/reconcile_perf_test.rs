//! Reconciliation perf smoke test (Spec §11.6).
//!
//! Builds a 10K-doc wiki tree, runs a first reconcile to populate the index,
//! then asserts that a no-change second pass completes within the spec's
//! local-SSD budget. Gated behind `--features perf` because it writes 10K
//! files and takes longer than the unit-test budget.
//!
//! Run with: `cargo test -p memex-core --test reconcile_perf_test --features perf --release`

#![cfg(feature = "perf")]

use memex_core::Memex;
use std::time::Instant;

#[test]
fn reconcile_no_change_under_2s_for_10k_docs() {
    let dir = tempfile::TempDir::new().unwrap();
    let memex = Memex::open_writer(dir.path().to_path_buf()).unwrap();
    for i in 0..10_000 {
        let p = memex.wiki_dir().join(format!("p{i:05}.md"));
        std::fs::write(
            p,
            format!(
                "---\ntitle: P{i}
sources: []\ncreated_at: 2026-04-26T00:00:00Z\nupdated_at: 2026-04-26T00:00:00Z\n---\n\nbody {i}\n"
            ),
        )
        .unwrap();
    }

    // First pass populates the DB.
    let r1 = memex_core::reconcile::reconcile(&memex, Default::default()).unwrap();
    assert_eq!(r1.indexed, 10_000);

    // Second pass: stat-and-skip everything.
    let t0 = Instant::now();
    let r2 = memex_core::reconcile::reconcile(&memex, Default::default()).unwrap();
    let dt = t0.elapsed();
    assert_eq!(r2.skipped, 10_000);
    assert_eq!(r2.indexed, 0);
    assert_eq!(r2.deleted, 0);
    assert!(
        dt.as_secs() < 2,
        "no-change reconcile took {:?}; spec target is < 2s on local SSD",
        dt
    );
}
