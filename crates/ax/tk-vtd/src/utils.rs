//! Intel VT-d page geometry, register sequencing and timeout helpers translated
//! from FreeBSD `sys/x86/iommu/intel_utils.c` (snapshot c2b7fe4, BSD-2-Clause).
//! Copyright (c) 2013 The FreeBSD Foundation; Konstantin Belousov under
//! Foundation sponsorship. OS resource, cache-maintenance and lock primitives
//! are expressed through `RegisterIo`; full grant is in LICENSES/BSD-2-Clause.txt.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::{
    Error,
    dmar::{DmarDomain, dmar_x2apic},
    reg::*,
};

const IOMMU_PAGE_SHIFT: u32 = 12;
const IOMMU_PAGE_SIZE: u64 = 1 << IOMMU_PAGE_SHIFT;
const DMAR_HW_TIMEOUT_DEFAULT: u64 = 1_000_000;
static DMAR_HW_TIMEOUT_NS: AtomicU64 = AtomicU64::new(DMAR_HW_TIMEOUT_DEFAULT);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SagawBits {
    agaw: u8,
    capability: u64,
    context_aw: u64,
    page_levels: u8,
}
const SAGAW_BITS: [SagawBits; 4] = [
    SagawBits {
        agaw: 30,
        capability: DMAR_CAP_SAGAW_2LVL,
        context_aw: DMAR_CTX2_AW_2LVL,
        page_levels: 2,
    },
    SagawBits {
        agaw: 39,
        capability: DMAR_CAP_SAGAW_3LVL,
        context_aw: DMAR_CTX2_AW_3LVL,
        page_levels: 3,
    },
    SagawBits {
        agaw: 48,
        capability: DMAR_CAP_SAGAW_4LVL,
        context_aw: DMAR_CTX2_AW_4LVL,
        page_levels: 4,
    },
    SagawBits {
        agaw: 57,
        capability: DMAR_CAP_SAGAW_5LVL,
        context_aw: DMAR_CTX2_AW_5LVL,
        page_levels: 5,
    },
];

/// FreeBSD dmar_nd2mask: valid domain-ID mask for CAP.ND encodings 0..=6.
// upstream: intel_utils.c dmar_nd2mask()
pub const fn dmar_nd2mask(nd: u32) -> Option<u16> {
    const MASKS: [u16; 8] = [0x000f, 0x002f, 0x00ff, 0x02ff, 0x0fff, 0x2fff, 0xffff, 0];
    if nd <= 6 {
        Some(MASKS[nd as usize])
    } else {
        None
    }
}

/// FreeBSD dmar_pglvl_supported().
// upstream: intel_utils.c dmar_pglvl_supported()
pub fn dmar_pglvl_supported(hw_cap: u64, page_levels: u8) -> bool {
    SAGAW_BITS.iter().any(|bits| {
        bits.page_levels == page_levels && DMAR_CAP_SAGAW(hw_cap) & bits.capability != 0
    })
}

/// FreeBSD domain_set_agaw(): choose the smallest supported AGAW >= requested MGAW.
// upstream: intel_utils.c domain_set_agaw()
pub fn domain_set_agaw(domain: &mut DmarDomain, hw_cap: u64, mgaw: i32) -> Result<(), Error> {
    domain.mgaw = mgaw;
    let _sagaw = DMAR_CAP_SAGAW(hw_cap);
    let bits = SAGAW_BITS
        .iter()
        .find(|bits| i32::from(bits.agaw) >= mgaw)
        .ok_or(Error::Unsupported)?;
    domain.agaw = i32::from(bits.agaw);
    domain.page_level = i32::from(bits.page_levels);
    domain.context_address_width = bits.context_aw as i32;
    Ok(())
}

