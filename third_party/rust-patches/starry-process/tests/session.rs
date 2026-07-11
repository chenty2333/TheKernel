use std::{any::Any, sync::Arc};

mod common;
use common::{ProcessExt, init, test_lock};

#[test]
fn basic() {
    let _guard = test_lock();
    let init = init();
    let group = init.group();
    let session = group.session();

    assert_eq!(group.pgid(), init.pid());
    assert_eq!(session.sid(), init.pid());
    assert!(
        session
            .try_process_groups()
            .unwrap()
            .iter()
            .any(|candidate| Arc::ptr_eq(candidate, &group))
    );
}

#[test]
fn create() {
    let _guard = test_lock();
    let parent = init();
    let group = parent.group();
    let session = group.session();
    let child = parent.new_child();
    let (child_session, child_group) = child.try_create_session().unwrap().unwrap();

    assert_eq!(child_group.pgid(), child.pid());
    assert_eq!(child_session.sid(), child.pid());
    assert!(Arc::ptr_eq(&child_group, &child.group()));
    assert!(Arc::ptr_eq(&child_session, &child_group.session()));
    assert_eq!(child_group.try_processes().unwrap().len(), 1);
    assert_eq!(child_session.try_process_groups().unwrap().len(), 1);
    assert!(
        group
            .try_processes()
            .unwrap()
            .iter()
            .all(|process| !Arc::ptr_eq(process, &child))
    );
    assert!(
        session
            .try_process_groups()
            .unwrap()
            .iter()
            .all(|candidate| !Arc::ptr_eq(candidate, &child_group))
    );
    child.exit_and_reap();
}

#[test]
fn create_leader() {
    let _guard = test_lock();
    assert!(init().try_create_session().unwrap().is_none());
}

#[test]
fn cleanup() {
    let _guard = test_lock();
    let child = init().new_child();
    let session = {
        let (session, _) = child.try_create_session().unwrap().unwrap();
        Arc::downgrade(&session)
    };

    assert!(session.upgrade().is_some());
    child.exit_and_reap();
    drop(child);
    assert!(session.upgrade().is_none());
}

#[test]
fn create_group() {
    let _guard = test_lock();
    let parent = init();
    let session = parent.group().session();
    let child = parent.new_child();
    let child_group = child.try_create_group().unwrap().unwrap();

    assert!(Arc::ptr_eq(&child_group.session(), &session));
    assert!(
        session
            .try_process_groups()
            .unwrap()
            .iter()
            .any(|candidate| Arc::ptr_eq(candidate, &child_group))
    );
    child.exit_and_reap();
}

#[test]
fn move_to_different_session() {
    let _guard = test_lock();
    let parent = init().new_child();
    let child = parent.new_child();
    let (session, group) = parent.try_create_session().unwrap().unwrap();

    assert!(!Arc::ptr_eq(&group, &child.group()));
    assert!(!Arc::ptr_eq(&session, &child.group().session()));
    assert!(!child.move_to_group(&group));
    child.exit_and_reap();
    parent.exit_and_reap();
}

#[test]
fn cleanup_groups() {
    let _guard = test_lock();
    let child = init().new_child();
    let (session, _) = child.try_create_session().unwrap().unwrap();

    child.exit_and_reap();
    drop(child);
    assert!(session.try_process_groups().unwrap().is_empty());
}

#[test]
fn terminal_set_unset() {
    let _guard = test_lock();
    let session = init().group().session();
    let terminal: Arc<dyn Any + Send + Sync> = Arc::new(0_u32);

    assert!(session.set_terminal_with(|| terminal.clone()));
    assert!(!session.set_terminal_with(|| Arc::new(1_u32)));

    let got = session.terminal().unwrap();
    assert!(Arc::ptr_eq(&got, &terminal));
    let other: Arc<dyn Any + Send + Sync> = Arc::new(2_u32);
    assert!(!session.unset_terminal(&other));
    assert!(session.unset_terminal(&terminal));
    assert!(session.terminal().is_none());
}
