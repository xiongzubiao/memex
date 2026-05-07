use memex_core::Memex;

#[test]
fn missing_index_db_triggers_full_rebuild_from_filesystem() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    {
        let m = Memex::open_writer(root.clone()).unwrap();
        std::fs::write(
            m.wiki_dir().join("foo.md"),
            "---\ntitle: Foo
sources: []\ncreated_at: 2026-04-26T00:00:00Z\nupdated_at: 2026-04-26T00:00:00Z\n---\n\nbody"
        ).unwrap();
        memex_core::reconcile::reconcile(&m, Default::default()).unwrap();
        let count: i64 = m.search().conn_for_test().query_row(
            "SELECT COUNT(*) FROM documents WHERE doc_type='wiki'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1, "preconditions: should have 1 indexed wiki doc");
    }

    // Wipe the DB (and any WAL sidecars).
    std::fs::remove_file(root.join("index.db")).ok();
    std::fs::remove_file(root.join("index.db-wal")).ok();
    std::fs::remove_file(root.join("index.db-shm")).ok();
    assert!(!root.join("index.db").exists());

    // Re-open: schema is created fresh; reconcile re-indexes foo.md from disk.
    let m = Memex::open_writer(root.clone()).unwrap();
    let report = memex_core::reconcile::reconcile(&m, Default::default()).unwrap();
    assert_eq!(report.indexed, 1);
    let count: i64 = m.search().conn_for_test().query_row(
        "SELECT COUNT(*) FROM documents WHERE doc_type='wiki'",
        [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1, "rebuild should restore the doc");
}
