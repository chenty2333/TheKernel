//! Original iTCO mechanism. Facts from Linux 7.2.3 iTCO_wdt.c/i2c-i801.c/lpc_ich.c.
//! Only Intel ICH9 (v2) and measured Alder Lake-M SMBus (v6) are admitted.
#![no_std]
pub mod bringup;
pub mod ids;
pub mod probe;
pub mod regs;
#[cfg(test)]
extern crate std;
#[cfg(test)]
mod fake;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Locked,
    Io,
    BadState,
}
pub type Result<T = ()> = core::result::Result<T, Error>;
