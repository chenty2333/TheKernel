//! Intel e1000 shared driver port.

pub mod api;
pub mod chip82540;
pub mod chip82541;
pub mod mac;
pub mod manage;
pub mod nic;
pub mod nvm;
pub mod osdep;
pub mod phy;
pub mod registers;

pub use nic::{E1000Hal, E1000Nic, MAX_FRAME_BYTES, RX_BUFFER_BYTES};
