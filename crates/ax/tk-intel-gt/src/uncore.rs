// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/intel_uncore.c: fw_domain_reset,
// wait_ack_{set,clear}, fw_domain_wait_ack_with_fallback, fw_domain_{get,put}.
// Copyright © 2013 Intel Corporation. Full grant: ../LICENSE-MIT.
// N305 Gen12 GT domain only, not media/render domain enumeration or runtime PM.
use crate::{Error, GtIo, masked_disable, masked_enable, wait};
pub const GT_REQUEST: u32 = 0xa188;
pub const GT_ACK: u32 = 0x130044;
pub const RENDER_REQUEST: u32 = 0xa278;
pub const RENDER_ACK: u32 = 0xd84;
const KERNEL: u32 = 1;
const FALLBACK: u32 = 1 << 15;
fn ack(io: &impl GtIo, ack_reg: u32, bit: u32, set: bool) -> Result<(), Error> {
    wait(io, ack_reg, bit, if set { bit } else { 0 }, 50_000).map(|_| ())
}
fn ack_with_fallback(io: &impl GtIo, request: u32, ack_reg: u32, set: bool) -> Result<(), Error> {
    match ack(io, ack_reg, KERNEL, set) {
        Ok(()) => return Ok(()),
        Err(Error::Timeout(_)) => {}
        Err(e) => return Err(e),
    }
    // WaRsForcewakeAddDelayForAck / HSDES1604254524, source Gen11+ path.
    for pass in 1..=10 {
        ack(io, ack_reg, FALLBACK, false)?;
        io.write(request, masked_enable(FALLBACK))?;
        io.delay_us(10 * pass);
        let result = ack(io, ack_reg, FALLBACK, true).and_then(|()| io.read(ack_reg));
        // Clear a possibly-landed fallback request on every fallible prefix.
        let clear = io.write(request, masked_disable(FALLBACK));
        clear?;
        let raw = result?;
        if raw & KERNEL == if set { KERNEL } else { 0 } {
            return Ok(());
        }
    }
    Err(Error::Timeout(ack_reg))
}
/// Fresh boot ownership, not borrowing an unowned ACK sample. Clear Gen12
/// forcewake request bits per WaRsClearFWBitsAtReset; preserve debug bit12.
/// Only the kernel's explicit fresh-GT initialization may call this once.
fn acquire(io: &impl GtIo, request: u32, ack_reg: u32) -> Result<(), Error> {
    let result = (|| {
        io.write(request, masked_disable(0xefff))?;
        ack_with_fallback(io, request, ack_reg, false)?;
        io.write(request, masked_enable(KERNEL))?;
        ack_with_fallback(io, request, ack_reg, true)
    })();
    if let Err(e) = result {
        if release(io, request, ack_reg).is_err() {
            return Err(Error::Quarantined);
        }
        return Err(e);
    }
    Ok(())
}
/// Caller has proved every own engine/DMA consumer quiescent before releasing.
/// A failed release is terminal, not permission to free pending GPU memory.
fn release(io: &impl GtIo, request: u32, ack_reg: u32) -> Result<(), Error> {
    io.write(request, masked_disable(KERNEL | FALLBACK))?;
    ack_with_fallback(io, request, ack_reg, false)
}

/// Exact Gen12 GT/render domains needed for BCS and its cache-policy registers.
pub fn acquire_gt(io: &impl GtIo) -> Result<(), Error> {
    acquire(io, GT_REQUEST, GT_ACK)
}
pub fn acquire_render(io: &impl GtIo) -> Result<(), Error> {
    acquire(io, RENDER_REQUEST, RENDER_ACK)
}
pub fn release_gt(io: &impl GtIo) -> Result<(), Error> {
    release(io, GT_REQUEST, GT_ACK)
}
pub fn release_render(io: &impl GtIo) -> Result<(), Error> {
    release(io, RENDER_REQUEST, RENDER_ACK)
}
