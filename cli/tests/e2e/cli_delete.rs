//! E2E tests for `memex delete` (and related sub-commands).
//! Spawns the actual `memex` binary; daemon-mediated commands
//! auto-spawn the daemon via connect_or_spawn.

use crate::common::*;

#[test]
fn delete_removes_page() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write a page via the write command so it is indexed.
    let content = make_page("Delete Me", "Temporary page content.");
    let out = run_write(&root, "delete-me", &content, &[]);
    assert!(out.status.success(), "write should succeed");

    // Delete it with --force (non-interactive).
    let output = memex_cmd(&root, None)
        .args(["delete", "delete-me", "--force"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "delete should succeed, stderr: {stderr}"
    );
    assert!(
        stdout.contains("deleted:"),
        "expected 'deleted:' in output, got: {stdout}"
    );
    // File should be gone.
    assert!(
        !root.join("wiki/delete-me.md").exists(),
        "wiki page file should be deleted"
    );
}
#[test]
fn delete_not_found() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    std::fs::create_dir_all(root.join("wiki")).unwrap();

    let output = memex_cmd(&root, None)
        .args(["delete", "no-such-page", "--force"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "delete of nonexistent page should fail"
    );
    assert!(
        stderr.contains("not found:") || stderr.contains("no-such-page"),
        "expected error message mentioning missing page, got: {stderr}"
    );
}
#[test]
fn delete_removes_file_and_db_row() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write a page.
    let content = make_page("Ephemeral", "This page will be deleted.");
    let out = run_write(&root, "ephemeral", &content, &[]);
    assert!(out.status.success(), "write should succeed");

    // Verify the page exists on disk and is findable.
    assert!(root.join("wiki/ephemeral.md").exists());
    let read_out = memex_cmd(&root, None)
        .args(["read", "ephemeral"])
        .output()
        .unwrap();
    assert!(
        read_out.status.success(),
        "read should find the page before delete"
    );

    // Delete it.
    let del_out = memex_cmd(&root, None)
        .args(["delete", "ephemeral", "--force"])
        .output()
        .unwrap();
    let del_stdout = String::from_utf8_lossy(&del_out.stdout);
    let del_stderr = String::from_utf8_lossy(&del_out.stderr);
    assert!(
        del_out.status.success(),
        "delete should succeed, stderr: {del_stderr}"
    );
    assert!(
        del_stdout.contains("deleted:"),
        "expected 'deleted:' in output, got: {del_stdout}"
    );

    // File should be gone from disk.
    assert!(
        !root.join("wiki/ephemeral.md").exists(),
        "file should be deleted from disk"
    );

    // DB row should be gone — read should report not found.
    let read_after = memex_cmd(&root, None)
        .args(["read", "ephemeral"])
        .output()
        .unwrap();
    let read_stderr = String::from_utf8_lossy(&read_after.stderr);
    assert!(
        read_stderr.contains("not found"),
        "expected 'not found' after delete, got stderr: {read_stderr}"
    );
}
