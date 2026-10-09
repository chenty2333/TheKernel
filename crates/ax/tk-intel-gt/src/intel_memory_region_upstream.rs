// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//! Linux 7.2.3 intel_memory_region.c initialization dependency functions, in
//! source order. IO mapping uses native axmm; snprintf uses the LinuxKPI C
//! formatter with explicit Rust argument slices rather than C va_list.
#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]
use core::ffi::{CStr, c_char, c_int, c_void};

use crate::{
    i915_gem_region_upstream::IntelMemoryRegionOps,
    linux::{
        gem_memory::{IntelMemoryRegion, Resource},
        i915_private::DrmI915Private,
        memory::{kfree, kzalloc_obj},
        mutex::mutex_init,
    },
    linux_list::INIT_LIST_HEAD,
};
fn random_below(limit: u64) -> u64 {
    if limit == 0 {
        0
    } else {
        (unsafe { core::arch::x86_64::_rdtsc() }) % limit
    }
}
// upstream: intel_memory_region.c __iopagetest()
unsafe fn __iopagetest(
    mem: *mut IntelMemoryRegion,
    va: *mut u8,
    pagesize: usize,
    value: u8,
    offset: u64,
    _caller: *const c_void,
) -> c_int {
    let byte = random_below(pagesize as u64) as usize;
    for i in 0..pagesize {
        va.add(i).write_volatile(value);
    }
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    let result = [
        va.read_volatile(),
        va.add(byte).read_volatile(),
        va.add(pagesize - 1).read_volatile(),
    ];
    if result.iter().any(|&v| v != value) {
        axlog::error!(
            "i915 memory region readback failed: region={:x} offset={:x} wrote={:x} read={:x?}",
            (*mem).io.start,
            offset,
            value,
            result
        );
        return -crate::linux_config::EINVAL;
    }
    0
}
// upstream: intel_memory_region.c iopagetest()
unsafe fn iopagetest(mem: *mut IntelMemoryRegion, offset: u64, caller: *const c_void) -> c_int {
    let val = [0u8, 0xa5, 0xc3, 0xf0];
    let va = match axmm::iomap((((*mem).io.start + offset) as usize).into(), 4096) {
        Ok(p) => p.as_usize() as *mut u8,
        Err(_) => return -crate::linux_config::EFAULT,
    };
    let mut err = 0;
    for value in val {
        err = __iopagetest(mem, va, 4096, value, offset, caller);
        if err != 0 {
            break;
        }
        err = __iopagetest(mem, va, 4096, !value, offset, caller);
        if err != 0 {
            break;
        }
    }
    // Native axmm iomap uses permanent direct-map mappings; unlike Linux
    // ioremap no temporary VA allocation needs to be released here.
    err
}
// upstream: intel_memory_region.c random_page()
fn random_page(last: u64) -> u64 {
    random_below(last >> 12) << 12
}
// upstream: intel_memory_region.c iomemtest()
unsafe fn iomemtest(mem: *mut IntelMemoryRegion, test_all: bool, caller: *const c_void) -> c_int {
    let size = (*mem).io.end.wrapping_sub((*mem).io.start).wrapping_add(1);
    if size < 4096 {
        return 0;
    }
    let last = size - 4096;
    if test_all {
        let mut page = 0;
        while page <= last {
            let err = iopagetest(mem, page, caller);
            if err != 0 {
                return err;
            }
            page += 4096;
        }
    } else {
        let err = iopagetest(mem, 0, caller);
        if err != 0 {
            return err;
        }
        let err = iopagetest(mem, last, caller);
        if err != 0 {
            return err;
        }
        let err = iopagetest(mem, random_page(last), caller);
        if err != 0 {
            return err;
        }
    }
    0
}
// upstream: intel_memory_region.c intel_memory_region_memtest()
unsafe fn intel_memory_region_memtest(mem: *mut IntelMemoryRegion, caller: *const c_void) -> c_int {
    let i915 = (*mem).i915;
    let mut err = 0;
    if (*mem).io.start == 0 {
        return 0;
    }
    if crate::linux_config::CONFIG_DRM_I915_DEBUG_GEM || (*i915).params.memtest {
        err = iomemtest(mem, (*i915).params.memtest, caller);
    }
    err
}
// upstream: intel_memory_region.c intel_memory_type_str()
pub fn intel_memory_type_str(kind: u16) -> &'static str {
    match kind {
        0 => "system",
        1 => "local",
        3 => "stolen-local",
        2 => "stolen-system",
        _ => "unknown",
    }
}
fn resource(start: u64, size: u64) -> Resource {
    Resource {
        start,
        end: start.wrapping_add(size).wrapping_sub(1),
        name: core::ptr::null(),
        flags: 0x200,
        desc: 0,
        parent: core::ptr::null_mut(),
        sibling: core::ptr::null_mut(),
        child: core::ptr::null_mut(),
    }
}
fn write_name(dst: &mut [c_char], text: &str) {
    let count = text.len().min(dst.len() - 1);
    for (out, input) in dst[..count].iter_mut().zip(text.as_bytes()) {
        *out = *input as c_char;
    }
    dst[count] = 0;
}
// upstream: intel_memory_region.c intel_memory_region_create()
pub unsafe fn intel_memory_region_create(
    i915: *mut DrmI915Private,
    start: u64,
    size: u64,
    min_page_size: u64,
    io_start: u64,
    io_size: u64,
    kind: u16,
    instance: u16,
    ops: *const IntelMemoryRegionOps,
) -> *mut IntelMemoryRegion {
    let mem = kzalloc_obj::<IntelMemoryRegion>();
    if mem.is_null() {
        return (-crate::linux_config::ENOMEM as isize) as *mut _;
    }
    (*mem).i915 = i915;
    (*mem).region = resource(start, size);
    (*mem).io = resource(io_start, io_size);
    (*mem).min_page_size = min_page_size;
    (*mem).ops = ops.cast();
    (*mem).total = size;
    (*mem).r#type = kind;
    (*mem).instance = instance;
    write_name(
        &mut (*mem).uabi_name,
        &alloc::format!("{}{}", intel_memory_type_str(kind), instance),
    );
    mutex_init(&mut (*mem).objects.lock);
    INIT_LIST_HEAD(&mut (*mem).objects.list);
    if let Some(init) = (*ops).init {
        let err = init(mem);
        if err != 0 {
            kfree(mem);
            return (err as isize) as *mut _;
        }
    }
    let err = intel_memory_region_memtest(mem, core::ptr::null());
    if err != 0 {
        if let Some(release) = (*ops).release {
            release(mem);
        }
        kfree(mem);
        return (err as isize) as *mut _;
    }
    mem
}
// upstream: intel_memory_region.c intel_memory_region_set_name()
pub unsafe fn intel_memory_region_set_name(
    mem: *mut IntelMemoryRegion,
    format: *const c_char,
    args: &[&dyn crate::linux::print::CFormatArg],
) {
    let format = CStr::from_ptr(format).to_string_lossy();
    write_name(
        &mut (*mem).name,
        &crate::linux::print::format_c_message(&format, args),
    );
}