/// FreeBSD dmar_maxaddr2mgaw(), retaining its largest-supported fallback.
// upstream: intel_utils.c dmar_maxaddr2mgaw()
pub fn dmar_maxaddr2mgaw(hw_cap: u64, max_address: u64, allow_less: bool) -> Result<u8, Error> {
    let sagaw = DMAR_CAP_SAGAW(hw_cap);
    if let Some(bits) = SAGAW_BITS.iter().find(|bits| {
        1u64.checked_shl(u32::from(bits.agaw))
            .is_some_and(|limit| limit >= max_address)
            && sagaw & bits.capability != 0
    }) {
        return Ok(bits.agaw);
    }
    if allow_less {
        if let Some(bits) = SAGAW_BITS
            .iter()
            .rev()
            .find(|bits| sagaw & bits.capability != 0)
        {
            return Ok(bits.agaw);
        }
    }
    Err(Error::Unsupported)
}

/// FreeBSD domain_is_sp_lvl(): map page-table level to CAP.SPS bit.
// upstream: intel_utils.c domain_is_sp_lvl()
pub fn domain_is_sp_lvl(hw_cap: u64, page_levels: u8, level: u8) -> bool {
    let Some(index) = page_levels
        .checked_sub(level)
        .and_then(|v| v.checked_sub(1))
    else {
        return false;
    };
    const SPS: [u64; 4] = [
        DMAR_CAP_SPS_2M,
        DMAR_CAP_SPS_1G,
        DMAR_CAP_SPS_512G,
        DMAR_CAP_SPS_1T,
    ];
    SPS.get(index as usize)
        .is_some_and(|bit| DMAR_CAP_SPS(hw_cap) & bit != 0)
}

/// FreeBSD domain_page_size()/pglvl_page_size().
// upstream: intel_utils.c domain_page_size()
pub const fn domain_page_size(page_levels: u8, level: u8) -> Option<u64> {
    if level == 0 || level > page_levels {
        return None;
    }
    IOMMU_PAGE_SIZE.checked_shl(9 * (level as u32 - 1))
}

/// FreeBSD calc_am(): choose the largest hardware page-selective invalidation
/// mask that covers an aligned prefix of the requested range.
// upstream: intel_utils.c calc_am()
pub fn calc_am(hw_cap: u64, base: u64, size: u64) -> (u8, u64) {
    let mut am = DMAR_CAP_MAMV(hw_cap) as u8;
    loop {
        let Some(isize) = 1u64.checked_shl(u32::from(am) + IOMMU_PAGE_SHIFT) else {
            if am == 0 {
                return (am, IOMMU_PAGE_SIZE);
            }
            am -= 1;
            continue;
        };
        if base & (isize - 1) == 0 && size >= isize {
            return (am, isize);
        }
        if am == 0 {
            return (am, isize);
        }
        am -= 1;
    }
}

/// Register and platform services used by the upstream utility functions.
/// `write64` must preserve the source's lower-then-upper ordering on hardware
/// that needs it; `flush_translation` is a no-op for coherent units.
pub trait RegisterIo {
    fn read32(&mut self, offset: u64) -> u32;
    fn write32(&mut self, offset: u64, value: u32);
    fn read64(&mut self, offset: u64) -> u64;
    fn write64(&mut self, offset: u64, value: u64);
    fn hw_cap(&self) -> u64;
    fn hw_ecap(&self) -> u64;
    fn hw_gcmd(&self) -> u32;
    fn set_hw_gcmd(&mut self, value: u32);
    fn qi_enabled(&self) -> bool;
    fn root_table_physical(&self) -> u64;
    fn interrupt_table_physical(&self) -> u64;
    fn interrupt_entry_count(&self) -> u32;
    fn x2apic_mode(&self) -> bool;
    fn now_ns(&mut self) -> u64;
    fn spin_wait(&mut self) {
        core::hint::spin_loop();
    }
    fn flush_translation(&mut self, _address: usize, _length: usize) {}
}

fn wait_until<I: RegisterIo>(
    io: &mut I,
    mut condition: impl FnMut(&mut I) -> bool,
) -> Result<(), Error> {
    let timeout = DMAR_HW_TIMEOUT_NS.load(Ordering::Acquire);
    let start = io.now_ns();
    loop {
        if condition(io) {
            return Ok(());
        }
        if timeout != 0 && io.now_ns().wrapping_sub(start) >= timeout {
            return Err(Error::Timeout);
        }
        io.spin_wait();
    }
}

