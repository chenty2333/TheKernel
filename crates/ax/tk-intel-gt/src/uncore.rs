// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/intel_uncore.c: fw_domain_reset,
// wait_ack_{set,clear}, fw_domain_wait_ack_with_fallback, fw_domain_{get,put}.
// Copyright © 2013 Intel Corporation. Full grant: ../LICENSE-MIT.
// N305 Gen12 GT domain only, not media/render domain enumeration or runtime PM.
use crate::{Error, GtIo, masked_disable, masked_enable, wait};
pub const GT_REQUEST: u32 = 0xa188;
pub const GT_ACK: u32 = 0x130044;
const KERNEL: u32 = 1;
const FALLBACK: u32 = 1 << 15;
fn ack(io: &impl GtIo, bit: u32, set: bool) -> Result<(), Error> {
    wait(io, GT_ACK, bit, if set { bit } else { 0 }, 50_000).map(|_| ())
}
fn ack_with_fallback(io: &impl GtIo, set: bool) -> Result<(), Error> {
    match ack(io, KERNEL, set) {
        Ok(()) => return Ok(()),
        Err(Error::Timeout(_)) => {}
        Err(e) => return Err(e),
    }
    // WaRsForcewakeAddDelayForAck / HSDES1604254524, source Gen11+ path.
    for pass in 1..=10 {
        ack(io, FALLBACK, false)?;
        io.write(GT_REQUEST, masked_enable(FALLBACK))?;
        io.delay_us(10 * pass);
        let result = ack(io, FALLBACK, true).and_then(|()| io.read(GT_ACK));
        // Clear a possibly-landed fallback request on every fallible prefix.
        let clear = io.write(GT_REQUEST, masked_disable(FALLBACK));
        clear?;
        let raw = result?;
        if raw & KERNEL == if set { KERNEL } else { 0 } {
            return Ok(());
        }
    }
    Err(Error::Timeout(GT_ACK))
}
/// Fresh boot ownership, not borrowing an unowned ACK sample. Clear Gen12
/// forcewake request bits per WaRsClearFWBitsAtReset; preserve debug bit12.
/// Only the kernel's explicit fresh-GT initialization may call this once.
pub fn acquire_gt(io: &impl GtIo) -> Result<(), Error> {
    let result = (|| {
        io.write(GT_REQUEST, masked_disable(0xefff))?;
        ack_with_fallback(io, false)?;
        io.write(GT_REQUEST, masked_enable(KERNEL))?;
        ack_with_fallback(io, true)
    })();
    if let Err(e) = result {
        if release_gt(io).is_err() {
            return Err(Error::Quarantined);
        }
        return Err(e);
    }
    Ok(())
}
/// Caller has proved every own engine/DMA consumer quiescent before releasing.
/// A failed release is terminal, not permission to free pending GPU memory.
pub fn release_gt(io: &impl GtIo) -> Result<(), Error> {
    io.write(GT_REQUEST, masked_disable(KERNEL | FALLBACK))?;
    ack_with_fallback(io, false)
}
