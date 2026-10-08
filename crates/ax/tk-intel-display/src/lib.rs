// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Display-12/13 i915 translations. Callers still own platform admission and writes.
//! Readout is not ownership of firmware scanout, nor proof of monitor pixels.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod audio;
pub mod cdclk;
pub mod color;
pub mod dc_state;
pub mod ddi;
pub mod ddi_buf_trans;
pub mod device;
pub mod display;
pub mod dkl_phy;
pub mod dmc;
pub mod dp_aux;
pub mod dpll;
pub mod dpll_mgr;
pub mod intel_dpll_mgr_full;
pub mod hdmi;
pub mod hdmi_packet;
pub mod intel_bios;
pub mod intel_atomic_full;
pub mod intel_audio_dp_full;
pub mod intel_audio_legacy_remainder;
pub mod intel_cdclk_full;
pub mod intel_color_full;
pub mod intel_cursor_full;
pub mod intel_crtc_full;
pub mod intel_fb_full;
pub mod intel_fbc_full;
pub mod intel_modeset_verify_full;
pub mod intel_modeset_setup_full;
pub mod intel_display_modeset_full;
pub mod intel_ddi_full;
pub mod intel_dp_link_training_full;
pub mod intel_dp_mst_full;
pub mod intel_psr_full;
pub mod intel_pcode_full;
pub mod intel_dp_full;
pub mod intel_gmbus_full;
pub mod intel_hdmi_full;
pub mod intel_hotplug_full;
pub mod intel_hotplug_irq_full;
pub mod intel_vblank_full;
pub mod opregion;
pub mod pipe_config;
pub mod power_domains;
pub mod power_map;
pub mod power_well;
pub mod scaler;
pub mod skl_scaler_full;
pub mod skl_universal_plane_full;
pub mod skl_watermark_full;
pub mod tc;
pub mod tc_state_machine;
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
