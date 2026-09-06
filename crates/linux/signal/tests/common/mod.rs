#![allow(dead_code)]

use std::{
    mem::MaybeUninit,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use thekernel_linux_signal::api::{
    ProcessSignalManager, SharedSignalActions, SignalActions, ThreadSignalManager,
    ThreadSignalRegistration,
};
use thekernel_linux_usercopy::{UserCopyError, UserMemory, VmResult};

static POOL: LazyLock<Arc<Mutex<Box<[u8]>>>> = LazyLock::new(|| {
    let size = 0x0100_0000; // 16 MiB
    Arc::new(Mutex::new(vec![0; size].into_boxed_slice()))
});

const STACK_SLOT_SIZE: usize = 0x1_0000;
static NEXT_STACK_SLOT: AtomicUsize = AtomicUsize::new(0);

pub fn initial_sp() -> usize {
    let pool = POOL.lock().unwrap();
    let slot = NEXT_STACK_SLOT.fetch_add(1, Ordering::Relaxed);
    let end = (slot + 1)
        .checked_mul(STACK_SLOT_SIZE)
        .expect("test stack slot overflow");
    assert!(end <= pool.len(), "signal test stack arena exhausted");
    pool.as_ptr() as usize + end
}

#[derive(Clone)]
pub struct Vm(Arc<Mutex<Box<[u8]>>>);

// SAFETY: this provider treats user addresses only as offsets into the fixed
// test arena, validates every complete range, and initializes every read byte.
unsafe impl UserMemory for Vm {
    fn read(&mut self, start: usize, buf: &mut [MaybeUninit<u8>]) -> VmResult {
        let pool = self.0.lock().unwrap();
        let base = pool.as_ptr() as usize;
        let offset = start.checked_sub(base).ok_or(UserCopyError::BadAddress)?;
        if offset
            .checked_add(buf.len())
            .ok_or(UserCopyError::BadAddress)?
            > pool.len()
        {
            return Err(UserCopyError::BadAddress);
        }
        let slice = &pool[offset..offset + buf.len()];
        for (dst, src) in buf.iter_mut().zip(slice) {
            dst.write(*src);
        }
        Ok(())
    }

    fn write(&mut self, start: usize, buf: &[u8]) -> VmResult {
        let mut pool = self.0.lock().unwrap();
        let base = pool.as_ptr() as usize;
        let offset = start.checked_sub(base).ok_or(UserCopyError::BadAddress)?;
        if offset
            .checked_add(buf.len())
            .ok_or(UserCopyError::BadAddress)?
            > pool.len()
        {
            return Err(UserCopyError::BadAddress);
        }
        let slice = &mut pool[offset..offset + buf.len()];
        slice.copy_from_slice(buf);
        Ok(())
    }
}

pub fn memory_provider() -> Vm {
    Vm(POOL.clone())
}

pub const TID: u32 = 7;

pub fn new_unregistered_test_env() -> (
    Arc<ProcessSignalManager>,
    Arc<ThreadSignalManager>,
    ThreadSignalRegistration,
) {
    let actions = SharedSignalActions::try_new(SignalActions::default()).unwrap();
    let proc = Arc::new(ProcessSignalManager::new(actions, 0));
    let thr = ThreadSignalManager::try_new(proc.clone()).unwrap();
    let registration = thr.try_register(TID).unwrap();
    (proc, thr, registration)
}

pub fn new_test_env() -> (Arc<ProcessSignalManager>, Arc<ThreadSignalManager>) {
    let (proc, thr, registration) = new_unregistered_test_env();
    registration.commit().unwrap();
    (proc, thr)
}
