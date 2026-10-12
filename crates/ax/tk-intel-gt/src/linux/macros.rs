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

/// Encode the Intel `REG_MASKED_FIELD_ENABLE(mask)` write value from
/// include/drm/intel/reg_bits.h (mask in the high half, value in low half).
#[macro_export]
macro_rules! REG_MASKED_FIELD_ENABLE {
    ($field:expr) => {{
        let __field = $field as u32;
        (__field << 16) | __field
    }};
}

/// Intel register mask/value encoding from include/drm/intel/reg_bits.h.
macro_rules! REG_MASKED_FIELD {
    ($mask:expr, $value:expr $(,)?) => {{ (($mask) << 16) | ($value) }};
}

/// Encode the Intel `REG_MASKED_FIELD_DISABLE(mask)` write value.
#[macro_export]
macro_rules! REG_MASKED_FIELD_DISABLE {
    ($field:expr) => {{
        let __field = $field as u32;
        __field << 16
    }};
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
        let member_ptr = ($pointer) as *const _ as *const u8;
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
            crate::i915_scheduler_types_upstream::I915SchedEngine,
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

macro_rules! smp_wmb {
    () => {{ $crate::linux::primitives::wmb() }};
}

// Source macro equivalents from include/linux/iosys-map.h. i915's GuC
// engine-usage records are naturally aligned scalar fields in the mapping.
macro_rules! iosys_map_rd_field {
    ($map:expr, $struct_offset:expr, $struct_type:ty, $field:ident) => {{
        let __map = $map;
        let __offset = ($struct_offset) + core::mem::offset_of!($struct_type, $field);
        let __base = unsafe {
            if __map.is_iomem {
                __map.addr.vaddr_iomem
            } else {
                __map.addr.vaddr
            }
        };
        let __field_ptr = unsafe { __base.cast::<u8>().add(__offset) };
        unsafe { core::ptr::read_volatile(__field_ptr.cast()) }
    }};
}

macro_rules! iosys_map_wr_field {
    ($map:expr, $struct_offset:expr, $struct_type:ty, $field:ident, $value:expr $(,)?) => {{
        let __map = $map;
        let __offset = ($struct_offset) + core::mem::offset_of!($struct_type, $field);
        let __base = unsafe {
            if __map.is_iomem {
                __map.addr.vaddr_iomem
            } else {
                __map.addr.vaddr
            }
        };
        let __field_ptr = unsafe { __base.cast::<u8>().add(__offset) };
        unsafe { core::ptr::write_volatile(__field_ptr.cast(), $value) }
    }};
}

// Linux rcu_dereference() is a single-copy pointer load with a dependency
// barrier. The supported i915 port is x86_64, where the compiler barrier
// plus READ_ONCE supplies the same pointer-publication ordering.
macro_rules! rcu_dereference {
    ($pointer:expr) => {{
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Acquire);
        let __value = READ_ONCE!($pointer);
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Acquire);
        __value
    }};
}

// include/drm/intel/reg_bits.h's 16-bit value/mask convention.
macro_rules! REG_MASKED_FIELD_ENABLE {
    ($field:expr) => {{
        let field = $field as u32;
        (field << 16) | field
    }};
}

