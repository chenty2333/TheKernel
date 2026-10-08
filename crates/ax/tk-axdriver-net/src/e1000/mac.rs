//! Shared Intel e1000 MAC operations.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_mac.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{
    osdep::{E1000PciConfig, E1000RegisterIo, read_pcie_cap_reg},
    registers::*,
};

const ETHER_ADDR_LEN: usize = 6;
const MTA_REG_COUNT: usize = 128;
const VFTA_REG_COUNT: usize = 128;
const RAL: u32 = 0x05400;
const RAH: u32 = 0x05404;
const PCI_HEADER_TYPE_REGISTER: u32 = 0x0e;
const PCI_HEADER_TYPE_MULTIFUNC: u16 = 0x80;
const PCIE_LINK_STATUS: u32 = 0x12;
const PCIE_LINK_SPEED_MASK: u16 = 0x000f;
const PCIE_LINK_WIDTH_MASK: u16 = 0x03f0;
const PCIE_LINK_WIDTH_SHIFT: u32 = 4;
const PCIX_COMMAND_REGISTER: u32 = 0xe6;
const PCIX_STATUS_REGISTER_HI: u32 = 0xea;
const PCIX_COMMAND_MMRBC_MASK: u16 = 0x000c;
const PCIX_COMMAND_MMRBC_SHIFT: u32 = 2;
const PCIX_STATUS_HI_MMRBC_MASK: u16 = 0x0060;
const PCIX_STATUS_HI_MMRBC_SHIFT: u32 = 5;
const NVM_ID_LED_SETTINGS: u16 = 0x0004;
const NVM_INIT_CONTROL2_REG: u16 = 0x000f;
const NVM_WORD0F_PAUSE_MASK: u16 = 0x3000;
const NVM_WORD0F_ASM_DIR: u16 = 0x2000;
const AUTO_READ_DONE_TIMEOUT_MS: usize = 10;
const SWFW_SYNC_TIMEOUT: usize = 200;
const MASTER_DISABLE_TIMEOUT: usize = 800;
const PCIE_NO_SNOOP_ALL: u32 = 0x0000000f;
const IFS_MIN: u32 = 40;
const IFS_MAX: u32 = 80;
const IFS_STEP: u32 = 10;
const IFS_RATIO: u32 = 4;
const MIN_NUM_XMITS: u32 = 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E1000BusType {
    Pci,
    PciX,
    PciExpress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E1000BusSpeed {
    Unknown,
    Reserved,
    Mhz33,
    Mhz66,
    Mhz100,
    Mhz133,
    Gt2500,
    Gt5000,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E1000BusWidth {
    Unknown,
    Bits32,
    Bits64,
    Pcie(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct E1000BusInfo {
    pub kind: E1000BusType,
    pub speed: E1000BusSpeed,
    pub width: E1000BusWidth,
    pub function: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowControlMode {
    None,
    RxPause,
    TxPause,
    Full,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E1000MediaType {
    Copper,
    Fiber,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LedModes {
    pub default: u32,
    pub mode1: u32,
    pub mode2: u32,
}

pub trait E1000NvmReader {
    fn read_word(&mut self, offset: u16) -> DevResult<u16>;
}

fn normalize_led_default(mut data: u16) -> u16 {
    if data == 0 || data == u16::MAX {
        data = (0x8u16 << 12) | (0x9u16 << 8) | (0x1u16 << 4) | 0x1;
    }
    data
}

/// upstream: e1000_mac.c e1000_valid_led_default_generic()
pub fn valid_led_default_generic<N: E1000NvmReader>(nvm: &mut N) -> DevResult<u16> {
    Ok(normalize_led_default(nvm.read_word(NVM_ID_LED_SETTINGS)?))
}

/// upstream: e1000_mac.c e1000_set_default_fc_generic()
pub fn set_default_fc_generic<N: E1000NvmReader>(
    nvm: &mut N,
    i350: bool,
    lan_function: u8,
) -> DevResult<FlowControlMode> {
    let offset = if i350 && lan_function != 0 {
        0x40u16
            .checked_add(
                0x40u16
                    .checked_mul(u16::from(lan_function))
                    .ok_or(DevError::InvalidParam)?,
            )
            .ok_or(DevError::InvalidParam)?
    } else {
        0
    };
    let data = nvm.read_word(NVM_INIT_CONTROL2_REG + offset)?;
    Ok(match data & NVM_WORD0F_PAUSE_MASK {
        0 => FlowControlMode::None,
        NVM_WORD0F_ASM_DIR => FlowControlMode::TxPause,
        _ => FlowControlMode::Full,
    })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdaptiveIfs {
    pub enabled: bool,
    pub current: u32,
    pub min: u32,
    pub max: u32,
    pub step: u32,
    pub ratio: u32,
    pub in_mode: bool,
    pub collision_delta: u32,
    pub tx_packet_delta: u32,
}

/// upstream: e1000_mac.c e1000_reset_adaptive_generic()
pub fn reset_adaptive_generic<I: E1000RegisterIo>(
    io: &mut I,
    state: &mut AdaptiveIfs,
) -> DevResult {
    if !state.enabled {
        return Ok(());
    }
    state.current = 0;
    state.min = IFS_MIN;
    state.max = IFS_MAX;
    state.step = IFS_STEP;
    state.ratio = IFS_RATIO;
    state.in_mode = false;
    io.write_register(E1000_AIT, 0)
}

/// upstream: e1000_mac.c e1000_update_adaptive_generic()
pub fn update_adaptive_generic<I: E1000RegisterIo>(
    io: &mut I,
    state: &mut AdaptiveIfs,
) -> DevResult {
    if !state.enabled {
        return Ok(());
    }
    if state.collision_delta.wrapping_mul(state.ratio) > state.tx_packet_delta {
        if state.tx_packet_delta > MIN_NUM_XMITS {
            state.in_mode = true;
            if state.current < state.max {
                state.current = if state.current == 0 {
                    state.min
                } else {
                    state.current + state.step
                };
                io.write_register(E1000_AIT, state.current)?;
            }
        }
    } else if state.in_mode && state.tx_packet_delta <= MIN_NUM_XMITS {
        state.current = 0;
        state.in_mode = false;
        io.write_register(E1000_AIT, 0)?;
    }
    Ok(())
}

/// upstream: e1000_mac.c e1000_set_pcie_no_snoop_generic()
pub fn set_pcie_no_snoop_generic<I: E1000RegisterIo>(
    io: &mut I,
    bus_is_pcie: bool,
    no_snoop: u32,
) -> DevResult {
    if !bus_is_pcie || no_snoop == 0 {
        return Ok(());
    }
    let mut gcr = io.read_register(E1000_GCR)?;
    gcr &= !PCIE_NO_SNOOP_ALL;
    gcr |= no_snoop;
    io.write_register(E1000_GCR, gcr)
}

/// upstream: e1000_mac.c e1000_disable_pcie_master_generic()
pub fn disable_pcie_master_generic<I: E1000RegisterIo>(io: &mut I, bus_is_pcie: bool) -> DevResult {
    if !bus_is_pcie {
        return Ok(());
    }
    let ctrl = io.read_register(E1000_CTRL)? | E1000_CTRL_GIO_MASTER_DISABLE;
    io.write_register(E1000_CTRL, ctrl)?;
    for _ in 0..MASTER_DISABLE_TIMEOUT {
        if io.read_register(E1000_STATUS)? & E1000_STATUS_GIO_MASTER_ENABLE == 0 {
            return Ok(());
        }
        io.delay_us(100);
    }
    Err(DevError::ResourceBusy)
}

/// upstream: e1000_mac.c e1000_pcix_mmrbc_workaround_generic()
pub fn pcix_mmrbc_workaround_generic<P: E1000PciConfig>(
    pci: &mut P,
    bus_is_pcix: bool,
) -> DevResult {
    if !bus_is_pcix {
        return Ok(());
    }
    let mut command = super::osdep::read_pci_cfg(pci, PCIX_COMMAND_REGISTER)?;
    let status = super::osdep::read_pci_cfg(pci, PCIX_STATUS_REGISTER_HI)?;
    let command_mmrbc = (command & PCIX_COMMAND_MMRBC_MASK) >> PCIX_COMMAND_MMRBC_SHIFT;
    let mut status_mmrbc = (status & PCIX_STATUS_HI_MMRBC_MASK) >> PCIX_STATUS_HI_MMRBC_SHIFT;
    if status_mmrbc == 3 {
        status_mmrbc = 2;
    }
    if command_mmrbc > status_mmrbc {
        command &= !PCIX_COMMAND_MMRBC_MASK;
        command |= status_mmrbc << PCIX_COMMAND_MMRBC_SHIFT;
        super::osdep::write_pci_cfg(pci, PCIX_COMMAND_REGISTER, command)?;
    }
    Ok(())
}

/// upstream: e1000_mac.c e1000_clear_hw_cntrs_base_generic()
pub fn clear_hw_cntrs_base_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    const COUNTERS: &[u32] = &[
        E1000_CRCERRS,
        E1000_SYMERRS,
        E1000_MPC,
        E1000_SCC,
        E1000_ECOL,
        E1000_MCC,
        E1000_LATECOL,
        E1000_COLC,
        E1000_DC,
        E1000_SEC,
        E1000_RLEC,
        E1000_XONRXC,
        E1000_XONTXC,
        E1000_XOFFRXC,
        E1000_XOFFTXC,
        E1000_FCRUC,
        E1000_GPRC,
        E1000_BPRC,
        E1000_MPRC,
        E1000_GPTC,
        E1000_GORCL,
        E1000_GORCH,
        E1000_GOTCL,
        E1000_GOTCH,
        E1000_RNBC,
        E1000_RUC,
        E1000_RFC,
        E1000_ROC,
        E1000_RJC,
        E1000_TORL,
        E1000_TORH,
        E1000_TOTL,
        E1000_TOTH,
        E1000_TPR,
        E1000_TPT,
        E1000_MPTC,
        E1000_BPTC,
        E1000_TLPIC,
        E1000_RLPIC,
    ];
    for register in COUNTERS {
        let _ = io.read_register(*register)?;
    }
    Ok(())
}

/// upstream: e1000_mac.c e1000_valid_led_default_generic()
/// upstream: e1000_mac.c e1000_id_led_init_generic()
pub fn id_led_init_generic<N: E1000NvmReader>(nvm: &mut N, ledctl: u32) -> DevResult<LedModes> {
    let data = valid_led_default_generic(nvm)?;
    let mut mode1 = ledctl;
    let mut mode2 = ledctl;
    for led in 0..4u32 {
        let shift = led * 8;
        let config = (data >> (led * 4)) & 0x0f;
        match config {
            4..=6 => {
                mode1 &= !(0xff << shift);
                mode1 |= E1000_LEDCTL_MODE_LED_ON << shift;
            }
            7..=9 => {
                mode1 &= !(0xff << shift);
                mode1 |= E1000_LEDCTL_MODE_LED_OFF << shift;
            }
            _ => {}
        }
        match config {
            2 | 5 | 8 => {
                mode2 &= !(0xff << shift);
                mode2 |= E1000_LEDCTL_MODE_LED_ON << shift;
            }
            3 | 6 | 9 => {
                mode2 &= !(0xff << shift);
                mode2 |= E1000_LEDCTL_MODE_LED_OFF << shift;
            }
            _ => {}
        }
    }
    Ok(LedModes {
        default: ledctl,
        mode1,
        mode2,
    })
}

/// upstream: e1000_mac.c e1000_setup_led_generic()
pub fn setup_led_generic<I: E1000RegisterIo>(
    io: &mut I,
    media: E1000MediaType,
    mut modes: LedModes,
    setup_is_generic: bool,
) -> DevResult<LedModes> {
    if !setup_is_generic {
        return Err(DevError::InvalidParam);
    }
    match media {
        E1000MediaType::Fiber => {
            let ledctl = io.read_register(E1000_LEDCTL)?;
            modes.default = ledctl;
            let mut value = ledctl
                & !(E1000_LEDCTL_LED0_IVRT | E1000_LEDCTL_LED0_BLINK | E1000_LEDCTL_LED0_MODE_MASK);
            value |= E1000_LEDCTL_MODE_LED_OFF << E1000_LEDCTL_LED0_MODE_SHIFT;
            io.write_register(E1000_LEDCTL, value)?;
        }
        E1000MediaType::Copper => io.write_register(E1000_LEDCTL, modes.mode1)?,
        E1000MediaType::Other => {}
    }
    Ok(modes)
}

/// upstream: e1000_mac.c e1000_cleanup_led_generic()
pub fn cleanup_led_generic<I: E1000RegisterIo>(io: &mut I, modes: LedModes) -> DevResult {
    io.write_register(E1000_LEDCTL, modes.default)
}

/// upstream: e1000_mac.c e1000_blink_led_generic()
pub fn blink_led_generic<I: E1000RegisterIo>(
    io: &mut I,
    media: E1000MediaType,
    modes: LedModes,
) -> DevResult {
    let blink = if media == E1000MediaType::Fiber {
        E1000_LEDCTL_LED0_BLINK | (E1000_LEDCTL_MODE_LED_ON << E1000_LEDCTL_LED0_MODE_SHIFT)
    } else {
        let mut value = modes.mode2;
        for shift in (0..32).step_by(8) {
            let mode = (modes.mode2 >> shift) & E1000_LEDCTL_LED0_MODE_MASK;
            let default = modes.default >> shift;
            if (default & E1000_LEDCTL_LED0_IVRT == 0 && mode == E1000_LEDCTL_MODE_LED_ON)
                || (default & E1000_LEDCTL_LED0_IVRT != 0 && mode == E1000_LEDCTL_MODE_LED_OFF)
            {
                value &= !(E1000_LEDCTL_LED0_MODE_MASK << shift);
                value |= (E1000_LEDCTL_LED0_BLINK | E1000_LEDCTL_MODE_LED_ON) << shift;
            }
        }
        value
    };
    io.write_register(E1000_LEDCTL, blink)
}

/// upstream: e1000_mac.c e1000_led_on_generic()
pub fn led_on_generic<I: E1000RegisterIo>(
    io: &mut I,
    media: E1000MediaType,
    modes: LedModes,
) -> DevResult {
    match media {
        E1000MediaType::Fiber => {
            let mut ctrl = io.read_register(E1000_CTRL)?;
            ctrl &= !E1000_CTRL_SWDPIN0;
            ctrl |= E1000_CTRL_SWDPIO0;
            io.write_register(E1000_CTRL, ctrl)
        }
        E1000MediaType::Copper => io.write_register(E1000_LEDCTL, modes.mode2),
        E1000MediaType::Other => Ok(()),
    }
}

/// upstream: e1000_mac.c e1000_led_off_generic()
pub fn led_off_generic<I: E1000RegisterIo>(
    io: &mut I,
    media: E1000MediaType,
    modes: LedModes,
) -> DevResult {
    match media {
        E1000MediaType::Fiber => {
            let mut ctrl = io.read_register(E1000_CTRL)?;
            ctrl |= E1000_CTRL_SWDPIN0 | E1000_CTRL_SWDPIO0;
            io.write_register(E1000_CTRL, ctrl)
        }
        E1000MediaType::Copper => io.write_register(E1000_LEDCTL, modes.mode1),
        E1000MediaType::Other => Ok(()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlowControl {
    pub mode: FlowControlMode,
    pub low_water: u32,
    pub high_water: u32,
    pub pause_time: u32,
    pub send_xon: bool,
}

/// upstream: e1000_mac.c e1000_set_fc_watermarks_generic()
pub fn set_fc_watermarks_generic<I: E1000RegisterIo>(io: &mut I, fc: FlowControl) -> DevResult {
    let tx_pause = matches!(fc.mode, FlowControlMode::TxPause | FlowControlMode::Full);
    let (mut low, high) = if tx_pause {
        (fc.low_water, fc.high_water)
    } else {
        (0, 0)
    };
    if tx_pause && fc.send_xon {
        low |= E1000_FCRTL_XONE;
    }
    io.write_register(E1000_FCRTL, low)?;
    io.write_register(E1000_FCRTH, high)?;
    Ok(())
}

/// upstream: e1000_mac.c e1000_force_mac_fc_generic()
pub fn force_mac_fc_generic<I: E1000RegisterIo>(io: &mut I, mode: FlowControlMode) -> DevResult {
    let mut control = io.read_register(E1000_CTRL)?;
    match mode {
        FlowControlMode::None => control &= !(E1000_CTRL_TFCE | E1000_CTRL_RFCE),
        FlowControlMode::RxPause => {
            control &= !E1000_CTRL_TFCE;
            control |= E1000_CTRL_RFCE;
        }
        FlowControlMode::TxPause => {
            control &= !E1000_CTRL_RFCE;
            control |= E1000_CTRL_TFCE;
        }
        FlowControlMode::Full => control |= E1000_CTRL_TFCE | E1000_CTRL_RFCE,
    }
    io.write_register(E1000_CTRL, control)
}

/// upstream: e1000_mac.c e1000_commit_fc_settings_generic()
pub fn commit_fc_settings_generic<I: E1000RegisterIo>(
    io: &mut I,
    mode: FlowControlMode,
) -> DevResult<u32> {
    let txcw = E1000_TXCW_ANE
        | E1000_TXCW_FD
        | match mode {
            FlowControlMode::None => 0,
            FlowControlMode::RxPause | FlowControlMode::Full => E1000_TXCW_PAUSE_MASK,
            FlowControlMode::TxPause => E1000_TXCW_ASM_DIR,
        };
    io.write_register(E1000_TXCW, txcw)?;
    Ok(txcw)
}

/// upstream: e1000_mac.c e1000_set_lan_id_multi_port_pcie()
pub fn set_lan_id_multi_port_pcie<I: E1000RegisterIo>(io: &mut I) -> DevResult<u8> {
    Ok(
        ((io.read_register(E1000_STATUS)? & E1000_STATUS_FUNC_MASK) >> E1000_STATUS_FUNC_SHIFT)
            as u8,
    )
}

/// upstream: e1000_mac.c e1000_set_lan_id_multi_port_pci()
pub fn set_lan_id_multi_port_pci<I: E1000RegisterIo, P: E1000PciConfig>(
    io: &mut I,
    pci: &mut P,
) -> DevResult<u8> {
    let header = super::osdep::read_pci_cfg(pci, PCI_HEADER_TYPE_REGISTER)?;
    if header & PCI_HEADER_TYPE_MULTIFUNC != 0 {
        set_lan_id_multi_port_pcie(io)
    } else {
        Ok(0)
    }
}

/// upstream: e1000_mac.c e1000_set_lan_id_single_port()
pub const fn set_lan_id_single_port() -> u8 {
    0
}

/// upstream: e1000_mac.c e1000_get_bus_info_pci_generic()
pub fn get_bus_info_pci_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult<E1000BusInfo> {
    let status = io.read_register(E1000_STATUS)?;
    let kind = if status & E1000_STATUS_PCIX_MODE != 0 {
        E1000BusType::PciX
    } else {
        E1000BusType::Pci
    };
    let speed = if kind == E1000BusType::Pci {
        if status & E1000_STATUS_PCI66 != 0 {
            E1000BusSpeed::Mhz66
        } else {
            E1000BusSpeed::Mhz33
        }
    } else {
        match status & E1000_STATUS_PCIX_SPEED {
            E1000_STATUS_PCIX_SPEED_66 => E1000BusSpeed::Mhz66,
            E1000_STATUS_PCIX_SPEED_100 => E1000BusSpeed::Mhz100,
            E1000_STATUS_PCIX_SPEED_133 => E1000BusSpeed::Mhz133,
            _ => E1000BusSpeed::Reserved,
        }
    };
    Ok(E1000BusInfo {
        kind,
        speed,
        width: if status & E1000_STATUS_BUS64 != 0 {
            E1000BusWidth::Bits64
        } else {
            E1000BusWidth::Bits32
        },
        function: set_lan_id_multi_port_pcie(io)?,
    })
}

/// upstream: e1000_mac.c e1000_get_bus_info_pcie_generic()
pub fn get_bus_info_pcie_generic<I: E1000RegisterIo, P: E1000PciConfig>(
    io: &mut I,
    pci: &mut P,
) -> DevResult<E1000BusInfo> {
    let link = read_pcie_cap_reg(pci, PCIE_LINK_STATUS).ok();
    let (speed, width) = match link {
        Some(link) => (
            match link & PCIE_LINK_SPEED_MASK {
                1 => E1000BusSpeed::Gt2500,
                2 => E1000BusSpeed::Gt5000,
                _ => E1000BusSpeed::Unknown,
            },
            E1000BusWidth::Pcie(((link & PCIE_LINK_WIDTH_MASK) >> PCIE_LINK_WIDTH_SHIFT) as u8),
        ),
        None => (E1000BusSpeed::Unknown, E1000BusWidth::Unknown),
    };
    Ok(E1000BusInfo {
        kind: E1000BusType::PciExpress,
        speed,
        width,
        function: set_lan_id_multi_port_pcie(io)?,
    })
}

/// upstream: e1000_mac.c e1000_config_collision_dist_generic()
pub fn config_collision_dist_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let mut control = io.read_register(E1000_TCTL)?;
    control &= !E1000_TCTL_COLD;
    control |= E1000_COLLISION_DISTANCE << E1000_COLD_SHIFT;
    io.write_register(E1000_TCTL, control)?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: e1000_mac.c e1000_get_speed_and_duplex_copper_generic()
pub fn get_speed_and_duplex_copper_generic<I: E1000RegisterIo>(
    io: &mut I,
) -> DevResult<(u16, u16)> {
    let status = io.read_register(E1000_STATUS)?;
    let speed = if status & E1000_STATUS_SPEED_1000 != 0 {
        1000
    } else if status & E1000_STATUS_SPEED_100 != 0 {
        100
    } else {
        10
    };
    let duplex = if status & E1000_STATUS_FD != 0 { 2 } else { 1 };
    Ok((speed, duplex))
}

/// upstream: e1000_mac.c e1000_get_speed_and_duplex_fiber_serdes_generic()
pub const fn get_speed_and_duplex_fiber_serdes_generic() -> (u16, u16) {
    (1000, 2)
}

/// upstream: e1000_mac.c e1000_get_auto_rd_done_generic()
pub fn get_auto_rd_done_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    for _ in 0..AUTO_READ_DONE_TIMEOUT_MS {
        if io.read_register(E1000_EECD)? & E1000_EECD_AUTO_RD != 0 {
            return Ok(());
        }
        io.delay_us(1000);
    }
    Err(DevError::Io)
}

/// upstream: e1000_mac.c e1000_validate_mdi_setting_generic()
pub fn validate_mdi_setting_generic(autoneg: bool, mdix: &mut u8) -> DevResult {
    if !autoneg && (*mdix == 0 || *mdix == 3) {
        *mdix = 1;
        return Err(DevError::InvalidParam);
    }
    Ok(())
}

/// upstream: e1000_mac.c e1000_validate_mdi_setting_crossover_generic()
pub const fn validate_mdi_setting_crossover_generic() -> DevResult {
    Ok(())
}

/// upstream: e1000_mac.c e1000_write_8bit_ctrl_reg_generic()
pub fn write_8bit_ctrl_reg_generic<I: E1000RegisterIo>(
    io: &mut I,
    register: u32,
    offset: u32,
    data: u8,
) -> DevResult {
    let value = u32::from(data) | (offset << E1000_GEN_CTL_ADDRESS_SHIFT);
    io.write_register(register, value)?;
    for _ in 0..E1000_GEN_POLL_TIMEOUT {
        io.delay_us(5);
        if io.read_register(register)? & E1000_GEN_CTL_READY != 0 {
            return Ok(());
        }
    }
    Err(DevError::Io)
}

/// upstream: e1000_mac.c e1000_get_hw_semaphore_generic()
pub fn get_hw_semaphore_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    for _ in 0..E1000_SWSM_TIMEOUT {
        if io.read_register(E1000_SWSM)? & E1000_SWSM_SMBI == 0 {
            break;
        }
        io.delay_us(50);
    }
    if io.read_register(E1000_SWSM)? & E1000_SWSM_SMBI != 0 {
        return Err(DevError::ResourceBusy);
    }

    for _ in 0..E1000_SWSM_TIMEOUT {
        let swsm = io.read_register(E1000_SWSM)?;
        io.write_register(E1000_SWSM, swsm | E1000_SWSM_SWESMBI)?;
        if io.read_register(E1000_SWSM)? & E1000_SWSM_SWESMBI != 0 {
            return Ok(());
        }
        io.delay_us(50);
    }
    put_hw_semaphore(io)?;
    Err(DevError::ResourceBusy)
}

/// upstream: e1000_mac.c e1000_put_hw_semaphore()
pub fn put_hw_semaphore<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let swsm = io.read_register(E1000_SWSM)?;
    io.write_register(E1000_SWSM, swsm & !(E1000_SWSM_SMBI | E1000_SWSM_SWESMBI))
}

/// upstream: e1000_mac.c e1000_acquire_swfw_sync()
pub fn acquire_swfw_sync<I: E1000RegisterIo>(io: &mut I, mask: u16) -> DevResult {
    let sw_mask = u32::from(mask);
    let fw_mask = sw_mask << 16;
    for _ in 0..SWFW_SYNC_TIMEOUT {
        get_hw_semaphore_generic(io)?;
        let mut sync = io.read_register(E1000_SW_FW_SYNC)?;
        if sync & (fw_mask | sw_mask) == 0 {
            sync |= sw_mask;
            io.write_register(E1000_SW_FW_SYNC, sync)?;
            put_hw_semaphore(io)?;
            return Ok(());
        }
        put_hw_semaphore(io)?;
        io.delay_us(5000);
    }
    Err(DevError::ResourceBusy)
}

/// upstream: e1000_mac.c e1000_release_swfw_sync()
pub fn release_swfw_sync<I: E1000RegisterIo>(io: &mut I, mask: u16) -> DevResult {
    // The C implementation waits until the hardware semaphore is acquired.
    // Preserve that synchronization order; the register access is bounded by
    // the device's semaphore helper and can fail only on MMIO failure here.
    get_hw_semaphore_generic(io)?;
    let sync = io.read_register(E1000_SW_FW_SYNC)?;
    io.write_register(E1000_SW_FW_SYNC, sync & !u32::from(mask))?;
    put_hw_semaphore(io)
}

/// upstream: e1000_mac.c e1000_null_ops_generic()
pub const fn null_ops_generic() -> i32 {
    0
}

/// upstream: e1000_mac.c e1000_null_mac_generic()
pub const fn null_mac_generic() {}

/// upstream: e1000_mac.c e1000_null_link_info()
pub const fn null_link_info() -> (i32, u16, u16) {
    (0, 0, 0)
}

/// upstream: e1000_mac.c e1000_null_mng_mode()
pub const fn null_mng_mode() -> bool {
    false
}

/// upstream: e1000_mac.c e1000_null_update_mc()
pub const fn null_update_mc() {}

/// upstream: e1000_mac.c e1000_null_write_vfta()
pub const fn null_write_vfta() {}

/// upstream: e1000_mac.c e1000_null_rar_set()
pub const fn null_rar_set() -> i32 {
    0
}

/// upstream: e1000_mac.c e1000_null_set_obff_timer()
pub const fn null_set_obff_timer() -> i32 {
    0
}

fn array_register(base: u32, index: usize, count: usize, limit: u32) -> DevResult<u32> {
    if index >= count {
        return Err(DevError::InvalidParam);
    }
    let offset = (index as u32)
        .checked_mul(4)
        .ok_or(DevError::InvalidParam)?;
    let register = base.checked_add(offset).ok_or(DevError::InvalidParam)?;
    if register >= limit {
        return Err(DevError::InvalidParam);
    }
    Ok(register)
}

/// upstream: e1000_mac.c e1000_rar_set_generic()
pub fn rar_set_generic<I: E1000RegisterIo>(io: &mut I, address: [u8; 6], index: u32) -> DevResult {
    let index = usize::try_from(index).map_err(|_| DevError::InvalidParam)?;
    let low_reg = array_register(RAL, index, 16, 0x6000)?;
    let high_reg = array_register(RAH, index, 16, 0x6000)?;
    let low = u32::from(address[0])
        | (u32::from(address[1]) << 8)
        | (u32::from(address[2]) << 16)
        | (u32::from(address[3]) << 24);
    let mut high = u32::from(address[4]) | (u32::from(address[5]) << 8);
    if low != 0 || high != 0 {
        high |= E1000_RAH_AV;
    }

    // Separate writes and flushes avoid bridge coalescing on affected parts.
    io.write_register(low_reg, low)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.write_register(high_reg, high)?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: e1000_mac.c e1000_hash_mc_addr_generic()
pub fn hash_mc_addr_generic(
    address: [u8; 6],
    mta_reg_count: usize,
    filter_type: u8,
) -> DevResult<u32> {
    if mta_reg_count == 0 || !mta_reg_count.is_power_of_two() {
        return Err(DevError::InvalidParam);
    }
    let hash_mask = u32::try_from(
        mta_reg_count
            .checked_mul(32)
            .ok_or(DevError::InvalidParam)?
            - 1,
    )
    .map_err(|_| DevError::InvalidParam)?;
    let mut bit_shift = 1u32;
    while bit_shift < 4 && (hash_mask >> bit_shift) != 0xff {
        bit_shift += 1;
    }
    bit_shift += match filter_type {
        1 => 1,
        2 => 2,
        3 => 4,
        _ => 0,
    };
    let mut hash = u32::from(address[4]) >> (8 - bit_shift);
    hash |= u32::from(address[5]) << bit_shift;
    Ok(hash & hash_mask)
}

/// upstream: e1000_mac.c e1000_clear_vfta_generic()
pub fn clear_vfta_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    for index in 0..VFTA_REG_COUNT {
        let register = array_register(E1000_VFTA, index, VFTA_REG_COUNT, 0x6000)?;
        io.write_register(register, 0)?;
        let _ = io.read_register(E1000_STATUS)?;
    }
    Ok(())
}

/// upstream: e1000_mac.c e1000_write_vfta_generic()
pub fn write_vfta_generic<I: E1000RegisterIo>(io: &mut I, offset: u32, value: u32) -> DevResult {
    let register = array_register(
        E1000_VFTA,
        usize::try_from(offset).map_err(|_| DevError::InvalidParam)?,
        VFTA_REG_COUNT,
        0x6000,
    )?;
    io.write_register(register, value)?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: e1000_mac.c e1000_update_mc_addr_list_generic()
pub fn update_mc_addr_list_generic<I: E1000RegisterIo>(
    io: &mut I,
    addresses: &[[u8; ETHER_ADDR_LEN]],
    mta_reg_count: usize,
    filter_type: u8,
) -> DevResult {
    if mta_reg_count == 0 || mta_reg_count > MTA_REG_COUNT || !mta_reg_count.is_power_of_two() {
        return Err(DevError::InvalidParam);
    }
    let mut shadow = [0u32; MTA_REG_COUNT];
    for address in addresses {
        let hash_value = hash_mc_addr_generic(*address, mta_reg_count, filter_type)?;
        let register = ((hash_value >> 5) as usize) & (mta_reg_count - 1);
        let bit = hash_value & 0x1f;
        shadow[register] |= 1u32 << bit;
    }
    for index in (0..mta_reg_count).rev() {
        let register = array_register(E1000_MTA, index, MTA_REG_COUNT, 0x6000)?;
        io.write_register(register, shadow[index])?;
    }
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Registers {
        writes: alloc::vec::Vec<(u32, u32)>,
        status: u32,
        eecd: u32,
        delays: usize,
        ready_register: Option<u32>,
    }
    impl E1000RegisterIo for Registers {
        fn read_register(&mut self, register: u32) -> DevResult<u32> {
            if register == E1000_STATUS {
                return Ok(self.status);
            }
            if register == E1000_EECD {
                return Ok(self.eecd);
            }
            if self.ready_register == Some(register) {
                return Ok(E1000_GEN_CTL_READY);
            }
            Ok(self
                .writes
                .iter()
                .rev()
                .find_map(|(written, value)| (*written == register).then_some(*value))
                .unwrap_or(0))
        }
        fn write_register(&mut self, register: u32, value: u32) -> DevResult {
            self.writes.push((register, value));
            Ok(())
        }
        fn delay_us(&mut self, _: u32) {
            self.delays += 1;
        }
        fn invalid_tail_write(&mut self, _: &'static str) {}
    }

    struct Pci {
        words: [u16; 128],
        pcie: Option<u32>,
    }

    struct Nvm {
        word: u16,
        last_offset: Option<u16>,
    }
    impl E1000NvmReader for Nvm {
        fn read_word(&mut self, offset: u16) -> DevResult<u16> {
            self.last_offset = Some(offset);
            Ok(self.word)
        }
    }
    impl Default for Pci {
        fn default() -> Self {
            Self {
                words: [0; 128],
                pcie: None,
            }
        }
    }
    impl E1000PciConfig for Pci {
        fn read_config_u16(&mut self, register: u32) -> Option<u16> {
            self.words.get(register as usize / 2).copied()
        }
        fn write_config_u16(&mut self, register: u32, value: u16) -> bool {
            let Some(word) = self.words.get_mut(register as usize / 2) else {
                return false;
            };
            *word = value;
            true
        }
        fn find_capability(&mut self, capability_id: u8) -> Option<u32> {
            (capability_id == 0x10).then_some(self.pcie).flatten()
        }
    }

    #[test]
    fn generic_rar_encodes_little_endian_and_valid_bit() {
        let mut io = Registers::default();
        rar_set_generic(&mut io, [0x02, 0x11, 0x22, 0x33, 0x44, 0x55], 0).unwrap();
        assert_eq!(
            io.writes,
            [(RAL, 0x3322_1102), (RAH, E1000_RAH_AV | 0x5544)]
        );
    }

    #[test]
    fn generic_null_ops_preserve_success_and_false_defaults() {
        assert_eq!(null_ops_generic(), 0);
        null_mac_generic();
        assert_eq!(null_link_info(), (0, 0, 0));
        assert!(!null_mng_mode());
        null_update_mc();
        null_write_vfta();
        assert_eq!(null_rar_set(), 0);
        assert_eq!(null_set_obff_timer(), 0);
    }

    #[test]
    fn generic_pci_bus_info_decodes_type_speed_width_and_function() {
        let mut io = Registers {
            status: E1000_STATUS_PCIX_MODE
                | E1000_STATUS_PCIX_SPEED_100
                | E1000_STATUS_BUS64
                | E1000_STATUS_FUNC_1,
            ..Registers::default()
        };
        let info = get_bus_info_pci_generic(&mut io).unwrap();
        assert_eq!(info.kind, E1000BusType::PciX);
        assert_eq!(info.speed, E1000BusSpeed::Mhz100);
        assert_eq!(info.width, E1000BusWidth::Bits64);
        assert_eq!(info.function, 1);
        assert_eq!(set_lan_id_single_port(), 0);
    }

    #[test]
    fn generic_pcie_bus_info_and_pci_function_mapping() {
        let mut io = Registers {
            status: E1000_STATUS_FUNC_MASK & (2 << E1000_STATUS_FUNC_SHIFT),
            ..Registers::default()
        };
        let mut pci = Pci {
            pcie: Some(0x40),
            ..Pci::default()
        };
        pci.words[0x52 / 2] = 1 | (8 << PCIE_LINK_WIDTH_SHIFT);
        pci.words[PCI_HEADER_TYPE_REGISTER as usize / 2] = PCI_HEADER_TYPE_MULTIFUNC;
        let info = get_bus_info_pcie_generic(&mut io, &mut pci).unwrap();
        assert_eq!(info.kind, E1000BusType::PciExpress);
        assert_eq!(info.speed, E1000BusSpeed::Gt2500);
        assert_eq!(info.width, E1000BusWidth::Pcie(8));
        assert_eq!(info.function, 2);
        assert_eq!(set_lan_id_multi_port_pci(&mut io, &mut pci).unwrap(), 2);
    }

    #[test]
    fn generic_collision_distance_programs_tctl_field() {
        let mut io = Registers::default();
        config_collision_dist_generic(&mut io).unwrap();
        assert_eq!(
            io.writes,
            [(E1000_TCTL, E1000_COLLISION_DISTANCE << E1000_COLD_SHIFT)]
        );
    }

    #[test]
    fn generic_link_speed_and_autoread_match_source_defaults_and_bounds() {
        let mut io = Registers {
            status: E1000_STATUS_SPEED_100 | E1000_STATUS_FD,
            eecd: E1000_EECD_AUTO_RD,
            ..Registers::default()
        };
        assert_eq!(
            get_speed_and_duplex_copper_generic(&mut io).unwrap(),
            (100, 2)
        );
        assert_eq!(get_speed_and_duplex_fiber_serdes_generic(), (1000, 2));
        assert!(get_auto_rd_done_generic(&mut io).is_ok());
        let mut not_ready = Registers::default();
        assert!(get_auto_rd_done_generic(&mut not_ready).is_err());
        assert_eq!(not_ready.delays, AUTO_READ_DONE_TIMEOUT_MS);
    }

    #[test]
    fn generic_flow_control_programming_preserves_mode_bits() {
        let fc = FlowControl {
            mode: FlowControlMode::Full,
            low_water: 0x1230,
            high_water: 0x4560,
            pause_time: 0x100,
            send_xon: true,
        };
        let mut io = Registers::default();
        set_fc_watermarks_generic(&mut io, fc).unwrap();
        assert_eq!(
            io.writes,
            [(E1000_FCRTL, 0x8000_1230), (E1000_FCRTH, 0x4560)]
        );
        assert_eq!(
            commit_fc_settings_generic(&mut io, FlowControlMode::TxPause).unwrap(),
            E1000_TXCW_ANE | E1000_TXCW_FD | E1000_TXCW_ASM_DIR
        );
        force_mac_fc_generic(&mut io, FlowControlMode::Full).unwrap();
        assert_eq!(
            io.writes.last(),
            Some(&(E1000_CTRL, E1000_CTRL_TFCE | E1000_CTRL_RFCE))
        );
        assert_eq!(
            commit_fc_settings_generic(&mut io, FlowControlMode::None).unwrap(),
            E1000_TXCW_ANE | E1000_TXCW_FD
        );
    }

    #[test]
    fn generic_nvm_modes_read_led_and_per_lan_flow_control_words() {
        let mut led_nvm = Nvm {
            word: 0,
            last_offset: None,
        };
        assert_eq!(valid_led_default_generic(&mut led_nvm).unwrap(), 0x8911);
        assert_eq!(led_nvm.last_offset, Some(NVM_ID_LED_SETTINGS));
        led_nvm.word = 0x0055;
        let modes = id_led_init_generic(&mut led_nvm, 0).unwrap();
        assert_eq!(modes.mode1 & 0xff, E1000_LEDCTL_MODE_LED_ON);
        assert_eq!(modes.mode2 & 0xff, E1000_LEDCTL_MODE_LED_ON);

        let mut fc_nvm = Nvm {
            word: NVM_WORD0F_ASM_DIR,
            last_offset: None,
        };
        assert_eq!(
            set_default_fc_generic(&mut fc_nvm, true, 2).unwrap(),
            FlowControlMode::TxPause
        );
        assert_eq!(fc_nvm.last_offset, Some(NVM_INIT_CONTROL2_REG + 0xc0));
        fc_nvm.word = 0;
        assert_eq!(
            set_default_fc_generic(&mut fc_nvm, false, 2).unwrap(),
            FlowControlMode::None
        );
    }

    #[test]
    fn generic_phy_mdi_and_register_semaphore_paths_are_bounded() {
        let mut mdi = 3;
        assert!(validate_mdi_setting_generic(false, &mut mdi).is_err());
        assert_eq!(mdi, 1);
        assert!(validate_mdi_setting_crossover_generic().is_ok());

        let mut io = Registers {
            ready_register: Some(E1000_SCTL),
            ..Registers::default()
        };
        write_8bit_ctrl_reg_generic(&mut io, E1000_SCTL, 0x12, 0x34).unwrap();
        assert_eq!(io.writes[0], (E1000_SCTL, (0x12 << 8) | 0x34));
        get_hw_semaphore_generic(&mut io).unwrap();
        assert_ne!(io.writes.last().unwrap().1 & E1000_SWSM_SWESMBI, 0);
        put_hw_semaphore(&mut io).unwrap();
        assert_eq!(io.writes.last().unwrap().1 & E1000_SWSM_SWESMBI, 0);
        acquire_swfw_sync(&mut io, 0x20).unwrap();
        assert_eq!(
            io.writes
                .iter()
                .rev()
                .find(|(r, _)| *r == E1000_SW_FW_SYNC)
                .unwrap()
                .1,
            0x20
        );
        release_swfw_sync(&mut io, 0x20).unwrap();
        assert_eq!(
            io.writes
                .iter()
                .rev()
                .find(|(r, _)| *r == E1000_SW_FW_SYNC)
                .unwrap()
                .1,
            0
        );
    }

    #[test]
    fn generic_led_nvm_defaults_and_modes_match_source_fields() {
        let mut nvm = Nvm {
            word: 0x0055,
            last_offset: None,
        };
        let modes = id_led_init_generic(&mut nvm, 0).unwrap();
        assert_eq!(modes.mode1 & 0xff, E1000_LEDCTL_MODE_LED_ON);
        assert_eq!(modes.mode2 & 0xff, E1000_LEDCTL_MODE_LED_ON);
        let mut io = Registers::default();
        setup_led_generic(&mut io, E1000MediaType::Copper, modes, true).unwrap();
        assert_eq!(io.writes.last(), Some(&(E1000_LEDCTL, modes.mode1)));
        blink_led_generic(&mut io, E1000MediaType::Fiber, modes).unwrap();
        assert_eq!(
            io.writes.last(),
            Some(&(
                E1000_LEDCTL,
                E1000_LEDCTL_LED0_BLINK | E1000_LEDCTL_MODE_LED_ON
            ))
        );
        led_on_generic(&mut io, E1000MediaType::Copper, modes).unwrap();
        assert_eq!(io.writes.last(), Some(&(E1000_LEDCTL, modes.mode2)));
        cleanup_led_generic(&mut io, modes).unwrap();
        assert_eq!(io.writes.last(), Some(&(E1000_LEDCTL, modes.default)));
        led_off_generic(&mut io, E1000MediaType::Other, modes).unwrap();
        assert_eq!(io.writes.len(), 4);
    }

    #[test]
    fn generic_adaptive_and_pcie_helpers_update_state_and_registers() {
        let mut io = Registers::default();
        let mut ifs = AdaptiveIfs {
            enabled: true,
            collision_delta: 600,
            tx_packet_delta: 2000,
            ..AdaptiveIfs::default()
        };
        reset_adaptive_generic(&mut io, &mut ifs).unwrap();
        update_adaptive_generic(&mut io, &mut ifs).unwrap();
        assert_eq!(ifs.current, IFS_MIN);
        assert!(ifs.in_mode);
        assert_eq!(io.writes.last(), Some(&(E1000_AIT, IFS_MIN)));
        ifs.tx_packet_delta = MIN_NUM_XMITS;
        ifs.collision_delta = 0;
        update_adaptive_generic(&mut io, &mut ifs).unwrap();
        assert!(!ifs.in_mode);
        assert_eq!(io.writes.last(), Some(&(E1000_AIT, 0)));

        set_pcie_no_snoop_generic(&mut io, false, 0xf).unwrap();
        let before = io.writes.len();
        set_pcie_no_snoop_generic(&mut io, true, 0x5).unwrap();
        assert_eq!(io.writes.len(), before + 1);
        assert_eq!(io.writes.last(), Some(&(E1000_GCR, 0x5)));
        disable_pcie_master_generic(&mut io, false).unwrap();
        disable_pcie_master_generic(&mut io, true).unwrap();
        assert_eq!(
            io.writes.last(),
            Some(&(E1000_CTRL, E1000_CTRL_GIO_MASTER_DISABLE))
        );
    }

    #[test]
    fn generic_pcix_workaround_caps_mmrbc_and_reads_counter_latches() {
        let mut pci = Pci::default();
        pci.words[PCIX_COMMAND_REGISTER as usize / 2] = 3 << PCIX_COMMAND_MMRBC_SHIFT;
        pci.words[PCIX_STATUS_REGISTER_HI as usize / 2] = 3 << PCIX_STATUS_HI_MMRBC_SHIFT;
        pcix_mmrbc_workaround_generic(&mut pci, true).unwrap();
        assert_eq!(
            pci.words[PCIX_COMMAND_REGISTER as usize / 2],
            2 << PCIX_COMMAND_MMRBC_SHIFT
        );

        let mut io = Registers::default();
        clear_hw_cntrs_base_generic(&mut io).unwrap();
        assert!(io.writes.is_empty());
    }

    #[test]
    fn multicast_hash_uses_upstream_filter_shifts() {
        let address = [1, 0xaa, 0, 0x12, 0x34, 0x56];
        assert_eq!(hash_mc_addr_generic(address, 128, 0).unwrap(), 0x563);
        assert_eq!(hash_mc_addr_generic(address, 128, 1).unwrap(), 0xac6);
        // The source comment's example says 0x163, but the executable C
        // expression shifts 0x56 by six and masks to 0x58d.
        assert_eq!(hash_mc_addr_generic(address, 128, 2).unwrap(), 0x58d);
        assert_eq!(hash_mc_addr_generic(address, 128, 3).unwrap(), 0x634);
    }

    #[test]
    fn generic_vfta_and_multicast_programming_are_bounded() {
        let mut io = Registers::default();
        write_vfta_generic(&mut io, 127, 0x1234).unwrap();
        assert_eq!(io.writes, [(E1000_VFTA + 127 * 4, 0x1234)]);
        assert!(write_vfta_generic(&mut io, 128, 1).is_err());
        update_mc_addr_list_generic(&mut io, &[[1, 0, 0, 0, 0, 1]], 128, 0).unwrap();
        assert_eq!(io.writes.len(), 129);
        assert_eq!(io.writes[1], (E1000_MTA + 127 * 4, 0));
    }
}
