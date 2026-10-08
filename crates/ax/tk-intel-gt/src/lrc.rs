// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/intel_lrc.c:
// gen12_xcs_offsets/set_offsets, init_common_regs, init_ppgtt_regs,
// lrc_update_regs and Gen12 BCS indirect/per-context WA setup.
// Copyright © 2014 Intel Corporation.
// intel_lrc_reg.h: Copyright © 2014-2018 Intel Corporation.
// gen8_engine_cs.c/.h: AUX invalidation command layout, Copyright © 2014 Intel.
// Full MIT grant: ../LICENSE-MIT. Gen12.0 XCS path, no RCS/DG2 paths.
use crate::{Error, ppgtt};
pub const WORDS: usize = 1024;
pub const CONTEXT_PAGES: usize = 4; // 2-page BCS image plus source two WA pages.
pub const SCRATCH: u32 = 0xd0;
fn mi(op: u32, flags: u32) -> u32 {
    (op << 23) | flags
}
fn lri(count: u32) -> u32 {
    mi(0x22, 2 * count - 1)
}
/// Caller supplies fresh zeroed context pages, never live GPU context memory.
/// Hardware image is compared separately from any guessed firmware state.
pub fn restore_context(
    regs: &mut [u32; WORDS],
    indirect: &mut [u32; WORDS],
    per_ctx: &mut [u32; WORDS],
    context: u32,
    ring: u32,
    tail: u32,
    pml4: u64,
) -> Result<u64, Error> {
    restore_engine_context(
        regs, indirect, per_ctx, context, ring, tail, pml4, 0x22000, 1, 0,
    )
}
pub fn restore_engine_context(
    regs: &mut [u32; WORDS],
    indirect: &mut [u32; WORDS],
    per_ctx: &mut [u32; WORDS],
    context: u32,
    ring: u32,
    tail: u32,
    pml4: u64,
    mmio_base: u32,
    engine_class: u8,
    engine_instance: u8,
) -> Result<u64, Error> {
    let mut fresh = [0; 1024];
    let descriptor = build_engine(
        &mut fresh,
        indirect,
        per_ctx,
        context,
        ring,
        tail,
        pml4,
        mmio_base,
        engine_class,
        engine_instance,
    )?;
    // Selected lrc_update_regs/init_ppgtt_regs/WA-pointer updates. Never
    // rebuild the GPU-generated restore instruction stream or opaque values.
    for index in [5usize, 7, 9, 11, 19, 21, 23, 49, 51] {
        regs[index] = fresh[index];
    }
    regs[3] = (regs[3] & !1) | (1 << 16); // known valid image: disable restore-inhibit.
    regs[0x61] = (regs[0x61] & !(1 << 8)) | (1 << 24); // source __reset_stop_ring.
    Ok(descriptor)
}
pub fn build(
    regs: &mut [u32; WORDS],
    indirect: &mut [u32; WORDS],
    per_ctx: &mut [u32; WORDS],
    context: u32,
    ring: u32,
    tail: u32,
    pml4: u64,
) -> Result<u64, Error> {
    build_engine(
        regs, indirect, per_ctx, context, ring, tail, pml4, 0x22000, 1, 0,
    )
}
pub fn build_engine(
    regs: &mut [u32; WORDS],
    indirect: &mut [u32; WORDS],
    per_ctx: &mut [u32; WORDS],
    context: u32,
    ring: u32,
    tail: u32,
    pml4: u64,
    mmio_base: u32,
    engine_class: u8,
    engine_instance: u8,
) -> Result<u64, Error> {
    ppgtt::physical(pml4)?;
    if context == 0
        || !context.is_multiple_of(4096)
        || context.checked_add(4 * 4096).is_none()
        || ring == 0
        || !ring.is_multiple_of(4096)
        || tail >= 4096
        || !tail.is_multiple_of(8)
        || !(1..=3).contains(&engine_class)
        || !matches!(
            (engine_class, engine_instance),
            (1, 0) | (2, 0) | (2, 2) | (3, 0)
        )
    {
        return Err(Error::Refused);
    }
    let aux_inv = match (engine_class, engine_instance) {
        (1, 0) => 0x4248, // GEN12_BCS0_AUX_INV
        (2, 0) => 0x4218, // GEN12_VD0_AUX_INV
        (2, 2) => 0x4298, // GEN12_VD2_AUX_INV
        (3, 0) => 0x4238, // GEN12_VE0_AUX_INV
        _ => return Err(Error::Refused),
    };
    regs.fill(0);
    indirect.fill(0);
    per_ctx.fill(0);
    // Fixed source gen12_xcs_offsets: two posted LRIs, with source skip slots.
    regs[1] = lri(13) | (1 << 12) | (1 << 19);
    for (n, offset) in [
        0x244, 0x34, 0x30, 0x38, 0x3c, 0x168, 0x140, 0x110, 0x1c0, 0x1c4, 0x1c8, 0x180, 0x2b4,
    ]
    .into_iter()
    .enumerate()
    {
        regs[2 + n * 2] = mmio_base + offset;
    }
    regs[33] = lri(9) | (1 << 12) | (1 << 19);
    for (n, offset) in [
        0x3a8, 0x28c, 0x288, 0x284, 0x280, 0x27c, 0x278, 0x274, 0x270,
    ]
    .into_iter()
    .enumerate()
    {
        regs[34 + n * 2] = mmio_base + offset;
    }
    // Gen12 ctx fake WA RING_CMD_CCTL final value uses N305 UC MOCS index 3.
    regs[27] = 0x0000_0306;
    regs[52] = mi(0xa, 0) | 1;
    regs[3] = 0x00090009; // inhibit sync switch + first-restore inhibit, masked.
    regs[5] = 0;
    regs[7] = tail;
    regs[9] = ring;
    regs[11] = 1; // 4KiB ring valid.
    regs[49] = (pml4 >> 32) as u32;
    regs[51] = pml4 as u32;
    regs[0x61] = 1 << 24; // __reset_stop_ring, Gen12 slot0x60+value.
    regs[0x71] = 0; // init_common_regs BB offset.
    // Timestamp restore twice (source WA), then restore scratch GPR0.
    let lrm = mi(0x29, 2) | (1 << 22) | (1 << 19);
    let lrr = mi(0x2a, 1) | (1 << 18) | (1 << 19);
    indirect[..14].copy_from_slice(&[
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
        context + 4096 + 0x75 * 4,
        0,
    ]);
    // Gen12 XCS AUX table invalidation and register-poll semaphore. The
    // register is engine-instance-specific per gen12_get_aux_inv_reg().
    indirect[14..22].copy_from_slice(&aux_invalidate_at(aux_inv));
    // Source pads to64B; 22 dwords =>128B. No manual END for INDIRECT_CTX.
    regs[21] = (context + 2 * 4096) | 2;
    regs[23] = 0xd << 6;
    // Empty N305 PER_CTX_BB; fast-color/DG2 WA does not apply to this path.
    per_ctx[0] = mi(0xa, 0);
    regs[19] = (context + 3 * 4096) | 5;
    // setup_predicate_disable_wa is source-unconditional for Gen12 images;
    // separate BB at2048, scratch qword at4088 of the indirect page.
    let predicate = context + 2 * 4096 + 4088;
    indirect[512..523].copy_from_slice(&[
        mi(0x20, 2) | (1 << 22),
        predicate,
        0,
        0,
        mi(0xa, 0) | (1 << 15),
        mi(1, 0),
        mi(0x20, 2) | (1 << 22),
        predicate,
        0,
        1,
        mi(0xa, 0),
    ]);
    // Descriptor: SW context1, selected XCS class/instance, 64b force-restore.
    Ok(u64::from(context | 0x10d)
        | (1u64 << 37)
        | (u64::from(engine_instance) << 48)
        | (u64::from(engine_class) << 61))
}
pub fn aux_invalidate() -> [u32; 8] {
    aux_invalidate_at(0x4248)
}
pub fn aux_invalidate_at(register: u32) -> [u32; 8] {
    [
        lri(1) | (1 << 17),
        register,
        1,
        mi(0x1c, 3) | (1 << 16) | (1 << 15) | (4 << 12),
        0,
        register,
        0,
        0,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gen12_xcs_lrc_offsets_descriptor_and_aux_register_follow_engine() {
        for (class, instance, base, aux) in [
            (1, 0, 0x22000, 0x4248),
            (2, 0, 0x1c0000, 0x4218),
            (2, 2, 0x1d0000, 0x4298),
            (3, 0, 0x1c8000, 0x4238),
        ] {
            let mut regs = [0; WORDS];
            let mut indirect = [0; WORDS];
            let mut per_ctx = [0; WORDS];
            let descriptor = build_engine(
                &mut regs,
                &mut indirect,
                &mut per_ctx,
                0x100000,
                0x104000,
                120,
                0x8000,
                base,
                class,
                instance,
            )
            .unwrap();
            assert_eq!(regs[2], base + 0x244);
            assert_eq!(regs[34], base + 0x3a8);
            assert_eq!(regs[27], 0x0000_0306);
            assert_eq!(indirect[15], aux);
            assert_eq!((descriptor >> 61) as u8, class);
            assert_eq!(((descriptor >> 48) & 0x3f) as u8, instance);
        }
    }

    #[test]
    fn gen12_xcs_lrc_rejects_unsupported_engine_instance() {
        let mut regs = [0; WORDS];
        let mut indirect = [0; WORDS];
        let mut per_ctx = [0; WORDS];
        assert_eq!(
            build_engine(
                &mut regs,
                &mut indirect,
                &mut per_ctx,
                0x100000,
                0x104000,
                120,
                0x8000,
                0x1e0000,
                2,
                3,
            ),
            Err(Error::Refused)
        );
    }
}
