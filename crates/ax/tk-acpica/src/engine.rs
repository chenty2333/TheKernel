//! Owned public API; the global ACPICA instance has one lifecycle owner.
use alloc::{ffi::CString, string::String, vec::Vec};
use core::{
    ffi::{CStr, c_char, c_void},
    marker::PhantomData,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crate::{
    BAD_PARAMETER, LIMIT, NO_MEMORY, OK, Status,
    backend::{BackendRegistration, install_backend},
};
static LIVE: AtomicBool = AtomicBool::new(false);
static NOTIFY: AtomicUsize = AtomicUsize::new(0);
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Null,
    Integer(u64),
    String(Vec<u8>),
    Buffer(Vec<u8>),
    Package(Vec<Value>),
    Reference(Vec<u8>),
}
#[derive(Clone, Debug)]
pub struct Node {
    pub path: String,
    pub kind: u32,
}
#[derive(Clone, Copy, Debug)]
pub enum Mode {
    Hardware,
    Offline,
}
pub struct Engine {
    _not_send_sync: PhantomData<*mut ()>,
    notify: bool,
}
unsafe extern "C" {
    fn AcpiInitializeSubsystem() -> Status;
    fn AcpiInitializeTables(storage: *mut c_void, count: u32, resize: u8) -> Status;
    fn AcpiLoadTables() -> Status;
    fn AcpiEnableSubsystem(flags: u32) -> Status;
    fn AcpiInitializeObjects(flags: u32) -> Status;
    fn AcpiTerminate() -> Status;
    fn AcpiUpdateAllGpes() -> Status;
    fn AcpiDisableAllGpes() -> Status;
    fn AcpiEnterSleepStatePrep(state: u8) -> Status;
    fn AcpiEnterSleepState(state: u8) -> Status;
    fn tk_acpi_evaluate(
        path: *const c_char,
        args: *const u64,
        argc: u32,
        out: *mut u8,
        capacity: usize,
        used: *mut usize,
    ) -> Status;
    fn tk_acpi_walk(
        callback: unsafe extern "C" fn(*const c_char, u32, *mut c_void) -> Status,
        context: *mut c_void,
    ) -> Status;
    fn tk_acpi_install_notify() -> Status;
    fn tk_acpi_remove_notify();
    fn tk_acpi_validate_resources(path: *const c_char, possible: u8) -> Status;
    fn tk_acpi_platform_osc() -> Status;
}
pub fn status(status: Status) -> Result<(), Status> {
    if status == OK { Ok(()) } else { Err(status) }
}
impl Engine {
    /// # Safety
    /// Caller owns ACPI hardware, SCI and mappings and has initialized allocation,
    /// scheduling and deferred work. Firmware AML may write hardware. Offline
    /// mode requires a backend that simulates *all* hardware accesses.
    pub unsafe fn initialize(
        registration: &'static BackendRegistration,
        mode: Mode,
    ) -> Result<Self, Status> {
        LIVE.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| crate::ALREADY_EXISTS)?;
        // SAFETY: global lifecycle claim excludes another engine.
        if let Err(e) = unsafe { install_backend(registration) } {
            LIVE.store(false, Ordering::Release);
            return Err(e);
        }
        let engine = Self {
            _not_send_sync: PhantomData,
            notify: false,
        };
        // SAFETY: caller established the complete OSL contract; shutdown on error.
        unsafe {
            status(AcpiInitializeSubsystem())?;
            status(AcpiInitializeTables(core::ptr::null_mut(), 32, 1))?;
            status(AcpiLoadTables())?;
            let flags = match mode {
                Mode::Hardware => 0,
                Mode::Offline => 0x1 | 0x2 | 0x4 | 0x8 | 0x10,
            };
            status(AcpiEnableSubsystem(flags))?;
        }
        Ok(engine)
    }
    /// Run _REG/_STA/_INI after custom handlers (notably EC) are installed.
    pub fn initialize_objects(&self) -> Result<(), Status> {
        // SAFETY: live instance, OSL is ready and hardware mode admits AML.
        unsafe { status(AcpiInitializeObjects(0)) }
    }
    pub fn evaluate(&self, path: &str, args: &[u64]) -> Result<Value, Status> {
        let path = CString::new(path).map_err(|_| BAD_PARAMETER)?;
        if args.len() > 4 {
            return Err(BAD_PARAMETER);
        }
        // Bounded output; do not retry AML methods on buffer overflow, because
        // repeating a method could repeat hardware side effects.
        let mut wire = Vec::new();
        wire.try_reserve_exact(1024 * 1024).map_err(|_| NO_MEMORY)?;
        wire.resize(1024 * 1024, 0);
        let mut used = 0;
        // SAFETY: all pointers refer to valid slices throughout the C call.
        unsafe {
            status(tk_acpi_evaluate(
                path.as_ptr(),
                args.as_ptr(),
                args.len() as u32,
                wire.as_mut_ptr(),
                wire.len(),
                &mut used,
            ))?;
        }
        wire.truncate(used);
        if wire.is_empty() {
            return Ok(Value::Null);
        }
        let mut cursor = 0;
        let value = decode(&wire, &mut cursor, 0)?;
        if cursor != wire.len() {
            return Err(BAD_PARAMETER);
        }
        Ok(value)
    }
    pub fn hardware_id(&self, path: &str) -> Result<String, Status> {
        match self.evaluate(&alloc::format!("{path}._HID"), &[])? {
            Value::String(s) => String::from_utf8(s).map_err(|_| BAD_PARAMETER),
            Value::Integer(v) => {
                let v = (v as u32).swap_bytes();
                let letters = [
                    (((v >> 26) & 31) as u8) + b'@',
                    (((v >> 21) & 31) as u8) + b'@',
                    (((v >> 16) & 31) as u8) + b'@',
                ];
                if !letters.iter().all(u8::is_ascii_uppercase) {
                    return Err(BAD_PARAMETER);
                }
                Ok(alloc::format!(
                    "{}{:04X}",
                    core::str::from_utf8(&letters).unwrap(),
                    v & 0xffff
                ))
            }
            _ => Err(0x1003),
        }
    }
    pub fn integer(&self, path: &str) -> Result<u64, Status> {
        match self.evaluate(path, &[])? {
            Value::Integer(v) => Ok(v),
            _ => Err(0x1003),
        }
    }
    pub fn namespace(&self) -> Result<Vec<Node>, Status> {
        let mut nodes = Vec::new();
        // SAFETY: synchronous callback borrows nodes only until walk returns.
        unsafe {
            status(tk_acpi_walk(
                walk_node,
                (&mut nodes as *mut Vec<Node>).cast(),
            ))?;
        }
        Ok(nodes)
    }
    /// Validated _CRS/_PRS returned as owned AML resource bytes (never native
    /// ACPICA resource pointers). Consumers must use a checked resource parser.
    pub fn resources(&self, path: &str, possible: bool) -> Result<Vec<u8>, Status> {
        let name = CString::new(path).map_err(|_| BAD_PARAMETER)?;
        // SAFETY: C validator releases its own native resource allocation.
        unsafe {
            status(tk_acpi_validate_resources(name.as_ptr(), possible.into()))?;
        }
        let method = alloc::format!("{path}.{}", if possible { "_PRS" } else { "_CRS" });
        match self.evaluate(&method, &[])? {
            Value::Buffer(v) => Ok(v),
            _ => Err(0x1003),
        }
    }
    /// Install the one root Notify observer. Callback runs on deferred OSL work,
    /// must not panic, and must not retain its temporary pathname reference.
    pub fn install_notify(&mut self, callback: fn(&str, u32)) -> Result<(), Status> {
        if self.notify {
            return Err(crate::ALREADY_EXISTS);
        }
        NOTIFY.store(callback as usize, Ordering::Release);
        // SAFETY: root observer is static; owner removes and drains on drop.
        if let Err(e) = unsafe { status(tk_acpi_install_notify()) } {
            NOTIFY.store(0, Ordering::Release);
            return Err(e);
        }
        self.notify = true;
        Ok(())
    }
    pub fn update_gpes(&self) -> Result<(), Status> {
        // SAFETY: invoked after all namespace/custom handlers are initialized.
        unsafe { status(AcpiUpdateAllGpes()) }
    }
    pub fn disable_gpes(&self) -> Result<(), Status> {
        // SAFETY: ACPICA synchronizes event state against its SCI handler.
        unsafe { status(AcpiDisableAllGpes()) }
    }
    pub fn platform_osc(&self) -> Result<(), Status> {
        // SAFETY: original C bridge supplies correctly typed _OSC arguments.
        unsafe { status(tk_acpi_platform_osc()) }
    }
    pub fn prepare_s5(&self) -> Result<(), Status> {
        // SAFETY: live engine; call in task context before IRQs are disabled.
        unsafe { status(AcpiEnterSleepStatePrep(5)) }
    }
    /// # Safety
    /// Final power transition only, with IRQs disabled and filesystem flush done.
    pub unsafe fn enter_s5(&self) -> Result<(), Status> {
        // SAFETY: caller owns the final transition.
        unsafe { status(AcpiEnterSleepState(5)) }
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: exclusive lifecycle owner; terminate drains deferred callbacks
        // and removes the SCI handler before deleting interpreter locks/caches.
        unsafe {
            if self.notify {
                tk_acpi_remove_notify();
            }
            let _ = AcpiTerminate();
        }
        NOTIFY.store(0, Ordering::Release);
        LIVE.store(false, Ordering::Release);
    }
}
unsafe extern "C" fn walk_node(path: *const c_char, kind: u32, context: *mut c_void) -> Status {
    // SAFETY: C bridge passes a NUL terminated path and our live Vec context.
    let (path, nodes) = unsafe { (CStr::from_ptr(path), &mut *context.cast::<Vec<Node>>()) };
    if nodes.len() >= 65536 {
        return LIMIT;
    }
    let Ok(path) = path.to_str() else {
        return BAD_PARAMETER;
    };
    if nodes.try_reserve(1).is_err() {
        return NO_MEMORY;
    }
    let mut owned = String::new();
    if owned.try_reserve_exact(path.len()).is_err() {
        return NO_MEMORY;
    }
    owned.push_str(path);
    nodes.push(Node { path: owned, kind });
    OK
}
#[unsafe(no_mangle)]
unsafe extern "C" fn tk_acpi_notify(path: *const c_char, value: u32) {
    let callback = NOTIFY.load(Ordering::Acquire);
    if callback == 0 {
        return;
    }
    // SAFETY: install_notify published precisely this function pointer type;
    // C bridge owns the NUL-terminated path for the duration of the call.
    unsafe {
        if let Ok(path) = CStr::from_ptr(path).to_str() {
            let f = core::mem::transmute::<usize, fn(&str, u32)>(callback);
            f(path, value);
        }
    }
}
fn decode(wire: &[u8], cursor: &mut usize, depth: usize) -> Result<Value, Status> {
    if depth > 32 {
        return Err(LIMIT);
    }
    let header = wire
        .get(*cursor..cursor.checked_add(8).ok_or(LIMIT)?)
        .ok_or(BAD_PARAMETER)?;
    *cursor += 8;
    let tag = u32::from_le_bytes(header[..4].try_into().unwrap());
    let count = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
    if tag == 4 {
        if count > 65536 {
            return Err(LIMIT);
        }
        let mut items = Vec::new();
        items.try_reserve_exact(count).map_err(|_| NO_MEMORY)?;
        for _ in 0..count {
            items.push(decode(wire, cursor, depth + 1)?);
        }
        return Ok(Value::Package(items));
    }
    let bytes = wire
        .get(*cursor..cursor.checked_add(count).ok_or(LIMIT)?)
        .ok_or(BAD_PARAMETER)?;
    *cursor += count;
    Ok(match tag {
        0 if count == 0 => Value::Null,
        1 if count == 8 => Value::Integer(u64::from_le_bytes(bytes.try_into().unwrap())),
        2 | 3 | 20 => {
            let mut b = Vec::new();
            b.try_reserve_exact(count).map_err(|_| NO_MEMORY)?;
            b.extend_from_slice(bytes);
            match tag {
                2 => Value::String(b),
                3 => Value::Buffer(b),
                _ => Value::Reference(b),
            }
        }
        _ => return Err(BAD_PARAMETER),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_rejects_truncation_and_bad_tags() {
        for n in 0..16 {
            let mut c = 0;
            let mut w = alloc::vec![0;16];
            w[0] = 1;
            w[4] = 8;
            assert!(decode(&w[..n], &mut c, 0).is_err());
        }
        let mut c = 0;
        assert!(decode(&[99, 0, 0, 0, 0, 0, 0, 0], &mut c, 0).is_err());
    }
}
