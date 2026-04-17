mod common;

use common::{assert_invariants, setup_temp_memex};
use memex_core::Memex;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

#[test]
fn two_writers_serialize() {
    let (_dir, root) = setup_temp_memex();
    let barrier = Arc::new(Barrier::new(2));
    let b1 = barrier.clone();
    let root1 = root.clone();

    let handle_a = std::thread::spawn(move || {
        let _memex = Memex::open_writer(root1).unwrap();
        b1.wait();
        std::thread::sleep(Duration::from_millis(200));
    });

    barrier.wait();
    let start = Instant::now();
    let _memex_b = Memex::open_writer(root.clone()).unwrap();
    assert!(start.elapsed() >= Duration::from_millis(150),
        "expected B to wait behind A, waited only {:?}", start.elapsed());
    handle_a.join().unwrap();
}

#[test]
fn writer_timeout_returns_correct_error() {
    let (_dir, root) = setup_temp_memex();
    let _holder = Memex::open_writer(root.clone()).unwrap();
    let result = Memex::open_writer_with_timeout(root, Duration::from_millis(100));
    assert!(matches!(result, Err(memex_core::error::MemexError::LockTimeout { .. })));
}

#[test]
fn concurrent_readers_ok_while_writer_holds() {
    let (_dir, root) = setup_temp_memex();
    let _writer = Memex::open_writer(root.clone()).unwrap();

    let mut handles = vec![];
    for _ in 0..10 {
        let r = root.clone();
        handles.push(std::thread::spawn(move || {
            let memex = Memex::open(r).unwrap();
            memex.search().search_collection("anything", "wiki", 5).unwrap()
        }));
    }
    for h in handles {
        let _ = h.join().unwrap();  // must not panic
    }
}

#[test]
fn stress_10_parallel_open_writer() {
    // 10 threads all race to Memex::open_writer; each takes the lock in turn.
    // Verifies lock serialization under high contention.
    let (_dir, root) = setup_temp_memex();
    let barrier = Arc::new(Barrier::new(10));
    let handles: Vec<_> = (0..10).map(|_| {
        let r = root.clone();
        let b = barrier.clone();
        std::thread::spawn(move || {
            b.wait();
            let _m = Memex::open_writer(r).unwrap();
            std::thread::sleep(Duration::from_millis(10));
        })
    }).collect();
    for h in handles { h.join().unwrap(); }

    let memex = Memex::open(root).unwrap();
    assert_invariants(&memex);
}

#[test]
fn lock_acquire_io_preserves_error_taxonomy() {
    // Permission-denied on lock creation → LockAcquireIo, not LockTimeout.
    let (_dir, root) = setup_temp_memex();
    let lock_path = root.join(".lock");
    // Create and chmod 000 so opening it for write fails.
    std::fs::write(&lock_path, "").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&lock_path).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&lock_path, perms).unwrap();
    }
    #[cfg(unix)]
    {
        let result = Memex::open_writer(root.clone());
        assert!(matches!(result, Err(memex_core::error::MemexError::LockAcquireIo { .. })),
            "expected LockAcquireIo, got {:?}", result);
        // Restore perms so TempDir can clean up.
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&lock_path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&lock_path, perms).unwrap();
    }
}
