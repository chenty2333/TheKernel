//! Thin DRM UAPI adapter.
//!
//! Linux DRM wire layouts, constants, ioctl encodings, and decoding are
//! owned by `thekernel-linux-drm`. The kernel only couples those records to
//! usercopy and device enactment.

pub use thekernel_linux_drm::*;
