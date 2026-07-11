use std::sync::Arc;

mod common;
use common::{ProcessExt, init, test_lock};

#[test]
fn basic() {
    let _guard = test_lock();
    let init = init();
    let group = init.group();
    assert_eq!(group.pgid(), init.pid());

    let child = init.new_child();
    assert!(Arc::ptr_eq(&group, &child.group()));

    let processes = group.try_processes().unwrap();
    assert!(processes.iter().any(|process| Arc::ptr_eq(process, &init)));
    assert!(processes.iter().any(|process| Arc::ptr_eq(process, &child)));
    child.exit_and_reap();
}

#[test]
fn create() {
    let _guard = test_lock();
    let parent = init();
    let child = parent.new_child();
    let child_group = child.try_create_group().unwrap().unwrap();

    assert!(Arc::ptr_eq(&child_group, &child.group()));
    assert_eq!(child_group.pgid(), child.pid());
    let processes = child_group.try_processes().unwrap();
    assert_eq!(processes.len(), 1);
    assert!(Arc::ptr_eq(&processes[0], &child));
    assert!(
        parent
            .group()
            .try_processes()
            .unwrap()
            .iter()
            .all(|process| !Arc::ptr_eq(process, &child))
    );
    child.exit_and_reap();
}

#[test]
fn create_leader() {
    let _guard = test_lock();
    let init = init();
    let group = init.group();
    assert!(init.try_create_group().unwrap().is_none());
    assert!(Arc::ptr_eq(&group, &init.group()));
}

#[test]
fn cleanup() {
    let _guard = test_lock();
    let child = init().new_child();
    let group = Arc::downgrade(&child.try_create_group().unwrap().unwrap());
    assert!(group.upgrade().is_some());

    child.exit_and_reap();
    drop(child);
    assert!(group.upgrade().is_none());
}

#[test]
fn inherit() {
    let _guard = test_lock();
    let parent = init().new_child();
    let group = parent.try_create_group().unwrap().unwrap();
    let child = parent.new_child();

    assert!(Arc::ptr_eq(&group, &child.group()));
    assert_eq!(group.try_processes().unwrap().len(), 2);
    child.exit_and_reap();
    parent.exit_and_reap();
}

#[test]
fn move_to() {
    let _guard = test_lock();
    let parent = init();
    let child1 = parent.new_child();
    let child1_group = child1.try_create_group().unwrap().unwrap();
    assert!(child1.move_to_group(&child1.group()));

    let child2 = parent.new_child();
    let child2_group = child2.try_create_group().unwrap().unwrap();
    assert!(child2.move_to_group(&child1_group));
    assert!(Arc::ptr_eq(&child1_group, &child2.group()));

    let processes = child1_group.try_processes().unwrap();
    assert_eq!(processes.len(), 2);
    assert!(
        processes
            .iter()
            .any(|process| Arc::ptr_eq(process, &child1))
    );
    assert!(
        processes
            .iter()
            .any(|process| Arc::ptr_eq(process, &child2))
    );
    assert!(child2_group.try_processes().unwrap().is_empty());

    child2.exit_and_reap();
    child1.exit_and_reap();
}

#[test]
fn move_cleanup() {
    let _guard = test_lock();
    let parent = init();
    let group = parent.group();
    let child = parent.new_child();
    let child_group = Arc::downgrade(&child.try_create_group().unwrap().unwrap());

    assert!(child_group.upgrade().is_some());
    assert!(child.move_to_group(&group));
    assert!(child_group.upgrade().is_none());
    child.exit_and_reap();
}

#[test]
fn move_back() {
    let _guard = test_lock();
    let parent = init();
    let group = parent.group();
    let child = parent.new_child();
    let child_group = child.try_create_group().unwrap().unwrap();

    assert!(child.move_to_group(&group));
    assert!(child.move_to_group(&child_group));
    assert!(Arc::ptr_eq(&child_group, &child.group()));
    assert!(
        child_group
            .try_processes()
            .unwrap()
            .iter()
            .any(|process| Arc::ptr_eq(process, &child))
    );
    child.exit_and_reap();
}

#[test]
fn cleanup_processes() {
    let _guard = test_lock();
    let parent = init().new_child();
    let group = parent.try_create_group().unwrap().unwrap();

    parent.exit_and_reap();
    drop(parent);
    assert!(group.try_processes().unwrap().is_empty());
}
