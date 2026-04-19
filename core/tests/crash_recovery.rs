mod common;

use memex_core::Memex;
use std::time::{Duration, SystemTime};

#[test]
fn stale_tmp_files_cleaned_on_next_open_writer() {
    let (_dir, root) = common::setup_temp_memex();
    let wiki = root.join("wiki");

    let stale = wiki.join(".foo.md.99999.deadbeef.tmp");
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

    let fresh = wiki.join(".foo.md.12345.abcd1234.tmp");
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

#[cfg(unix)]
#[test]
fn sigkilled_writer_releases_lock_via_os() {
    use std::process::{Command, Stdio};

    let (_dir, root) = common::setup_temp_memex();

    let lock_path = root.join(".lock");
    std::fs::write(&lock_path, "").unwrap();

    let has_flock = Command::new("flock")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !has_flock {
        eprintln!("flock(1) not available; skipping sigkill test");
        return;
    }

    let mut child = Command::new("flock")
        .arg("--exclusive")
        .arg(&lock_path)
        .arg("--command")
        .arg("sleep 10")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    std::thread::sleep(Duration::from_millis(100)); // let child grab lock

    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    let _ = child.wait();

    let start = std::time::Instant::now();
    let result = Memex::open_writer_with_timeout(root, Duration::from_millis(500));
    assert!(
        result.is_ok(),
        "open_writer after SIGKILL failed: {result:?}"
    );
    assert!(start.elapsed() < Duration::from_millis(500));
}
