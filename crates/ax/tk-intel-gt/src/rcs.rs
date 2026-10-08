// SPDX-License-Identifier: MIT
// Linux7.2.3 gt/intel_lrc.c gen12_rcs_offsets, init_common_regs,
// init_ppgtt_regs, lrc_update_regs, gen12_emit_indirect_ctx_rcs and predicate WA.
// Copyright © 2014 Intel Corporation. intel_lrc_reg.h: ©2014-2018 Intel.
// intel_sseu.c::intel_sseu_make_rpcs: ©2019 Intel Corporation.
// Full MIT grant: ../LICENSE-MIT. Only Gen12.0 single-slice RCS is selected.
use crate::{Error, lrc, ppgtt};
pub const CONTEXT_PAGES: usize = 16; // source14-page RCS image plus2 WA pages.
pub fn restore_context(
    regs: &mut [u32; 1024],
    indirect: &mut [u32; 1024],
    per_ctx: &mut [u32; 1024],
    context: u32,
    ring: u32,
    tail: u32,
    root: u64,
) -> Result<u64, Error> {
    let mut fresh = [0; 1024];
    let descriptor = build_context(&mut fresh, indirect, per_ctx, context, ring, tail, root)?;
    // Selected lrc_update_regs/init_ppgtt_regs/WA-pointer updates. Never
    // rebuild the GPU-generated restore instruction stream or opaque values.
    for index in [5usize, 7, 9, 11, 19, 21, 23, 49, 51] {
        regs[index] = fresh[index];
    }
    regs[3] = (regs[3] & !1) | (1 << 16); // known valid image: disable restore-inhibit.
    regs[0x61] = (regs[0x61] & !(1 << 8)) | (1 << 24); // source __reset_stop_ring.
    regs[0x43] = fresh[0x43]; // whole fused-slice RPCS.
    Ok(descriptor)
}
pub fn build_context(
    regs: &mut [u32; 1024],
    indirect: &mut [u32; 1024],
    per_ctx: &mut [u32; 1024],
    context: u32,
    ring: u32,
    tail: u32,
    root: u64,
) -> Result<u64, Error> {
    ppgtt::physical(root)?;
    if context == 0
        || !context.is_multiple_of(4096)
        || context.checked_add(16 * 4096).is_none()
        || ring == 0
        || !ring.is_multiple_of(4096)
        || tail >= 4096
        || !tail.is_multiple_of(8)
    {
        return Err(Error::Refused);
    }
    // Common timestamp/predicate semantics, rebuilt below for RCS and its14-page image.
    per_ctx.fill(0);
    per_ctx[0] = 0x05000000; // source empty N305 per-context BB.
    regs.fill(0);
    regs[1] = 0x11081019;
    regs[2] = 0x2244;
    regs[4] = 0x2034;
    regs[6] = 0x2030;
    regs[8] = 0x2038;
    regs[10] = 0x203c;
    regs[12] = 0x2168;
    regs[14] = 0x2140;
    regs[16] = 0x2110;
    regs[18] = 0x21c0;
    regs[20] = 0x21c4;
    regs[22] = 0x21c8;
    regs[24] = 0x2180;
    regs[26] = 0x22b4;
    regs[27] = 0x0000_0306; // Gen12 ctx fake WA final value, N305 UC MOCS index3.
    regs[33] = 0x11081011;
    regs[34] = 0x23a8;
    regs[36] = 0x228c;
    regs[38] = 0x2288;
    regs[40] = 0x2284;
    regs[42] = 0x2280;
    regs[44] = 0x227c;
    regs[46] = 0x2278;
    regs[48] = 0x2274;
    regs[50] = 0x2270;
    regs[52] = 0x11081005;
    regs[53] = 0x21b0;
    regs[55] = 0x25a8;
    regs[57] = 0x25ac;
    regs[65] = 0x11080001;
    regs[66] = 0x20c8;
    regs[81] = 0x11081065;
    regs[82] = 0x2588;
    regs[84] = 0x2588;
    regs[86] = 0x2588;
    regs[88] = 0x2588;
    regs[90] = 0x2588;
    regs[92] = 0x2588;
    regs[94] = 0x2028;
    regs[96] = 0x209c;
    regs[98] = 0x20c0;
    regs[100] = 0x2178;
    regs[102] = 0x217c;
    regs[104] = 0x2358;
    regs[106] = 0x2170;
    regs[108] = 0x2150;
    regs[110] = 0x2154;
    regs[112] = 0x2158;
    regs[114] = 0x241c;
    regs[116] = 0x2600;
    regs[118] = 0x2604;
    regs[120] = 0x2608;
    regs[122] = 0x260c;
    regs[124] = 0x2610;
    regs[126] = 0x2614;
    regs[128] = 0x2618;
    regs[130] = 0x261c;
    regs[132] = 0x2620;
    regs[134] = 0x2624;
    regs[136] = 0x2628;
    regs[138] = 0x262c;
    regs[140] = 0x2630;
    regs[142] = 0x2634;
    regs[144] = 0x2638;
    regs[146] = 0x263c;
    regs[148] = 0x2640;
    regs[150] = 0x2644;
    regs[152] = 0x2648;
    regs[154] = 0x264c;
    regs[156] = 0x2650;
    regs[158] = 0x2654;
    regs[160] = 0x2658;
    regs[162] = 0x265c;
    regs[164] = 0x2660;
    regs[166] = 0x2664;
    regs[168] = 0x2668;
    regs[170] = 0x266c;
    regs[172] = 0x2670;
    regs[174] = 0x2674;
    regs[176] = 0x2678;
    regs[178] = 0x267c;
    regs[180] = 0x2068;
    regs[182] = 0x2084;
    regs[185] = 0x05000001;
    regs[3] = 0x00090009;
    regs[5] = 0;
    regs[7] = tail;
    regs[9] = ring;
    regs[11] = 1;
    regs[49] = (root >> 32) as u32;
    regs[51] = root as u32;
    regs[0x61] = 1 << 24;
    regs[0x71] = 0;
    // Source Gen12 supports only slice-level PG, actual admission proves one
    // enabled slice. Whole fused slice, no fabricated EU/subslice mask.
    regs[0x43] = 0x80041000;
    let lrm = 0x14c80002;
    let lrr = 0x150c0001;
    indirect.fill(0);
    indirect[..21].copy_from_slice(&[
        lrm,
        0x600,
        context + 4096 + 35 * 4,
        0,
        lrr,
        0x600,
        0x3a8,
        lrr,
        0x600,
        0x3a8,
        lrm,
        0x600,
        context + 4096 + 0xb7 * 4,
        0,
        lrr,
        0x600,
        0x84,
        lrm,
        0x600,
        context + 4096 + 0x75 * 4,
        0,
    ]);
    let mut aux = lrc::aux_invalidate();
    aux[1] = AUX_REG;
    aux[5] = AUX_REG;
    indirect[21..29].copy_from_slice(&aux);
    // Wa_18022495364 applies to12.0..12.10, so must follow AUX invalidation.
    indirect[29..32].copy_from_slice(&[0x11000001, DEBUG_MODE2, 0x00400040]);
    // 32 words padded to128 bytes (2 cachelines), source default offset0xd.
    regs[21] = (context + 14 * 4096) | 2;
    regs[23] = 0xd << 6;
    regs[19] = (context + 15 * 4096) | 5;
    let predicate = context + 14 * 4096 + 4088;
    indirect[512..523].copy_from_slice(&[
        0x10400002, predicate, 0, 0, 0x05008000, 0x00800000, 0x10400002, predicate, 0, 1,
        0x05000000,
    ]);
    Ok(u64::from(context | 0x10d) | (2u64 << 37)) // RCS class0/instance0, SW context2.
}
// Values below are filled from authoritative gt/intel_gt_regs.h definitions.

