// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/intel_uncore.c: fw_domain_reset,
// wait_ack_{set,clear}, fw_domain_wait_ack_with_fallback, fw_domain_{get,put}.
// Copyright © 2013 Intel Corporation. Full grant: ../LICENSE-MIT.
// Selected N305 GT/render and fused VDBOX0/2/VEBOX0 domains; no runtime PM.
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

/// ADL-P/ADL-N source platform mask permits VCS0,VCS2,VECS0; the live
/// GEN11_GT_VEBOX_VDBOX_DISABLE fuse removes absent instances. These domains
/// are woken only to exclude unowned media activity before shared cache writes.
/// Tuple: disable bit, request, acknowledge, engine MI_MODE. No media reset/job.
pub const MEDIA: [(u32, u32, u32, u32); 3] = [
    (1, 0xa540, 0xd50, 0x1c009c),
    (4, 0xa548, 0xd58, 0x1d009c),
    (1 << 16, 0xa560, 0xd70, 0x1c809c),
];
pub fn media_mask(disabled: u32) -> u8 {
    MEDIA
        .iter()
        .enumerate()
        .fold(0, |mask, (index, (bit, ..))| {
            mask | if disabled & bit == 0 { 1 << index } else { 0 }
        })
}
pub fn acquire_media(io: &impl GtIo, index: usize) -> Result<(), Error> {
    let &(_, request, ack, _) = MEDIA.get(index).ok_or(Error::Refused)?;
    acquire(io, request, ack)
}
/// Source intel_engine_cs.c::ring_is_idle hardware checks. The controller
/// and software submission queues must already be excluded by the caller.
pub fn ring_idle(io: &impl GtIo, base: u32) -> Result<bool, Error> {
    if ![0x2000, 0x1c0000, 0x1d0000, 0x1c8000].contains(&base) {
        return Err(Error::Refused);
    }
    let head = io.read(base + 0x34)? & 0x001ffffc;
    let tail = io.read(base + 0x30)? & 0x001ffff8;
    let mode = io.read(base + 0x9c)?;
    Ok(head == tail && mode & (1 << 9) != 0)
}
/// Sole controller owner retains every acquired domain. Idle proof is required
/// even though this driver never submits media work: PAT/MOCS/L3 are shared.
pub fn idle_media(io: &impl GtIo, present: u8, owned: u8) -> Result<(), Error> {
    if present & !7 != 0 || owned & present != present {
        return Err(Error::Refused);
    }
    for (index, (_, _, ack, mode)) in MEDIA.iter().enumerate() {
        if present & (1 << index) != 0
            && (io.read(*ack)? & KERNEL == 0 || !ring_idle(io, *mode - 0x9c)?)
        {
            return Err(Error::Refused);
        }
    }
    Ok(())
}
