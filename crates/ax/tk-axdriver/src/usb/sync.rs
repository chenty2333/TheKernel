//! Private runtime bridge for CrabUSB's upstream synchronization wrappers.
//!
//! Context changes use the kernel's existing guards; this does not install a
//! second scheduler or IRQ implementation. Lock metadata is unused because the
//! kernel does not provide upstream ax-sync's lockdep runtime.

use core::{
    panic::Location,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use ax_sync::interface::{
    AcquireResult, CONTEXT_IRQSAVE, CONTEXT_PREEMPT, CONTEXT_PREEMPT_IRQSAVE, CONTEXT_RAW,
    ContextOps, LOCK_MODE_READ, LOCK_MODE_WRITE, LockMetadata, RwLockOps, SpinOps,
};
use kernel_guard::{BaseGuard, IrqSave, NoPreempt, NoPreemptIrqSave};

// Host kernel_guard aliases use (), while bare-metal IRQ guards save usize.
trait Token: Copy {
    fn encode(self) -> usize;
    fn decode(value: usize) -> Self;
}
impl Token for () {
    fn encode(self) -> usize {
        0
    }
    fn decode(_: usize) -> Self {}
}
impl Token for usize {
    fn encode(self) -> usize {
        self
    }
    fn decode(value: usize) -> Self {
        value
    }
}
fn enter_guard<G: BaseGuard>() -> usize
where
    G::State: Token,
{
    G::acquire().encode()
}
fn exit_guard<G: BaseGuard>(state: usize)
where
    G::State: Token,
{
    G::release(G::State::decode(state));
}

struct UsbSync;
struct UsbSpin;
struct UsbRwLock;
#[ax_crate_interface::impl_interface]
impl ContextOps for UsbSync {
    fn enter(context: u8) -> usize {
        match context {
            CONTEXT_RAW => 0,
            CONTEXT_PREEMPT => enter_guard::<NoPreempt>(),
            CONTEXT_IRQSAVE => enter_guard::<IrqSave>(),
            CONTEXT_PREEMPT_IRQSAVE => enter_guard::<NoPreemptIrqSave>(),
            _ => panic!("invalid USB lock context"),
        }
    }
    fn exit(context: u8, state: usize) {
        match context {
            CONTEXT_RAW => {}
            CONTEXT_PREEMPT => exit_guard::<NoPreempt>(state),
            CONTEXT_IRQSAVE => exit_guard::<IrqSave>(state),
            CONTEXT_PREEMPT_IRQSAVE => exit_guard::<NoPreemptIrqSave>(state),
            _ => panic!("invalid USB lock context"),
        }
    }
    fn exit_preempt_from_irq_return(state: usize) {
        exit_guard::<NoPreempt>(state);
    }
}

// Restore context on a failed try operation (and on unwinding in host tests).
struct PendingContext {
    context: u8,
    state: usize,
    held: bool,
}
impl PendingContext {
    fn new(context: u8) -> Self {
        Self {
            context,
            state: UsbSync::enter(context),
            held: true,
        }
    }
    fn finish(mut self, acquired: bool) -> AcquireResult {
        if acquired {
            self.held = false;
            AcquireResult::new(true, self.state)
        } else {
            AcquireResult::new(false, 0)
        }
    }
}
impl Drop for PendingContext {
    fn drop(&mut self) {
        if self.held {
            UsbSync::exit(self.context, self.state);
        }
    }
}

#[ax_crate_interface::impl_interface]
impl SpinOps for UsbSpin {
    fn acquire(
        locked: &AtomicBool,
        _metadata: &LockMetadata,
        _lock_addr: usize,
        context: u8,
        _subclass: u32,
        is_try: bool,
        _caller: &'static Location<'static>,
    ) -> AcquireResult {
        let pending = PendingContext::new(context);
        loop {
            if locked
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return pending.finish(true);
            }
            if is_try {
                return pending.finish(false);
            }
            while locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
    }
    fn release(locked: &AtomicBool, _lock_addr: usize, context: u8, context_state: usize) {
        locked.store(false, Ordering::Release);
        UsbSync::exit(context, context_state);
    }
    fn force_release(locked: &AtomicBool, _lock_addr: usize, _context: u8) {
        // Upstream force-unlock deliberately leaves context restoration to caller.
        locked.store(false, Ordering::Release);
    }
    fn is_locked(locked: &AtomicBool) -> bool {
        locked.load(Ordering::Acquire)
    }
}

const WRITER: usize = 1 << (usize::BITS - 1);
const MAX_READERS: usize = 1 << (usize::BITS - 2);
fn try_read(state: &AtomicUsize) -> bool {
    let mut observed = state.load(Ordering::Relaxed);
    loop {
        if observed >= MAX_READERS {
            return false;
        }
        match state.compare_exchange_weak(
            observed,
            observed + 1,
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(current) => observed = current,
        }
    }
}
#[ax_crate_interface::impl_interface]
impl RwLockOps for UsbRwLock {
    fn acquire(
        state: &AtomicUsize,
        _metadata: &LockMetadata,
        _lock_addr: usize,
        context: u8,
        mode: u8,
        is_try: bool,
        _caller: &'static Location<'static>,
    ) -> AcquireResult {
        let pending = PendingContext::new(context);
        loop {
            let acquired = match mode {
                LOCK_MODE_READ => try_read(state),
                LOCK_MODE_WRITE => state
                    .compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok(),
                _ => panic!("invalid USB rwlock mode"),
            };
            if acquired || is_try {
                return pending.finish(acquired);
            }
            core::hint::spin_loop();
        }
    }
    fn release(
        state: &AtomicUsize,
        _lock_addr: usize,
        context: u8,
        context_state: usize,
        mode: u8,
    ) {
        match mode {
            LOCK_MODE_READ => {
                state.fetch_sub(1, Ordering::Release);
            }
            LOCK_MODE_WRITE => {
                state.store(0, Ordering::Release);
            }
            _ => panic!("invalid USB rwlock mode"),
        }
        UsbSync::exit(context, context_state);
    }
    fn force_read_decrement(state: &AtomicUsize, _lock_addr: usize, _context: u8) {
        let _ = state.try_update(Ordering::Release, Ordering::Relaxed, |value| {
            (value != 0 && value <= MAX_READERS).then(|| value - 1)
        });
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::{
        sync::{Arc, Barrier},
        thread,
    };

    use ax_sync::{SpinLock, SpinRwLock};

    use super::*;

    #[test]
    fn spin_excludes_other_owners_and_publishes_writes() {
        let lock = Arc::new(SpinLock::new(0));
        thread::scope(|scope| {
            for _ in 0..4 {
                let lock = &lock;
                scope.spawn(move || {
                    for _ in 0..1000 {
                        *lock.lock() += 1;
                    }
                });
            }
        });
        assert_eq!(*lock.lock(), 4000);
        let guard = lock.lock_irqsave();
        assert!(lock.try_lock_irqsave().is_none());
        drop(guard);
        assert!(lock.try_lock_irqsave().is_some());
    }

    #[test]
    fn blocking_reader_waits_for_writer_and_observes_update() {
        let lock = Arc::new(SpinRwLock::new(0));
        let barrier = Barrier::new(2);
        thread::scope(|scope| {
            let mut writer = lock.write();
            let reader = scope.spawn(|| {
                assert!(lock.try_read_irqsave().is_none());
                barrier.wait();
                assert_eq!(*lock.read(), 42);
            });
            barrier.wait();
            *writer = 42;
            drop(writer);
            reader.join().unwrap();
        });
        let first = lock.read();
        let second = lock.try_read().unwrap();
        assert!(lock.try_write().is_none());
        drop((first, second));
        assert!(lock.try_write().is_some());
    }

    #[test]
    fn reader_limit_and_force_decrement_preserve_writer() {
        let state = AtomicUsize::new(MAX_READERS);
        assert!(!try_read(&state));
        assert_eq!(state.load(Ordering::Relaxed), MAX_READERS);
        UsbRwLock::force_read_decrement(&state, 0, CONTEXT_RAW);
        assert!(try_read(&state));
        state.store(WRITER, Ordering::Relaxed);
        assert!(!try_read(&state));
        UsbRwLock::force_read_decrement(&state, 0, CONTEXT_RAW);
        assert_eq!(state.load(Ordering::Relaxed), WRITER);
    }
}