// upstream: intel_utils.c dmar_flush_transl_to_ram()
pub fn dmar_flush_transl_to_ram<I: RegisterIo>(io: &mut I, destination: usize, size: usize) {
    if io.hw_ecap() & DMAR_ECAP_C == 0 {
        io.flush_translation(destination, size);
    }
}
// upstream: intel_utils.c dmar_flush_pte_to_ram()
pub fn dmar_flush_pte_to_ram<I: RegisterIo>(io: &mut I, destination: usize) {
    dmar_flush_transl_to_ram(io, destination, core::mem::size_of::<u64>());
}
// upstream: intel_utils.c dmar_flush_ctx_to_ram()
pub fn dmar_flush_ctx_to_ram<I: RegisterIo>(io: &mut I, destination: usize) {
    dmar_flush_transl_to_ram(
        io,
        destination,
        core::mem::size_of::<crate::reg::ContextEntry>(),
    );
}
// upstream: intel_utils.c dmar_flush_root_to_ram()
pub fn dmar_flush_root_to_ram<I: RegisterIo>(io: &mut I, destination: usize) {
    dmar_flush_transl_to_ram(io, destination, core::mem::size_of::<RootEntry>());
}

/// FreeBSD dmar_load_root_entry_ptr(), including RTADDR write, SRTP command,
/// and GSTS.RTPS completion wait.
// upstream: intel_utils.c dmar_load_root_entry_ptr()
pub fn dmar_load_root_entry_ptr<I: RegisterIo>(io: &mut I) -> Result<(), Error> {
    io.write64(
        DMAR_RTADDR_REG,
        io.root_table_physical() & DMAR_RTADDR_RTA_MASK,
    );
    io.write32(DMAR_GCMD_REG, io.hw_gcmd() | DMAR_GCMD_SRTP as u32);
    wait_until(io, |io| {
        io.read32(DMAR_GSTS_REG) & DMAR_GSTS_RTPS as u32 != 0
    })
}

/// FreeBSD dmar_inv_ctx_glob(): register invalidation is valid only while QI is off.
// upstream: intel_utils.c dmar_inv_ctx_glob()
pub fn dmar_inv_ctx_glob<I: RegisterIo>(io: &mut I) -> Result<(), Error> {
    if io.qi_enabled() {
        return Err(Error::Unsupported);
    }
    io.write64(DMAR_CCMD_REG, DMAR_CCMD_ICC | DMAR_CCMD_CIRG_GLOB);
    wait_until(io, |io| {
        io.read32(DMAR_CCMD_REG + 4) & DMAR_CCMD_ICC32 as u32 == 0
    })
}

/// FreeBSD dmar_inv_iotlb_glob().
// upstream: intel_utils.c dmar_inv_iotlb_glob()
pub fn dmar_inv_iotlb_glob<I: RegisterIo>(io: &mut I) -> Result<(), Error> {
    if io.qi_enabled() {
        return Err(Error::Unsupported);
    }
    let reg = 16 * DMAR_ECAP_IRO(io.hw_ecap());
    io.write64(
        reg + DMAR_IOTLB_REG_OFF,
        DMAR_IOTLB_IVT | DMAR_IOTLB_IIRG_GLB | DMAR_IOTLB_DR | DMAR_IOTLB_DW,
    );
    wait_until(io, |io| {
        io.read32(reg + DMAR_IOTLB_REG_OFF + 4) & DMAR_IOTLB_IVT32 as u32 == 0
    })
}

/// FreeBSD dmar_flush_write_bufs().
// upstream: intel_utils.c dmar_flush_write_bufs()
pub fn dmar_flush_write_bufs<I: RegisterIo>(io: &mut I) -> Result<(), Error> {
    if io.hw_cap() & DMAR_CAP_RWBF == 0 {
        return Err(Error::Unsupported);
    }
    io.write32(DMAR_GCMD_REG, io.hw_gcmd() | DMAR_GCMD_WBF as u32);
    wait_until(io, |io| {
        io.read32(DMAR_GSTS_REG) & DMAR_GSTS_WBFS as u32 != 0
    })
}

