//! Intel 82543/82544 MAC and PHY workarounds.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_82543.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{
    api::E1000MacType,
    mac::E1000MediaType,
    nvm::{E1000NvmAccess, E1000NvmConfig, E1000NvmType},
    osdep::E1000RegisterIo,
    registers::*,
};

const TBI_COMPAT_ENABLED: u8 = 1;
const TBI_SBP_ENABLED: u8 = 2;
const PHY_PREAMBLE: u32 = u32::MAX;
const PHY_PREAMBLE_SIZE: u16 = 32;
const PHY_SOF: u32 = 1;
const PHY_OP_READ: u32 = 2;
const PHY_OP_WRITE: u32 = 1;
const PHY_TURNAROUND: u32 = 2;
const MAX_PHY_REG_ADDRESS: u32 = 0x1f;
const CTRL_MDIO_DIR: u32 = 0x0100_0000;
const CTRL_MDIO: u32 = 0x0010_0000;
const CTRL_MDC_DIR: u32 = 0x0200_0000;
const CTRL_MDC: u32 = 0x0020_0000;
const CTRL_SWDPIN1: u32 = 0x0008_0000;
const CTRL_SLU: u32 = E1000_CTRL_SLU;
const CTRL_FD: u32 = 0x0000_0001;
const RXCW_C: u32 = 0x2000_0000;
const STATUS_LU: u32 = 0x0000_0002;
const CTRL_LRST: u32 = 0x0000_0008;
const NVM_INIT_CONTROL2_REG: u16 = 0x000f;
const NVM_WORD0F_SWPDIO_EXT_MASK: u16 = 0x00f0;
const NVM_SWDPIO_EXT_SHIFT: u32 = 4;
const M88_PHY_SPEC_STATUS: u16 = 0x11;
const M88_PSSR_DPLX: u16 = 0x2000;
const M88_PSSR_SPEED: u16 = 0xc000;
const M88_PSSR_1000MBS: u16 = 0x8000;
const M88_PSSR_100MBS: u16 = 0x4000;
const PHY_FORCE_TIME: usize = 20;
const ALL_10_SPEED: u16 = 0x0003;
const IMS_ENABLE_MASK: u32 = 0x0000_001f;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TbiState {
    pub flags: u8,
    pub init_phy_disabled: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TbiStats {
    pub crcerrs: u64,
    pub gprc: u64,
    pub gorc: u64,
    pub bprc: u64,
    pub mprc: u64,
    pub roc: u64,
    pub prc64: u64,
    pub prc127: u64,
    pub prc255: u64,
    pub prc511: u64,
    pub prc1023: u64,
    pub prc1522: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MacParams82543 {
    pub media: E1000MediaType,
    pub mta_count: u16,
    pub rar_count: u16,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitPointers82543 {
    pub mac: bool,
    pub nvm: bool,
    pub phy: bool,
}

pub trait Ops82543 {
    fn phy_reset(&mut self) -> DevResult;
    fn setup_link_generic(&mut self) -> DevResult;
    fn setup_copper_m88(&mut self) -> DevResult;
    fn copper_autoneg(&mut self) -> DevResult;
    fn phy_has_link(&mut self, iterations: usize, interval_us: u32) -> DevResult<bool>;
    fn config_fc_after_link_up(&mut self) -> DevResult;
    fn config_collision_dist(&mut self) -> DevResult;
    fn poll_fiber_link(&mut self) -> DevResult;
    fn clear_vfta_generic(&mut self, offset: u32, value: u32) -> DevResult;
    fn check_downshift(&mut self) -> DevResult;
    fn link_speed_duplex(&mut self) -> DevResult<(u16, u16)>;
    fn read_phy(&mut self, reg: u16) -> DevResult<u16>;
    fn write_phy(&mut self, reg: u16, value: u16) -> DevResult;
    fn phy_force_speed_duplex_m88(&mut self) -> DevResult;
    fn setup_fiber(&mut self) -> DevResult;
    fn commit_flow_control(&mut self) -> DevResult;
    fn phy_has_link_status(&mut self) -> DevResult<bool>;
    fn generic_phy_reset(&mut self) -> DevResult;
    fn get_cfg_done(&mut self) -> Option<DevResult>;
    fn config_mac_link(&mut self, speed: u16, duplex: u16) -> DevResult;
    fn delay_us(&mut self, micros: u32) -> DevResult;
}

/// upstream: e1000_82543.c e1000_init_phy_params_82543()
pub fn init_phy_params_82543(
    media: E1000MediaType,
    mac: E1000MacType,
    phy_id: Option<u32>,
    phy_disabled: bool,
) -> DevResult<Option<u32>> {
    if media != E1000MediaType::Copper {
        return Ok(None);
    }
    if !phy_disabled { /* The caller runs the family PHY reset before identification. */ }
    let expected = match mac {
        E1000MacType::I82543 => 0x01410c50,
        E1000MacType::I82544 => 0x01410c30,
        _ => return Err(DevError::Io),
    };
    if phy_id != Some(expected) {
        return Err(DevError::Io);
    }
    Ok(Some(expected))
}

/// upstream: e1000_82543.c e1000_init_nvm_params_82543()
pub const fn init_nvm_params_82543() -> E1000NvmConfig {
    E1000NvmConfig {
        kind: E1000NvmType::Microwire,
        word_size: 64,
        delay_usec: 50,
        opcode_bits: 3,
        address_bits: 6,
        page_size: 1,
    }
}

/// upstream: e1000_82543.c e1000_init_mac_params_82543()
pub const fn init_mac_params_82543(device_id: u16) -> MacParams82543 {
    let media = if matches!(device_id, 0x1001 | 0x1009) {
        E1000MediaType::Fiber
    } else {
        E1000MediaType::Copper
    };
    MacParams82543 {
        media,
        mta_count: 128,
        rar_count: E1000_RAR_ENTRIES as u16,
    }
}

/// upstream: e1000_82543.c e1000_init_function_pointers_82543()
pub const fn init_function_pointers_82543() -> InitPointers82543 {
    InitPointers82543 {
        mac: true,
        nvm: true,
        phy: true,
    }
}

/// upstream: e1000_82543.c e1000_tbi_compatibility_enabled_82543()
pub fn tbi_compatibility_enabled_82543(mac: E1000MacType, state: TbiState) -> bool {
    mac == E1000MacType::I82543 && state.flags & TBI_COMPAT_ENABLED != 0
}

/// upstream: e1000_82543.c e1000_set_tbi_compatibility_82543()
pub fn set_tbi_compatibility_82543(mac: E1000MacType, state: &mut TbiState, enabled: bool) {
    if mac == E1000MacType::I82543 {
        if enabled {
            state.flags |= TBI_COMPAT_ENABLED
        } else {
            state.flags &= !TBI_COMPAT_ENABLED
        }
    }
}

/// upstream: e1000_82543.c e1000_tbi_sbp_enabled_82543()
pub fn tbi_sbp_enabled_82543(mac: E1000MacType, state: TbiState) -> bool {
    mac == E1000MacType::I82543 && state.flags & TBI_SBP_ENABLED != 0
}

/// upstream: e1000_82543.c e1000_set_tbi_sbp_82543()
pub fn set_tbi_sbp_82543(mac: E1000MacType, state: &mut TbiState, enabled: bool) {
    if enabled && tbi_compatibility_enabled_82543(mac, *state) {
        state.flags |= TBI_SBP_ENABLED
    } else {
        state.flags &= !TBI_SBP_ENABLED
    }
}

/// upstream: e1000_82543.c e1000_init_phy_disabled_82543()
pub fn init_phy_disabled_82543(mac: E1000MacType, state: TbiState) -> bool {
    mac == E1000MacType::I82543 && state.init_phy_disabled
}

/// upstream: e1000_82543.c e1000_tbi_adjust_stats_82543()
pub fn tbi_adjust_stats_82543(
    mac: E1000MacType,
    state: TbiState,
    stats: &mut TbiStats,
    frame_len: u32,
    dest: [u8; 6],
    max_frame_size: u32,
) {
    if !tbi_sbp_enabled_82543(mac, state) {
        return;
    }
    let frame_len = frame_len.saturating_sub(1);
    stats.crcerrs = stats.crcerrs.wrapping_sub(1);
    stats.gprc += 1;
    stats.gorc += u64::from(frame_len);
    if dest[0] == 0xff && dest[1] == 0xff {
        stats.bprc += 1
    } else if dest[0] & 1 != 0 {
        stats.mprc += 1
    }
    if frame_len == max_frame_size && stats.roc > 0 {
        stats.roc -= 1
    }
    match frame_len {
        64 => {
            stats.prc64 += 1;
            stats.prc127 = stats.prc127.wrapping_sub(1)
        }
        127 => {
            stats.prc127 += 1;
            stats.prc255 = stats.prc255.wrapping_sub(1)
        }
        255 => {
            stats.prc255 += 1;
            stats.prc511 = stats.prc511.wrapping_sub(1)
        }
        511 => {
            stats.prc511 += 1;
            stats.prc1023 = stats.prc1023.wrapping_sub(1)
        }
        1023 => {
            stats.prc1023 += 1;
            stats.prc1522 = stats.prc1522.wrapping_sub(1)
        }
        1522 => stats.prc1522 += 1,
        _ => {}
    }
}

/// upstream: e1000_82543.c e1000_raise_mdi_clk_82543()
pub fn raise_mdi_clk_82543<I: E1000RegisterIo>(io: &mut I, ctrl: &mut u32) -> DevResult {
    io.write_register(E1000_CTRL, *ctrl | CTRL_MDC)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(10);
    Ok(())
}

/// upstream: e1000_82543.c e1000_lower_mdi_clk_82543()
pub fn lower_mdi_clk_82543<I: E1000RegisterIo>(io: &mut I, ctrl: &mut u32) -> DevResult {
    *ctrl &= !CTRL_MDC;
    io.write_register(E1000_CTRL, *ctrl)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(10);
    Ok(())
}

/// upstream: e1000_82543.c e1000_shift_out_mdi_bits_82543()
pub fn shift_out_mdi_bits_82543<I: E1000RegisterIo>(
    io: &mut I,
    data: u32,
    count: u16,
) -> DevResult {
    if count == 0 || count > 32 {
        return Err(DevError::InvalidParam);
    }
    let mut mask = 1u32 << (count - 1);
    let mut ctrl = io.read_register(E1000_CTRL)? | CTRL_MDIO_DIR | CTRL_MDC_DIR;
    while mask != 0 {
        if data & mask != 0 {
            ctrl |= CTRL_MDIO
        } else {
            ctrl &= !CTRL_MDIO
        };
        io.write_register(E1000_CTRL, ctrl)?;
        let _ = io.read_register(E1000_STATUS)?;
        io.delay_us(10);
        raise_mdi_clk_82543(io, &mut ctrl)?;
        lower_mdi_clk_82543(io, &mut ctrl)?;
        mask >>= 1;
    }
    Ok(())
}

/// upstream: e1000_82543.c e1000_shift_in_mdi_bits_82543()
pub fn shift_in_mdi_bits_82543<I: E1000RegisterIo>(io: &mut I) -> DevResult<u16> {
    let mut ctrl = io.read_register(E1000_CTRL)? & !CTRL_MDIO_DIR & !CTRL_MDIO;
    io.write_register(E1000_CTRL, ctrl)?;
    let _ = io.read_register(E1000_STATUS)?;
    raise_mdi_clk_82543(io, &mut ctrl)?;
    lower_mdi_clk_82543(io, &mut ctrl)?;
    let mut data = 0u16;
    for _ in 0..16 {
        data <<= 1;
        raise_mdi_clk_82543(io, &mut ctrl)?;
        ctrl = io.read_register(E1000_CTRL)?;
        if ctrl & CTRL_MDIO != 0 {
            data |= 1
        };
        lower_mdi_clk_82543(io, &mut ctrl)?;
    }
    raise_mdi_clk_82543(io, &mut ctrl)?;
    lower_mdi_clk_82543(io, &mut ctrl)?;
    Ok(data)
}

/// upstream: e1000_82543.c e1000_read_phy_reg_82543()
pub fn read_phy_reg_82543<I: E1000RegisterIo>(
    io: &mut I,
    phy_addr: u8,
    offset: u32,
) -> DevResult<u16> {
    if offset > MAX_PHY_REG_ADDRESS {
        return Err(DevError::InvalidParam);
    }
    shift_out_mdi_bits_82543(io, PHY_PREAMBLE, PHY_PREAMBLE_SIZE)?;
    let mdic = offset | (u32::from(phy_addr) << 5) | (PHY_OP_READ << 10) | (PHY_SOF << 12);
    shift_out_mdi_bits_82543(io, mdic, 14)?;
    shift_in_mdi_bits_82543(io)
}

/// upstream: e1000_82543.c e1000_write_phy_reg_82543()
pub fn write_phy_reg_82543<I: E1000RegisterIo>(
    io: &mut I,
    phy_addr: u8,
    offset: u32,
    data: u16,
) -> DevResult {
    if offset > MAX_PHY_REG_ADDRESS {
        return Err(DevError::InvalidParam);
    }
    shift_out_mdi_bits_82543(io, PHY_PREAMBLE, PHY_PREAMBLE_SIZE)?;
    let mut mdic = (PHY_TURNAROUND
        | (offset << 2)
        | (u32::from(phy_addr) << 7)
        | (PHY_OP_WRITE << 12)
        | (PHY_SOF << 14))
        << 16;
    mdic |= u32::from(data);
    shift_out_mdi_bits_82543(io, mdic, 32)
}

/// upstream: e1000_82543.c e1000_phy_force_speed_duplex_82543()
pub fn phy_force_speed_duplex_82543<O: Ops82543>(
    ops: &mut O,
    autoneg: bool,
    forced_speed_duplex: u16,
) -> DevResult {
    ops.phy_force_speed_duplex_m88()?;
    if !autoneg && forced_speed_duplex & ALL_10_SPEED != 0 {
        polarity_reversal_workaround_82543(ops)?;
    }
    Ok(())
}

/// upstream: e1000_82543.c e1000_polarity_reversal_workaround_82543()
pub fn polarity_reversal_workaround_82543<O: Ops82543>(ops: &mut O) -> DevResult {
    const PAGE: u16 = 0x1f;
    const GEN_CONTROL: u16 = 0x1e;
    const STATUS: u16 = 1;
    const LINK_STATUS: u16 = 0x0004;
    ops.write_phy(PAGE, 0x0019)?;
    ops.write_phy(GEN_CONTROL, 0xffff)?;
    ops.write_phy(PAGE, 0)?;
    for _ in (0..PHY_FORCE_TIME).rev() {
        let _ = ops.read_phy(STATUS)?;
        let value = ops.read_phy(STATUS)?;
        if value & !LINK_STATUS == 0 {
            break;
        }
        ops.delay_us(100_000)?;
    }
    ops.delay_us(1_000_000)?;
    ops.write_phy(PAGE, 0x0019)?;
    ops.delay_us(50_000)?;
    ops.write_phy(GEN_CONTROL, 0xfff0)?;
    ops.delay_us(50_000)?;
    ops.write_phy(GEN_CONTROL, 0xff00)?;
    ops.delay_us(50_000)?;
    ops.write_phy(GEN_CONTROL, 0)?;
    ops.write_phy(PAGE, 0)?;
    let _ = ops.phy_has_link(PHY_FORCE_TIME, 100_000)?;
    Ok(())
}

/// upstream: e1000_82543.c e1000_phy_hw_reset_82543()
pub fn phy_hw_reset_82543<I: E1000RegisterIo, O: Ops82543>(io: &mut I, ops: &mut O) -> DevResult {
    let mut ctrl = io.read_register(E1000_CTRL_EXT)? | E1000_CTRL_EXT_SDP4_DIR;
    ctrl &= !E1000_CTRL_EXT_SDP4_DATA;
    io.write_register(E1000_CTRL_EXT, ctrl)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(10_000);
    ctrl |= E1000_CTRL_EXT_SDP4_DATA;
    io.write_register(E1000_CTRL_EXT, ctrl)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(150);
    if let Some(result) = ops.get_cfg_done() {
        result?;
    }
    Ok(())
}

/// upstream: e1000_82543.c e1000_reset_hw_82543()
pub fn reset_hw_82543<I: E1000RegisterIo, O: Ops82543, R: FnMut() -> DevResult>(
    io: &mut I,
    ops: &mut O,
    tbi: &mut TbiState,
    mac: E1000MacType,
    mut reload_nvm: R,
) -> DevResult {
    io.write_register(E1000_IMC, u32::MAX)?;
    io.write_register(E1000_RCTL, 0)?;
    io.write_register(E1000_TCTL, E1000_TCTL_PSP)?;
    let _ = io.read_register(E1000_STATUS)?;
    set_tbi_sbp_82543(mac, tbi, false);
    io.delay_us(10_000);
    let ctrl = io.read_register(E1000_CTRL)?;
    if mac == E1000MacType::I82543 {
        io.write_register(E1000_CTRL, ctrl | E1000_CTRL_RST)?;
    } else {
        io.write_register_io(E1000_CTRL, ctrl | E1000_CTRL_RST)?;
    }
    reload_nvm()?;
    io.delay_us(2_000);
    io.write_register(E1000_IMC, u32::MAX)?;
    let _ = io.read_register(E1000_ICR)?;
    let _ = ops;
    Ok(())
}

pub trait InitOps82543 {
    fn clear_vfta(&mut self) -> DevResult;
    fn init_rx_addrs(&mut self, count: u16) -> DevResult;
    fn pcix_mmrbc_workaround(&mut self) -> DevResult;
    fn setup_link(&mut self) -> DevResult;
    fn clear_counters(&mut self) -> DevResult;
}

/// upstream: e1000_82543.c e1000_init_hw_82543()
pub fn init_hw_82543<I: E1000RegisterIo, O: InitOps82543>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
    dma_fairness: bool,
    mta_count: u16,
    rar_count: u16,
) -> DevResult {
    io.write_register(E1000_VET, 0)?;
    ops.clear_vfta()?;
    ops.init_rx_addrs(rar_count)?;
    for i in 0..mta_count {
        io.write_register(E1000_MTA + u32::from(i) * 4, 0)?;
        let _ = io.read_register(E1000_STATUS)?;
    }
    if mac == E1000MacType::I82543 && dma_fairness {
        let ctrl = io.read_register(E1000_CTRL)?;
        io.write_register(E1000_CTRL, ctrl | E1000_CTRL_PRIOR)?;
    }
    ops.pcix_mmrbc_workaround()?;
    let link = ops.setup_link();
    ops.clear_counters()?;
    link
}

/// upstream: e1000_82543.c e1000_setup_link_82543()
pub fn setup_link_82543<I: E1000RegisterIo, N: E1000NvmAccess, O: Ops82543>(
    io: &mut I,
    nvm: &mut N,
    ops: &mut O,
    mac: E1000MacType,
) -> DevResult {
    if mac == E1000MacType::I82543 {
        let word = nvm
            .read_nvm_words(NVM_INIT_CONTROL2_REG, 1)?
            .first()
            .copied()
            .ok_or(DevError::Io)?;
        let ctrl_ext = u32::from(word & NVM_WORD0F_SWPDIO_EXT_MASK) << NVM_SWDPIO_EXT_SHIFT;
        io.write_register(E1000_CTRL_EXT, ctrl_ext)?;
    }
    ops.setup_link_generic()
}

/// upstream: e1000_82543.c e1000_setup_copper_link_82543()
pub fn setup_copper_link_82543<I: E1000RegisterIo, O: Ops82543>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
    autoneg: bool,
    forced_speed_duplex: u16,
) -> DevResult {
    let mut ctrl = io.read_register(E1000_CTRL)? | CTRL_SLU;
    if mac == E1000MacType::I82543 {
        ctrl |= E1000_CTRL_FRCSPD | E1000_CTRL_FRCDPX;
        io.write_register(E1000_CTRL, ctrl)?;
        ops.phy_reset()?;
    } else {
        ctrl &= !(E1000_CTRL_FRCSPD | E1000_CTRL_FRCDPX);
        io.write_register(E1000_CTRL, ctrl)?;
    }
    ops.setup_copper_m88()?;
    if autoneg {
        ops.copper_autoneg()?;
    } else {
        phy_force_speed_duplex_82543(ops, false, forced_speed_duplex)?;
    }
    if ops.phy_has_link(10, 10)? {
        if mac == E1000MacType::I82544 {
            ops.config_collision_dist()?;
        } else {
            let (speed, duplex) = ops.link_speed_duplex()?;
            ops.config_mac_link(speed, duplex)?;
        }
        ops.config_fc_after_link_up()?;
    }
    Ok(())
}

/// upstream: e1000_82543.c e1000_setup_fiber_link_82543()
pub fn setup_fiber_link_82543<I: E1000RegisterIo, O: Ops82543>(
    io: &mut I,
    ops: &mut O,
) -> DevResult {
    let mut ctrl = io.read_register(E1000_CTRL)? & !CTRL_LRST;
    ops.config_collision_dist()?;
    ops.commit_flow_control()?;
    io.write_register(E1000_CTRL, ctrl)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(1_000);
    ctrl = io.read_register(E1000_CTRL)?;
    if ctrl & CTRL_SWDPIN1 == 0 {
        ops.poll_fiber_link()?;
    }
    Ok(())
}

/// upstream: e1000_82543.c e1000_check_for_copper_link_82543()
pub fn check_for_copper_link_82543<I: E1000RegisterIo, O: Ops82543>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
    tbi: &mut TbiState,
    get_link_status: &mut bool,
    autoneg: bool,
    forced_speed_duplex: u16,
) -> DevResult {
    if !*get_link_status {
        return Ok(());
    }
    if !ops.phy_has_link(1, 0)? {
        return Ok(());
    }
    *get_link_status = false;
    ops.check_downshift()?;
    if !autoneg {
        if forced_speed_duplex & ALL_10_SPEED != 0 {
            io.write_register(E1000_IMC, u32::MAX)?;
            let _ = polarity_reversal_workaround_82543(ops);
            let icr = io.read_register(E1000_ICR)?;
            io.write_register(E1000_ICS, icr & !0x0000_0004)?;
            io.write_register(E1000_IMS, IMS_ENABLE_MASK)?;
        }
        return Err(DevError::Unsupported);
    }
    if mac == E1000MacType::I82544 {
        ops.config_collision_dist()?;
    } else {
        let (speed, duplex) = ops.link_speed_duplex()?;
        ops.config_mac_link(speed, duplex)?;
    }
    let result = ops.config_fc_after_link_up();
    if tbi_compatibility_enabled_82543(mac, *tbi) {
        match ops.link_speed_duplex() {
            Err(e) => return Err(e),
            Ok((speed, _)) if speed != 1000 => {
                if tbi_sbp_enabled_82543(mac, *tbi) {
                    set_tbi_sbp_82543(mac, tbi, false);
                    let rctl = io.read_register(E1000_RCTL)? & !E1000_RCTL_SBP;
                    io.write_register(E1000_RCTL, rctl)?;
                }
            }
            Ok(_) => {
                if !tbi_sbp_enabled_82543(mac, *tbi) {
                    set_tbi_sbp_82543(mac, tbi, true);
                    let rctl = io.read_register(E1000_RCTL)? | E1000_RCTL_SBP;
                    io.write_register(E1000_RCTL, rctl)?;
                }
            }
        }
    }
    result
}

