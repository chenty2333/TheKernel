// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! N305 GT/media A0 is independent of display D0. No implicit hardware access,
//! firmware load or userspace command stream. Kernel intel.gt=1 owns invocation.
#![no_std]
#![deny(unsafe_code)]
extern crate alloc;
#[cfg(test)]
extern crate std;
#[cfg(feature = "upstream-gt")]
pub mod linux_config;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[macro_use]
mod linux_macros;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[macro_use]
pub(crate) mod linux_heap;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
#[macro_use]
pub(crate) mod linux_list;
pub mod bcs;
pub mod cache;
pub mod guc_ads;
pub mod guc_capture;
pub mod guc_config;
pub mod guc_ct;
pub mod guc_fw;
pub mod guc_log;
pub mod guc_submission;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod guc_submission_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_breadcrumbs_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_context_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_engine_cs_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_lrc_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_execlists_submission_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_timeline_upstream;
#[cfg(feature = "upstream-gt")]
#[allow(unsafe_code)]
pub mod intel_workarounds_upstream;
pub mod execlists;
pub mod huc;
pub mod info;
pub mod intel_ring;
pub mod lrc;
pub mod ppgtt;
pub mod rcs;
pub mod rcs_page;
pub mod reset;
pub mod uc;
pub mod uncore;
pub mod wopcm;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Unavailable(u32),
    Timeout(u32),
    Refused,
    Quarantined,
}
/// One exclusive GT owner. MMIO preserves ordering; an error may have landed.
/// Timing is monotonic, delays bounded. No default implementation or raw uAPI.
pub trait GtIo {
    fn read(&self, offset: u32) -> Result<u32, Error>;
    fn write(&self, offset: u32, value: u32) -> Result<(), Error>;
    fn now_us(&self) -> u64;
    fn delay_us(&self, micros: u32);
}
pub fn masked_enable(bits: u32) -> u32 {
    (bits << 16) | bits
}
pub fn masked_disable(bits: u32) -> u32 {
    bits << 16
}
pub(crate) fn wait(
    io: &impl GtIo,
    reg: u32,
    mask: u32,
    value: u32,
    micros: u64,
) -> Result<u32, Error> {
    let start = io.now_us();
    for _ in 0..1_000_000 {
        let raw = io.read(reg)?;
        if raw == u32::MAX {
            return Err(Error::Unavailable(reg));
        }
        if raw & mask == value {
            return Ok(raw);
        }
        if io.now_us().saturating_sub(start) >= micros {
            return Err(Error::Timeout(reg));
        }
        io.delay_us(1);
    }
    Err(Error::Timeout(reg))
}
