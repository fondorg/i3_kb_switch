//! Integration test for the flock-based single-instance guard.

use std::fs;
use std::path::PathBuf;

use i3_kb_switch::daemon::SingleInstance;

/// Unique temporary directory with the lock file path inside.
fn temp_lock_path() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("i3_kb_switch_flock_{}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir creation must succeed");
    dir.join("lock")
}

#[test]
fn flock_second_acquire_fails() {
    let lock = temp_lock_path();

    // First acquire: the lock is taken.
    let first = SingleInstance::acquire(&lock);
    assert!(first.is_some(), "first acquire must take the lock");

    // Second acquire while the guard is alive: rejected.
    assert!(
        SingleInstance::acquire(&lock).is_none(),
        "second acquire must fail while the first guard holds the lock"
    );

    // After the guard is dropped the lock is free again.
    drop(first);
    let again = SingleInstance::acquire(&lock);
    assert!(
        again.is_some(),
        "acquire must succeed after the guard is dropped"
    );

    let _ = fs::remove_dir_all(lock.parent().expect("lock path has a parent"));
}
