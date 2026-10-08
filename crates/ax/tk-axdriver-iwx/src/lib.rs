//! Intel iwx wireless driver components.
//!
//! The implementation is based on OpenBSD's ISC-licensed iwx driver. The
//! framework-facing portions are mapped to TheKernel driver interfaces.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod config;
mod firmware;

pub use config::{DeviceConfig, FirmwareConfig, RuntimeConfig, lookup_config};
pub use firmware::{FirmwareError, FirmwareImage, FirmwareSection, SectionType};
