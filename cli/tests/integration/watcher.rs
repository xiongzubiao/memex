use memex_cli::daemon::watcher::{WatcherConfig, WatcherEvent, spawn_watcher};
use std::time::Duration;

#[tokio::test(flavor = "current_thread")]
async fn watcher_emits_touch_when_wiki_file_is_created() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("wiki")).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<WatcherEvent>(16);
    let _handle = spawn_watcher(
        WatcherConfig {
            wiki_dir: root.join("wiki"),
            raw_dir: root.join("raw"),
            poll_interval: Duration::from_secs(60),
            force_polling: false,
        },
        tx,
    )
    .unwrap();

    // Give the watcher time to register the directory.
    tokio::time::sleep(Duration::from_millis(200)).await;
    std::fs::write(root.join("wiki/p.md"), "x").unwrap();

    // Wait for an event (notify can take ~50ms-1s on Linux).
    let evt = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("watcher event timed out")
        .expect("watcher channel closed");
    match evt {
        WatcherEvent::Touch(path) => assert!(
            path.ends_with("p.md"),
            "expected Touch on p.md, got {}",
            path.display()
        ),
        other => panic!("expected Touch, got {other:?}"),
    }
}
