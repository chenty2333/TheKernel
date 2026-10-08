//! Rootfs firmware lookup shared by drivers.
//!
//! The runtime installs a reader once the root filesystem is mounted and then
//! runs the registered rootfs-ready callbacks. Probe usually happens before
//! that point, so a driver that needs a firmware image registers a callback
//! with [`on_rootfs_ready`] and requests the image from there. Calls made
//! before the reader is installed return `None`.

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Reads at most `max_len` bytes of a rootfs file; `None` if it is missing,
/// unreadable or larger than `max_len`.
pub type FirmwareReader = fn(path: &str, max_len: usize) -> Option<Vec<u8>>;

const CALLBACK_SLOTS: usize = 32;

static READER: AtomicUsize = AtomicUsize::new(0);
static READY: AtomicBool = AtomicBool::new(false);
static CALLBACKS: [AtomicUsize; CALLBACK_SLOTS] = [const { AtomicUsize::new(0) }; CALLBACK_SLOTS];

/// Installs the rootfs reader. Called by the runtime after the root mount.
pub fn set_reader(reader: FirmwareReader) {
    READER.store(reader as usize, Ordering::Release);
}

/// Returns the contents of `/lib/firmware/...` style `path`, capped at `max_len`.
pub fn request(path: &str, max_len: usize) -> Option<Vec<u8>> {
    let raw = READER.load(Ordering::Acquire);
    if raw == 0 {
        return None;
    }
    // SAFETY: the only non-zero value ever stored is a `FirmwareReader`
    // converted by `set_reader`, and fn pointers round-trip through usize.
    let reader: FirmwareReader = unsafe { core::mem::transmute::<usize, FirmwareReader>(raw) };
    reader(path, max_len)
}

/// Whether the rootfs-ready callbacks have already run.
pub fn rootfs_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Registers `callback` to run once the reader is installed. If that already
/// happened, runs it immediately. Returns `false` when every slot is taken.
pub fn on_rootfs_ready(callback: fn()) -> bool {
    if rootfs_ready() {
        callback();
        return true;
    }
    for slot in &CALLBACKS {
        if slot
            .compare_exchange(0, callback as usize, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return true;
        }
    }
    false
}

/// Runs and clears the registered callbacks. Called once by the runtime after
/// [`set_reader`].
pub fn run_rootfs_ready() {
    READY.store(true, Ordering::Release);
    for slot in &CALLBACKS {
        let raw = slot.swap(0, Ordering::AcqRel);
        if raw != 0 {
            // SAFETY: slots only ever hold `fn()` values stored by
            // `on_rootfs_ready`.
            let callback: fn() = unsafe { core::mem::transmute::<usize, fn()>(raw) };
            callback();
        }
    }
}