/// upstream: e1000_82543.c e1000_check_for_fiber_link_82543()
pub fn check_for_fiber_link_82543<I: E1000RegisterIo, O: Ops82543>(
    io: &mut I,
    ops: &mut O,
    autoneg: bool,
    autoneg_failed: &mut bool,
    txcw: u32,
    serdes_has_link: &mut bool,
) -> DevResult {
    const RXCW_ANE: u32 = 0x8000_0000;
    let ctrl = io.read_register(E1000_CTRL)?;
    let status = io.read_register(E1000_STATUS)?;
    let rxcw = io.read_register(E1000_RXCW)?;
    if ctrl & CTRL_SWDPIN1 == 0 && status & STATUS_LU == 0 && rxcw & RXCW_C == 0 {
        if !*autoneg_failed {
            *autoneg_failed = true;
            return Ok(());
        }
        let _ = autoneg;
        io.write_register(E1000_TXCW, txcw & !RXCW_ANE)?;
        let ctrl = io.read_register(E1000_CTRL)? | CTRL_SLU | CTRL_FD;
        io.write_register(E1000_CTRL, ctrl)?;
        ops.config_fc_after_link_up()?;
    } else if ctrl & CTRL_SLU != 0 && rxcw & RXCW_C != 0 {
        io.write_register(E1000_TXCW, txcw)?;
        io.write_register(E1000_CTRL, ctrl & !CTRL_SLU)?;
        *serdes_has_link = true;
    }
    Ok(())
}

