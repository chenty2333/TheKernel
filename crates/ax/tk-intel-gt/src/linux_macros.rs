// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Rust spellings of the Linux macros used by the imported i915 sources.
// Runtime behavior follows the Linux 7.2.3 wt-dev configuration in
// `linux_config`; these are primitives, not substitute GT/GEM operations.

pub(crate) unsafe fn read_once<T>(pointer: *const T) -> T {
    core::ptr::read_volatile(pointer)
}

// Some imported expressions retain the C spelling as a call rather than a
// macro invocation. Linux's likely/unlikely annotations do not change value.
#[allow(non_snake_case)]
pub const fn likely(condition: bool) -> bool {
    condition
}

#[allow(non_snake_case)]
pub const fn unlikely(condition: bool) -> bool {
    condition
}

macro_rules! likely {
    ($condition:expr) => {{ $condition }};
}

macro_rules! unlikely {
    ($condition:expr) => {{ $condition }};
}

macro_rules! READ_ONCE {
    ($value:expr) => {{
        let pointer: *const _ = core::ptr::addr_of!($value);
        unsafe { $crate::linux_macros::read_once(pointer) }
    }};
}

macro_rules! WRITE_ONCE {
    ($place:expr, $value:expr) => {{ unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!($place), $value) } }};
}

macro_rules! container_of {
    ($pointer:expr, $container:ty, $($member:tt)+) => {{
        let member_ptr = $pointer as *mut u8;
        member_ptr.wrapping_sub(core::mem::offset_of!($container, $($member)+)) as *mut $container
    }};
}

macro_rules! rb_entry {
    ($pointer:expr, $container:ty, nodes[$index:expr].rb) => {{
        let storage = core::mem::MaybeUninit::<$container>::uninit();
        let base = storage.as_ptr();
        let member = unsafe { core::ptr::addr_of!((*base).nodes[$index as usize].rb) };
        let offset = (member as usize).wrapping_sub(base as usize);
        ($pointer as *mut u8).wrapping_sub(offset) as *mut $container
    }};
    ($pointer:expr, $container:ty, $member:ident $(.$member_rest:ident)*) => {
        container_of!($pointer, $container, $member $(.$member_rest)*)
    };
}

macro_rules! from_tasklet {
    ($pointer:expr, $container:ty, $member:ident) => {
        container_of!($pointer, $container, $member)
    };
    ($pointer:expr,tasklet) => {
        container_of!(
            $pointer,
            crate::intel_context_upstream::I915SchedEngine,
            tasklet
        )
    };
}

macro_rules! IS_ERR {
    ($pointer:expr) => {{ crate::linux_config::IS_ERR($pointer) }};
}

macro_rules! PTR_ERR {
    ($pointer:expr) => {{ crate::linux_config::PTR_ERR($pointer) }};
}

macro_rules! ERR_PTR {
    ($error:expr) => {{ crate::linux_config::ERR_PTR($error) }};
}

macro_rules! BIT {
    ($bit:expr) => {{ 1 << $bit }};
}

// All current i915 translation sites use BITS_PER_TYPE on u32 masks/fields.
macro_rules! BITS_PER_TYPE {
    ($value:expr) => {{ 32 }};
}

macro_rules! GENMASK {
    (BITS_PER_LONG - 2,0) => {{ u64::MAX >> 2 }};
    ($high:expr, $low:expr) => {{ (u32::MAX >> (31 - $high)) & (u32::MAX << $low) }};
}

macro_rules! BUILD_BUG_ON {
    ($condition:expr) => {{ const { assert!(!($condition), "Linux BUILD_BUG_ON condition is true") } }};
}

// CONFIG_DRM_I915_TRACE_GEM is unset in the wt-dev Linux 7.2.3 config, so the
// upstream trace macros compile to no operations while still omitting argument
// evaluation, exactly as the C headers do for this build.
macro_rules! ENGINE_TRACE {
    ($($argument:tt)*) => {{}};
}
macro_rules! GT_TRACE {
    ($($argument:tt)*) => {{}};
}
macro_rules! CE_TRACE {
    ($($argument:tt)*) => {{}};
}
macro_rules! RQ_TRACE {
    ($($argument:tt)*) => {{}};
}

macro_rules! I915_SELFTEST_ONLY {
    ($value:expr) => {{ false }};
}

