// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/intel_wopcm.c and intel_wopcm.h.
// Copyright © 2014-2019 Intel Corporation. Full MIT grant: ../LICENSE-MIT.

use crate::Error;

pub const GEN11_WOPCM_SIZE: u32 = 2 * 1024 * 1024;
pub const MAX_WOPCM_SIZE: u32 = 8 * 1024 * 1024;
pub const WOPCM_RESERVED_SIZE: u32 = 16 * 1024;
pub const GUC_WOPCM_RESERVED: u32 = 16 * 1024;
pub const GUC_WOPCM_STACK_RESERVED: u32 = 8 * 1024;
pub const ICL_WOPCM_HW_CONTEXT_RESERVED: u32 = 32 * 1024 + 4 * 1024;
pub const GUC_WOPCM_OFFSET_SHIFT: u32 = 14;
pub const GUC_WOPCM_OFFSET_ALIGNMENT: u32 = 1 << GUC_WOPCM_OFFSET_SHIFT;
pub const GUC_WOPCM_OFFSET_MASK: u32 = 0x3ffff << GUC_WOPCM_OFFSET_SHIFT;
pub const GUC_WOPCM_OFFSET_VALID: u32 = 1;
pub const HUC_LOADING_AGENT_GUC: u32 = 1 << 1;
pub const GUC_WOPCM_SIZE_SHIFT: u32 = 12;
pub const GUC_WOPCM_SIZE_MASK: u32 = 0xfffff << GUC_WOPCM_SIZE_SHIFT;
pub const GUC_WOPCM_SIZE_LOCKED: u32 = 1;
pub const DMA_GUC_WOPCM_OFFSET: u32 = 0xc340;
pub const GUC_WOPCM_SIZE: u32 = 0xc050;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WopcmLayout {
    pub base: u32,
    pub size: u32,
    pub huc_loading_agent: u32,
}

/// Partition WOPCM for a Gen11+ integrated GT.
/// upstream: intel_wopcm.c intel_wopcm_init()/__check_layout().
pub fn partition(
    wopcm_size: u32,
    guc_fw_size: u32,
    huc_fw_size: u32,
    supports_huc: bool,
) -> Result<WopcmLayout, Error> {
    if wopcm_size == 0
        || guc_fw_size == 0
        || guc_fw_size >= wopcm_size
        || huc_fw_size >= wopcm_size
        || ICL_WOPCM_HW_CONTEXT_RESERVED + WOPCM_RESERVED_SIZE >= wopcm_size
    {
        return Err(Error::Refused);
    }
    let guc_base_unaligned = if supports_huc {
        huc_fw_size
            .checked_add(WOPCM_RESERVED_SIZE)
            .ok_or(Error::Refused)?
    } else {
        WOPCM_RESERVED_SIZE
    };
    let guc_base = guc_base_unaligned
        .checked_add(GUC_WOPCM_OFFSET_ALIGNMENT - 1)
        .ok_or(Error::Refused)?
        & !(GUC_WOPCM_OFFSET_ALIGNMENT - 1);
    let usable_size = wopcm_size
        .checked_sub(ICL_WOPCM_HW_CONTEXT_RESERVED)
        .ok_or(Error::Refused)?;
    let guc_base = guc_base.min(usable_size);
    let remaining = usable_size.checked_sub(guc_base).ok_or(Error::Refused)?;
    let guc_size = remaining & GUC_WOPCM_SIZE_MASK;
    let layout = WopcmLayout {
        base: guc_base,
        size: guc_size,
        huc_loading_agent: if supports_huc {
            HUC_LOADING_AGENT_GUC
        } else {
            0
        },
    };
    check_layout(wopcm_size, layout, guc_fw_size, huc_fw_size, supports_huc)?;
    Ok(layout)
}

/// Validate both a computed partition and BIOS-locked register values.
/// upstream: intel_wopcm.c __check_layout()/check_hw_restrictions().
pub fn check_layout(
    wopcm_size: u32,
    layout: WopcmLayout,
    guc_fw_size: u32,
    huc_fw_size: u32,
    supports_huc: bool,
) -> Result<(), Error> {
    let usable_size = wopcm_size
        .checked_sub(ICL_WOPCM_HW_CONTEXT_RESERVED)
        .ok_or(Error::Refused)?;
    if layout.base == 0
        || layout.size == 0
        || layout.base & !GUC_WOPCM_OFFSET_MASK != 0
        || layout.size & !GUC_WOPCM_SIZE_MASK != 0
        || layout.base & (GUC_WOPCM_OFFSET_ALIGNMENT - 1) != 0
        || layout
            .base
            .checked_add(layout.size)
            .filter(|end| *end <= usable_size)
            .is_none()
    {
        return Err(Error::Refused);
    }
    let guc_required = guc_fw_size
        .checked_add(GUC_WOPCM_RESERVED)
        .and_then(|size| size.checked_add(GUC_WOPCM_STACK_RESERVED))
        .ok_or(Error::Refused)?;
    if layout.size < guc_required {
        return Err(Error::Refused);
    }
    if supports_huc {
        let huc_required = huc_fw_size
            .checked_add(WOPCM_RESERVED_SIZE)
            .ok_or(Error::Refused)?;
        if layout.base < huc_required || layout.huc_loading_agent != HUC_LOADING_AGENT_GUC {
            return Err(Error::Refused);
        }
    } else if layout.huc_loading_agent != 0 {
        return Err(Error::Refused);
    }
    Ok(())
}