/// upstream: e1000_82543.c e1000_config_mac_to_phy_82543()
pub fn config_mac_to_phy_82543<I: E1000RegisterIo, O: Ops82543>(
    io: &mut I,
    ops: &mut O,
) -> DevResult {
    let mut ctrl = io.read_register(E1000_CTRL)? | E1000_CTRL_FRCSPD | E1000_CTRL_FRCDPX;
    ctrl &= !(E1000_CTRL_SPD_SEL | E1000_CTRL_ILOS);
    let phy = ops.read_phy(M88_PHY_SPEC_STATUS)?;
    ctrl &= !CTRL_FD;
    if phy & M88_PSSR_DPLX != 0 {
        ctrl |= CTRL_FD;
    }
    ops.config_collision_dist()?;
    if phy & M88_PSSR_SPEED == M88_PSSR_1000MBS {
        ctrl |= 0x0000_0040;
    } else if phy & M88_PSSR_SPEED == M88_PSSR_100MBS {
        ctrl |= 0x0000_0020;
    }
    io.write_register(E1000_CTRL, ctrl)
}

/// upstream: e1000_82543.c e1000_write_vfta_82543()
pub fn write_vfta_82543<I: E1000RegisterIo, O: Ops82543>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
    offset: u32,
    value: u32,
) -> DevResult {
    if mac == E1000MacType::I82544 && offset & 1 != 0 {
        let prior = io.read_register(E1000_VFTA + (offset - 1) * 4)?;
        io.write_register(E1000_VFTA + offset * 4, value)?;
        let _ = io.read_register(E1000_STATUS)?;
        io.write_register(E1000_VFTA + (offset - 1) * 4, prior)?;
        let _ = io.read_register(E1000_STATUS)?;
        Ok(())
    } else {
        ops.clear_vfta_generic(offset, value)
    }
}