macro_rules! RB_ROOT_CACHED {
    () => {{
        $crate::intel_engine_cs_upstream::RbRootCached {
            root: $crate::intel_engine_cs_upstream::RbRoot {
                node: core::ptr::null_mut(),
            },
            leftmost: core::ptr::null_mut(),
        }
    }};
}

macro_rules! ENGINE_READ {
    ($engine:expr, $reg:ident) => {{ intel_uncore_read((*$engine).uncore, $reg((*$engine).mmio_base)) }};
}

macro_rules! ENGINE_READ_FW {
    ($engine:expr, $reg:ident) => {{ intel_uncore_read_fw((*$engine).uncore, $reg((*$engine).mmio_base)) }};
}

macro_rules! ENGINE_POSTING_READ {
    ($engine:expr, $reg:ident) => {{ intel_uncore_posting_read_fw((*$engine).uncore, $reg((*$engine).mmio_base)) }};
}

macro_rules! ENGINE_READ64 {
    ($engine:expr, $lower:ident, $upper:ident) => {{
        intel_uncore_read64_2x32(
            (*$engine).uncore,
            $lower((*$engine).mmio_base),
            $upper((*$engine).mmio_base),
        )
    }};
}

macro_rules! ENGINE_WRITE {
    ($engine:expr, $reg:ident, $value:expr) => {{ intel_uncore_write((*$engine).uncore, $reg((*$engine).mmio_base), $value) }};
}

macro_rules! ENGINE_WRITE16 {
    ($engine:expr, $reg:ident, $value:expr) => {{ intel_uncore_write16((*$engine).uncore, $reg((*$engine).mmio_base), $value) }};
}

macro_rules! ENGINE_WRITE_FW {
    ($engine:expr, $reg:ident, $value:expr) => {{ intel_uncore_write_fw((*$engine).uncore, $reg((*$engine).mmio_base), $value) }};
}

macro_rules! for_each_engine_masked {
    ($engine:ident, $remaining:ident, $gt:expr, $mask:expr, $body:block) => {{
        let gt = $gt;
        let mut $remaining = ($mask) & unsafe { (*gt).info.engine_mask };
        while $remaining != 0 {
            let engine_id = $remaining.trailing_zeros() as usize;
            $remaining &= $remaining - 1;
            let $engine = unsafe { (*gt).engine[engine_id] };
            $body
        }
    }};
}

macro_rules! GEM_BUG_ON {
    ($condition:expr) => {{
        if false {
            let _ = $condition;
        }
    }};
}

macro_rules! GEM_WARN_ON {
    ($condition:expr $(,)?) => {{ WARN_ON!($condition) }};
}

macro_rules! WARN_ON {
    ($condition:expr) => {{
        let condition = $condition;
        if condition {
            axlog::warn!("i915 WARN_ON at {}:{}", file!(), line!());
        }
        condition
    }};
}

macro_rules! WARN_ONCE {
    ($condition:expr, $($argument:tt)+) => {{
        static WARNED: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        let condition = $condition;
        if condition && !WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            axlog::warn!($($argument)+);
        }
        condition
    }};
}

macro_rules! gt_WARN_ONCE {
    ($gt:expr, $condition:expr, $($argument:tt)+) => {{
        static WARNED: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        let gt = $gt;
        let condition = $condition;
        if condition && !WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            axlog::warn!("i915 GT warning for {:p}: {}", gt, format_args!($($argument)+));
        }
        condition
    }};
}

macro_rules! gt_WARN_ON_ONCE {
    ($gt:expr, $condition:expr $(,)?) => {{
        static WARNED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        let gt = $gt;
        let condition = $condition;
        if condition && !WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            axlog::warn!("i915 GT warning for {:p} at {}:{}", gt, file!(), line!());
        }
        condition
    }};
}

macro_rules! drm_WARN_ON {
    ($device:expr, $condition:expr $(,)?) => {{
        let device = $device;
        let condition = $condition;
        if condition {
            axlog::warn!(
                "i915 drm_WARN_ON device={:p} at {}:{}",
                device,
                file!(),
                line!()
            );
        }
        condition
    }};
}

macro_rules! GEM_DEBUG_WARN_ON {
    ($condition:expr) => {{
        if false {
            let _ = $condition;
        }
        false
    }};
}

macro_rules! GEM_SHOW_DEBUG {
    () => {{ false }};
}

macro_rules! IS_ENABLED {
    ($config:expr) => {{ crate::linux_config::IS_ENABLED($config) }};
}