macro_rules! REG_MASKED_FIELD_DISABLE {
    ($field:expr) => {{
        let field = $field as u32;
        field << 16
    }};
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

macro_rules! INIT_ACTIVE_FENCE {
    ($active:expr) => {{ unsafe { $crate::linux::contexts::init_active_fence($active) } }};
}

macro_rules! build_bug_on {
    ($condition:expr) => {{ BUILD_BUG_ON!($condition) }};
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

macro_rules! RB_ROOT {
    () => { $crate::linux::rbtree::RB_ROOT };
}

macro_rules! RB_EMPTY_NODE {
    ($node:expr) => {{
        let __node = $node;
        unsafe { (*__node).parent_color == (__node as *const _ as usize) }
    }};
}

macro_rules! RB_CLEAR_NODE {
    ($node:expr) => {{
        let __node = $node;
        unsafe { (*__node).parent_color = __node as *const _ as usize; }
    }};
}

macro_rules! wmb {
    () => { $crate::linux::primitives::wmb() };
}

macro_rules! ENGINE_READ {
    ($engine:expr, $reg:expr) => {{
        $crate::intel_uncore_types_upstream::intel_uncore_read(
            (*$engine).uncore,
            ($reg)((*$engine).mmio_base),
        )
    }};
}

macro_rules! ENGINE_READ_FW {
    ($engine:expr, $reg:expr) => {{
        $crate::intel_uncore_types_upstream::intel_uncore_read_fw(
            (*$engine).uncore,
            ($reg)((*$engine).mmio_base),
        )
    }};
}

macro_rules! ENGINE_POSTING_READ {
    ($engine:expr, $reg:expr) => {{
        unsafe { $crate::intel_uncore_types_upstream::intel_uncore_posting_read_fw(
            (*$engine).uncore,
            ($reg)((*$engine).mmio_base),
        ) }
    }};
}

macro_rules! ENGINE_READ64 {
    ($engine:expr, $lower:expr, $upper:expr) => {{
        $crate::intel_uncore_types_upstream::intel_uncore_read64_2x32(
            (*$engine).uncore,
            ($lower)((*$engine).mmio_base),
            ($upper)((*$engine).mmio_base),
        )
    }};
}

macro_rules! ENGINE_WRITE {
    ($engine:expr, $reg:expr, $value:expr) => {{
        $crate::intel_uncore_types_upstream::intel_uncore_write(
            (*$engine).uncore,
            ($reg)((*$engine).mmio_base),
            $value,
        )
    }};
}

macro_rules! ENGINE_WRITE16 {
    ($engine:expr, $reg:expr, $value:expr) => {{
        $crate::intel_uncore_types_upstream::intel_uncore_write16(
            (*$engine).uncore,
            ($reg)((*$engine).mmio_base),
            $value,
        )
    }};
}

macro_rules! ENGINE_WRITE_FW {
    ($engine:expr, $reg:expr, $value:expr) => {{
        unsafe { $crate::intel_uncore_types_upstream::intel_uncore_write_fw(
            (*$engine).uncore,
            ($reg)((*$engine).mmio_base),
            $value,
        ) }
    }};
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

macro_rules! for_each_engine {
    ($engine:ident, $id:ident, $gt:expr, $body:block) => {{
        let __gt = $gt;
        let mut __engine_index = 0usize;
        while __engine_index < $crate::intel_engine_cs_upstream::I915_NUM_ENGINES as usize {
            let $id: $crate::intel_engine_cs_upstream::IntelEngineId =
                __engine_index as $crate::intel_engine_cs_upstream::IntelEngineId;
            let __engine_ptr = unsafe { (*__gt).engine[__engine_index] };
            __engine_index += 1;
            if !__engine_ptr.is_null() {
                let $engine = unsafe { &mut *__engine_ptr };
                $body
            }
        }
    }};
}

macro_rules! for_each_clear_bit {
    ($bit:ident, $address:expr, $count:expr, $body:block) => {{
        let __bits = unsafe { *($address) } as u64;
        let mut __bit_index = 0u32;
        while __bit_index < $count as u32 {
            if __bits & (1u64 << __bit_index) == 0 {
                let $bit = __bit_index;
                $body
            }
            __bit_index += 1;
        }
    }};
}

macro_rules! GEM_SHOW_DEBUG {
    () => {{ false }};
}

macro_rules! IS_ENABLED {
    ($config:expr) => {{ crate::linux_config::IS_ENABLED($config) }};
}

macro_rules! IS_ALIGNED {
    ($address:expr, $alignment:expr $(,)?) => {{
        let __address = $address;
        let __alignment = $alignment;
        __alignment != 0 && (__address & (__alignment - 1)) == 0
    }};
}

macro_rules! overflows_type {
    ($value:expr, $type:ty $(,)?) => {{ <$type as core::convert::TryFrom<_>>::try_from($value).is_err() }};
}

macro_rules! INIT_RADIX_TREE {
    ($root:expr, $gfp:expr $(,)?) => {{
        let __root = &mut *($root);
        __root.height = 0;
        __root.gfp_mask = $gfp;
        __root.rnode = core::ptr::null_mut();
    }};
}

// `mutex_release()` / `mutex_acquire()` are disabled inline helpers when the
// source kernel is built without CONFIG_LOCKDEP. The disabled expansion does
// not evaluate (or type-check) its lockdep_map arguments.
macro_rules! mutex_release {
    ($($argument:tt)*) => {{
        if crate::linux_config::CONFIG_LOCKDEP {
            panic!("CONFIG_LOCKDEP mutex release backend is not configured");
        }
    }};
}

macro_rules! mutex_acquire {
    ($($argument:tt)*) => {{
        if crate::linux_config::CONFIG_LOCKDEP {
            panic!("CONFIG_LOCKDEP mutex acquire backend is not configured");
        }
    }};
}

// `might_lock()` is a lockdep annotation which compiles out when the source
// kernel is built with CONFIG_LOCKDEP=n. Keep the argument unevaluated, as in
// the disabled Linux macro expansion.
macro_rules! might_lock {
    ($($argument:tt)*) => {{}};
}

/// `dma_fence_assert_held()` compiles away for this configured Linux build
/// (`CONFIG_LOCKDEP=n`); preserve the expression's evaluation side effects.
#[macro_export]
macro_rules! dma_fence_assert_held {
    ($fence:expr) => {{
        let _ = $fence;
    }};
}

/// Initialize an atomic notifier head in the CONFIG_LOCKDEP=n target layout.
#[macro_export]
macro_rules! ATOMIC_INIT_NOTIFIER_HEAD {
    ($head:expr) => {{
        let __head = $head;
        unsafe { core::ptr::write_bytes(__head, 0, 1) }
    }};
}

/// Release-publish an RCU pointer slot, matching `rcu_assign_pointer()`.
#[macro_export]
macro_rules! rcu_assign_pointer {
    ($slot:expr, $value:expr) => {{
        let __slot = $slot;
        let __value = $value;
        let __atomic = unsafe {
            &*core::ptr::addr_of_mut!(*__slot).cast::<core::sync::atomic::AtomicPtr<_>>()
        };
        __atomic.store(__value, core::sync::atomic::Ordering::Release);
    }};
}

macro_rules! gt_dbg {
    ($gt:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let __gt = $gt;
        let _ = __gt;
        if ::tk_linux_kapi::print::drm_debug_enabled(::tk_linux_kapi::print::DRM_UT_DRIVER) {
            let __args: &[&dyn ::tk_linux_kapi::print::CFormatArg] = &[
                $(&($argument) as &dyn ::tk_linux_kapi::print::CFormatArg),*
            ];
            let __message = ::tk_linux_kapi::print::format_message($format, __args);
            ::tk_linux_kapi::print::drm_log_at(
                ::tk_linux_kapi::print::DrmLogLevel::Debug,
                "i915 GT debug",
                file!(),
                line!(),
                &__message,
            );
        }
    }};
}

macro_rules! guc_err {
    ($guc:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let __guc = $guc;
        let _ = __guc;
        let __args: &[&dyn ::tk_linux_kapi::print::CFormatArg] = &[
            $(&($argument) as &dyn ::tk_linux_kapi::print::CFormatArg),*
        ];
        let __message = ::tk_linux_kapi::print::format_message($format, __args);
        ::tk_linux_kapi::print::drm_log_at(
            ::tk_linux_kapi::print::DrmLogLevel::Error,
            "i915 GuC error",
            file!(),
            line!(),
            &__message,
        );
    }};
}

/// Linux `guc_info()` / `guc_printk(..., info, ...)` logging path.
macro_rules! guc_info {
    ($guc:expr, $format:literal $(, $argument:expr)* $(,)?) => {{
        let __guc = $guc;
        let _ = __guc;
        let __args: &[&dyn ::tk_linux_kapi::print::CFormatArg] = &[
            $(&($argument) as &dyn ::tk_linux_kapi::print::CFormatArg),*
        ];
        let __message = ::tk_linux_kapi::print::format_message(concat!("GUC: ", $format), __args);
        ::tk_linux_kapi::print::drm_log_at(
            ::tk_linux_kapi::print::DrmLogLevel::Info,
            "i915 GuC info",
            file!(),
            line!(),
            &__message,
        );
    }};
}

macro_rules! GEM_TRACE_ERR {
    ($format:expr $(, $argument:expr)* $(,)?) => {{
        let __args: &[&dyn ::tk_linux_kapi::print::CFormatArg] = &[
            $(&($argument) as &dyn ::tk_linux_kapi::print::CFormatArg),*
        ];
        let __message = ::tk_linux_kapi::print::format_message($format, __args);
        ::tk_linux_kapi::print::drm_log_at(
            ::tk_linux_kapi::print::DrmLogLevel::Error,
            "i915 GEM trace",
            file!(),
            line!(),
            &__message,
        );
    }};
}

macro_rules! guc_warn {
    ($guc:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let __guc = $guc;
        let _ = __guc;
        let __args: &[&dyn ::tk_linux_kapi::print::CFormatArg] = &[
            $(&($argument) as &dyn ::tk_linux_kapi::print::CFormatArg),*
        ];
        let __message = ::tk_linux_kapi::print::format_message($format, __args);
        ::tk_linux_kapi::print::drm_log_at(
            ::tk_linux_kapi::print::DrmLogLevel::Warn,
            "i915 GuC warning",
            file!(),
            line!(),
            &__message,
        );
    }};
}

