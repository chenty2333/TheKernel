//! Thin DRM UAPI adapter.
//!
//! Linux DRM wire layouts, constants, ioctl encodings, and decoding are
//! owned by `tk-linux-drm`. The kernel only couples those records to
//! usercopy and device enactment.

pub use tk_linux_drm::*;