pub const AUX_REG: u32 = 0x4208;
pub const DEBUG_MODE2: u32 = 0x20d8;

/// Source gen12_emit_flush_rcs(INVALIDATE), noarb BB start and RCS completion.
/// Includes Wa_1409600907 depth stall and N305 L3 flush. Kernel IRQs are masked.
/// Context WAs precede this ring segment in the native renderer.
pub fn ring(out: &mut [u32], batch: u64, context: u32, seqno: u32) -> Result<usize, Error> {
    if out.len() < 42
        || batch >= 1 << 48
        || !batch.is_multiple_of(8)
        || context == 0
        || !context.is_multiple_of(4096)
        || context.checked_add(16 * 4096).is_none()
        || seqno == 0
    {
        return Err(Error::Refused);
    }
    out[..42].copy_from_slice(&[
        0x7a000204,
        0x103070a1,
        0xd0,
        0,
        0,
        0,
        0x02800101,
        0x7a000004,
        0x20344c1c,
        0xd0,
        0,
        0,
        0,
        0x11020001,
        0x4208,
        1,
        0x0e01c003,
        0,
        0x4208,
        0,
        0,
        0x02800100,
        0x04000000,
        0x18800101,
        batch as u32,
        (batch >> 32) as u32,
        0x7a000204,
        0x181430a1,
        0,
        0,
        0,
        0,
        0x7a000004,
        0x01104080,
        context + 0xd0,
        0,
        seqno,
        0,
        0x01000000,
        0x04000001,
        0x02800000,
        0,
    ]);
    Ok(42)
}