macro_rules! gt_err {
    ($gt:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let __gt = $gt;
        let __id = unsafe { (*__gt).info.id };
        let __args: &[&dyn ::tk_linux_kapi::print::CFormatArg] = &[
            $(&($argument) as &dyn ::tk_linux_kapi::print::CFormatArg),*
        ];
        let __message = ::tk_linux_kapi::print::format_message($format, __args);
        ::tk_linux_kapi::print::drm_log_at(
            ::tk_linux_kapi::print::DrmLogLevel::Error,
            &alloc::format!("GT{__id}"),
            file!(),
            line!(),
            &__message,
        );
    }};
}

macro_rules! gt_notice {
    ($gt:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let __gt = $gt;
        let __id = unsafe { (*__gt).info.id };
        let __args: &[&dyn ::tk_linux_kapi::print::CFormatArg] = &[
            $(&($argument) as &dyn ::tk_linux_kapi::print::CFormatArg),*
        ];
        let __message = ::tk_linux_kapi::print::format_message($format, __args);
        ::tk_linux_kapi::print::drm_log_at(
            ::tk_linux_kapi::print::DrmLogLevel::Info,
            &alloc::format!("GT{__id}"),
            file!(),
            line!(),
            &__message,
        );
    }};
}

/// `guc_notice()` from `intel_guc_print.h`: retain the GuC tag/prefix while
/// logging through the owning GT at Linux's notice level.
macro_rules! guc_notice {
    ($guc:expr, $format:literal $(, $argument:expr)* $(,)?) => {{
        let __guc = $guc;
        let __gt = unsafe {
            $crate::intel_gt_api_upstream::guc_to_gt(
                __guc as *mut $crate::intel_guc_types_upstream::IntelGuc,
            )
        };
        gt_notice!(__gt, concat!("GUC: ", $format) $(, $argument)*);
    }};
}

