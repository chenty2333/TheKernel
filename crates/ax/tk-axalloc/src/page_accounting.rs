//! Allocation-lifetime admission, independent of the caller's policy engine.
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::UsageKind;

/// A once-installed policy observer. `allocated` runs after physical
/// reservation, outside allocator locks, but before publication to the caller.
/// False must leave no charge: the allocator rolls its reservation back.
/// `deallocated` runs exactly once at actual frame return, not VMA removal.
/// Callbacks must not recursively allocate accounted kinds of pages.
pub struct PageAccountingHooks {
    pub allocated: fn(usize, usize, UsageKind) -> bool,
    pub deallocated: fn(usize, usize, UsageKind),
}

pub(crate) struct AccountingSlot(AtomicPtr<PageAccountingHooks>);
impl AccountingSlot {
    pub(crate) const fn new() -> Self {
        Self(AtomicPtr::new(core::ptr::null_mut()))
    }
    pub(crate) fn install(&self, hooks: &'static PageAccountingHooks) -> bool {
        self.0
            .compare_exchange(
                core::ptr::null_mut(),
                core::ptr::from_ref(hooks).cast_mut(),
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_ok()
    }
    fn get(&self, kind: UsageKind) -> Option<&'static PageAccountingHooks> {
        if !matches!(kind, UsageKind::VirtMem | UsageKind::PageCache) {
            return None;
        }
        let ptr = self.0.load(Ordering::Acquire);
        // SAFETY: only immutable static hooks are installed, once; they cannot
        // be replaced or reclaimed during any allocation lifetime.
        unsafe { ptr.as_ref() }
    }
    pub(crate) fn admit(&self, addr: usize, pages: usize, kind: UsageKind) -> bool {
        self.get(kind)
            .is_none_or(|hooks| (hooks.allocated)(addr, pages, kind))
    }
    pub(crate) fn retire(&self, addr: usize, pages: usize, kind: UsageKind) {
        if let Some(hooks) = self.get(kind) {
            (hooks.deallocated)(addr, pages, kind);
        }
    }
}
