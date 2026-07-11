use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock,
    atomic::{AtomicU32, Ordering},
};

use starry_process::Process;

static NEXT_PID: AtomicU32 = AtomicU32::new(1);
static INIT: OnceLock<Arc<Process>> = OnceLock::new();
static TEST_LOCK: Mutex<()> = Mutex::new(());

pub fn alloc_pid() -> u32 {
    NEXT_PID.fetch_add(1, Ordering::Relaxed)
}

pub fn init() -> Arc<Process> {
    INIT.get_or_init(|| Process::try_new_init(alloc_pid(), None).unwrap())
        .clone()
}

pub fn test_lock() -> MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

pub trait ProcessExt {
    fn new_child(&self) -> Arc<Process>;
    fn exit_and_reap(&self);
}

impl ProcessExt for Arc<Process> {
    fn new_child(&self) -> Arc<Process> {
        let admission = self.prepare_fork(alloc_pid(), None).unwrap();
        let child = admission.process().clone();
        admission.commit();
        child
    }

    fn exit_and_reap(&self) {
        if !self.is_zombie() {
            self.exit(drop);
        }
        assert!(self.reap());
    }
}
