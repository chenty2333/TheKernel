// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! LinuxKPI entry points whose state belongs to the kernel: task identity and
//! credentials, file descriptors and anonymous inodes, PCI resources, runtime
//! PM, and cross-CPU calls. Each group reaches TheKernel through an
//! install-once provider table (registry in `PROVIDERS.md`). With no table
//! installed an entry point fails closed: it returns the Linux error value that
//! the caller checks, or it panics where Linux has no error path.

#![allow(unsafe_code)]

use alloc::{collections::BTreeMap, vec::Vec};
use core::{
    ffi::{c_char, c_int, c_void},
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use spin::Mutex;

use crate::{
    intel_context_types_upstream::File,
    linux_config::{EINVAL, ENODEV, ERR_PTR},
};

/// Install-once slot for one provider table.
pub struct ProviderSlot<T: 'static> {
    table: AtomicPtr<T>,
}

impl<T: 'static> ProviderSlot<T> {
    pub const fn new() -> Self {
        Self {
            table: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Install the table. Reinstalling the same table is idempotent; a
    /// different table is refused.
    pub fn install(&self, table: &'static T) -> Result<(), &'static str> {
        let candidate = (table as *const T).cast_mut();
        match self.table.compare_exchange(
            ptr::null_mut(),
            candidate,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(existing) if existing == candidate => Ok(()),
            Err(_) => Err("kernel provider table is already installed"),
        }
    }

    pub fn get(&self) -> Option<&'static T> {
        // SAFETY: only `'static` tables are ever stored.
        unsafe { self.table.load(Ordering::Acquire).as_ref() }
    }

    pub fn require(&self, what: &str) -> &'static T {
        self.get()
            .unwrap_or_else(|| panic!("{} provider is not installed", what))
    }
}

/// Task identity and credentials (`current`, `struct pid`, `capable()`).
#[repr(C)]
pub struct TaskOps {
    /// NUL-terminated `current->comm` of the calling task, valid while it runs.
    pub current_comm: unsafe extern "C" fn() -> *const c_char,
    /// Copy the 16-byte (`TASK_COMM_LEN`) name of `task` into `buf`.
    pub task_comm: unsafe extern "C" fn(task: *const c_void, buf: *mut c_char),
    /// `task_pid_nr(task)` in the initial PID namespace.
    pub task_pid_nr: unsafe extern "C" fn(task: *const c_void) -> c_int,
    /// Referenced `struct pid` of `task` for `pid_type`, released by `put_pid`.
    pub task_pid: unsafe extern "C" fn(task: *mut c_void, pid_type: c_int) -> *mut c_void,
    /// `pid_nr(pid)` in the initial PID namespace.
    pub pid_nr: unsafe extern "C" fn(pid: *const c_void) -> c_int,
    pub put_pid: unsafe extern "C" fn(pid: *mut c_void),
    /// `capable(cap)` for the calling task.
    pub capable: unsafe extern "C" fn(capability: c_int) -> bool,
    /// `perfmon_capable()` for the calling task.
    pub perfmon_capable: unsafe extern "C" fn() -> bool,
}

/// File descriptors and anonymous inodes.
#[repr(C)]
pub struct FileOps {
    /// Linux `anon_inode_getfile()`: a new file, or an `ERR_PTR` on failure.
    pub anon_inode_getfile: unsafe extern "C" fn(
        name: *const c_char,
        fops: *const c_void,
        private_data: *mut c_void,
        flags: c_int,
    ) -> *mut c_void,
    /// Linux `get_unused_fd_flags()`: a descriptor, or a negative errno.
    pub get_unused_fd_flags: unsafe extern "C" fn(flags: c_int) -> c_int,
    pub put_unused_fd: unsafe extern "C" fn(fd: c_int),
    pub fd_install: unsafe extern "C" fn(fd: c_int, file: *mut c_void),
}

/// PCI device resources and claimed memory regions.
#[repr(C)]
pub struct PciOps {
    pub resource_start: unsafe extern "C" fn(dev: *mut c_void, bar: u32) -> u64,
    pub resource_len: unsafe extern "C" fn(dev: *mut c_void, bar: u32) -> u64,
    /// `devm_request_mem_region()`: `struct resource *`, or NULL when the range
    /// is unavailable.
    pub request_mem_region: unsafe extern "C" fn(
        dev: *mut c_void,
        start: u64,
        n: u64,
        name: *const c_char,
    ) -> *mut c_void,
}

