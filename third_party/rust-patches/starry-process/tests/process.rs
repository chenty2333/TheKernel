use std::sync::Arc;

mod common;
use common::{ProcessExt, alloc_pid, init, test_lock};

#[test]
fn child() {
    let _guard = test_lock();
    let parent = init();
    let child = parent.new_child();
    assert!(Arc::ptr_eq(&parent, &child.parent().unwrap()));
    assert!(
        parent
            .try_children()
            .unwrap()
            .iter()
            .any(|candidate| Arc::ptr_eq(candidate, &child))
    );
    child.exit_and_reap();
}

#[test]
fn exit() {
    let _guard = test_lock();
    let parent = init();
    let child = parent.new_child();
    child.exit(drop);
    assert!(child.is_zombie());
    assert!(
        parent
            .try_children()
            .unwrap()
            .iter()
            .any(|candidate| Arc::ptr_eq(candidate, &child))
    );
    assert!(child.reap());
}

#[test]
fn reap_not_zombie_is_rejected() {
    let _guard = test_lock();
    let child = init().new_child();
    assert!(!child.reap());
    child.exit_and_reap();
}

#[test]
fn reap() {
    let _guard = test_lock();
    let parent = init().new_child();
    let child = parent.new_child();
    child.exit(drop);
    assert!(child.reap());
    assert!(parent.try_children().unwrap().is_empty());
    parent.exit_and_reap();
}

#[test]
fn reparent() {
    let _guard = test_lock();
    let init = init();
    let parent = init.new_child();
    let child = parent.new_child();

    parent.exit(drop);
    assert!(Arc::ptr_eq(&init, &child.parent().unwrap()));
    child.exit_and_reap();
    assert!(parent.reap());
}

#[test]
fn thread_exit() {
    let _guard = test_lock();
    let child = init().new_child();
    let tid1 = alloc_pid();
    let tid2 = alloc_pid();
    child.prepare_thread(tid1).unwrap().commit();
    child.prepare_thread(tid2).unwrap().commit();

    let mut threads = child.try_threads().unwrap();
    threads.sort_unstable();
    assert_eq!(threads, vec![tid1, tid2]);

    assert!(!child.exit_thread(tid1, 7));
    assert_eq!(child.exit_code(), 7);
    child.group_exit();
    assert!(child.is_group_exited());
    assert!(child.exit_thread(tid2, 3));
    assert_eq!(child.exit_code(), 7);
    child.exit_and_reap();
}
