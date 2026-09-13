//! The probe's report, as a file in the DRM debug filesystem.
//!
//! The target machine has no serial port and no other console than the screen
//! the kernel draws on, so a diagnostic that only reaches the log is a
//! diagnostic that is gone as soon as the log scrolls.  The report is therefore
//! served from the same place the kernel's other DRM diagnostics live --
//! `/sys/kernel/debug/dri/0` -- as a plain read-only file:
//!
//! ```text
//! cat /sys/kernel/debug/dri/0/intel_gpu
//! ```
//!
//! The file holds the text of the *boot* probe, not a fresh probe: the device
//! is discovered once, and a diagnostic read must not become a source of
//! hardware traffic or of state changes.  Every line is prefixed so that a
//! reader who pipes the file into a log can grep for one word to find it.

use alloc::string::String;

use axfs_ng_vfs::VfsResult;

/// Render the boot probe's report.
///
/// This is registered as a `SimpleFile` closure, which regenerates the text on
/// every read and honours the caller's offset, so `cat`, `head` and a partial
/// read all behave the way they do for any other file.
pub(crate) fn report() -> VfsResult<String> {
    Ok(super::report_text())
}
