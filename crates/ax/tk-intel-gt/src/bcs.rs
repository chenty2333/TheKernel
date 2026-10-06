// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gem/selftests/i915_gem_client_blt.c:
// prepare_blit (Gen12 linear32 fast-copy branch), Copyright © 2019 Intel.
// gt/gen8_engine_cs.c/.h: gen12_emit_flush_xcs, gen8_emit_bb_start_noarb,
// __gen8_emit_flush_dw/gen8_emit_ggtt_write and no-preemption breadcrumb tail.
// Copyright © 2014 Intel Corporation. Full MIT grant: ../LICENSE-MIT.
// Original bounded kernel-owned copy plan; no arbitrary userspace batch API.
use crate::{Error, GtIo, lrc};
fn mi(op: u32, flags: u32) -> u32 {
    (op << 23) | flags
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Copy {
    pub source: u64,
    pub destination: u64,
    pub source_bytes: u64,
    pub destination_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
}
impl Copy {
    pub fn validate(self) -> Result<(), Error> {
        if self.width == 0
            || self.height == 0
            || self.width > 16383
            || self.height > 65535
            || self.pitch > 32767
            || !self.pitch.is_multiple_of(4)
            || self.pitch < self.width * 4
            || !self.source.is_multiple_of(4)
            || !self.destination.is_multiple_of(4)
        {
            return Err(Error::Refused);
        }
        let span = u64::from(self.height - 1) * u64::from(self.pitch) + u64::from(self.width) * 4;
        let src = self.source.checked_add(span).ok_or(Error::Refused)?;
        let dst = self.destination.checked_add(span).ok_or(Error::Refused)?;
        if src > 1 << 48
            || dst > 1 << 48
            || span > self.source_bytes
            || span > self.destination_bytes
            || (self.source < dst && self.destination < src)
        {
            return Err(Error::Refused);
        }
        Ok(())
    }
}
/// 14 words: source's BLIT_CCTL LRI +10-word copy +BB END. The caller verifies
/// MOCS3 is genuinely UC and the object extents belong to its private PPGTT.
pub fn batch(copy: Copy) -> Result<[u32; 14], Error> {
    copy.validate()?;
    Ok([
        mi(0x22, 1),
        0x22204,
        6 | (6 << 8),
        (2 << 29) | (0x42 << 22) | 8,
        (3 << 24) | copy.pitch,
        0,
        (copy.height << 16) | copy.width,
        copy.destination as u32,
        (copy.destination >> 32) as u32,
        0,
        copy.pitch,
        copy.source as u32,
        (copy.source >> 32) as u32,
        mi(0xa, 0),
    ])
}
/// Source Gen12 flush/TLB/preparser/AUX, noarb batch start, stalling flush,
/// explicit GGTT breadcrumb, IRQ (masked by adapter), ARB-enable and WA tail.
/// Single-context polling deliberately has no scheduler/preempt semaphores.
pub fn ring(out: &mut [u32], batch: u64, context: u32, seqno: u32) -> Result<usize, Error> {
    if out.len() < 32
        || batch >= 1 << 48
        || !batch.is_multiple_of(8)
        || context == 0
        || !context.is_multiple_of(4096)
        || context.checked_add(4 * 4096).is_none()
        || seqno == 0
    {
        return Err(Error::Refused);
    }
    out[..32].fill(0);
    out[0] = mi(5, 0) | (1 << 8) | 1; // preparser_disable(true)
    out[1..5].copy_from_slice(&[
        mi(0x26, 2) | (1 << 21) | (1 << 14) | (1 << 18) | (1 << 16),
        lrc::SCRATCH,
        0,
        0,
    ]);
    out[5..13].copy_from_slice(&lrc::aux_invalidate());
    out[13] = mi(5, 0) | (1 << 8);
    out[14..18].copy_from_slice(&[
        mi(8, 0),
        mi(0x31, 1) | (1 << 8),
        batch as u32,
        (batch >> 32) as u32,
    ]);
    // gen12 xcs requires a separate stalling flush before post-sync breadcrumb.
    out[18..22].copy_from_slice(&[mi(0x26, 2), 0, 0, 0]);
    out[22..26].copy_from_slice(&[
        mi(0x26, 2) | (1 << 14),
        (context + lrc::SCRATCH) | 4,
        0,
        seqno,
    ]);
    out[26] = mi(2, 0);
    out[27] = mi(8, 0) | 1;
    // Source preemption point + NOOP WA tail; 30 dwords remain aligned8.
    out[28] = mi(5, 0);
    Ok(30)
}

/// N305's relevant shared GT workarounds and the sole UC policy used by this
/// copy. Caller owns GT+render wake, has stopped BCS, and proved firmware RCS
/// idle. From intel_workarounds.c::icl_wa_init_mcr/gen12_gt_workarounds_init,
/// intel_gtt.c::tgl_setup_private_ppat and intel_mocs.c Gen12 UC index3.
/// Copyright © 2014-2018/2020/2015 Intel. Full grant: ../LICENSE-MIT.
/// Media IECP clock-gate workaround is not applied: no media engine is used
/// or reset. RCS context/engine WAs are not a prerequisite for BCS commands.
pub fn prepare(io: &impl GtIo) -> Result<(), Error> {
    let slices = io.read(0x9138)? & 0xff;
    let dss = io.read(0x913c)?;
    if slices != 1 || dss == 0 || dss & !0x3f != 0 {
        return Err(Error::Refused);
    }
    // Lowest enabled DSS is powered in minconfig (icl_wa_init_mcr), not a
    // guessed slice. Our serialized owner restores this shared selector.
    let before = io.read(0xfdc)?;
    let selected = (before & !0x7f000000) | (dss.trailing_zeros() << 24);
    // Source multicast write assumes multicast already enabled. Refuse an
    // unexpected foreign unicast agent instead of silently updating one DSS.
    if before & (1 << 31) == 0 {
        return Err(Error::Refused);
    }
    let updated = (|| {
        io.write(0xfdc, selected)?;
        if io.read(0xfdc)? != selected {
            return Err(Error::Refused);
        }
        let dfr = io.read(0x9550)?;
        io.write(0x9550, dfr | (1 << 9))?; // Wa_14011059788
        if io.read(0x9550)? & (1 << 9) == 0 {
            return Err(Error::Refused);
        }
        Ok(())
    })();
    let restored = (|| {
        io.write(0xfdc, before)?;
        if io.read(0xfdc)? != before {
            return Err(Error::Quarantined);
        }
        Ok(())
    })();
    restored.map_err(|_| Error::Quarantined)?;
    updated?;
    // Wa_14015795083: source intentionally does not verify this register,
    // because firmware may lock it. Do not invent an unlock sequence.
    let misc = io.read(0x9424)?;
    io.write(0x9424, misc & !2)?;
    for (r, v) in [(0x480c, 0), (0x400c, 5)] {
        io.write(r, v)?;
        if io.read(r)? != v {
            return Err(Error::Refused);
        }
    }
    let l3 = io.read(0xb024)?;
    let value = (l3 & 0xffff) | (0x10 << 16);
    io.write(0xb024, value)?;
    if io.read(0xb024)? != value {
        return Err(Error::Refused);
    }
    apply_nonpriv(io, false)
}

/// Strict admission for one snapshotted linear fast-copy and END. The user
/// batch is NEVER executed: the native adapter rebuilds this verified plan.
/// Only the three bounded private-VM windows currently implemented are exposed.
pub fn decode_copy(
    words: &[u32; 11],
    source_size: u64,
    destination_size: u64,
) -> Result<Copy, Error> {
    let source = u64::from(words[8]) | (u64::from(words[9]) << 32);
    let destination = u64::from(words[4]) | (u64::from(words[5]) << 32);
    let source_offset = source.checked_sub(0x10000).ok_or(Error::Refused)?;
    let destination_offset = destination.checked_sub(0x20000).ok_or(Error::Refused)?;
    if source_size > 65536 || destination_size > 65536 {
        return Err(Error::Refused);
    }
    let copy = Copy {
        source,
        destination,
        source_bytes: source_size
            .checked_sub(source_offset)
            .ok_or(Error::Refused)?,
        destination_bytes: destination_size
            .checked_sub(destination_offset)
            .ok_or(Error::Refused)?,
        width: words[3] & 0xffff,
        height: words[3] >> 16,
        pitch: words[1] & 0xffff,
    };
    if &batch(copy)?[3..] != words {
        return Err(Error::Refused);
    }
    Ok(copy)
}

/// Linux7.2.3 intel_workarounds.c tgl_whitelist_build/allow_read_ctx_timestamp
/// and intel_engine_apply_whitelist. Copyright ©2014-2018 Intel Corporation;
/// full MIT grant ../LICENSE-MIT. Exact Gen12.0 RCS/BCS only.
/// The encoded addresses are ordered as source _wa_add, including access bits.
pub fn apply_nonpriv(io: &impl GtIo, render: bool) -> Result<(), Error> {
    let base = if render { 0x2000 } else { 0x22000 };
    let render_regs = [0x7010, 0x7018, 0x7304, 0x10002349];
    let copy_regs = [0x100223a8];
    let regs: &[u32] = if render { &render_regs } else { &copy_regs };
    for index in 0..12 {
        let value = regs.get(index).copied().unwrap_or(base + 0x94);
        let register = base + 0x4d0 + (index as u32) * 4;
        io.write(register, value)?;
        // Safety addition: refuse failed stores/readback; no wider whitelist
        // or permissive retry. Caller retains reset/ownership on error.
        if io.read(register)? != value {
            return Err(Error::Refused);
        }
    }
    Ok(())
}
