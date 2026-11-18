//! Lock seam: exclusive, non-blocking `flock` on the instance lock file.

use murmduck::session::lock::InstanceLock;

#[test]
fn only_one_holder_can_acquire_the_lock_at_a_time() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("instance.lock");

    let first = InstanceLock::try_acquire(&path)
        .expect("acquire succeeds")
        .expect("first caller becomes owner");
    let second = InstanceLock::try_acquire(&path).expect("attempt succeeds");
    assert!(second.is_none(), "a second holder must be refused");

    drop(first);
    let third = InstanceLock::try_acquire(&path).expect("attempt succeeds");
    assert!(third.is_some(), "lock is reusable after release");
}

#[test]
fn the_lock_file_is_never_deleted_on_release() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("instance.lock");

    {
        let _guard = InstanceLock::try_acquire(&path)
            .expect("acquire succeeds")
            .expect("owner");
        assert!(path.is_file(), "lock file exists while held");
    }

    assert!(path.is_file(), "lock file must survive release");
}

#[test]
fn acquiring_the_lock_creates_the_runtime_directory() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("murmduck").join("instance.lock");

    let guard = InstanceLock::try_acquire(&path)
        .expect("acquire succeeds")
        .expect("owner");
    assert!(path.is_file());
    drop(guard);
}
