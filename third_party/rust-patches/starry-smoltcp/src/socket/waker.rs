use core::task::Waker;

/// Utility struct to register and wake a waker.
#[derive(Debug)]
pub struct WakerRegistration {
    waker: Option<Waker>,
}

impl WakerRegistration {
    pub const fn new() -> Self {
        Self { waker: None }
    }

    /// Register a waker. Overwrites the previous waker, if any.
    pub fn register(&mut self, w: &Waker) {
        match self.waker {
            // Optimization: If both the old and new Wakers wake the same task, we can simply
            // keep the old waker, skipping the clone. (In most executor implementations,
            // cloning a waker is somewhat expensive, comparable to cloning an Arc).
            Some(ref w2) if (w2.will_wake(w)) => {}
            // In all other cases
            // - we have no waker registered
            // - we have a waker registered but it's for a different task.
            // then clone the new waker and store it
            _ => self.waker = Some(w.clone()),
        }
    }

    /// Wake the registered waker, if any.
    pub fn wake(&mut self) {
        self.waker.take().map(|w| w.wake());
    }

    /// Drop the registered waker without invoking it.
    pub fn clear(&mut self) {
        // A `Waker` owns its cloned task reference.  Clearing a registration
        // must release that reference exactly once; leaking it to hide a bad
        // vtable would turn memory corruption into an unbounded resource leak.
        // Callers that need deferred destruction must take the registration
        // outside their lock rather than weakening this ownership contract.
        self.waker = None;
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, task::Wake};
    use core::task::Waker;

    use super::WakerRegistration;

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    #[test]
    fn clear_releases_the_registered_waker_reference() {
        let task = Arc::new(NoopWake);
        let waker = Waker::from(task.clone());
        let mut registration = WakerRegistration::new();

        registration.register(&waker);
        assert_eq!(Arc::strong_count(&task), 3);

        registration.clear();
        assert_eq!(Arc::strong_count(&task), 2);
    }
}