/// upstream: e1000_82543.c e1000_led_on_82543()
pub fn led_on_82543<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    media: E1000MediaType,
) -> DevResult {
    let mut ctrl = io.read_register(E1000_CTRL)?;
    if mac == E1000MacType::I82544 && media == E1000MediaType::Copper {
        ctrl &= !E1000_CTRL_SWDPIN0;
        ctrl |= E1000_CTRL_SWDPIO0;
    } else {
        ctrl |= E1000_CTRL_SWDPIN0 | E1000_CTRL_SWDPIO0;
    }
    io.write_register(E1000_CTRL, ctrl)
}

/// upstream: e1000_82543.c e1000_led_off_82543()
pub fn led_off_82543<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    media: E1000MediaType,
) -> DevResult {
    let mut ctrl = io.read_register(E1000_CTRL)?;
    if mac == E1000MacType::I82544 && media == E1000MediaType::Copper {
        ctrl |= E1000_CTRL_SWDPIN0 | E1000_CTRL_SWDPIO0;
    } else {
        ctrl &= !E1000_CTRL_SWDPIN0;
        ctrl |= E1000_CTRL_SWDPIO0;
    }
    io.write_register(E1000_CTRL, ctrl)
}

/// upstream: e1000_82543.c e1000_clear_hw_cntrs_82543()
pub fn clear_hw_cntrs_82543<I: E1000RegisterIo, B: FnMut() -> DevResult>(
    io: &mut I,
    mut clear_base: B,
) -> DevResult {
    clear_base()?;
    for reg in [
        0x405c, 0x4060, 0x4064, 0x4068, 0x406c, 0x4070, 0x40d8, 0x40dc, 0x40e0, 0x40e4, 0x40e8,
        0x40ec, 0x4004, 0x400c, 0x4034, 0x403c, 0x40f8, 0x40fc,
    ] {
        let _ = io.read_register(reg)?;
    }
    Ok(())
}