/// Extract a BIOS-locked WOPCM partition only when both lock/valid bits are
/// asserted. A partial state is left to the caller's write-and-verify path.
/// upstream: intel_wopcm.c __wopcm_regs_locked().
pub const fn locked_layout(offset_reg: u32, size_reg: u32) -> Option<WopcmLayout> {
    if offset_reg & GUC_WOPCM_OFFSET_VALID == 0 || size_reg & GUC_WOPCM_SIZE_LOCKED == 0 {
        return None;
    }
    Some(WopcmLayout {
        base: offset_reg & GUC_WOPCM_OFFSET_MASK,
        size: size_reg & GUC_WOPCM_SIZE_MASK,
        huc_loading_agent: offset_reg & HUC_LOADING_AGENT_GUC,
    })
}

/// Program and verify the Gen12 WOPCM partition before HuC/GuC DMA. A partial
/// firmware lock is ambiguous and fails closed; unlocked media-GT layouts are
/// refused because their WOPCM is shared/partitioned outside this GT.
/// upstream: intel_uc.c uc_init_wopcm() + intel_wopcm.c intel_wopcm_init().
pub fn initialize_gen12(
    io: &impl crate::GtIo,
    guc_fw_size: u32,
    huc_fw_size: u32,
    supports_huc: bool,
    media_gt_present: bool,
) -> Result<WopcmLayout, Error> {
    let offset_reg = io.read(DMA_GUC_WOPCM_OFFSET)?;
    let size_reg = io.read(GUC_WOPCM_SIZE)?;
    if let Some(layout) = locked_layout(offset_reg, size_reg) {
        check_layout(
            GEN11_WOPCM_SIZE,
            layout,
            guc_fw_size,
            huc_fw_size,
            supports_huc,
        )?;
        return Ok(layout);
    }
    if offset_reg & GUC_WOPCM_OFFSET_VALID != 0 || size_reg & GUC_WOPCM_SIZE_LOCKED != 0 {
        return Err(Error::Quarantined);
    }
    if media_gt_present {
        return Err(Error::Refused);
    }
    let layout = partition(GEN11_WOPCM_SIZE, guc_fw_size, huc_fw_size, supports_huc)?;
    io.write(GUC_WOPCM_SIZE, layout.size)?;
    let size_verify = io.read(GUC_WOPCM_SIZE)?;
    let size_mask = GUC_WOPCM_SIZE_MASK | GUC_WOPCM_SIZE_LOCKED;
    let size_expected = layout.size | GUC_WOPCM_SIZE_LOCKED;
    if size_verify & size_mask != size_expected {
        return Err(Error::Quarantined);
    }
    io.write(DMA_GUC_WOPCM_OFFSET, layout.base | layout.huc_loading_agent)?;
    let offset_verify = io.read(DMA_GUC_WOPCM_OFFSET)?;
    let offset_mask = GUC_WOPCM_OFFSET_MASK | GUC_WOPCM_OFFSET_VALID | layout.huc_loading_agent;
    let offset_expected = layout.base | layout.huc_loading_agent | GUC_WOPCM_OFFSET_VALID;
    if offset_verify & offset_mask != offset_expected {
        return Err(Error::Quarantined);
    }
    Ok(layout)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use super::*;
    use crate::GtIo;

    struct Io {
        registers: RefCell<[(u32, u32); 2]>,
        writes: RefCell<Vec<(u32, u32)>>,
        fail_verify: bool,
    }

    impl crate::GtIo for Io {
        fn read(&self, offset: u32) -> Result<u32, Error> {
            self.registers
                .borrow()
                .iter()
                .find_map(|(reg, value)| (*reg == offset).then_some(*value))
                .ok_or(Error::Unavailable(offset))
        }

        fn write(&self, offset: u32, value: u32) -> Result<(), Error> {
            self.writes.borrow_mut().push((offset, value));
            if !self.fail_verify {
                let stored = match offset {
                    GUC_WOPCM_SIZE => value | GUC_WOPCM_SIZE_LOCKED,
                    DMA_GUC_WOPCM_OFFSET => value | GUC_WOPCM_OFFSET_VALID,
                    _ => return Err(Error::Unavailable(offset)),
                };
                if let Some(slot) = self
                    .registers
                    .borrow_mut()
                    .iter_mut()
                    .find(|(reg, _)| *reg == offset)
                {
                    slot.1 = stored;
                }
            }
            Ok(())
        }

        fn now_us(&self) -> u64 {
            0
        }

        fn delay_us(&self, _micros: u32) {}
    }

    #[test]
    fn adl_wopcm_partition_matches_alignment_and_firmware_reservations() {
        let layout = partition(GEN11_WOPCM_SIZE, 512 * 1024, 256 * 1024, true).unwrap();
        assert_eq!(layout.base, 272 * 1024);
        assert_eq!(layout.base % GUC_WOPCM_OFFSET_ALIGNMENT, 0);
        assert_eq!(
            layout.size,
            (GEN11_WOPCM_SIZE - ICL_WOPCM_HW_CONTEXT_RESERVED - layout.base) & GUC_WOPCM_SIZE_MASK
        );
        assert_eq!(layout.huc_loading_agent, HUC_LOADING_AGENT_GUC);
        assert!(check_layout(GEN11_WOPCM_SIZE, layout, 512 * 1024, 256 * 1024, true).is_ok());
    }

    #[test]
    fn locked_wopcm_layout_requires_both_firmware_lock_bits() {
        let layout = partition(GEN11_WOPCM_SIZE, 512 * 1024, 256 * 1024, true).unwrap();
        assert_eq!(
            locked_layout(
                layout.base | HUC_LOADING_AGENT_GUC | GUC_WOPCM_OFFSET_VALID,
                layout.size | GUC_WOPCM_SIZE_LOCKED
            ),
            Some(layout)
        );
        assert_eq!(locked_layout(layout.base, layout.size), None);
    }

    #[test]
    fn wopcm_layout_fails_closed_on_unfit_firmware_or_reserved_ranges() {
        assert_eq!(
            partition(GEN11_WOPCM_SIZE, GEN11_WOPCM_SIZE, 0, false),
            Err(Error::Refused)
        );
        let too_small = WopcmLayout {
            base: 16 * 1024,
            size: 4 * 1024,
            huc_loading_agent: 0,
        };
        assert_eq!(
            check_layout(GEN11_WOPCM_SIZE, too_small, 128 * 1024, 0, false),
            Err(Error::Refused)
        );
    }

    #[test]
    fn gen12_wopcm_programs_size_then_offset_and_verifies_lock() {
        let io = Io {
            registers: RefCell::new([(DMA_GUC_WOPCM_OFFSET, 0), (GUC_WOPCM_SIZE, 0)]),
            writes: RefCell::new(Vec::new()),
            fail_verify: false,
        };
        let layout = initialize_gen12(&io, 512 * 1024, 256 * 1024, true, false).unwrap();
        assert_eq!(io.writes.borrow().len(), 2);
        assert_eq!(io.writes.borrow()[0].0, GUC_WOPCM_SIZE);
        assert_eq!(io.writes.borrow()[1].0, DMA_GUC_WOPCM_OFFSET);
        assert_eq!(
            io.read(GUC_WOPCM_SIZE).unwrap() & GUC_WOPCM_SIZE_MASK,
            layout.size
        );
        assert_eq!(
            io.read(DMA_GUC_WOPCM_OFFSET).unwrap() & GUC_WOPCM_OFFSET_MASK,
            layout.base
        );
        assert_eq!(
            initialize_gen12(&io, 512 * 1024, 256 * 1024, true, false),
            Ok(layout)
        );
        assert_eq!(io.writes.borrow().len(), 2);
    }

    #[test]
    fn gen12_wopcm_refuses_ambiguous_locks_media_gt_and_verify_failure() {
        let partial_lock = Io {
            registers: RefCell::new([
                (DMA_GUC_WOPCM_OFFSET, GUC_WOPCM_OFFSET_VALID),
                (GUC_WOPCM_SIZE, 0),
            ]),
            writes: RefCell::new(Vec::new()),
            fail_verify: false,
        };
        assert_eq!(
            initialize_gen12(&partial_lock, 512 * 1024, 256 * 1024, true, false),
            Err(Error::Quarantined)
        );
        let unlocked = Io {
            registers: RefCell::new([(DMA_GUC_WOPCM_OFFSET, 0), (GUC_WOPCM_SIZE, 0)]),
            writes: RefCell::new(Vec::new()),
            fail_verify: false,
        };
        assert_eq!(
            initialize_gen12(&unlocked, 512 * 1024, 256 * 1024, true, true),
            Err(Error::Refused)
        );
        let verify_failure = Io {
            registers: RefCell::new([(DMA_GUC_WOPCM_OFFSET, 0), (GUC_WOPCM_SIZE, 0)]),
            writes: RefCell::new(Vec::new()),
            fail_verify: true,
        };
        assert_eq!(
            initialize_gen12(&verify_failure, 512 * 1024, 256 * 1024, true, false),
            Err(Error::Quarantined)
        );
    }
}
