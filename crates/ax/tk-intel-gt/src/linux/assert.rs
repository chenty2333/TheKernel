// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// Linux/i915 assertion semantics for the source-order GT translation. In
// particular, GEM_BUG_ON is not erased: a violated source invariant panics.

#![allow(unsafe_code, non_snake_case)]

/// Linux lockdep assertions compile to no-ops in the recorded wt-dev kernel
/// configuration (`CONFIG_LOCKDEP=n`). If this compatibility layer is ever
/// built against a configuration with lockdep enabled, failing loudly is safer
/// than silently claiming that lock ownership was checked.
#[inline]
pub fn lockdep_assert_held<T: ?Sized>(_lock: &T) {
    if crate::linux_config::CONFIG_LOCKDEP {
        panic!("CONFIG_LOCKDEP enabled but LinuxKPI lock ownership tracking is not implemented");
    }
}

#[inline]
pub fn lockdep_assert_not_held<T: ?Sized>(_lock: &T) {
    if crate::linux_config::CONFIG_LOCKDEP {
        panic!("CONFIG_LOCKDEP enabled but LinuxKPI lock ownership tracking is not implemented");
    }
}

#[inline]
pub fn lockdep_assert_held_once<T: ?Sized>(lock: &T) {
    lockdep_assert_held(lock);
}

#[inline]
pub fn lockdep_assert_held_write<T: ?Sized>(lock: &T) {
    lockdep_assert_held(lock);
}

/// Runtime equivalent of Linux `WARN_ON`: evaluate once, report the source
/// location when true, and return the condition to preserve the C macro's
/// expression semantics.
#[track_caller]
pub fn warn_on(condition: bool) -> bool {
    if condition {
        let location = core::panic::Location::caller();
        axlog::warn!("i915 WARN_ON at {}:{}", location.file(), location.line());
    }
    condition
}

/// i915's `MISSING_CASE()` reports unsupported switch values without changing
/// control flow, as the Linux helper does.
#[allow(non_snake_case)]
pub fn MISSING_CASE<T: core::fmt::Display>(value: T) {
    let location = core::panic::Location::caller();
    axlog::warn!(
        "i915 MISSING_CASE({}) at {}:{}",
        value,
        location.file(),
        location.line()
    );
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

macro_rules! BUG_ON {
    ($condition:expr $(,)?) => {{
        let __condition = $condition;
        if __condition {
            panic!(
                "Linux BUG_ON({}) at {}:{}",
                stringify!($condition),
                file!(),
                line!()
            );
        }
    }};
}

macro_rules! BUG {
    () => {{ panic!("Linux BUG at {}:{}", file!(), line!()) }};
}

macro_rules! WARN_ON {
    ($condition:expr $(,)?) => {{
        let __condition = $condition;
        if __condition {
            axlog::warn!(
                "i915 WARN_ON({}) at {}:{}",
                stringify!($condition),
                file!(),
                line!()
            );
        }
        __condition
    }};
}

macro_rules! WARN_ON_ONCE {
    ($condition:expr $(,)?) => {{
        static __WARNED: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        let __condition = $condition;
        if __condition && !__WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            axlog::warn!(
                "i915 WARN_ON_ONCE({}) at {}:{}",
                stringify!($condition),
                file!(),
                line!()
            );
        }
        __condition
    }};
}

macro_rules! WARN_ONCE {
    ($condition:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        static __WARNED: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        let __condition = $condition;
        if __condition && !__WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            let __message = alloc::format!($format $(, $argument)*);
            $crate::linux_print::drm_log_at(
                $crate::linux_print::DrmLogLevel::Warn,
                "i915 WARN_ONCE",
                file!(),
                line!(),
                &__message,
            );
        }
        __condition
    }};
}

macro_rules! gt_WARN_ONCE {
    ($gt:expr, $condition:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        static __WARNED: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        let __gt = $gt;
        let __condition = $condition;
        if __condition && !__WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            let __args: &[&dyn $crate::linux_print::CFormatArg] = &[
                $(&($argument) as &dyn $crate::linux_print::CFormatArg),*
            ];
            let __message = $crate::linux_print::format_message($format, __args);
            $crate::linux_print::drm_log_at(
                $crate::linux_print::DrmLogLevel::Warn,
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
            $crate::linux_print::drm_log_at(
                $crate::linux_print::DrmLogLevel::Warn,
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

macro_rules! drm_WARN_ON {
    ($device:expr, $condition:expr $(,)?) => {{
        let __device = $device;
        let _ = __device;
        let __condition = $condition;
        if __condition {
            axlog::warn!("i915 drm_WARN_ON at {}:{}", file!(), line!());
        }
        __condition
    }};
}

macro_rules! MISSING_CASE {
    ($value:expr $(,)?) => {{ $crate::linux_assert::MISSING_CASE($value) }};
}

macro_rules! missing_case {
    ($value:expr $(,)?) => {{ MISSING_CASE!($value) }};
}

macro_rules! lockdep_assert_held {
    ($lock:expr $(,)?) => {{ $crate::linux_assert::lockdep_assert_held(&$lock) }};
}

macro_rules! lockdep_assert_not_held {
    ($lock:expr $(,)?) => {{ $crate::linux_assert::lockdep_assert_not_held(&$lock) }};
}