/// upstream: e1000_82543.c e1000_read_mac_addr_82543()
pub fn read_mac_addr_82543<N: E1000NvmAccess>(nvm: &mut N, pci_function: u8) -> DevResult<[u8; 6]> {
    let mut addr = [0u8; 6];
    for i in (0..6).step_by(2) {
        let word = nvm
            .read_nvm_words((i / 2) as u16, 1)?
            .first()
            .copied()
            .ok_or(DevError::Io)?;
        addr[i] = word as u8;
        addr[i + 1] = (word >> 8) as u8;
    }
    if pci_function == 1 {
        addr[5] ^= 1;
    }
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Io {
        regs: alloc::vec::Vec<(u32, u32)>,
        writes: alloc::vec::Vec<(u32, u32)>,
        delay: u64,
    }
    impl E1000RegisterIo for Io {
        fn read_register(&mut self, r: u32) -> DevResult<u32> {
            Ok(self
                .regs
                .iter()
                .find(|(x, _)| *x == r)
                .map(|(_, v)| *v)
                .unwrap_or(0))
        }
        fn write_register(&mut self, r: u32, v: u32) -> DevResult {
            self.writes.push((r, v));
            if let Some((_, x)) = self.regs.iter_mut().find(|(x, _)| *x == r) {
                *x = v
            } else {
                self.regs.push((r, v))
            }
            Ok(())
        }
        fn delay_us(&mut self, u: u32) {
            self.delay += u64::from(u)
        }
        fn invalid_tail_write(&mut self, _: &'static str) {}
    }
    struct Ops;
    impl Ops82543 for Ops {
        fn phy_reset(&mut self) -> DevResult {
            Ok(())
        }
        fn setup_link_generic(&mut self) -> DevResult {
            Ok(())
        }
        fn setup_copper_m88(&mut self) -> DevResult {
            Ok(())
        }
        fn copper_autoneg(&mut self) -> DevResult {
            Ok(())
        }
        fn phy_has_link(&mut self, _: usize, _: u32) -> DevResult<bool> {
            Ok(false)
        }
        fn config_fc_after_link_up(&mut self) -> DevResult {
            Ok(())
        }
        fn config_collision_dist(&mut self) -> DevResult {
            Ok(())
        }
        fn poll_fiber_link(&mut self) -> DevResult {
            Ok(())
        }
        fn clear_vfta_generic(&mut self, _: u32, _: u32) -> DevResult {
            Ok(())
        }
        fn check_downshift(&mut self) -> DevResult {
            Ok(())
        }
        fn link_speed_duplex(&mut self) -> DevResult<(u16, u16)> {
            Ok((1000, 1))
        }
        fn read_phy(&mut self, _: u16) -> DevResult<u16> {
            Ok(0)
        }
        fn write_phy(&mut self, _: u16, _: u16) -> DevResult {
            Ok(())
        }
        fn phy_force_speed_duplex_m88(&mut self) -> DevResult {
            Ok(())
        }
        fn setup_fiber(&mut self) -> DevResult {
            Ok(())
        }
        fn commit_flow_control(&mut self) -> DevResult {
            Ok(())
        }
        fn phy_has_link_status(&mut self) -> DevResult<bool> {
            Ok(false)
        }
        fn generic_phy_reset(&mut self) -> DevResult {
            Ok(())
        }
        fn get_cfg_done(&mut self) -> Option<DevResult> {
            None
        }
        fn config_mac_link(&mut self, _: u16, _: u16) -> DevResult {
            Ok(())
        }
        fn delay_us(&mut self, _: u32) -> DevResult {
            Ok(())
        }
    }
    #[test]
    fn mdi_and_tbi_helpers_preserve_wire_order_and_counters() {
        let mut io = Io::default();
        shift_out_mdi_bits_82543(&mut io, 0b101, 3).unwrap();
        assert!(io.writes.len() >= 9);
        assert_eq!(io.delay, 3 * 30);
        assert!(read_phy_reg_82543(&mut io, 1, 32).is_err());
        let mut state = TbiState::default();
        set_tbi_compatibility_82543(E1000MacType::I82543, &mut state, true);
        set_tbi_sbp_82543(E1000MacType::I82543, &mut state, true);
        assert!(tbi_sbp_enabled_82543(E1000MacType::I82543, state));
        let mut stats = TbiStats {
            crcerrs: 1,
            roc: 1,
            prc127: 1,
            ..TbiStats::default()
        };
        tbi_adjust_stats_82543(
            E1000MacType::I82543,
            state,
            &mut stats,
            65,
            [0xff, 0xff, 0, 0, 0, 0],
            1518,
        );
        assert_eq!(stats.crcerrs, 0);
        assert_eq!(stats.gprc, 1);
        assert_eq!(stats.gorc, 64);
        assert_eq!(stats.bprc, 1);
        assert_eq!(init_nvm_params_82543().word_size, 64);
        assert_eq!(init_mac_params_82543(0x1001).media, E1000MediaType::Fiber);
        assert!(
            init_phy_params_82543(
                E1000MediaType::Copper,
                E1000MacType::I82543,
                Some(0x01410c50),
                true
            )
            .is_ok()
        );
    }
    #[test]
    fn phy_reset_and_vfta_paths_keep_family_workarounds() {
        let mut io = Io::default();
        let mut ops = Ops;
        phy_hw_reset_82543(&mut io, &mut ops).unwrap();
        assert_eq!(io.delay, 10_150);
        write_vfta_82543(&mut io, &mut ops, E1000MacType::I82544, 3, 0x55).unwrap();
        assert!(io.writes.contains(&(E1000_VFTA + 12, 0x55)));
        assert!(led_on_82543(&mut io, E1000MacType::I82544, E1000MediaType::Copper).is_ok());
    }
}
