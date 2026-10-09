//! Intel 82541/82547 family helpers.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_82541.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{
    api::E1000MacType,
    mac::E1000MediaType,
    nvm::{E1000NvmAccess, E1000NvmConfig, E1000NvmType},
    osdep::E1000RegisterIo,
    phy::SmartSpeedMode,
    registers::*,
};

const PHY_ID_IGP01E1000: u32 = 0x02a8_0380;
const IGP_PHY_CHANNELS: [u16; 4] = [0x1172, 0x1272, 0x1472, 0x1872];
const IGP_DSP_CHANNELS: [u16; 4] = [0x1171, 0x1271, 0x1471, 0x1871];
const IGP_AGC_LENGTH_SHIFT: u32 = 7;
const IGP_AGC_RANGE: u16 = 10;
const IGP_AGC_TABLE: [u16; 128] = [
    5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 10, 10, 10, 10, 10, 10, 10, 20, 20, 20, 20,
    20, 25, 25, 25, 25, 25, 25, 25, 30, 30, 30, 30, 40, 40, 40, 40, 40, 40, 40, 40, 40, 50, 50, 50,
    50, 50, 50, 50, 60, 60, 60, 60, 60, 60, 60, 60, 60, 70, 70, 70, 70, 70, 70, 80, 80, 80, 80, 80,
    80, 90, 90, 90, 90, 90, 90, 90, 90, 90, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100, 100,
    100, 100, 100, 110, 110, 110, 110, 110, 110, 110, 110, 110, 110, 110, 110, 110, 110, 110, 110,
    110, 110, 120, 120, 120, 120, 120, 120, 120, 120, 120, 120,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NvmOverride82541 {
    Auto,
    SpiLarge,
    SpiSmall,
    MicrowireLarge,
    MicrowireSmall,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhyParams82541 {
    pub address: u8,
    pub reset_delay_us: u32,
    pub phy_id: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MacParams82541 {
    pub media: E1000MediaType,
    pub mta_register_count: u16,
    pub rar_entry_count: u16,
    pub asf_firmware_present: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitPointers82541 {
    pub mac: bool,
    pub nvm: bool,
    pub phy: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DspConfig {
    Disabled,
    Enabled,
    Activated,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FfeConfig {
    Active,
    Enabled,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CableLength {
    pub min: u16,
    pub max: u16,
    pub average: u16,
}

pub trait Phy82541 {
    fn read_phy(&mut self, reg: u16) -> DevResult<u16>;
    fn write_phy(&mut self, reg: u16, val: u16) -> DevResult;
    fn reset_phy_generic(&mut self) -> DevResult;
    fn power_down_phy(&mut self) -> DevResult;
    fn delay_us(&mut self, micros: u32) -> DevResult;
    fn setup_copper_igp(&mut self) -> DevResult;
    fn setup_copper_generic(&mut self) -> DevResult;
    fn get_link_up_info(&mut self) -> DevResult<(u16, u16)>;
    fn get_cable_length(&mut self) -> DevResult<CableLength>;
}

pub trait InitOps82541 {
    fn init_id_led(&mut self) -> DevResult;
    fn read_spd_default(&mut self) -> DevResult<u16>;
    fn clear_vfta(&mut self) -> DevResult;
    fn init_rx_addrs(&mut self, count: u16) -> DevResult;
    fn setup_link(&mut self) -> DevResult;
    fn clear_hw_counters(&mut self) -> DevResult;
}

pub trait LinkCheckOps82541 {
    fn phy_has_link(&mut self) -> DevResult<bool>;
    fn check_downshift(&mut self) -> DevResult;
    fn configure_dsp(&mut self, link_up: bool) -> DevResult;
    fn config_collision_distance(&mut self) -> DevResult;
    fn config_flow_control(&mut self) -> DevResult;
}

/// upstream: e1000_82541.c e1000_init_phy_params_82541()
pub fn init_phy_params_82541(phy_id: u32) -> DevResult<PhyParams82541> {
    if phy_id != PHY_ID_IGP01E1000 {
        return Err(DevError::Io);
    }
    Ok(PhyParams82541 {
        address: 1,
        reset_delay_us: 10_000,
        phy_id,
    })
}

/// upstream: e1000_82541.c e1000_init_nvm_params_82541()
pub fn init_nvm_params_82541<I: E1000RegisterIo, N: E1000NvmAccess>(
    io: &mut I,
    nvm: &mut N,
    override_kind: NvmOverride82541,
) -> DevResult<E1000NvmConfig> {
    const EECD_ADDR_BITS: u32 = 0x400;
    const EECD_TYPE: u32 = 0x2000;
    const NVM_CFG: u16 = 0x12;
    const NVM_SIZE_MASK: u16 = 0x1c00;
    let mut eecd = io.read_register(E1000_EECD)?;
    let kind = match override_kind {
        NvmOverride82541::SpiLarge => {
            eecd |= EECD_ADDR_BITS;
            E1000NvmType::Spi
        }
        NvmOverride82541::SpiSmall => {
            eecd &= !EECD_ADDR_BITS;
            E1000NvmType::Spi
        }
        NvmOverride82541::MicrowireLarge => {
            eecd |= E1000_EECD_SIZE;
            E1000NvmType::Microwire
        }
        NvmOverride82541::MicrowireSmall => {
            eecd &= !E1000_EECD_SIZE;
            E1000NvmType::Microwire
        }
        NvmOverride82541::Auto if eecd & EECD_TYPE != 0 => E1000NvmType::Spi,
        NvmOverride82541::Auto => E1000NvmType::Microwire,
    };
    if kind == E1000NvmType::Spi {
        let address_bits = if eecd & EECD_ADDR_BITS != 0 { 16 } else { 8 };
        let page_size = if address_bits == 16 { 32 } else { 8 };
        let mut config = E1000NvmConfig {
            kind,
            word_size: 64,
            delay_usec: 1,
            opcode_bits: 8,
            address_bits,
            page_size,
        };
        let data = nvm
            .read_nvm_words(NVM_CFG, 1)?
            .first()
            .copied()
            .ok_or(DevError::Io)?;
        let size = (data & NVM_SIZE_MASK) >> 10;
        if size != 0 {
            config.word_size = 1u16
                .checked_shl(u32::from(size + 7))
                .ok_or(DevError::InvalidParam)?;
        }
        Ok(config)
    } else {
        Ok(E1000NvmConfig {
            kind,
            word_size: if eecd & EECD_ADDR_BITS != 0 { 256 } else { 64 },
            delay_usec: 50,
            opcode_bits: 3,
            address_bits: if eecd & EECD_ADDR_BITS != 0 { 8 } else { 6 },
            page_size: 1,
        })
    }
}

/// upstream: e1000_82541.c e1000_init_mac_params_82541()
pub const fn init_mac_params_82541() -> MacParams82541 {
    MacParams82541 {
        media: E1000MediaType::Copper,
        mta_register_count: 128,
        rar_entry_count: E1000_RAR_ENTRIES as u16,
        asf_firmware_present: true,
    }
}

/// upstream: e1000_82541.c e1000_init_function_pointers_82541()
pub const fn init_function_pointers_82541() -> InitPointers82541 {
    InitPointers82541 {
        mac: true,
        nvm: true,
        phy: true,
    }
}

/// upstream: e1000_82541.c e1000_phy_hw_reset_82541()
pub fn phy_hw_reset_82541<I: E1000RegisterIo, P: Phy82541, S: FnMut(&mut P) -> DevResult>(
    io: &mut I,
    phy: &mut P,
    mac: E1000MacType,
    mut init_script: S,
) -> DevResult {
    phy.reset_phy_generic()?;
    init_script(phy)?;
    if matches!(mac, E1000MacType::I82541 | E1000MacType::I82547) {
        let mut ledctl = io.read_register(E1000_LEDCTL)? & 0x0000_00f0;
        ledctl |= 0x0000_0040 | 0x0000_0003;
        io.write_register(E1000_LEDCTL, ledctl)?;
    }
    Ok(())
}

/// upstream: e1000_82541.c e1000_setup_copper_link_82541()
pub fn setup_copper_link_82541<I: E1000RegisterIo, P: Phy82541>(
    io: &mut I,
    phy: &mut P,
    mac: E1000MacType,
    autoneg: bool,
    dsp: &mut DspConfig,
    ffe: &mut FfeConfig,
    mdix: &mut u8,
) -> DevResult {
    let mut ctrl = io.read_register(E1000_CTRL)? | E1000_CTRL_SLU;
    ctrl &= !(E1000_CTRL_FRCSPD | E1000_CTRL_FRCDPX);
    io.write_register(E1000_CTRL, ctrl)?;
    if matches!(mac, E1000MacType::I82541 | E1000MacType::I82547) {
        *dsp = DspConfig::Disabled;
        *mdix = 1;
    } else {
        *dsp = DspConfig::Enabled;
    }
    phy.setup_copper_igp()?;
    if autoneg && *ffe == FfeConfig::Active {
        *ffe = FfeConfig::Enabled;
    }
    let mut ledctl = io.read_register(E1000_LEDCTL)? & 0x0000_00f0;
    ledctl |= 0x0000_0040 | 0x0000_0003;
    io.write_register(E1000_LEDCTL, ledctl)?;
    phy.setup_copper_generic()
}

/// upstream: e1000_82541.c e1000_get_cable_length_igp_82541()
pub fn get_cable_length_igp_82541<P: Phy82541>(phy: &mut P) -> DevResult<CableLength> {
    let mut sum = 0u16;
    let mut min = IGP_AGC_TABLE.len() as u16;
    for reg in IGP_PHY_CHANNELS {
        let value = phy.read_phy(reg)? >> IGP_AGC_LENGTH_SHIFT;
        if value == 0 || usize::from(value) >= IGP_AGC_TABLE.len() - 1 {
            return Err(DevError::Io);
        }
        sum += value;
        min = core::cmp::min(min, value);
    }
    let average_index = if sum < 4 * 50 {
        (sum - min) / 3
    } else {
        sum / 4
    };
    let length = *IGP_AGC_TABLE
        .get(usize::from(average_index))
        .ok_or(DevError::Io)?;
    let min_len = length.saturating_sub(IGP_AGC_RANGE);
    let max_len = length + IGP_AGC_RANGE;
    Ok(CableLength {
        min: min_len,
        max: max_len,
        average: (min_len + max_len) / 2,
    })
}

/// upstream: e1000_82541.c e1000_power_down_phy_copper_82541()
pub fn power_down_phy_copper_82541<I: E1000RegisterIo, P: Phy82541>(
    io: &mut I,
    phy: &mut P,
) -> DevResult {
    if io.read_register(E1000_MANC)? & E1000_MANC_SMBUS_EN == 0 {
        phy.power_down_phy()?;
    }
    Ok(())
}

/// upstream: e1000_82541.c e1000_get_link_up_info_82541()
pub fn get_link_up_info_82541<P: Phy82541>(
    phy: &mut P,
    speed_downgraded: bool,
) -> DevResult<(u16, u16)> {
    let (speed, mut duplex) = phy.get_link_up_info()?;
    if !speed_downgraded {
        return Ok((speed, duplex));
    }
    const PHY_AUTONEG_EXP: u16 = 0x06;
    const PHY_LP_ABILITY: u16 = 0x05;
    const NWAY_ER_LP_NWAY_CAPS: u16 = 0x0001;
    const NWAY_LPAR_10T_FD_CAPS: u16 = 0x0040;
    const NWAY_LPAR_100TX_FD_CAPS: u16 = 0x0100;
    let ability = phy.read_phy(PHY_AUTONEG_EXP)?;
    if ability & NWAY_ER_LP_NWAY_CAPS == 0 {
        duplex = 0;
    } else {
        let partner = phy.read_phy(PHY_LP_ABILITY)?;
        if (speed == 100 && partner & NWAY_LPAR_100TX_FD_CAPS == 0)
            || (speed == 10 && partner & NWAY_LPAR_10T_FD_CAPS == 0)
        {
            duplex = 0;
        }
    }
    Ok((speed, duplex))
}

/// upstream: e1000_82541.c e1000_check_for_link_82541()
pub fn check_for_link_82541<O: LinkCheckOps82541>(
    ops: &mut O,
    get_link_status: &mut bool,
    autoneg: bool,
) -> DevResult {
    if !*get_link_status {
        return Ok(());
    }
    if !ops.phy_has_link()? {
        return ops.configure_dsp(false);
    }
    *get_link_status = false;
    ops.check_downshift()?;
    if !autoneg {
        return Err(DevError::Unsupported);
    }
    let _ = ops.configure_dsp(true);
    let _ = ops.config_collision_distance();
    ops.config_flow_control()
}

/// upstream: e1000_82541.c e1000_config_dsp_after_link_change_82541()
pub fn config_dsp_after_link_change_82541<P: Phy82541>(
    phy: &mut P,
    dsp: &mut DspConfig,
    ffe: &mut FfeConfig,
    link_up: bool,
) -> DevResult {
    const PHY_1000T_STATUS: u16 = 0x0a;
    const SR_1000T_IDLE_ERROR_CNT: u16 = 0x00ff;
    const EXCESSIVE_IDLE_ERRORS: u32 = 5;
    const EDAC_MU_INDEX: u16 = 0xc000;
    const EDAC_SIGN_EXT_9_BITS: u16 = 0x8000;
    const PHY_DSP_FFE: u16 = 0x1f35;
    const DSP_FFE_CM_CP: u16 = 0x0069;
    const DSP_FFE_DEFAULT: u16 = 0x002a;
    const FORCE_GIG: u16 = 0x0140;
    const RESTART_AUTONEG: u16 = 0x3300;
    const PHY_TX_CONTROL: u16 = 0x2f5b;
    if link_up {
        let (speed, _) = phy.get_link_up_info()?;
        if speed != 1000 {
            return Ok(());
        }
        let cable = phy.get_cable_length()?;
        if *dsp == DspConfig::Enabled && cable.min >= 50 {
            for reg in IGP_DSP_CHANNELS {
                let data = phy.read_phy(reg)? & !EDAC_MU_INDEX;
                phy.write_phy(reg, data)?;
            }
            *dsp = DspConfig::Activated;
        }
        if *ffe != FfeConfig::Enabled || cable.min >= 50 {
            return Ok(());
        }
        let mut idle_errors = 0u32;
        let mut timeout = 20;
        phy.read_phy(PHY_1000T_STATUS)?; // clear previous latched idle errors.
        let mut i = 0;
        while i < timeout {
            phy.delay_us(1_000)?;
            let status = phy.read_phy(PHY_1000T_STATUS)?;
            idle_errors += u32::from(status & SR_1000T_IDLE_ERROR_CNT);
            if idle_errors > EXCESSIVE_IDLE_ERRORS {
                *ffe = FfeConfig::Active;
                phy.write_phy(PHY_DSP_FFE, DSP_FFE_CM_CP)?;
                break;
            }
            if idle_errors != 0 {
                timeout = 100;
            }
            i += 1;
        }
    } else {
        if *dsp == DspConfig::Activated {
            let saved = phy.read_phy(PHY_TX_CONTROL)?;
            phy.write_phy(PHY_TX_CONTROL, 3)?;
            phy.delay_us(20_000)?;
            phy.write_phy(0, FORCE_GIG)?;
            for reg in IGP_DSP_CHANNELS {
                let data = (phy.read_phy(reg)? & !EDAC_MU_INDEX) | EDAC_SIGN_EXT_9_BITS;
                phy.write_phy(reg, data)?;
            }
            phy.write_phy(0, RESTART_AUTONEG)?;
            phy.delay_us(20_000)?;
            phy.write_phy(PHY_TX_CONTROL, saved)?;
            *dsp = DspConfig::Enabled;
        }
        if *ffe != FfeConfig::Active {
            return Ok(());
        }
        let saved = phy.read_phy(PHY_TX_CONTROL)?;
        phy.write_phy(PHY_TX_CONTROL, 3)?;
        phy.delay_us(20_000)?;
        phy.write_phy(0, FORCE_GIG)?;
        phy.write_phy(PHY_DSP_FFE, DSP_FFE_DEFAULT)?;
        phy.write_phy(0, RESTART_AUTONEG)?;
        phy.delay_us(20_000)?;
        phy.write_phy(PHY_TX_CONTROL, saved)?;
        *ffe = FfeConfig::Enabled;
    }
    Ok(())
}

/// upstream: e1000_82541.c e1000_reset_hw_82541()
pub fn reset_hw_82541<I: E1000RegisterIo, P: Phy82541, S: FnMut(&mut P) -> DevResult>(
    io: &mut I,
    phy: &mut P,
    mac: E1000MacType,
    init_script_enabled: bool,
    mut init_script: S,
) -> DevResult {
    io.write_register(E1000_IMC, u32::MAX)?;
    io.write_register(E1000_RCTL, 0)?;
    io.write_register(E1000_TCTL, E1000_TCTL_PSP)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(10_000);
    let ctrl = io.read_register(E1000_CTRL)?;
    if matches!(mac, E1000MacType::I82541 | E1000MacType::I82547) {
        io.write_register(E1000_CTRL, ctrl | (1 << 31))?; // E1000_CTRL_PHY_RST
        let _ = io.read_register(E1000_STATUS)?;
        io.delay_us(5_000);
    }
    if matches!(mac, E1000MacType::I82541 | E1000MacType::I82541Rev2) {
        io.write_register_io(E1000_CTRL, ctrl | E1000_CTRL_RST)?;
    } else {
        io.write_register(E1000_CTRL, ctrl | E1000_CTRL_RST)?;
    }
    io.delay_us(20_000);
    let manc = io.read_register(E1000_MANC)? & !E1000_MANC_ARP_EN;
    io.write_register(E1000_MANC, manc)?;
    if matches!(mac, E1000MacType::I82541 | E1000MacType::I82547) {
        if init_script_enabled {
            init_script(phy)?;
        }
        let mut ledctl = io.read_register(E1000_LEDCTL)? & 0xffff_f0ff;
        ledctl |= 0x0000_0300 | 0x0700_0000;
        io.write_register(E1000_LEDCTL, ledctl)?;
    }
    io.write_register(E1000_IMC, u32::MAX)?;
    let _ = io.read_register(E1000_ICR)?;
    Ok(())
}

/// upstream: e1000_82541.c e1000_init_hw_82541()
pub fn init_hw_82541<I: E1000RegisterIo, P: Phy82541, O: InitOps82541>(
    io: &mut I,
    phy: &mut P,
    ops: &mut O,
    mta_count: u16,
    rar_count: u16,
    spd_default: &mut u16,
) -> DevResult {
    let _ = ops.init_id_led();
    *spd_default = phy.read_phy(0x14)?;
    ops.clear_vfta()?;
    ops.init_rx_addrs(rar_count)?;
    for i in 0..mta_count {
        io.write_register(E1000_MTA + u32::from(i) * 4, 0)?;
        let _ = io.read_register(E1000_STATUS)?;
    }
    let link = ops.setup_link();
    let txdctl = (io.read_register(0x3828)? & !E1000_TXDCTL_WTHRESH) | E1000_TXDCTL_FULL_TX_DESC_WB;
    io.write_register(0x3828, txdctl)?;
    ops.clear_hw_counters()?;
    link
}

/// upstream: e1000_82541.c e1000_set_d3_lplu_state_82541()
pub fn set_d3_lplu_state_82541<P: Phy82541, G: FnMut(bool) -> DevResult>(
    phy: &mut P,
    mac: E1000MacType,
    active: bool,
    advertised: u16,
    smart_speed: SmartSpeedMode,
    mut generic: G,
) -> DevResult {
    if !matches!(mac, E1000MacType::I82541Rev2 | E1000MacType::I82547Rev2) {
        return generic(active);
    }
    const GMII_FIFO: u16 = 0x14;
    const FLEX_SPD: u16 = 0x0010;
    const PORT_CONFIG: u16 = 0x10;
    const SMART_SPEED: u16 = 0x0080;
    let mut data = phy.read_phy(GMII_FIFO)?;
    if !active {
        data &= !FLEX_SPD;
        phy.write_phy(GMII_FIFO, data)?;
        if smart_speed != SmartSpeedMode::Default {
            data = phy.read_phy(PORT_CONFIG)?;
            if smart_speed == SmartSpeedMode::On {
                data |= SMART_SPEED;
            } else {
                data &= !SMART_SPEED;
            }
            phy.write_phy(PORT_CONFIG, data)?;
        }
    } else if matches!(advertised, 0x2f | 0x0f | 0x03) {
        phy.write_phy(GMII_FIFO, data | FLEX_SPD)?;
        data = phy.read_phy(PORT_CONFIG)? & !SMART_SPEED;
        phy.write_phy(PORT_CONFIG, data)?;
    }
    Ok(())
}

/// upstream: e1000_82541.c e1000_setup_led_82541()
pub fn setup_led_82541<I: E1000RegisterIo, P: Phy82541>(
    io: &mut I,
    phy: &mut P,
    spd_default: &mut u16,
    ledctl_mode1: u32,
) -> DevResult {
    const GMII_FIFO: u16 = 0x14;
    const GMII_SPD: u16 = 0x20;
    *spd_default = phy.read_phy(GMII_FIFO)?;
    phy.write_phy(GMII_FIFO, *spd_default & !GMII_SPD)?;
    io.write_register(E1000_LEDCTL, ledctl_mode1)
}

/// upstream: e1000_82541.c e1000_cleanup_led_82541()
pub fn cleanup_led_82541<I: E1000RegisterIo, P: Phy82541>(
    io: &mut I,
    phy: &mut P,
    spd_default: u16,
    ledctl_default: u32,
) -> DevResult {
    phy.write_phy(0x14, spd_default)?;
    io.write_register(E1000_LEDCTL, ledctl_default)
}

/// upstream: e1000_82541.c e1000_init_script_state_82541()
pub fn init_script_state_82541(is_igp_phy: bool, requested_state: bool, state: &mut bool) {
    if is_igp_phy {
        *state = requested_state;
    }
}

/// upstream: e1000_82541.c e1000_phy_init_script_82541()
pub fn phy_init_script_82541<P: Phy82541>(
    phy: &mut P,
    mac: E1000MacType,
    enabled: bool,
) -> DevResult {
    if !enabled {
        return Ok(());
    }
    phy.delay_us(20_000)?;
    let saved = phy.read_phy(0x2f5b)?;
    // The source deliberately ignores errors from the scripted writes, but
    // keeps the first register-read result as its return status.
    let _ = phy.write_phy(0x2f5b, 0x0003);
    phy.delay_us(20_000)?;
    let _ = phy.write_phy(0x0000, 0x0140);
    phy.delay_us(5_000)?;
    match mac {
        E1000MacType::I82541 | E1000MacType::I82547 => {
            for (reg, value) in [
                (0x1f95, 1),
                (0x1f71, 0xbd21),
                (0x1f79, 0x0018),
                (0x1f30, 0x1600),
                (0x1f31, 0x0014),
                (0x1f32, 0x161c),
                (0x1f94, 3),
                (0x1f96, 0x003f),
                (0x2010, 8),
            ] {
                let _ = phy.write_phy(reg, value);
            }
        }
        E1000MacType::I82541Rev2 | E1000MacType::I82547Rev2 => {
            let _ = phy.write_phy(0x1f73, 0x0099);
        }
        _ => {}
    }
    let _ = phy.write_phy(0x0000, 0x3300);
    phy.delay_us(20_000)?;
    let _ = phy.write_phy(0x2f5b, saved);
    if mac == E1000MacType::I82547 {
        const SPARE_STATUS: u16 = 0x20d1;
        const FUSE_STATUS: u16 = 0x20d0;
        const FUSE_CONTROL: u16 = 0x20dc;
        const FUSE_BYPASS: u16 = 0x20de;
        const SPARE_ENABLED: u16 = 0x0100;
        const FINE_MASK: u16 = 0x0f80;
        const COARSE_MASK: u16 = 0x0070;
        const POLY_MASK: u16 = 0xf000;
        const COARSE_THRESH: u16 = 0x0040;
        let mut fused = phy.read_phy(SPARE_STATUS)?;
        if fused & SPARE_ENABLED == 0 {
            fused = phy.read_phy(FUSE_STATUS)?;
            let mut fine = fused & FINE_MASK;
            let mut coarse = fused & COARSE_MASK;
            if coarse > COARSE_THRESH {
                coarse = coarse.wrapping_sub(0x0010);
                fine = fine.wrapping_sub(0x0080);
            } else if coarse == COARSE_THRESH {
                fine = fine.wrapping_sub(0x0500);
            }
            fused = (fused & POLY_MASK) | (fine & FINE_MASK) | (coarse & COARSE_MASK);
            let _ = phy.write_phy(FUSE_CONTROL, fused);
            let _ = phy.write_phy(FUSE_BYPASS, 0x0002);
        }
    }
    Ok(())
}

/// upstream: e1000_82541.c e1000_clear_hw_cntrs_82541()
pub fn clear_hw_cntrs_82541<I: E1000RegisterIo, B: FnMut() -> DevResult>(
    io: &mut I,
    mut clear_base: B,
) -> DevResult {
    clear_base()?;
    for register in [
        0x405c, 0x4060, 0x4064, 0x4068, 0x406c, 0x4070, 0x40d8, 0x40dc, 0x40e0, 0x40e4, 0x40e8,
        0x40ec, 0x4004, 0x400c, 0x4034, 0x403c, 0x40f8, 0x40fc, 0x40b4, 0x40b8, 0x40bc,
    ] {
        let _ = io.read_register(register)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Io {
        regs: alloc::vec::Vec<(u32, u32)>,
    }
    impl E1000RegisterIo for Io {
        fn read_register(&mut self, reg: u32) -> DevResult<u32> {
            Ok(self
                .regs
                .iter()
                .find(|(r, _)| *r == reg)
                .map(|(_, v)| *v)
                .unwrap_or(0))
        }
        fn write_register(&mut self, reg: u32, value: u32) -> DevResult {
            if let Some((_, v)) = self.regs.iter_mut().find(|(r, _)| *r == reg) {
                *v = value;
            } else {
                self.regs.push((reg, value));
            }
            Ok(())
        }
        fn delay_us(&mut self, _: u32) {}
        fn invalid_tail_write(&mut self, _: &'static str) {}
    }
    struct Nvm {
        data: alloc::vec::Vec<u16>,
    }
    impl E1000NvmAccess for Nvm {
        fn read_nvm_words(&mut self, offset: u16, words: u16) -> DevResult<alloc::vec::Vec<u16>> {
            let a = offset as usize;
            self.data
                .get(a..a + words as usize)
                .map(|v| v.to_vec())
                .ok_or(DevError::InvalidParam)
        }
        fn write_nvm_words(&mut self, _: u16, _: &[u16]) -> DevResult {
            Ok(())
        }
    }
    #[derive(Default)]
    struct Phy {
        regs: alloc::vec::Vec<(u16, u16)>,
    }
    impl Phy82541 for Phy {
        fn read_phy(&mut self, reg: u16) -> DevResult<u16> {
            Ok(self
                .regs
                .iter()
                .find(|(r, _)| *r == reg)
                .map(|(_, v)| *v)
                .unwrap_or(0))
        }
        fn write_phy(&mut self, reg: u16, val: u16) -> DevResult {
            if let Some((_, v)) = self.regs.iter_mut().find(|(r, _)| *r == reg) {
                *v = val;
            } else {
                self.regs.push((reg, val));
            }
            Ok(())
        }
        fn reset_phy_generic(&mut self) -> DevResult {
            Ok(())
        }
        fn power_down_phy(&mut self) -> DevResult {
            Ok(())
        }
        fn delay_us(&mut self, _: u32) -> DevResult {
            Ok(())
        }
        fn setup_copper_igp(&mut self) -> DevResult {
            Ok(())
        }
        fn setup_copper_generic(&mut self) -> DevResult {
            Ok(())
        }
        fn get_link_up_info(&mut self) -> DevResult<(u16, u16)> {
            Ok((1000, 1))
        }
        fn get_cable_length(&mut self) -> DevResult<CableLength> {
            Ok(CableLength {
                min: 30,
                max: 50,
                average: 40,
            })
        }
    }
    #[test]
    fn family_nvm_cable_and_state_helpers_preserve_bounds() {
        assert!(init_phy_params_82541(PHY_ID_IGP01E1000).is_ok());
        assert!(init_phy_params_82541(0).is_err());
        let mut nvm = Nvm {
            data: alloc::vec![0; 64],
        };
        let config =
            init_nvm_params_82541(&mut Io::default(), &mut nvm, NvmOverride82541::SpiLarge)
                .unwrap();
        assert_eq!(
            (
                config.kind,
                config.address_bits,
                config.page_size,
                config.word_size
            ),
            (E1000NvmType::Spi, 16, 32, 64)
        );
        assert_eq!(
            init_mac_params_82541().rar_entry_count,
            E1000_RAR_ENTRIES as u16
        );
        let mut phy = Phy::default();
        for reg in IGP_PHY_CHANNELS {
            phy.write_phy(reg, 80 << IGP_AGC_LENGTH_SHIFT).unwrap();
        }
        let cable = get_cable_length_igp_82541(&mut phy).unwrap();
        assert_eq!(
            cable,
            CableLength {
                min: 80,
                max: 100,
                average: 90
            }
        );
        let mut state = false;
        init_script_state_82541(false, true, &mut state);
        assert!(!state);
        init_script_state_82541(true, true, &mut state);
        assert!(state);
    }
}

/// upstream: e1000_82541.c e1000_read_mac_addr_82541()
pub fn read_mac_addr_82541<N: E1000NvmAccess>(nvm: &mut N, pci_function: u8) -> DevResult<[u8; 6]> {
    let mut addr = [0; 6];
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
