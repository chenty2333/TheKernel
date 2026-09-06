#![allow(dead_code)]

use std::sync::Arc;

use thekernel_linux_process::{Process, ProcessDomain};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Zombie {
    pub wait_status: i32,
    pub uid: u32,
    pub user_ns_cookie: u64,
}

pub fn zombie(pid: u32) -> Arc<Zombie> {
    Arc::new(Zombie {
        wait_status: pid as i32,
        uid: pid,
        user_ns_cookie: 1,
    })
}

pub fn domain() -> ProcessDomain<Zombie> {
    ProcessDomain::try_new().unwrap()
}

pub fn init(domain: &ProcessDomain<Zombie>) -> Arc<Process<Zombie>> {
    domain.try_new_init(1, None).unwrap()
}

pub fn child(
    domain: &ProcessDomain<Zombie>,
    parent: &Arc<Process<Zombie>>,
    pid: u32,
) -> Arc<Process<Zombie>> {
    let admission = domain.prepare_fork(parent, pid, Some(17)).unwrap();
    let child = admission.process().clone();
    admission.commit();
    child
}

pub fn exit_and_reap(domain: &ProcessDomain<Zombie>, process: &Arc<Process<Zombie>>) {
    if !process.is_zombie() {
        domain.exit(process, zombie(process.pid()), drop).unwrap();
    }
    assert!(domain.reap(process).unwrap());
}