macro_rules! gt_dbg {
    ($gt:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let __gt = $gt;
        if ::tk_linux_kapi::print::drm_debug_enabled(::tk_linux_kapi::print::DRM_UT_DRIVER) {
            let __id = unsafe { (*__gt).info.id };
            let __args: &[&dyn ::tk_linux_kapi::print::CFormatArg] = &[
                $(&($argument) as &dyn ::tk_linux_kapi::print::CFormatArg),*
            ];
            let __message = ::tk_linux_kapi::print::format_message($format, __args);
            ::tk_linux_kapi::print::drm_log_at(
                ::tk_linux_kapi::print::DrmLogLevel::Debug,
                &alloc::format!("GT{__id}"),
                file!(),
                line!(),
                &__message,
            );
        }
    }};
}

macro_rules! guc_probe_error {
    ($guc:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let __guc = $guc;
        let __gt = unsafe {
            $crate::linux::i915::guc_to_gt(__guc as *mut _)
        };
        let __id = unsafe { (*__gt).info.id };
        let __args: &[&dyn ::tk_linux_kapi::print::CFormatArg] = &[
            $(&($argument) as &dyn ::tk_linux_kapi::print::CFormatArg),*
        ];
        let __message = ::tk_linux_kapi::print::format_message($format, __args);
        ::tk_linux_kapi::print::drm_log_at(
            ::tk_linux_kapi::print::DrmLogLevel::Error,
            &alloc::format!("GT{__id} GuC probe"),
            file!(),
            line!(),
            &__message,
        );
    }};
}