/// FreeBSD dmar_disable_protected_regions(); retain source PRS wait polarity.
// upstream: intel_utils.c dmar_disable_protected_regions()
pub fn dmar_disable_protected_regions<I: RegisterIo>(io: &mut I) -> Result<(), Error> {
    if io.hw_cap() & (DMAR_CAP_PLMR | DMAR_CAP_PHMR) == 0 {
        return Ok(());
    }
    let mut value = io.read32(DMAR_PMEN_REG);
    if value & DMAR_PMEN_EPM as u32 == 0 {
        return Ok(());
    }
    value &= !(DMAR_PMEN_EPM as u32);
    io.write32(DMAR_PMEN_REG, value);
    wait_until(io, |io| {
        io.read32(DMAR_PMEN_REG) & DMAR_PMEN_PRS as u32 != 0
    })
}

/// FreeBSD dmar_enable_translation(): update software GCMD shadow before writing.
// upstream: intel_utils.c dmar_enable_translation()
pub fn dmar_enable_translation<I: RegisterIo>(io: &mut I) -> Result<(), Error> {
    let command = io.hw_gcmd() | DMAR_GCMD_TE as u32;
    io.set_hw_gcmd(command);
    io.write32(DMAR_GCMD_REG, command);
    wait_until(io, |io| {
        io.read32(DMAR_GSTS_REG) & DMAR_GSTS_TES as u32 != 0
    })
}

/// FreeBSD dmar_disable_translation().
// upstream: intel_utils.c dmar_disable_translation()
pub fn dmar_disable_translation<I: RegisterIo>(io: &mut I) -> Result<(), Error> {
    let command = io.hw_gcmd() & !(DMAR_GCMD_TE as u32);
    io.set_hw_gcmd(command);
    io.write32(DMAR_GCMD_REG, command);
    wait_until(io, |io| {
        io.read32(DMAR_GSTS_REG) & DMAR_GSTS_TES as u32 == 0
    })
}

/// FreeBSD dmar_load_irt_ptr(): table base, EIME, size encoding, SIRTP and wait.
// upstream: intel_utils.c dmar_load_irt_ptr()
pub fn dmar_load_irt_ptr<I: RegisterIo>(io: &mut I) -> Result<(), Error> {
    let count = io.interrupt_entry_count();
    if count < 2 || !count.is_power_of_two() {
        return Err(Error::InvalidRange);
    }
    let size = count.ilog2() - 1;
    if size as u64 > DMAR_IRTA_S_MASK {
        return Err(Error::InvalidRange);
    }
    let mut irta = io.interrupt_table_physical() & !DMAR_IRTA_S_MASK;
    if dmar_x2apic(io.x2apic_mode(), io.hw_ecap()) {
        irta |= DMAR_IRTA_EIME;
    }
    irta |= size as u64;
    io.write64(DMAR_IRTA_REG, irta);
    io.write32(DMAR_GCMD_REG, io.hw_gcmd() | DMAR_GCMD_SIRTP as u32);
    wait_until(io, |io| {
        io.read32(DMAR_GSTS_REG) & DMAR_GSTS_IRTPS as u32 != 0
    })
}

/// FreeBSD dmar_enable_ir(): IRE set and compatibility format interrupts cleared.
// upstream: intel_utils.c dmar_enable_ir()
pub fn dmar_enable_ir<I: RegisterIo>(io: &mut I) -> Result<(), Error> {
    let command = (io.hw_gcmd() | DMAR_GCMD_IRE as u32) & !(DMAR_GCMD_CFI as u32);
    io.set_hw_gcmd(command);
    io.write32(DMAR_GCMD_REG, command);
    wait_until(io, |io| {
        io.read32(DMAR_GSTS_REG) & DMAR_GSTS_IRES as u32 != 0
    })
}

