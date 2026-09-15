//! IRQ-safe stack listeners shared by blocking synchronization primitives.

use core::{future::Future, mem::ManuallyDrop, pin::Pin};

use event_listener::{__private::StackSlot, Event};
use kernel_guard::NoPreemptIrqSave;

// The no_std Event backend uses a plain spinlock, including during listener
// insertion/removal. An IRQ may notify this same queue, so every operation
// touching its listener list must exclude local IRQs and preemption. Keep
// ownership of the pinned slot here so cancellation also removes it guarded.
/// Owns a stack listener with IRQ-safe registration, polling and destruction.
pub struct IrqSafeListenerSlot<'ev> {
    slot: ManuallyDrop<StackSlot<'ev, ()>>,
}

impl<'ev> IrqSafeListenerSlot<'ev> {
    /// Creates an unregistered slot; pin it before calling `listen`.
    pub fn new(event: &'ev Event) -> Self {
        let _guard = NoPreemptIrqSave::new();
        Self {
            slot: ManuallyDrop::new(StackSlot::new(event)),
        }
    }

    /// Registers a listener whose polling excludes local IRQs and preemption.
    pub fn listen(self: Pin<&mut Self>) -> impl Future<Output = ()> + Unpin + '_ {
        let _guard = NoPreemptIrqSave::new();
        // SAFETY: the slot is structurally pinned with this owner and is never
        // moved again, including during its guarded in-place destruction.
        let mut listener =
            unsafe { Pin::new_unchecked(&mut *self.get_unchecked_mut().slot) }.listen();
        core::future::poll_fn(move |cx| {
            let _guard = NoPreemptIrqSave::new();
            Pin::new(&mut listener).poll(cx)
        })
    }
}

impl Drop for IrqSafeListenerSlot<'_> {
    fn drop(&mut self) {
        let _guard = NoPreemptIrqSave::new();
        // SAFETY: this is the sole destruction of the slot; ManuallyDrop keeps
        // its listener removal inside the IRQ exclusion boundary.
        unsafe { ManuallyDrop::drop(&mut self.slot) };
    }
}
