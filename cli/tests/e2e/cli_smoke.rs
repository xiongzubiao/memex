use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;

fn memex_cmd(root: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_memex"));
    cmd.env("MEMEX_ROOT", root);
    cmd
}

#[test]
fn invalid_env_var_exits_1() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    std::fs::create_dir_all(root.join("wiki")).unwrap();

    let output = memex_cmd(&root)
        .env("MEMEX_LOCK_TIMEOUT_SECONDS", "not_a_number")
        .arg("search")
        .arg("foo")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("MEMEX_LOCK_TIMEOUT_SECONDS"));
}

#[test]
fn search_during_concurrent_writer_not_blocking() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    std::fs::create_dir_all(root.join("wiki")).unwrap();

    use fs2::FileExt;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(root.join(".lock"))
        .unwrap();
    lock_file.lock_exclusive().unwrap();

    let start = std::time::Instant::now();
    let output = memex_cmd(&root)
        .arg("search")
        .arg("anything")
        .output()
        .unwrap();
    // A reader holds no OS flock, so it should complete without waiting for
    // the writer lock. 30s is a generous upper bound: if search blocked on
    // the default 120s lock timeout, it would take much longer.
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "search took {:?} while writer held lock",
        start.elapsed()
    );
    assert!(output.status.success() || output.status.code() == Some(0));

    FileExt::unlock(&lock_file).unwrap();
}
