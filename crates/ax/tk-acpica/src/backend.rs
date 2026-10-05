//! OS-specific facilities supplied by the caller; no scheduler/HAL dependency.
use core::{ffi::c_void, ptr, sync::atomic::{AtomicPtr, Ordering}};
use crate::{Status, SUPPORT};

pub type IrqHandler = unsafe extern "C" fn(*mut c_void) -> u32;
pub type Work = unsafe extern "C" fn(*mut c_void);
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct PciId { pub segment: u16, pub bus: u16, pub device: u16, pub function: u16 }

/// # Safety
/// Mappings must remain live until unmap; MMIO/PCI accesses must target only
/// admitted ranges. IRQ installation/removal must synchronize callbacks. Work
/// must be deferred (never run inline in interrupt context), and drained before
/// termination. Thread IDs are nonzero and stable while a task holds AML locks.
pub unsafe trait Backend: Sync {
    fn root_pointer(&self) -> u64;
    fn map(&self, address: u64, size: usize) -> *mut c_void;
    fn unmap(&self, address: *mut c_void, size: usize);
    fn physical_address(&self, _address: *mut c_void) -> Result<u64, Status> { Err(SUPPORT) }
    fn readable(&self, _address: *mut c_void, _size: usize) -> bool { false }
    fn writable(&self, _address: *mut c_void, _size: usize) -> bool { false }
    fn timer_100ns(&self) -> u64;
    fn sleep(&self, millis: u64) -> bool;
    fn stall(&self, micros: u32);
    fn thread_id(&self) -> u64;
    fn irq_save(&self) -> usize;
    fn irq_restore(&self, flags: usize);
    fn install_irq(&self, _irq: u32, _handler: IrqHandler, _context: *mut c_void) -> Status { SUPPORT }
    fn remove_irq(&self, _irq: u32, _handler: IrqHandler) -> Status { SUPPORT }
    fn execute(&self, _kind: u32, _function: Work, _context: *mut c_void) -> Status { SUPPORT }
    fn wait_events(&self);
    fn read_port(&self, _address: u16, _width: u32) -> Result<u32, Status> { Err(SUPPORT) }
    fn write_port(&self, _address: u16, _width: u32, _value: u32) -> Status { SUPPORT }
    fn read_pci(&self, _id: PciId, _reg: u32, _width: u32) -> Result<u64, Status> { Err(SUPPORT) }
    fn write_pci(&self, _id: PciId, _reg: u32, _width: u32, _value: u64) -> Status { SUPPORT }
    fn log(&self, message: &[u8]);
}

pub struct BackendRegistration(pub &'static dyn Backend);
static BACKEND: AtomicPtr<BackendRegistration> = AtomicPtr::new(ptr::null_mut());
/// # Safety
/// Install only with no live ACPICA instance; registration and mapped firmware
/// must outlive it. Replacement is rejected rather than silently swapping OSL.
pub unsafe fn install_backend(registration: &'static BackendRegistration) -> Result<(), Status> {
    BACKEND.compare_exchange(ptr::null_mut(), ptr::from_ref(registration).cast_mut(),
        Ordering::AcqRel, Ordering::Acquire).map(|_| ()).map_err(|_| crate::ALREADY_EXISTS)
}
pub(crate) fn backend() -> &'static dyn Backend {
    let p = BACKEND.load(Ordering::Acquire);
    assert!(!p.is_null(), "ACPICA used before backend installation");
    // SAFETY: install_backend publishes a static registration once.
    unsafe { (*p).0 }
}
