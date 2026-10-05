// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! ADL-P/N display-only i915 translations. No platform mapping or implicit writes.
//! Readout is not ownership of firmware scanout, nor proof of monitor pixels.
#![no_std]
#![forbid(unsafe_code)]

pub mod bios;
pub mod cdclk;
pub mod color;
pub mod ddi;
pub mod device;
pub mod display;
pub mod dkl_phy;
pub mod dpll_mgr;
pub mod hdmi;
pub mod hdmi_packet;
pub mod opregion;
pub mod pipe_config;
pub mod scaler;
pub mod tc;
pub mod universal_plane;
pub mod watermark;

/// Backend access failure: unavailable/powered-off registers never become zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Truncated,
    InvalidHeader,
    InvalidBlock,
    UnsupportedVersion,
    UnsupportedDevice,
    Unavailable(u32),
    Refused,
    /// Recovery could not be verified; the caller must quarantine the device.
    RestoreFailed(u32),
}

fn bytes(data: &[u8], offset: usize, length: usize) -> Result<&[u8], Error> {
    let end = offset.checked_add(length).ok_or(Error::Truncated)?;
    data.get(offset..end).ok_or(Error::Truncated)
}
fn le16(data: &[u8], offset: usize) -> Result<u16, Error> {
    Ok(u16::from_le_bytes(
        bytes(data, offset, 2)?.try_into().unwrap(),
    ))
}
fn le32(data: &[u8], offset: usize) -> Result<u32, Error> {
    Ok(u32::from_le_bytes(
        bytes(data, offset, 4)?.try_into().unwrap(),
    ))
}
fn le64(data: &[u8], offset: usize) -> Result<u64, Error> {
    Ok(u64::from_le_bytes(
        bytes(data, offset, 8)?.try_into().unwrap(),
    ))
}

/// Raw offsets are local to this audited display-13 port, not arbitrary userspace.
/// Implementors preserve MMIO ordering; writes must return errors even if a store
/// may have landed (the outer transaction owns recovery). No default backend.
pub trait RegisterIo {
    fn read32(&self, offset: u32) -> Result<u32, Error>;
    fn write32(&self, offset: u32, value: u32) -> Result<(), Error>;
}