// Selected intel_workarounds.c Gen12 context/engine/general-render settings.
// Copyright ©2014-2018 Intel Corporation, full grant ../LICENSE-MIT.
// Values are source masked writes. No DG2/MTL/media/non-N305 workaround is included.
pub const ENGINE_MASKED: [(u32, u32); 6] = [
    (0x20c4, 0x3fff0306), // command UC read/write override index3.
    (0x20ec, 0x00020002), // FF DOP gate disable.
    (0xe4f4, 0x41004100), // early read and push-constant dereference hold.
    (0xe18c, 0x80018001), // smallPL + indirect-state sampler override.
    (0xe48c, 0x02000200), // TDL push disable.
    (0x2050, 0x10801080), // wait-for-event and RC semaphore idle-msg disable.
];
pub const CONTEXT_MASKED: [(u32, u32); 4] = [
    (0x7304, 0x02000200), // CPS-aware color pipeline disable.
    (0x2580, 0x00060002), // GPGPU thread-group rather than mid-thread preemption.
    (0x7018, 0x20002000), // HIZ LE/GE depth optimization disable.
    (0x7300, 0x00400040), // TDC load-balancing calculation disable.
];
/// Serialized sole owner has both wake domains and a stopped RCS. Reapply
/// required settings after every render-domain reset; failed selector recovery
/// quarantines the owner, rather than continuing with possibly wrong steering.
pub fn prepare(io: &impl crate::GtIo) -> Result<(), Error> {
    let dss = io.read(0x913c)?;
    if io.read(0x9138)? & 0xff != 1 || dss == 0 || dss & !0x3f != 0 {
        return Err(Error::Refused);
    }
    let before = io.read(0xfdc)?;
    if before & (1 << 31) == 0 {
        return Err(Error::Refused);
    }
    let selected = (before & !0x7f000000) | (dss.trailing_zeros() << 24);
    let outcome = (|| {
        io.write(0xfdc, selected)?;
        if io.read(0xfdc)? != selected {
            return Err(Error::Refused);
        }
        for (r, v) in ENGINE_MASKED {
            io.write(r, v)?;
        }
        io.write(0x20e0, 0x40004000)?; // per-context preemption granularity.
        let garb = io.read(0xb004)?;
        io.write(0xb004, garb & !0x80)?;
        let threads = io.read(0x20a0)?;
        io.write(0x20a0, threads | (1 << 19))?;
        crate::bcs::apply_nonpriv(io, true)
    })();
    let restore = (|| {
        io.write(0xfdc, before)?;
        if io.read(0xfdc)? != before {
            return Err(Error::Quarantined);
        }
        Ok(())
    })();
    restore.map_err(|_| Error::Quarantined)?;
    outcome
}
/// Context settings travel inside the kernel-owned ring, as in source
/// intel_engine_emit_ctx_wa. Its barrier/flush segments are supplied by caller.
pub fn context_wa(io: &impl crate::GtIo, out: &mut [u32]) -> Result<usize, Error> {
    if out.len() < 14 {
        return Err(Error::Refused);
    }
    let before = io.read(0xfdc)?;
    let dss = io.read(0x913c)?;
    if dss == 0 || dss & !0x3f != 0 || before & (1 << 31) == 0 {
        return Err(Error::Refused);
    }
    let selected = (before & !0x7f000000) | (dss.trailing_zeros() << 24);
    let read = (|| {
        io.write(0xfdc, selected)?;
        if io.read(0xfdc)? != selected {
            return Err(Error::Refused);
        }
        io.read(0x5584)
    })();
    io.write(0xfdc, before).map_err(|_| Error::Quarantined)?;
    if io.read(0xfdc).map_err(|_| Error::Quarantined)? != before {
        return Err(Error::Quarantined);
    }
    let wm = read? | 0x20;
    out[0] = 0x1100000b;
    // Source WA list is sorted by register before it is emitted.
    for (i, (r, v)) in [
        (0x2580, 0x00060002),
        (0x5584, wm),
        (0x6604, 0xe0040000),
        (0x7018, 0x20002000),
        (0x7300, 0x00400040),
        (0x7304, 0x02000200),
    ]
    .into_iter()
    .enumerate()
    {
        out[1 + 2 * i] = r;
        out[2 + 2 * i] = v;
    }
    // Wa_1608008084: FF_MODE2's CPU readback is unreliable; complete known
    // value replaces it, without reading or inventing an unlock sequence.
    out[13] = 0;
    Ok(14)
}
