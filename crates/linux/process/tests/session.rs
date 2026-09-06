use std::{any::Any, sync::Arc};

use thekernel_linux_process::ProcessError;

mod common;
use common::{child, domain, exit_and_reap, init};

#[test]
fn session_groups_are_registry_scoped_and_deduplicated() {
    let process_domain = domain();
    let other_domain = domain();
    let init_process = init(&process_domain);
    let _other_init = init(&other_domain);
    let session = init_process.group().session();
    let child = child(&process_domain, &init_process, 2);
    process_domain
        .prepare_thread(&child, 2)
        .unwrap()
        .commit()
        .unwrap();
    let child_group = process_domain.try_create_group(&child).unwrap().unwrap();

    let groups = session
        .try_process_groups(process_domain.registry())
        .unwrap();
    assert_eq!(groups.len(), 2);
    assert!(groups.iter().any(|group| Arc::ptr_eq(group, &child_group)));
    assert_eq!(
        session.try_process_groups(other_domain.registry()).err(),
        Some(ProcessError::WrongDomain)
    );
    assert!(child.remove_thread(2));
    exit_and_reap(&process_domain, &child);
}

#[test]
fn terminal_installation_is_identity_checked() {
    let process_domain = domain();
    let session = init(&process_domain).group().session();
    let terminal: Arc<dyn Any + Send + Sync> = Arc::new(0_u32);
    let other: Arc<dyn Any + Send + Sync> = Arc::new(1_u32);

    assert!(session.set_terminal_with(|| terminal.clone()));
    assert!(!session.set_terminal_with(|| other.clone()));
    assert!(Arc::ptr_eq(&session.terminal().unwrap(), &terminal));
    assert!(!session.unset_terminal(&other));
    assert!(session.unset_terminal(&terminal));
    assert!(session.terminal().is_none());
}
