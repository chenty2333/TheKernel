//! Small, OS-neutral GPU and display coordination mechanisms.
//!
//! This crate intentionally owns neither device discovery nor memory mapping.
//! An adapter supplies those policies through [`DisplayAdapter`] and
//! [`ResourceProvider`]; all public identifiers are opaque capabilities rather
//! than an operating-system ABI.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod atomic;
mod fence;
mod property;
mod resource;
mod scanout;
mod sync;

pub use atomic::{
    AtomicError, AtomicPlanner, CommitPlan, DisplayAdapter, FrameLayout, PlaneState,
    PresentationState,
};
pub use fence::{Fence, Reservation};
pub use property::{Property, PropertyKind, PropertyValue};
pub use resource::{ResourceDescriptor, ResourceError, ResourceHandle, ResourceProvider};
pub use scanout::{DisplayLimits, Mode, ScanoutId};
pub use sync::{SyncObject, SyncSnapshot};
