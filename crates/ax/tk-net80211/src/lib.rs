//! The 802.11 station protocol substrate.
//!
//! This crate translates the station-mode Ethernet/802.11 frame conversion
//! performed by OpenBSD net80211 and defines shared wireless frame types.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod frame;
mod rsn;

pub use frame::{DecapError, EthernetFrame, MacAddress, decap_data, encap_station};
pub use rsn::{Akm, Cipher, RsnParams, RsnStatus, parse_akm, parse_cipher, parse_rsn, parse_wpa};