/// Runtime PM for a device. Return values are Linux's.
#[repr(C)]
pub struct PmOps {
    pub get_sync: unsafe extern "C" fn(dev: *mut c_void) -> c_int,
    pub put: unsafe extern "C" fn(dev: *mut c_void) -> c_int,
    pub put_autosuspend: unsafe extern "C" fn(dev: *mut c_void) -> c_int,
    pub get_if_active: unsafe extern "C" fn(dev: *mut c_void) -> c_int,
    pub get_if_in_use: unsafe extern "C" fn(dev: *mut c_void) -> c_int,
    pub get_noresume: unsafe extern "C" fn(dev: *mut c_void),
    pub mark_last_busy: unsafe extern "C" fn(dev: *mut c_void),
    pub set_autosuspend_delay: unsafe extern "C" fn(dev: *mut c_void, delay: c_int),
    pub use_autosuspend: unsafe extern "C" fn(dev: *mut c_void),
    pub dont_use_autosuspend: unsafe extern "C" fn(dev: *mut c_void),
    pub allow: unsafe extern "C" fn(dev: *mut c_void),
    pub suspended: unsafe extern "C" fn(dev: *mut c_void) -> bool,
    pub set_driver_flags: unsafe extern "C" fn(dev: *mut c_void, flags: u32),
}

/// Cross-CPU execution.
#[repr(C)]
pub struct SmpOps {
    /// Run `func(arg)` on every CPU except the caller and wait for all of them.
    pub call_on_other_cpus:
        unsafe extern "C" fn(func: unsafe extern "C" fn(*mut c_void), arg: *mut c_void),
    /// Linux `stop_machine()`: run `func(data)` on one CPU while the others are
    /// halted. `cpus` is a `struct cpumask *` or NULL for all online CPUs.
    pub stop_machine: unsafe extern "C" fn(
        func: unsafe extern "C" fn(*mut c_void) -> c_int,
        data: *mut c_void,
        cpus: *const c_void,
    ) -> c_int,
}

pub static TASK: ProviderSlot<TaskOps> = ProviderSlot::new();
pub static FILES: ProviderSlot<FileOps> = ProviderSlot::new();
pub static PCI: ProviderSlot<PciOps> = ProviderSlot::new();
pub static PM: ProviderSlot<PmOps> = ProviderSlot::new();
pub static SMP: ProviderSlot<SmpOps> = ProviderSlot::new();

// ---------------------------------------------------------------------------
// Task identity and credentials
// ---------------------------------------------------------------------------

/// Linux `current_comm()` (an i915 helper that returns `current->comm`).
#[unsafe(export_name = "current_comm")]
pub unsafe extern "C" fn current_comm() -> *const c_char {
    unsafe { (TASK.require("task").current_comm)() }
}

/// Linux `get_task_comm(buf, task)`.
#[unsafe(export_name = "get_task_comm")]
pub unsafe extern "C" fn get_task_comm(buf: *mut c_char, task: *const c_void) {
    unsafe { (TASK.require("task").task_comm)(task, buf) };
}

/// Linux `task_pid_nr(task)`.
#[unsafe(export_name = "task_pid_nr")]
pub unsafe extern "C" fn task_pid_nr(task: *const c_void) -> c_int {
    unsafe { (TASK.require("task").task_pid_nr)(task) }
}

/// Linux `get_task_pid(task, type)`: a referenced `struct pid`.
#[unsafe(export_name = "get_task_pid")]
pub unsafe extern "C" fn get_task_pid(task: *mut c_void, pid_type: c_int) -> *mut c_void {
    unsafe { (TASK.require("task").task_pid)(task, pid_type) }
}

/// Linux `pid_nr(pid)`.
#[unsafe(export_name = "pid_nr")]
pub unsafe extern "C" fn pid_nr(pid: *const c_void) -> c_int {
    unsafe { (TASK.require("task").pid_nr)(pid) }
}

