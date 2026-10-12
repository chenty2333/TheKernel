//! Bounded work multiplexed by the runtime-owned CallFunction IPI lane.
use core::sync::atomic::{AtomicUsize, Ordering};

const TIMER: usize = 1;
const EXTENSION: usize = 2;

pub(crate) struct CallFunctionWork<const N: usize> {
    pending: [AtomicUsize; N],
    extension: AtomicUsize,
}

impl<const N: usize> CallFunctionWork<N> {
    pub(crate) const fn new() -> Self {
        Self {
            pending: [const { AtomicUsize::new(0) }; N],
            extension: AtomicUsize::new(0),
        }
    }

    pub(crate) fn register_extension(&self, handler: fn()) -> bool {
        let pointer = handler as usize;
        match self.extension.compare_exchange(0, pointer, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => true,
            Err(installed) => installed == pointer,
        }
    }

    pub(crate) fn publish_timer(&self, cpu: usize) {
        self.pending[cpu].fetch_or(TIMER, Ordering::Release);
    }

    pub(crate) fn publish_extension(&self, cpu: usize) -> bool {
        if self.extension.load(Ordering::Acquire) == 0 {
            return false;
        }
        self.pending[cpu].fetch_or(EXTENSION, Ordering::Release);
        true
    }

    pub(crate) fn dispatch(&self, cpu: usize, timer: fn()) -> Result<(), usize> {
        let pending = self.pending[cpu].swap(0, Ordering::AcqRel);
        let unknown = pending & !(TIMER | EXTENSION);
        if unknown != 0 {
            return Err(unknown);
        }
        if pending & TIMER != 0 {
            timer();
        }
        if pending & EXTENSION != 0 {
            let pointer = self.extension.load(Ordering::Acquire);
            assert_ne!(pointer, 0, "unregistered call-function extension");
            // SAFETY: registration accepts only a resident fn() pointer and
            // never replaces it. Publication acquires that registration first.
            let handler: fn() = unsafe { core::mem::transmute(pointer) };
            handler();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_and_extension_coalesce_without_consuming_each_others_work() {
        static TIMER_CALLS: AtomicUsize = AtomicUsize::new(0);
        static DRIVER_CALLS: AtomicUsize = AtomicUsize::new(0);
        fn timer() { TIMER_CALLS.fetch_add(1, Ordering::SeqCst); }
        fn driver() { DRIVER_CALLS.fetch_add(1, Ordering::SeqCst); }
        let work = CallFunctionWork::<2>::new();
        assert!(work.register_extension(driver));
        work.publish_timer(0);
        work.dispatch(0, timer).unwrap();
        assert_eq!(DRIVER_CALLS.load(Ordering::SeqCst), 0);
        for _ in 0..2 {
            work.publish_timer(1);
            assert!(work.publish_extension(1));
        }
        work.dispatch(1, timer).unwrap();
        work.dispatch(1, timer).unwrap();
        assert_eq!(TIMER_CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(DRIVER_CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn extension_registration_is_idempotent_and_rejects_other_owners() {
        #[inline(never)] fn first() { core::hint::black_box(1); }
        #[inline(never)] fn second() { core::hint::black_box(2); }
        let work = CallFunctionWork::<1>::new();
        assert!(!work.publish_extension(0));
        assert!(work.register_extension(first));
        assert!(work.register_extension(first));
        assert!(!work.register_extension(second));
    }

    #[test]
    fn unknown_work_is_rejected_before_running_callbacks() {
        let work = CallFunctionWork::<1>::new();
        work.pending[0].store(0x80 | TIMER, Ordering::Release);
        assert_eq!(work.dispatch(0, || panic!("invalid work ran")), Err(0x80));
    }

    #[test]
    fn panic_halt_is_explicit_and_does_not_change_default_shutdown() {
        assert!(crate::panic_halt_requested(Some("halt")));
        for value in [None, Some("0"), Some("1"), Some("invalid")] {
            assert!(!crate::panic_halt_requested(value));
        }
    }
}
