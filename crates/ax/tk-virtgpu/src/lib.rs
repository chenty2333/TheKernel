// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
//! VirtIO GPU DRM driver components.
//!
//! The userspace ABI is the Linux `virtgpu_drm.h` ABI.  Keep its fixed-width
//! layouts independent of the DRM core and transport implementations so that
//! ioctl dispatch and protocol work can share one audited definition.
#![no_std]
#![allow(non_camel_case_types, non_snake_case, dead_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

#[cfg(test)]
pub(crate) mod test_support {
    use core::{
        ffi::{c_char, c_int, c_void},
        sync::atomic::{AtomicUsize, Ordering},
    };
    use std::sync::{Mutex, MutexGuard, Once};

    use tk_linux_kapi::kernel_services::TaskOps;

    static TEST_SERIAL: Mutex<()> = Mutex::new(());
    static SCHEDULER_INIT: Once = Once::new();
    static TASK_OPS_INIT: Once = Once::new();
    static MAIN_TASK_PTR: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn current_comm() -> *const c_char {
        c"virtgpu-test".as_ptr()
    }

    unsafe extern "C" fn task_comm(_task: *const c_void, buf: *mut c_char) {
        // SAFETY: TaskOps provides a TASK_COMM_LEN-sized output buffer; the
        // fixed test name including its terminator occupies only 13 bytes.
        unsafe { core::ptr::copy_nonoverlapping(c"virtgpu-test".as_ptr(), buf, 13) };
    }

    unsafe extern "C" fn task_pid_nr(_task: *const c_void) -> c_int {
        1
    }

    unsafe extern "C" fn task_pid(_task: *mut c_void, _pid_type: c_int) -> *mut c_void {
        core::ptr::null_mut()
    }

    unsafe extern "C" fn pid_nr(_pid: *const c_void) -> c_int {
        1
    }

    unsafe extern "C" fn put_pid(_pid: *mut c_void) {}

    unsafe extern "C" fn capable(_capability: c_int) -> bool {
        false
    }

    unsafe extern "C" fn perfmon_capable() -> bool {
        false
    }

    static TEST_TASK_OPS: TaskOps = TaskOps {
        current_comm,
        task_comm,
        task_pid_nr,
        task_pid,
        pid_nr,
        put_pid,
        capable,
        perfmon_capable,
    };

    /// Install the host test thread as the scheduler task for LinuxKPI paths
    /// that acquire kernel mutexes or consult current-task providers.
    pub(crate) struct SchedulerContext {
        previous_task: *const (),
        _serial: MutexGuard<'static, ()>,
    }

    impl Drop for SchedulerContext {
        fn drop(&mut self) {
            // SAFETY: Restore the valid task pointer captured before test setup.
            unsafe { axhal::percpu::set_current_task_ptr(self.previous_task) };
        }
    }

    pub(crate) fn scheduler_context() -> SchedulerContext {
        let serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Hosted tests run on independent OS threads; initialize each thread's
        // per-CPU GS base before consulting or changing its current-task slot.
        axhal::percpu::init_primary(0);
        let previous_task = axhal::percpu::current_task_ptr::<()>();
        SCHEDULER_INIT.call_once(|| {
            let _ = axtask::init_scheduler();
            MAIN_TASK_PTR.store(
                axhal::percpu::current_task_ptr::<()>() as usize,
                Ordering::SeqCst,
            );
        });
        let main_task = MAIN_TASK_PTR.load(Ordering::SeqCst);
        assert_ne!(main_task, 0, "host-test scheduler did not provide a task");
        // SAFETY: The scheduler's once-initialized main task remains live for
        // this host-test process and the guard restores the previous pointer.
        unsafe { axhal::percpu::set_current_task_ptr(main_task as *const ()) };
        TASK_OPS_INIT.call_once(|| {
            let _ = tk_linux_kapi::kernel_services::install_task_ops(&TEST_TASK_OPS);
        });
        SchedulerContext {
            previous_task,
            _serial: serial,
        }
    }
}

mod device;
pub mod gem;
pub mod ioctl;
pub mod transport;
pub mod uapi;

pub use device::{VirtioGpuDevice, VirtioGpuInstallError};