/// Linux `put_pid(pid)`.
#[unsafe(export_name = "put_pid")]
pub unsafe extern "C" fn put_pid(pid: *mut c_void) {
    unsafe { (TASK.require("task").put_pid)(pid) };
}

/// Linux `capable(cap)`. Denies when no credential provider is installed.
#[unsafe(export_name = "capable")]
pub unsafe extern "C" fn capable(capability: c_int) -> bool {
    TASK.get()
        .is_some_and(|ops| unsafe { (ops.capable)(capability) })
}

/// Linux `perfmon_capable()`. Denies when no credential provider is installed.
#[unsafe(export_name = "perfmon_capable")]
pub unsafe extern "C" fn perfmon_capable() -> bool {
    TASK.get()
        .is_some_and(|ops| unsafe { (ops.perfmon_capable)() })
}

// ---------------------------------------------------------------------------
// File descriptors and anonymous inodes
// ---------------------------------------------------------------------------

/// Linux `anon_inode_getfile()`. Without a file table the result is
/// `ERR_PTR(-ENODEV)`, which the caller already checks with `IS_ERR`.
#[unsafe(export_name = "anon_inode_getfile")]
pub unsafe extern "C" fn anon_inode_getfile(
    name: *const c_char,
    fops: *const c_void,
    private_data: *mut c_void,
    flags: c_int,
) -> *mut File {
    match FILES.get() {
        Some(ops) => unsafe { (ops.anon_inode_getfile)(name, fops, private_data, flags) }.cast(),
        None => ERR_PTR(-ENODEV),
    }
}

/// Linux `get_unused_fd_flags(flags)`. Without a file table: `-ENODEV`.
#[unsafe(export_name = "get_unused_fd_flags")]
pub unsafe extern "C" fn get_unused_fd_flags(flags: c_int) -> c_int {
    match FILES.get() {
        Some(ops) => unsafe { (ops.get_unused_fd_flags)(flags) },
        None => -ENODEV,
    }
}

/// Linux `put_unused_fd(fd)`.
#[unsafe(export_name = "put_unused_fd")]
pub unsafe extern "C" fn put_unused_fd(fd: c_int) {
    unsafe { (FILES.require("file table").put_unused_fd)(fd) };
}

/// Linux `fd_install(fd, file)`.
#[unsafe(export_name = "fd_install")]
pub unsafe extern "C" fn fd_install(fd: c_int, file: *mut c_void) {
    unsafe { (FILES.require("file table").fd_install)(fd, file) };
}

// ---------------------------------------------------------------------------
// PCI resources
// ---------------------------------------------------------------------------

/// Linux `pci_resource_start(dev, bar)`.
#[unsafe(export_name = "pci_resource_start")]
pub unsafe extern "C" fn pci_resource_start(dev: *mut c_void, bar: u32) -> u64 {
    unsafe { (PCI.require("PCI resource").resource_start)(dev, bar) }
}

/// Linux `pci_resource_len(dev, bar)`.
#[unsafe(export_name = "pci_resource_len")]
pub unsafe extern "C" fn pci_resource_len(dev: *mut c_void, bar: u32) -> u64 {
    unsafe { (PCI.require("PCI resource").resource_len)(dev, bar) }
}

