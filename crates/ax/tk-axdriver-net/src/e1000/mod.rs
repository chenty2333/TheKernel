//! Intel e1000 shared driver port.

pub mod api;
pub mod mac;
pub mod nic;
pub mod osdep;
pub mod registers;

pub use nic::{E1000Hal, E1000Nic, MAX_FRAME_BYTES, RX_BUFFER_BYTES};
