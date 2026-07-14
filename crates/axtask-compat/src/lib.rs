//! Transitional package-name facade for downstream ArceOS crates.
//!
//! Registry crates such as `axsync` still depend on the historical `axtask`
//! package name. TheKernel itself and all maintained manifests use the
//! `thekernel-axtask` package directly. This facade forwards features and
//! re-exports the exact same crate instance, avoiding a second scheduler or a
//! second set of task globals while those external manifests catch up.

#![no_std]

pub use axtask_core::*;

#[cfg(feature = "multitask")]
extern crate alloc;

/// Legacy current-task view used only by unported registry consumers.
///
/// The maintained `thekernel-axtask` API keeps `id_name()` fallible because
/// formatting a task name allocates. Historical `axsync` uses that value only
/// in invariant diagnostics and requires the old displayable return type.
#[cfg(feature = "multitask")]
pub struct CurrentTask(axtask_core::CurrentTask);

#[cfg(feature = "multitask")]
impl CurrentTask {
    /// Returns a diagnostic task name, preserving the historical facade API.
    /// Allocation failure is made visible in the diagnostic text; maintained
    /// consumers use the core crate and receive `TaskNameError` directly.
    pub fn id_name(&self) -> alloc::string::String {
        self.0
            .id_name()
            .unwrap_or_else(|_| alloc::string::String::from("<task-name-unavailable>"))
    }
}

#[cfg(feature = "multitask")]
impl core::ops::Deref for CurrentTask {
    type Target = axtask_core::CurrentTask;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Returns the current task through the historical package facade.
///
/// New code should depend on `thekernel-axtask` directly so fallible APIs are
/// not narrowed by compatibility shims.
#[cfg(feature = "multitask")]
pub fn current() -> CurrentTask {
    CurrentTask(axtask_core::current())
}
