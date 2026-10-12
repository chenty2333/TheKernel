// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Linux DRM core for x86_64: type definitions (`*_types`) mirroring the
//! Linux 7.2.3 headers field-for-field, and function ports (`*_upstream`)
//! mirroring the Linux `.c` files named in each module header.
//!
//! The crate is `no_std`; host unit tests run under `cfg(test)`.
#![no_std]
#![feature(c_variadic)]
// Machine-translated from Linux C (see each module header). Unsafe blocks mirror
// the C statements and carry `// upstream:` provenance rather than per-block
// SAFETY prose, the same policy tk-intel-gt applies to its i915 ports.
#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::too_many_arguments,
    // C-shaped signatures and `goto`-as-loop control flow are kept as written upstream.
    clippy::not_unsafe_ptr_arg_deref,
    clippy::macro_metavars_in_unsafe,
    clippy::never_loop,
    clippy::absurd_extreme_comparisons,
    clippy::erasing_op,
    clippy::empty_line_after_doc_comments
)]

extern crate alloc;

#[cfg(test)]
extern crate std;

// Type modules: generated from the oracle DWARF layout; see NOTICE.
pub mod dma_resv_types;
pub mod drm_atomic_types;
pub mod drm_auth_types;
pub mod drm_client_types;
pub mod drm_color_mgmt_types;
pub mod drm_connector_types;
pub mod drm_crtc_types;
pub mod drm_damage_helper_types;
pub mod drm_debugfs_crc_types;
pub mod drm_device_types;
pub mod drm_displayid_internal_types;
pub mod drm_dp_helper_types;
pub mod drm_dp_types;
pub mod drm_drv_types;
pub mod drm_edid_types;
pub mod drm_encoder_types;
pub mod drm_fb_helper_types;
pub mod drm_file_types;
pub mod drm_format_helper_types;
pub mod drm_fourcc_types;
pub mod drm_framebuffer_types;
pub mod drm_gem_atomic_helper_types;
pub mod drm_gem_types;
pub mod drm_ioctl_types;
pub mod drm_mm_types;
pub mod drm_mode_config_types;
pub mod drm_mode_object_types;
pub mod drm_modes_types;
pub mod drm_modeset_helper_vtables_types;
pub mod drm_modeset_lock_types;
pub mod drm_plane_types;
pub mod drm_prime_types;
pub mod drm_print_types;
pub mod drm_property_types;
pub mod drm_rect_types;
pub mod drm_vblank_types;
pub mod drm_vblank_work_types;
pub mod drm_vma_manager_types;
pub mod fb_types;
pub mod hdmi_types;
pub mod iosys_map_types;
pub mod kernel_placeholder_types;
pub mod kmsg_dump_types;
pub mod uapi_drm_mode_types;
pub mod uapi_drm_types;

// Header-only uapi constants and function-like macros (see each module header).
pub mod uapi_drm_consts;
pub mod uapi_drm_macros;

// Function-port scaffolds (empty until their batch lands).
pub mod drm_atomic_upstream;
pub mod drm_atomic_helper_upstream;
pub mod drm_atomic_state_helper_upstream;
pub mod drm_atomic_uapi_upstream;
pub mod drm_blend_upstream;
pub mod drm_client_upstream;
pub mod drm_client_event_upstream;
pub mod drm_client_modeset_upstream;
pub mod drm_client_setup_upstream;
pub mod drm_color_mgmt_upstream;
pub mod drm_connector_upstream;
pub mod drm_crtc_upstream;
pub mod drm_damage_helper_upstream;
pub mod drm_displayid_upstream;
pub mod drm_dp_dual_mode_helper_upstream;
pub mod drm_dp_helper_upstream;
pub mod drm_drv_upstream;
pub mod drm_edid_upstream;
pub mod drm_eld_upstream;
pub mod drm_encoder_upstream;
pub mod drm_fb_helper_upstream;
pub mod drm_fbdev_client_upstream;
pub mod drm_file_upstream;
pub mod drm_fourcc_upstream;
pub mod drm_framebuffer_upstream;
pub mod drm_gem_upstream;
pub mod drm_gem_atomic_helper_upstream;
pub mod drm_gem_framebuffer_helper_upstream;
pub mod drm_hdmi_helper_upstream;
pub mod drm_hdmi_state_helper_upstream;
pub mod drm_ioctl_upstream;
pub mod drm_managed_upstream;
pub mod drm_mode_config_upstream;
pub mod drm_mode_object_upstream;
pub mod drm_modes_upstream;
pub mod drm_modeset_helper_upstream;
pub mod drm_modeset_lock_upstream;
pub mod drm_plane_upstream;
pub mod drm_plane_helper_upstream;
pub mod drm_prime_upstream;
pub mod drm_print_upstream;
pub mod drm_probe_helper_upstream;
pub mod drm_property_upstream;
pub mod drm_rect_upstream;
pub mod drm_scdc_helper_upstream;
pub mod drm_vblank_upstream;
pub mod drm_vblank_work_upstream;
pub mod hdmi_upstream;

// TEMPORARY batch B1b stubs for not-yet-landed owners (see pending_stubs/).
pub mod pending_stubs;

/// Linux 7.2.3 renamed  to 
/// (include/drm/drm_atomic.h); the old name is kept for call-site readability.
pub use drm_atomic_types::DrmAtomicCommit as DrmAtomicState;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod b1b_tests;

/// Host-test helpers shared by the batch test modules (test drm_device setup).
#[cfg(test)]
pub mod test_support;