/// FreeBSD dmar_disable_ir().
// upstream: intel_utils.c dmar_disable_ir()
pub fn dmar_disable_ir<I: RegisterIo>(io: &mut I) -> Result<(), Error> {
    let command = io.hw_gcmd() & !(DMAR_GCMD_IRE as u32);
    io.set_hw_gcmd(command);
    io.write32(DMAR_GCMD_REG, command);
    wait_until(io, |io| {
        io.read32(DMAR_GSTS_REG) & DMAR_GSTS_IRES as u32 == 0
    })
}

const fn barrier_masks(barrier_id: usize) -> Option<(u32, u32, u32)> {
    if barrier_id >= 10 {
        return None;
    }
    let shift = barrier_id as u32 * 3;
    Some((1 << shift, 1 << (shift + 1), 1 << (shift + 2)))
}

/// Atomic single-winner barrier corresponding to `dmar_barrier_enter()`.
// upstream: intel_utils.c dmar_barrier_enter()
pub fn dmar_barrier_enter(
    flags: &core::sync::atomic::AtomicU32,
    barrier_id: usize,
) -> Result<bool, Error> {
    let (done, inproc, wakeup) = barrier_masks(barrier_id).ok_or(Error::InvalidRange)?;
    loop {
        let state = flags.load(Ordering::Acquire);
        if state & done != 0 {
            return Ok(false);
        }
        if state & inproc != 0 {
            flags.fetch_or(wakeup, Ordering::AcqRel);
            core::hint::spin_loop();
            continue;
        }
        if flags
            .compare_exchange_weak(state, state | inproc, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Ok(true);
        }
    }
}

/// Mark the barrier complete and clear transient bits, corresponding to exit.
// upstream: intel_utils.c dmar_barrier_exit()
pub fn dmar_barrier_exit(
    flags: &core::sync::atomic::AtomicU32,
    barrier_id: usize,
) -> Result<(), Error> {
    let (done, inproc, wakeup) = barrier_masks(barrier_id).ok_or(Error::InvalidRange)?;
    let mut state = flags.load(Ordering::Acquire);
    loop {
        if state & inproc == 0 || state & done != 0 {
            return Err(Error::InvalidRange);
        }
        let next = (state | done) & !(inproc | wakeup);
        match flags.compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(()),
            Err(now) => state = now,
        }
    }
}

