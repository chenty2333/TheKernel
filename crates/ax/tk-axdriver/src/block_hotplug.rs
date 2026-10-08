//! Narrow one-way bridge for block devices discovered after boot enumeration.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::AxBlockDevice;

/// Runtime add callback installed by AXFS once its mutable block registry is
/// ready. Returning false keeps the controller's hotplug slot pending for a
/// later retry; callbacks must not hold PCI/controller locks while publishing.
pub type RuntimeBlockAddHook = fn(AxBlockDevice) -> bool;

static RUNTIME_BLOCK_ADD_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Installs one add hook; reinstalling the same function is idempotent.
pub fn install_runtime_block_add_hook(hook: RuntimeBlockAddHook) -> bool {
    match RUNTIME_BLOCK_ADD_HOOK.compare_exchange(
        0,
        hook as usize,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => true,
        Err(existing) => existing == hook as usize,
    }
}

/// Transfers ownership of a newly detected block device to the installed
/// runtime registry. False means no registry accepted the device.
pub fn publish_runtime_block_device(device: AxBlockDevice) -> bool {
    let address = RUNTIME_BLOCK_ADD_HOOK.load(Ordering::Acquire);
    if address == 0 {
        return false;
    }
    // SAFETY: the hook is a `fn` pointer installed once with this exact ABI.
    let hook = unsafe { core::mem::transmute::<usize, RuntimeBlockAddHook>(address) };
    hook(device)
}
