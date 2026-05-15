mod common;

use memex_core::Memex;
use std::time::{Duration, SystemTime};

#[test]
fn stale_tmp_files_cleaned_on_next_open_writer() {
    let (_dir, root) = common::setup_temp_memex();
    let wiki = root.join("wiki");

    let stale = wiki.join(".foo.md.deadbeef.tmp");
    std::fs::write(&stale, "stale").unwrap();
    let two_hours_ago = SystemTime::now() - Duration::from_secs(7200);
    filetime::set_file_mtime(&stale, filetime::FileTime::from_system_time(two_hours_ago)).unwrap();

    let _m = Memex::open_writer(root).unwrap();
    assert!(!stale.exists(), "stale tmp should have been cleaned");
}

#[test]
fn fresh_tmp_files_preserved_on_open_writer() {
    let (_dir, root) = common::setup_temp_memex();
    let wiki = root.join("wiki");

    let fresh = wiki.join(".foo.md.abcd1234.tmp");
    std::fs::write(&fresh, "fresh").unwrap();

    let _m = Memex::open_writer(root).unwrap();
    assert!(fresh.exists(), "fresh tmp should NOT have been cleaned");
}

#[test]
fn cleanup_ignores_non_memex_dotfiles() {
    let (_dir, root) = common::setup_temp_memex();
    let wiki = root.join("wiki");

    let ds_store = wiki.join(".DS_Store");
    let swp = wiki.join(".foo.swp");
    std::fs::write(&ds_store, "").unwrap();
    std::fs::write(&swp, "").unwrap();
    let two_hours_ago = SystemTime::now() - Duration::from_secs(7200);
    filetime::set_file_mtime(
        &ds_store,
        filetime::FileTime::from_system_time(two_hours_ago),
    )
    .unwrap();
    filetime::set_file_mtime(&swp, filetime::FileTime::from_system_time(two_hours_ago)).unwrap();

    let _m = Memex::open_writer(root).unwrap();
    assert!(ds_store.exists());
    assert!(swp.exists());
}