/// FreeBSD dmar_update_timeout() / dmar_get_timeout(); zero means no deadline.
// upstream: intel_utils.c dmar_update_timeout()
pub fn dmar_update_timeout(new_value_ns: u64) {
    DMAR_HW_TIMEOUT_NS.store(new_value_ns, Ordering::Release);
}
// upstream: intel_utils.c dmar_get_timeout()
pub fn dmar_get_timeout() -> u64 {
    DMAR_HW_TIMEOUT_NS.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::sync::atomic::AtomicU32;

    use super::*;
    use crate::dmar::DMAR_BARRIER_RMRR;

    struct FakeIo {
        regs: [u32; 128],
        cap: u64,
        ecap: u64,
        gcmd: u32,
        root: u64,
        irt: u64,
        irte_count: u32,
        now: u64,
        write_log: Vec<(u64, u64)>,
        flushes: Vec<(usize, usize)>,
    }
    impl Default for FakeIo {
        fn default() -> Self {
            Self {
                regs: [0; 128],
                cap: 0,
                ecap: 0,
                gcmd: 0,
                root: 0,
                irt: 0,
                irte_count: 0,
                now: 0,
                write_log: Vec::new(),
                flushes: Vec::new(),
            }
        }
    }
    impl RegisterIo for FakeIo {
        fn read32(&mut self, o: u64) -> u32 {
            self.regs[o as usize / 4]
        }
        fn write32(&mut self, o: u64, v: u32) {
            self.write_log.push((o, u64::from(v)));
            self.regs[o as usize / 4] = v;
            if o == DMAR_GCMD_REG {
                let status = &mut self.regs[DMAR_GSTS_REG as usize / 4];
                *status = (*status & !(DMAR_GSTS_TES as u32 | DMAR_GSTS_WBFS as u32))
                    | if v & DMAR_GCMD_TE as u32 != 0 {
                        DMAR_GSTS_TES as u32
                    } else {
                        0
                    }
                    | if v & DMAR_GCMD_WBF as u32 != 0 {
                        DMAR_GSTS_WBFS as u32
                    } else {
                        0
                    };
            }
            if o == DMAR_PMEN_REG && v & DMAR_PMEN_EPM as u32 == 0 {
                self.regs[o as usize / 4] |= DMAR_PMEN_PRS as u32;
            }
        }
        fn read64(&mut self, o: u64) -> u64 {
            u64::from(self.regs[o as usize / 4]) | (u64::from(self.regs[o as usize / 4 + 1]) << 32)
        }
        fn write64(&mut self, o: u64, v: u64) {
            self.write_log.push((o, v));
            self.regs[o as usize / 4] = v as u32;
            self.regs[o as usize / 4 + 1] = (v >> 32) as u32;
            if o == DMAR_CCMD_REG {
                self.regs[o as usize / 4 + 1] &= !(DMAR_CCMD_ICC32 as u32);
            } else if o % 16 == DMAR_IOTLB_REG_OFF {
                self.regs[o as usize / 4 + 1] &= !(DMAR_IOTLB_IVT32 as u32);
            }
        }
        fn hw_cap(&self) -> u64 {
            self.cap
        }
        fn hw_ecap(&self) -> u64 {
            self.ecap
        }
        fn hw_gcmd(&self) -> u32 {
            self.gcmd
        }
        fn set_hw_gcmd(&mut self, value: u32) {
            self.gcmd = value;
        }
        fn qi_enabled(&self) -> bool {
            self.regs[DMAR_GSTS_REG as usize / 4] & DMAR_GSTS_QIES as u32 != 0
        }
        fn root_table_physical(&self) -> u64 {
            self.root
        }
        fn interrupt_table_physical(&self) -> u64 {
            self.irt
        }
        fn interrupt_entry_count(&self) -> u32 {
            self.irte_count
        }
        fn x2apic_mode(&self) -> bool {
            true
        }
        fn now_ns(&mut self) -> u64 {
            self.now += 1;
            self.now
        }
        fn spin_wait(&mut self) {
            self.now += 1;
        }
        fn flush_translation(&mut self, address: usize, length: usize) {
            self.flushes.push((address, length));
        }
    }

    #[test]
    fn sagaw_page_geometry_and_domain_agaw_match_upstream_selection_order() {
        let cap = (DMAR_CAP_SAGAW_3LVL | DMAR_CAP_SAGAW_4LVL) << 8 | (DMAR_CAP_SPS_2M << 34);
        assert_eq!(dmar_nd2mask(6), Some(0xffff));
        assert_eq!(dmar_nd2mask(7), None);
        assert!(dmar_pglvl_supported(cap, 3));
        assert!(!dmar_pglvl_supported(cap, 2));
        let mut domain = DmarDomain::default();
        domain_set_agaw(&mut domain, cap, 39).unwrap();
        assert_eq!((domain.agaw, domain.page_level), (39, 3));
        assert_eq!(dmar_maxaddr2mgaw(cap, 1 << 38, true), Ok(39));
        assert_eq!(domain_page_size(4, 2), Some(1 << 21));
        assert!(domain_is_sp_lvl(cap, 4, 3));
        assert_eq!(
            calc_am(cap | (9 << 48), 0x20_0000, 0x20_0000),
            (9, 0x20_0000)
        );
    }

    #[test]
    fn utility_register_commands_write_then_wait_for_source_status_bits() {
        let mut io = FakeIo {
            root: 0x1234_5000,
            ..Default::default()
        };
        io.regs[DMAR_GSTS_REG as usize / 4] = DMAR_GSTS_RTPS as u32;
        assert_eq!(dmar_load_root_entry_ptr(&mut io), Ok(()));
        assert_eq!(io.write_log[0], (DMAR_RTADDR_REG, 0x1234_5000));
        assert_eq!(io.write_log[1], (DMAR_GCMD_REG, DMAR_GCMD_SRTP));
        io.regs[DMAR_GSTS_REG as usize / 4] = DMAR_GSTS_TES as u32;
        assert_eq!(dmar_enable_translation(&mut io), Ok(()));
        assert_eq!(io.hw_gcmd(), DMAR_GCMD_TE as u32);
        assert_eq!(dmar_disable_translation(&mut io), Ok(()));
        assert_eq!(io.hw_gcmd(), 0);
    }

    #[test]
    fn irta_size_and_eime_are_loaded_before_sirtp_completion_wait() {
        let mut io = FakeIo {
            irt: 0x4000,
            irte_count: 256,
            ..Default::default()
        };
        io.regs[DMAR_GSTS_REG as usize / 4] = DMAR_GSTS_IRTPS as u32;
        io.ecap = DMAR_ECAP_EIM;
        assert_eq!(dmar_load_irt_ptr(&mut io), Ok(()));
        assert_eq!(
            io.write_log,
            [
                (DMAR_IRTA_REG, 0x4000 | DMAR_IRTA_EIME | 7),
                (DMAR_GCMD_REG, DMAR_GCMD_SIRTP)
            ]
        );
    }

    #[test]
    fn translation_flush_obeys_noncoherent_capability() {
        let mut io = FakeIo::default();
        dmar_flush_pte_to_ram(&mut io, 0x1000);
        assert_eq!(io.flushes, [(0x1000, 8)]);
        io.ecap = DMAR_ECAP_C;
        dmar_flush_root_to_ram(&mut io, 0x2000);
        assert_eq!(io.flushes.len(), 1);
    }

    #[test]
    fn register_invalidations_preserve_global_context_and_iotlb_commands() {
        let mut io = FakeIo::default();
        assert_eq!(dmar_inv_ctx_glob(&mut io), Ok(()));
        assert_eq!(
            io.write_log[0],
            (DMAR_CCMD_REG, DMAR_CCMD_ICC | DMAR_CCMD_CIRG_GLOB)
        );
        io.write_log.clear();
        io.ecap = 0x12 << 8;
        assert_eq!(dmar_inv_iotlb_glob(&mut io), Ok(()));
        assert_eq!(
            io.write_log[0],
            (
                16 * 0x12 + DMAR_IOTLB_REG_OFF,
                DMAR_IOTLB_IVT | DMAR_IOTLB_IIRG_GLB | DMAR_IOTLB_DR | DMAR_IOTLB_DW
            )
        );
    }

    #[test]
    fn write_buffer_and_protected_region_paths_follow_capability_gates() {
        let mut io = FakeIo::default();
        assert_eq!(dmar_flush_write_bufs(&mut io), Err(Error::Unsupported));
        io.cap = DMAR_CAP_RWBF;
        assert_eq!(dmar_flush_write_bufs(&mut io), Ok(()));
        io.cap = DMAR_CAP_PLMR;
        io.regs[DMAR_PMEN_REG as usize / 4] = DMAR_PMEN_EPM as u32;
        assert_eq!(dmar_disable_protected_regions(&mut io), Ok(()));
        assert_eq!(
            io.regs[DMAR_PMEN_REG as usize / 4] & DMAR_PMEN_EPM as u32,
            0
        );
    }

    #[test]
    fn atomic_barrier_runs_exactly_one_initializer() {
        let flags = AtomicU32::new(0);
        assert_eq!(dmar_barrier_enter(&flags, DMAR_BARRIER_RMRR), Ok(true));
        assert_eq!(dmar_barrier_exit(&flags, DMAR_BARRIER_RMRR), Ok(()));
        assert_eq!(dmar_barrier_enter(&flags, DMAR_BARRIER_RMRR), Ok(false));
    }
}