macro_rules! guc_dbg {
    ($guc:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let __guc = $guc;
        let _ = __guc;
        if ::tk_linux_kapi::print::drm_debug_enabled(::tk_linux_kapi::print::DRM_UT_DRIVER) {
            let __args: &[&dyn ::tk_linux_kapi::print::CFormatArg] = &[
                $(&($argument) as &dyn ::tk_linux_kapi::print::CFormatArg),*
            ];
            let __message = ::tk_linux_kapi::print::format_message($format, __args);
            ::tk_linux_kapi::print::drm_log_at(
                ::tk_linux_kapi::print::DrmLogLevel::Debug,
                "i915 GuC debug",
                file!(),
                line!(),
                &__message,
            );
        }
    }};
}

macro_rules! GEM_BUG_ON {
    ($condition:expr $(,)?) => {{
        let __condition = $condition;
        if __condition {
            panic!(
                "i915 GEM_BUG_ON({}) at {}:{}",
                stringify!($condition),
                file!(),
                line!()
            );
        }
    }};
}

macro_rules! gem_bug_on {
    ($condition:expr $(,)?) => {{ GEM_BUG_ON!($condition) }};
}

macro_rules! gem_warn_on {
    ($condition:expr $(,)?) => {{ GEM_WARN_ON!($condition) }};
}

macro_rules! gt_WARN_ONCE {
    ($gt:expr, $condition:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        static __WARNED: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        let __gt = $gt;
        let __condition = $condition;
        if __condition && !__WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            let __args: &[&dyn ::tk_linux_kapi::print::CFormatArg] = &[
                $(&($argument) as &dyn ::tk_linux_kapi::print::CFormatArg),*
            ];
            let __message = ::tk_linux_kapi::print::format_message($format, __args);
            ::tk_linux_kapi::print::drm_log_at(
                ::tk_linux_kapi::print::DrmLogLevel::Warn,
                "i915 GT WARN_ONCE",
                file!(),
                line!(),
                &__message,
            );
        }
        let _ = __gt;
        __condition
    }};
}

macro_rules! gt_WARN_ON_ONCE {
    ($gt:expr, $condition:expr $(,)?) => {{
        static __WARNED: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        let __gt = $gt;
        let __condition = $condition;
        if __condition && !__WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            ::tk_linux_kapi::print::drm_log_at(
                ::tk_linux_kapi::print::DrmLogLevel::Warn,
                "i915 GT WARN_ON_ONCE",
                file!(),
                line!(),
                "condition was true",
            );
        }
        let _ = __gt;
        __condition
    }};
}

macro_rules! GEM_WARN_ON {
    ($condition:expr $(,)?) => {{ WARN_ON!($condition) }};
}

macro_rules! GEM_DEBUG_WARN_ON {
    ($condition:expr $(,)?) => {{
        if crate::linux_config::CONFIG_DRM_I915_DEBUG_GEM {
            WARN_ON!($condition)
        } else {
            false
        }
    }};
}

macro_rules! list_next_entry {
    ($entry:expr,signal_link) => {{
        let entry = ($entry) as *mut $crate::i915_request_types_upstream::I915Request;
        let member = unsafe { core::ptr::addr_of_mut!((*entry).signal_link) }
            as *mut ::tk_linux_kapi::types::ListHead;
        let offset = (member as usize).wrapping_sub(entry as usize);
        let next = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*member).next)) };
        ::tk_linux_kapi::list::entry_from_link::<$crate::i915_request_types_upstream::I915Request>(
            next, offset,
        )
    }};
    ($entry:expr,parallel.children.child_link) => {{
        let entry = ($entry) as *mut $crate::intel_context_types_upstream::IntelContext;
        let member = unsafe { core::ptr::addr_of_mut!((*entry).parallel.children.child_link) }
            as *mut ::tk_linux_kapi::types::ListHead;
        let offset = (member as usize).wrapping_sub(entry as usize);
        let next = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*member).next)) };
        unsafe {
            &mut *::tk_linux_kapi::list::entry_from_link::<
                $crate::intel_context_types_upstream::IntelContext,
            >(next, offset)
        }
    }};
    ($entry:expr,sched.link) => {{
        let entry = ($entry) as *mut $crate::i915_request_types_upstream::I915Request;
        let member = unsafe { core::ptr::addr_of_mut!((*entry).sched.link) }
            as *mut ::tk_linux_kapi::types::ListHead;
        let offset = (member as usize).wrapping_sub(entry as usize);
        let next = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*member).next)) };
        ::tk_linux_kapi::list::entry_from_link::<$crate::i915_request_types_upstream::I915Request>(
            next, offset,
        )
    }};
}