/// Linux `devm_request_mem_region(dev, start, n, name)`. Without a PCI owner
/// no region can be claimed, so the result is NULL.
#[unsafe(export_name = "devm_request_mem_region")]
pub unsafe extern "C" fn devm_request_mem_region(
    dev: *mut c_void,
    start: u64,
    n: u64,
    name: *const c_char,
) -> *mut c_void {
    match PCI.get() {
        Some(ops) => unsafe { (ops.request_mem_region)(dev, start, n, name) },
        None => ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
// Runtime PM
// ---------------------------------------------------------------------------

#[unsafe(export_name = "pm_runtime_get_sync")]
pub unsafe extern "C" fn pm_runtime_get_sync(dev: *mut c_void) -> c_int {
    unsafe { (PM.require("runtime PM").get_sync)(dev) }
}

#[unsafe(export_name = "pm_runtime_put")]
pub unsafe extern "C" fn pm_runtime_put(dev: *mut c_void) -> c_int {
    unsafe { (PM.require("runtime PM").put)(dev) }
}

#[unsafe(export_name = "pm_runtime_put_autosuspend")]
pub unsafe extern "C" fn pm_runtime_put_autosuspend(dev: *mut c_void) -> c_int {
    unsafe { (PM.require("runtime PM").put_autosuspend)(dev) }
}

#[unsafe(export_name = "pm_runtime_get_if_active")]
pub unsafe extern "C" fn pm_runtime_get_if_active(dev: *mut c_void) -> c_int {
    unsafe { (PM.require("runtime PM").get_if_active)(dev) }
}

#[unsafe(export_name = "pm_runtime_get_if_in_use")]
pub unsafe extern "C" fn pm_runtime_get_if_in_use(dev: *mut c_void) -> c_int {
    unsafe { (PM.require("runtime PM").get_if_in_use)(dev) }
}

#[unsafe(export_name = "pm_runtime_get_noresume")]
pub unsafe extern "C" fn pm_runtime_get_noresume(dev: *mut c_void) {
    unsafe { (PM.require("runtime PM").get_noresume)(dev) };
}

#[unsafe(export_name = "pm_runtime_mark_last_busy")]
pub unsafe extern "C" fn pm_runtime_mark_last_busy(dev: *mut c_void) {
    unsafe { (PM.require("runtime PM").mark_last_busy)(dev) };
}

#[unsafe(export_name = "pm_runtime_set_autosuspend_delay")]
pub unsafe extern "C" fn pm_runtime_set_autosuspend_delay(dev: *mut c_void, delay: c_int) {
    unsafe { (PM.require("runtime PM").set_autosuspend_delay)(dev, delay) };
}

#[unsafe(export_name = "pm_runtime_use_autosuspend")]
pub unsafe extern "C" fn pm_runtime_use_autosuspend(dev: *mut c_void) {
    unsafe { (PM.require("runtime PM").use_autosuspend)(dev) };
}

#[unsafe(export_name = "pm_runtime_dont_use_autosuspend")]
pub unsafe extern "C" fn pm_runtime_dont_use_autosuspend(dev: *mut c_void) {
    unsafe { (PM.require("runtime PM").dont_use_autosuspend)(dev) };
}

#[unsafe(export_name = "pm_runtime_allow")]
pub unsafe extern "C" fn pm_runtime_allow(dev: *mut c_void) {
    unsafe { (PM.require("runtime PM").allow)(dev) };
}

#[unsafe(export_name = "pm_runtime_suspended")]
pub unsafe extern "C" fn pm_runtime_suspended(dev: *mut c_void) -> bool {
    unsafe { (PM.require("runtime PM").suspended)(dev) }
}

#[unsafe(export_name = "dev_pm_set_driver_flags")]
pub unsafe extern "C" fn dev_pm_set_driver_flags(dev: *mut c_void, flags: u32) {
    unsafe { (PM.require("runtime PM").set_driver_flags)(dev, flags) };
}

// ---------------------------------------------------------------------------
// Bus notifier chains
// ---------------------------------------------------------------------------

/// Notifier blocks per bus. TheKernel has no bus-level device add/remove
/// events, so a chain is never walked; registration keeps Linux's bookkeeping.
static BUS_NOTIFIERS: Mutex<BTreeMap<usize, Vec<usize>>> = Mutex::new(BTreeMap::new());

/// Linux `bus_register_notifier(bus, nb)`.
#[unsafe(export_name = "bus_register_notifier")]
pub unsafe extern "C" fn bus_register_notifier(bus: *const c_void, nb: *mut c_void) -> c_int {
    if bus.is_null() || nb.is_null() {
        return -EINVAL;
    }
    let mut chains = BUS_NOTIFIERS.lock();
    let chain = chains.entry(bus as usize).or_default();
    if !chain.contains(&(nb as usize)) {
        chain.push(nb as usize);
    }
    0
}

/// Linux `bus_unregister_notifier(bus, nb)`: `-ENOENT` if not registered.
#[unsafe(export_name = "bus_unregister_notifier")]
pub unsafe extern "C" fn bus_unregister_notifier(bus: *const c_void, nb: *mut c_void) -> c_int {
    let mut chains = BUS_NOTIFIERS.lock();
    let Some(chain) = chains.get_mut(&(bus as usize)) else {
        return -crate::linux_config::ENOENT;
    };
    let before = chain.len();
    chain.retain(|&entry| entry != nb as usize);
    if chain.len() == before {
        -crate::linux_config::ENOENT
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Module symbol lookup
// ---------------------------------------------------------------------------

/// Linux `__symbol_get(name)`: the address of an exported kernel symbol, or
/// NULL. TheKernel exports no loadable-module symbol table, so every lookup
/// misses, which is Linux's result for a symbol that is not present.
#[unsafe(export_name = "__symbol_get")]
pub unsafe extern "C" fn __symbol_get(_name: *const c_char) -> *mut c_void {
    ptr::null_mut()
}

/// Linux `__symbol_put(name)`: drops the module reference taken by a
/// successful `__symbol_get`. No lookup can succeed here, so no reference
/// exists to drop.
#[unsafe(export_name = "__symbol_put")]
pub unsafe extern "C" fn __symbol_put(_name: *const c_char) {}

// ---------------------------------------------------------------------------
// Cross-CPU
// ---------------------------------------------------------------------------

unsafe extern "C" fn wbinvd_local(_arg: *mut c_void) {
    unsafe { core::arch::asm!("wbinvd", options(nostack, preserves_flags)) };
}

/// Linux `wbinvd_on_all_cpus()`: write back and invalidate every CPU's caches.
/// The local write-back runs directly; other CPUs run it through the SMP
/// provider, which must wait for all of them before returning.
#[unsafe(export_name = "wbinvd_on_all_cpus")]
pub unsafe extern "C" fn wbinvd_on_all_cpus() {
    unsafe { wbinvd_local(ptr::null_mut()) };
    if axhal::cpu_num() > 1 {
        unsafe { (SMP.require("SMP").call_on_other_cpus)(wbinvd_local, ptr::null_mut()) };
    }
}

/// Linux `stop_machine(fn, data, cpus)`. A one-CPU system runs the callback
/// directly, which is what Linux's single-CPU path does.
#[unsafe(export_name = "stop_machine")]
pub unsafe extern "C" fn stop_machine(
    func: unsafe extern "C" fn(*mut c_void) -> c_int,
    data: *mut c_void,
    cpus: *const c_void,
) -> c_int {
    if axhal::cpu_num() <= 1 {
        return unsafe { func(data) };
    }
    unsafe { (SMP.require("SMP").stop_machine)(func, data, cpus) }
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

pub fn install_task_ops(ops: &'static TaskOps) -> Result<(), &'static str> {
    TASK.install(ops)
}

pub fn install_file_ops(ops: &'static FileOps) -> Result<(), &'static str> {
    FILES.install(ops)
}

pub fn install_pci_ops(ops: &'static PciOps) -> Result<(), &'static str> {
    PCI.install(ops)
}

pub fn install_pm_ops(ops: &'static PmOps) -> Result<(), &'static str> {
    PM.install(ops)
}

pub fn install_smp_ops(ops: &'static SmpOps) -> Result<(), &'static str> {
    SMP.install(ops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux_config::ENOENT;

    #[test]
    fn absent_providers_fail_closed() {
        unsafe {
            assert!(crate::linux_config::IS_ERR(anon_inode_getfile(
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
                0,
            )));
            assert_eq!(get_unused_fd_flags(0), -ENODEV);
            assert!(devm_request_mem_region(ptr::null_mut(), 0, 0, ptr::null()).is_null());
            assert!(!capable(0));
            assert!(!perfmon_capable());
            assert!(__symbol_get(ptr::null()).is_null());
        }
    }

    #[test]
    fn bus_notifier_chain_registers_once_and_reports_absence() {
        unsafe {
            let bus = 0x4000_0000usize as *const c_void;
            let nb = 0x4000_1000usize as *mut c_void;
            assert_eq!(bus_register_notifier(bus, nb), 0);
            assert_eq!(bus_register_notifier(bus, nb), 0);
            assert_eq!(bus_unregister_notifier(bus, nb), 0);
            assert_eq!(bus_unregister_notifier(bus, nb), -ENOENT);
        }
    }
}