macro_rules! list_prev_entry {
    ($entry:expr,signal_link) => {{
        let entry = ($entry) as *mut $crate::i915_request_types_upstream::I915Request;
        let member = unsafe { core::ptr::addr_of_mut!((*entry).signal_link) }
            as *mut ::tk_linux_kapi::types::ListHead;
        let offset = (member as usize).wrapping_sub(entry as usize);
        let prev = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*member).prev)) };
        ::tk_linux_kapi::list::entry_from_link::<$crate::i915_request_types_upstream::I915Request>(
            prev, offset,
        )
    }};
}

macro_rules! priolist_for_each_request_consume {
    ($request:ident, $next:ident, $plist:expr, $body:block) => {{
        let mut $request: *mut $crate::i915_request_types_upstream::I915Request =
            core::ptr::null_mut();
        let mut $next: *mut $crate::i915_request_types_upstream::I915Request =
            core::ptr::null_mut();
        let __priolist =
            ($plist) as *const _ as *mut $crate::i915_scheduler_types_upstream::I915Priolist;
        list_for_each_entry_safe!(
            $request,
            $next,
            unsafe { core::ptr::addr_of_mut!((*__priolist).requests) },
            sched.link,
            $body
        );
    }};
}

macro_rules! priolist_for_each_request {
    ($request:ident, $plist:expr, $body:block) => {{
        let mut $request: *mut $crate::i915_request_types_upstream::I915Request =
            core::ptr::null_mut();
        let __priolist =
            ($plist) as *const _ as *mut $crate::i915_scheduler_types_upstream::I915Priolist;
        list_for_each_entry!(
            $request,
            unsafe { core::ptr::addr_of_mut!((*__priolist).requests) },
            sched.link,
            $body
        );
    }};
}

macro_rules! for_each_child_safe {
    ($parent:ident, $child:ident, $next:ident, $body:block) => {{
        let mut $child: *mut $crate::intel_context_types_upstream::IntelContext =
            core::ptr::null_mut();
        let mut $next: *mut $crate::intel_context_types_upstream::IntelContext =
            core::ptr::null_mut();
        let $parent =
            ($parent) as *const _ as *mut $crate::intel_context_types_upstream::IntelContext;
        let head = unsafe {
            core::ptr::addr_of_mut!((*$parent).parallel.children.child_list)
                as *mut ::tk_linux_kapi::types::ListHead
        };
        list_for_each_entry_safe!($child, $next, head, parallel.children.child_link, $body);
    }};
}

macro_rules! for_each_child {
    ($parent:ident, $child:ident, $($body:tt)+) => {{
        let mut $child: *mut $crate::intel_context_types_upstream::IntelContext = core::ptr::null_mut();
        let $parent = ($parent) as *const _ as *mut $crate::intel_context_types_upstream::IntelContext;
        let head = unsafe {
            core::ptr::addr_of!((*$parent).parallel.children.child_list)
                as *mut ::tk_linux_kapi::types::ListHead
        };
        list_for_each_entry!($child, head, parallel.children.child_link, { $($body)+ });
    }};
}

macro_rules! wait_for {
    ($condition:expr, $timeout_ms:expr $(,)?) => {{
        ::tk_linux_kapi::wait::wait_until(
            ($timeout_ms as u64).saturating_mul(1_000_000),
            || $condition,
            true,
        )
    }};
}

macro_rules! wait_for_atomic_us {
    ($condition:expr, $timeout_us:expr $(,)?) => {{
        ::tk_linux_kapi::wait::wait_until(
            ($timeout_us as u64).saturating_mul(1_000),
            || $condition,
            false,
        )
    }};
}
