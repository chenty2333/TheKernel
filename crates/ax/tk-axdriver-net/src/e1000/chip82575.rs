//! Intel 82575/82576/82580/i350/i354/i210/i211 family helpers.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_82575.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{
    api::E1000MacType,
    chip82571::PhyOps82571,
    mac::E1000MediaType,
    nvm::{E1000NvmAccess, E1000NvmConfig, E1000NvmType},
    osdep::E1000RegisterIo,
    registers::*,
};

const NVM_SIZE_MASK: u32 = 0x7800;
const NVM_SIZE_SHIFT: u32 = 11;
const NVM_WORD_BASE: u32 = 6;
const NVM_MAX_WORD_SHIFT: u32 = 15;
const PHY_CONFIG_TIMEOUT: usize = 100;
const NVM_CHECKSUM_REG: u16 = 0x3f;
const NVM_SUM: u16 = 0xbaba;
const SGMII_PHY_ADDR_MAX: u32 = 0x1f;
const SGMII_MAX_PHY_ADDR: u8 = 8;
const SGMII_ADDR_START: u8 = 1;
const SWFW_EEP_SM: u16 = 0x0001;
const NVM_CFG_DONE: [u32; 4] = [0x40000, 0x80000, 0x100000, 0x200000];
const IGP_PM_D0_LPLU: u16 = 2;
const PHY_82580_POWER_MGMT: u32 = 0x0e14;
const IGP_PM_SMART_SPEED: u16 = 0x80;
const PHY_POWER_MGMT: u16 = 0x19;
const PHY_PORT_CONFIG: u16 = 0x10;
const EECD_PRES: u32 = 0x100;
const EECD_ERROR_FLAGS: u32 = 0x0000c000;
const EECD_ERROR_CLR: u32 = 0x00004000;
const M88E1512_ID: u32 = 0x01410dd1;
const M88E1543_ID: u32 = 0x01410ea0;
const M88E1112_ID: u32 = 0x01410c90;
const IGP03_ID: u32 = 0x02a80390;
const IGP04_ID: u32 = 0x02a80391;
const I82580_ID: u32 = 0x015403a0;
const I350_ID: u32 = 0x015403b0;
const I210_ID: u32 = 0x01410c00;
const NVM_LED_WORD: u16 = 4;
const ID_LED_DEFAULT: u16 = 0x8911;
const ID_LED_DEFAULT_SERDES: u16 = 0x1118;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhyType82575 {
    None,
    M88,
    Igp3,
    Phy82580,
    I210,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhyParams82575 {
    pub kind: PhyType82575,
    pub phy_id: u32,
    pub address: u8,
    pub reset_delay_us: u32,
    pub sgmii: bool,
    pub uses_mdio: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NvmParams82575 {
    pub config: E1000NvmConfig,
    pub flash: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MacParams82575 {
    pub media: E1000MediaType,
    pub rar_count: u16,
    pub mta_count: u16,
    pub uta_count: u16,
    pub eee_default: bool,
    pub clear_semaphore_once: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct I2cLines {
    pub scl: bool,
    pub sda: bool,
}

pub trait SgmiiPhyOps {
    fn acquire_phy(&mut self) -> DevResult;
    fn release_phy(&mut self);
    fn read_i2c_phy(&mut self, addr: u8, reg: u32) -> DevResult<u16>;
    fn write_i2c_phy(&mut self, addr: u8, reg: u32, value: u16) -> DevResult;
    fn generic_phy_id(&mut self, addr: u8) -> DevResult<(u32, u32)>;
    fn delay_us(&mut self, micros: u32);
}
pub trait NvmSemaphore82575 {
    fn acquire_swfw(&mut self, mask: u16) -> DevResult;
    fn release_swfw(&mut self, mask: u16);
    fn acquire_eeprom_generic(&mut self) -> DevResult;
    fn release_eeprom_generic(&mut self);
}
pub trait LinkOps82575 {
    fn generic_check_copper(&mut self) -> DevResult;
    fn generic_check_link(&mut self) -> DevResult;
    fn generic_fc(&mut self) -> DevResult;
    fn generic_copper_setup(&mut self) -> DevResult;
    fn generic_serdes_setup(&mut self) -> DevResult;
    fn phy_setup_m88(&mut self) -> DevResult;
    fn phy_setup_igp(&mut self) -> DevResult;
    fn init_base(&mut self) -> DevResult;
    fn clear_vfta(&mut self) -> DevResult;
    fn clear_counters(&mut self) -> DevResult;
    fn id_led_init(&mut self) -> DevResult;
    fn generic_copper_speed(&mut self) -> DevResult<(u16, u16)>;
    fn check_alt_mac(&mut self) -> DevResult;
}
pub trait SfpEeprom82575 {
    fn read_sfp_byte(&mut self, offset: u8) -> DevResult<u8>;
}
pub trait ScriptIo82575 {
    fn write_8bit_ctrl_field(&mut self, register: u32, offset: u8, value: u8) -> DevResult;
}

/// upstream: e1000_82575.c e1000_sgmii_uses_mdio_82575()
pub fn sgmii_uses_mdio_82575<I: E1000RegisterIo>(io: &mut I, mac: E1000MacType) -> DevResult<bool> {
    let (reg, mask) = match mac {
        E1000MacType::I82575 | E1000MacType::I82576 => (E1000_MDIC, 0x80000000),
        E1000MacType::I82580
        | E1000MacType::I350
        | E1000MacType::I354
        | E1000MacType::I210
        | E1000MacType::I211 => (E1000_MDICNFG, 0x40000000),
        _ => return Ok(false),
    };
    Ok(io.read_register(reg)? & mask != 0)
}

/// upstream: e1000_82575.c e1000_sgmii_active_82575()
pub const fn sgmii_active_82575(active: bool) -> bool {
    active
}

/// upstream: e1000_82575.c e1000_init_phy_params_82575()
pub fn init_phy_params_82575<I: E1000RegisterIo>(
    io: &mut I,
    media: E1000MediaType,
    mac: E1000MacType,
    phy_id: u32,
    sgmii: bool,
    uses_mdio: bool,
) -> DevResult<Option<PhyParams82575>> {
    if media != E1000MediaType::Copper {
        return Ok(None);
    }
    let (kind, valid) = match phy_id {
        M88E1543_ID | M88E1512_ID | 0x01410dc0 | M88E1112_ID | 0x01410e40 => {
            (PhyType82575::M88, true)
        }
        0x01410c50 => (PhyType82575::M88, true),
        IGP03_ID | IGP04_ID => (PhyType82575::Igp3, true),
        I82580_ID | I350_ID => (PhyType82575::Phy82580, true),
        I210_ID => (PhyType82575::I210, true),
        _ => return Err(DevError::Io),
    };
    let mut ctrl = io.read_register(E1000_CTRL_EXT)?;
    if sgmii {
        ctrl |= E1000_CTRL_I2C_ENA
    } else {
        ctrl &= !E1000_CTRL_I2C_ENA
    }
    io.write_register(E1000_CTRL_EXT, ctrl)?;
    if !valid || matches!(mac, E1000MacType::VfAdapt | E1000MacType::VfAdaptI350) {
        return Err(DevError::Io);
    }
    Ok(Some(PhyParams82575 {
        kind,
        phy_id,
        address: 1,
        reset_delay_us: 100,
        sgmii,
        uses_mdio,
    }))
}

/// upstream: e1000_82575.c e1000_init_mac_params_82575()
pub fn init_mac_params_82575<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    media: E1000MediaType,
    device_id: u16,
) -> DevResult<MacParams82575> {
    let rar_count = match mac {
        E1000MacType::I82576 => 24,
        E1000MacType::I82580 => 24,
        E1000MacType::I350 | E1000MacType::I354 => 32,
        _ => 16,
    };
    let uta_count = if mac == E1000MacType::I82575 { 0 } else { 128 };
    let _fwsm = io.read_register(E1000_FWSM)?;
    let _ = device_id;
    Ok(MacParams82575 {
        media,
        rar_count,
        mta_count: 128,
        uta_count,
        eee_default: matches!(
            mac,
            E1000MacType::I350 | E1000MacType::I354 | E1000MacType::I210 | E1000MacType::I211
        ),
        clear_semaphore_once: matches!(mac, E1000MacType::I210 | E1000MacType::I211),
    })
}

/// upstream: e1000_82575.c e1000_init_nvm_params_82575()
pub fn init_nvm_params_82575<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    spi_large: bool,
    spi_small: bool,
) -> DevResult<NvmParams82575> {
    let eecd = io.read_register(E1000_EECD)?;
    let mut shift = ((eecd & NVM_SIZE_MASK) >> NVM_SIZE_SHIFT) + NVM_WORD_BASE;
    if shift > NVM_MAX_WORD_SHIFT {
        shift = 15
    }
    let word_size = 1u16.checked_shl(shift).ok_or(DevError::InvalidParam)?;
    if matches!(mac, E1000MacType::I210 | E1000MacType::I211) {
        return Ok(NvmParams82575 {
            config: E1000NvmConfig {
                kind: E1000NvmType::Spi,
                word_size,
                delay_usec: 1,
                opcode_bits: 8,
                address_bits: 16,
                page_size: 32,
            },
            flash: true,
        });
    }
    let address_bits = if spi_large {
        16
    } else if spi_small {
        8
    } else if eecd & 0x400 != 0 {
        16
    } else {
        8
    };
    let mut page_size = if address_bits == 16 { 32 } else { 8 };
    if word_size == (1 << 15) {
        page_size = 128
    }
    Ok(NvmParams82575 {
        config: E1000NvmConfig {
            kind: E1000NvmType::Spi,
            word_size,
            delay_usec: 1,
            opcode_bits: 8,
            address_bits,
            page_size,
        },
        flash: false,
    })
}

/// upstream: e1000_82575.c e1000_init_function_pointers_82575()
pub const fn init_function_pointers_82575() -> (bool, bool, bool) {
    (true, true, true)
}

/// upstream: e1000_82575.c e1000_read_phy_reg_sgmii_82575()
pub fn read_phy_reg_sgmii_82575<P: SgmiiPhyOps>(
    phy: &mut P,
    address: u8,
    offset: u32,
) -> DevResult<u16> {
    if offset > SGMII_PHY_ADDR_MAX {
        return Err(DevError::InvalidParam);
    }
    phy.acquire_phy()?;
    let result = phy.read_i2c_phy(address, offset);
    phy.release_phy();
    result
}

/// upstream: e1000_82575.c e1000_write_phy_reg_sgmii_82575()
pub fn write_phy_reg_sgmii_82575<P: SgmiiPhyOps>(
    phy: &mut P,
    address: u8,
    offset: u32,
    value: u16,
) -> DevResult {
    if offset > SGMII_PHY_ADDR_MAX {
        return Err(DevError::InvalidParam);
    }
    phy.acquire_phy()?;
    let result = phy.write_i2c_phy(address, offset, value);
    phy.release_phy();
    result
}

/// upstream: e1000_82575.c e1000_get_phy_id_82575()
pub fn get_phy_id_82575<I: E1000RegisterIo, P: SgmiiPhyOps>(
    io: &mut I,
    phy: &mut P,
    mac: E1000MacType,
    sgmii: bool,
    uses_mdio: bool,
) -> DevResult<(u8, u32, u32)> {
    if matches!(mac, E1000MacType::I354) {
        let _ = phy.generic_phy_id(1)?;
    }
    if !sgmii {
        return phy.generic_phy_id(1).map(|(id, rev)| (1, id, rev));
    }
    if uses_mdio {
        let (addrreg, mask, shift) = match mac {
            E1000MacType::I82575 | E1000MacType::I82576 => (E1000_MDIC, 0x03e00000, 21),
            _ => (E1000_MDICNFG, 0x000007c0, 6),
        };
        let addr = ((io.read_register(addrreg)? & mask) >> shift) as u8;
        let (id, rev) = phy.generic_phy_id(addr)?;
        return Ok((addr, id, rev));
    }
    let ext = io.read_register(E1000_CTRL_EXT)?;
    io.write_register(E1000_CTRL_EXT, ext & !E1000_CTRL_EXT_SDP3_DATA)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(300_000);
    for addr in SGMII_ADDR_START..SGMII_MAX_PHY_ADDR {
        if let Ok(id) = read_phy_reg_sgmii_82575(phy, addr, 2) {
            if id == 0x0141 {
                let (id, rev) = phy.generic_phy_id(addr)?;
                io.write_register(E1000_CTRL_EXT, ext)?;
                return Ok((addr, id, rev));
            }
        }
    }
    io.write_register(E1000_CTRL_EXT, ext)?;
    Err(DevError::Io)
}

/// upstream: e1000_82575.c e1000_phy_hw_reset_sgmii_82575()
pub fn phy_hw_reset_sgmii_82575<P: PhyOps82571>(phy: &mut P, phy_id: u32) -> DevResult {
    phy.write_phy(0x1b, 0x8084)?;
    phy.reset_phy()?;
    if phy_id == M88E1512_ID {
        Ok(())
    } else {
        Ok(())
    }
}

/// upstream: e1000_82575.c e1000_set_d0_lplu_state_82575()
pub fn set_d0_lplu_state_82575<P: PhyOps82571>(
    phy: &mut P,
    active: bool,
    smart: super::phy::SmartSpeedMode,
) -> DevResult {
    let mut d = phy.read_phy(PHY_POWER_MGMT)?;
    if active {
        d |= IGP_PM_D0_LPLU;
        phy.write_phy(PHY_POWER_MGMT, d)?;
        let mut p = phy.read_phy(PHY_PORT_CONFIG)?;
        p &= !IGP_PM_SMART_SPEED;
        phy.write_phy(PHY_PORT_CONFIG, p)
    } else {
        d &= !IGP_PM_D0_LPLU;
        phy.write_phy(PHY_POWER_MGMT, d)?;
        if smart != super::phy::SmartSpeedMode::Default {
            let mut p = phy.read_phy(PHY_PORT_CONFIG)?;
            if smart == super::phy::SmartSpeedMode::On {
                p |= IGP_PM_SMART_SPEED
            } else {
                p &= !IGP_PM_SMART_SPEED
            }
            phy.write_phy(PHY_PORT_CONFIG, p)?;
        }
        Ok(())
    }
}

/// upstream: e1000_82575.c e1000_set_d0_lplu_state_82580()
pub fn set_d0_lplu_state_82580<I: E1000RegisterIo>(
    io: &mut I,
    smart: super::phy::SmartSpeedMode,
    active: bool,
) -> DevResult {
    let mut v = io.read_register(PHY_82580_POWER_MGMT)?;
    if active {
        v |= 0x2;
        v &= !0x10
    } else {
        v &= !0x2;
        if smart == super::phy::SmartSpeedMode::On {
            v |= 0x10
        } else if smart == super::phy::SmartSpeedMode::Off {
            v &= !0x10
        }
    }
    io.write_register(PHY_82580_POWER_MGMT, v)
}

/// upstream: e1000_82575.c e1000_set_d3_lplu_state_82580()
pub fn set_d3_lplu_state_82580<I: E1000RegisterIo>(
    io: &mut I,
    smart: super::phy::SmartSpeedMode,
    advertised: u16,
    active: bool,
) -> DevResult {
    let mut v = io.read_register(PHY_82580_POWER_MGMT)?;
    if !active {
        v &= !0x4;
        if smart == super::phy::SmartSpeedMode::On {
            v |= 0x10
        } else if smart == super::phy::SmartSpeedMode::Off {
            v &= !0x10
        }
    } else if matches!(advertised, 0x2f | 0x0f | 0x03) {
        v |= 0x4;
        v &= !0x10
    }
    io.write_register(PHY_82580_POWER_MGMT, v)
}

/// upstream: e1000_82575.c e1000_acquire_nvm_82575()
pub fn acquire_nvm_82575<I: E1000RegisterIo, S: NvmSemaphore82575>(
    io: &mut I,
    sem: &mut S,
    mac: E1000MacType,
) -> DevResult {
    sem.acquire_swfw(SWFW_EEP_SM)?;
    if mac == E1000MacType::I350 {
        let eecd = io.read_register(E1000_EECD)?;
        if eecd & EECD_ERROR_FLAGS != 0 {
            io.write_register(E1000_EECD, eecd | EECD_ERROR_CLR)?;
        }
    }
    if mac == E1000MacType::I82580 {
        let eecd = io.read_register(E1000_EECD)?;
        if eecd & 0x0000_8000 != 0 {
            io.write_register(E1000_EECD, eecd | 0x0000_8000)?;
        }
    }
    if let Err(e) = sem.acquire_eeprom_generic() {
        sem.release_swfw(SWFW_EEP_SM);
        return Err(e);
    }
    Ok(())
}

/// upstream: e1000_82575.c e1000_release_nvm_82575()
pub fn release_nvm_82575<S: NvmSemaphore82575>(sem: &mut S) {
    sem.release_eeprom_generic();
    sem.release_swfw(SWFW_EEP_SM);
}

/// upstream: e1000_82575.c e1000_get_cfg_done_82575()
pub fn get_cfg_done_82575<I: E1000RegisterIo>(
    io: &mut I,
    function: u8,
    igp3: bool,
    mut init_phy_script: impl FnMut() -> DevResult,
) -> DevResult {
    let mask = NVM_CFG_DONE[usize::from(function.min(3))];
    for _ in 0..PHY_CONFIG_TIMEOUT {
        if io.read_register(E1000_EEMNGCTL)? & mask != 0 {
            break;
        }
        io.delay_us(1_000)
    }
    if io.read_register(E1000_EECD)? & EECD_PRES == 0 && igp3 {
        init_phy_script()?
    }
    Ok(())
}

/// upstream: e1000_82575.c e1000_get_link_up_info_82575()
pub fn get_link_up_info_82575<O: LinkOps82575>(
    ops: &mut O,
    media: E1000MediaType,
    pcs: Option<(u16, u16)>,
) -> DevResult<(u16, u16)> {
    if media != E1000MediaType::Copper {
        pcs.ok_or(DevError::Io)
    } else {
        ops.generic_copper_speed()
    }
}

/// upstream: e1000_82575.c e1000_check_for_link_82575()
pub fn check_for_link_82575<O: LinkOps82575>(
    ops: &mut O,
    media: E1000MediaType,
    serdes_has_link: bool,
) -> DevResult {
    if media != E1000MediaType::Copper {
        let _ = serdes_has_link;
        ops.generic_fc()
    } else {
        ops.generic_check_copper()
    }
}

/// upstream: e1000_82575.c e1000_check_for_link_media_swap()
pub fn check_for_link_media_swap<P: PhyOps82571, O: LinkOps82575>(
    phy: &mut P,
    ops: &mut O,
    media_port: &mut u8,
    media_changed: &mut bool,
) -> DevResult {
    phy.write_phy(0x16, 0)?;
    let copper = phy.read_phy(0x11)? & 0x0400 != 0;
    phy.write_phy(0x16, 1)?;
    let other = phy.read_phy(0x11)? & 0x0400 != 0;
    let port = if other {
        2
    } else if copper {
        1
    } else {
        0
    };
    if port != 0 && port != *media_port {
        *media_port = port;
        *media_changed = true;
    }
    phy.write_phy(0x16, 0)?;
    ops.generic_check_link()
}

/// upstream: e1000_82575.c e1000_power_up_serdes_link_82575()
pub fn power_up_serdes_link_82575<I: E1000RegisterIo>(
    io: &mut I,
    media: E1000MediaType,
    sgmii: bool,
) -> DevResult {
    if media != E1000MediaType::InternalSerdes && !sgmii {
        return Ok(());
    }
    let pcs = io.read_register(E1000_PCS_CFG0)? | E1000_PCS_CFG_PCS_EN;
    io.write_register(E1000_PCS_CFG0, pcs)?;
    let ext = io.read_register(E1000_CTRL_EXT)? & !E1000_CTRL_EXT_SDP3_DATA;
    io.write_register(E1000_CTRL_EXT, ext)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(1_000);
    Ok(())
}

/// upstream: e1000_82575.c e1000_get_pcs_speed_and_duplex_82575()
pub fn get_pcs_speed_and_duplex_82575<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    serdes_has_link: &mut bool,
) -> DevResult<(u16, u16)> {
    let pcs = io.read_register(E1000_PCS_LSTAT)?;
    if pcs & E1000_PCS_LSTS_LINK_OK == 0 {
        *serdes_has_link = false;
        return Ok((0, 0));
    }
    *serdes_has_link = true;
    let mut speed = if pcs & E1000_PCS_LSTS_SPEED_1000 != 0 {
        1000
    } else if pcs & E1000_PCS_LSTS_SPEED_100 != 0 {
        100
    } else {
        10
    };
    let mut duplex = if pcs & E1000_PCS_LSTS_DUPLEX_FULL != 0 {
        1
    } else {
        0
    };
    if mac == E1000MacType::I354 {
        let s = io.read_register(E1000_STATUS)?;
        if s & 0x0040_0000 != 0 && s & 0x0080_0000 == 0 {
            speed = 2500;
            duplex = 1;
        }
    }
    Ok((speed, duplex))
}

/// upstream: e1000_82575.c e1000_shutdown_serdes_link_82575()
pub fn shutdown_serdes_link_82575<I: E1000RegisterIo>(
    io: &mut I,
    media: E1000MediaType,
    sgmii: bool,
    manageability_passthrough: bool,
) -> DevResult {
    if media != E1000MediaType::InternalSerdes && !sgmii {
        return Ok(());
    }
    if !manageability_passthrough {
        let pcs = io.read_register(E1000_PCS_CFG0)? & !E1000_PCS_CFG_PCS_EN;
        io.write_register(E1000_PCS_CFG0, pcs)?;
        let ext = io.read_register(E1000_CTRL_EXT)? | E1000_CTRL_EXT_SDP3_DATA;
        io.write_register(E1000_CTRL_EXT, ext)?;
        let _ = io.read_register(E1000_STATUS)?;
        io.delay_us(1_000);
    }
    Ok(())
}

/// upstream: e1000_82575.c e1000_reset_hw_82575()
pub fn reset_hw_82575<I: E1000RegisterIo + ScriptIo82575, O: LinkOps82575>(
    io: &mut I,
    ops: &mut O,
    mut disable_pcie_master: impl FnMut() -> DevResult,
    mut set_pcie_timeout: impl FnMut() -> DevResult,
    mut auto_read_done: impl FnMut() -> DevResult,
    mac: E1000MacType,
    eeprom_present: bool,
) -> DevResult {
    let _ = disable_pcie_master();
    let _ = set_pcie_timeout();
    io.write_register(E1000_IMC, u32::MAX)?;
    io.write_register(E1000_RCTL, 0)?;
    io.write_register(E1000_TCTL, E1000_TCTL_PSP)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(10_000);
    let ctrl = io.read_register(E1000_CTRL)?;
    io.write_register(E1000_CTRL, ctrl | E1000_CTRL_RST)?;
    let _ = auto_read_done();
    if !eeprom_present {
        let _ = reset_init_script_82575(io, mac);
    }
    io.write_register(E1000_IMC, u32::MAX)?;
    let _ = io.read_register(E1000_ICR)?;
    ops.check_alt_mac()
}

/// upstream: e1000_82575.c e1000_init_hw_82575()
pub fn init_hw_82575<O: LinkOps82575>(
    ops: &mut O,
    mut init_base: impl FnMut() -> DevResult,
    mtu: &mut u32,
) -> DevResult {
    let _ = ops.id_led_init();
    ops.clear_vfta()?;
    let result = init_base();
    *mtu = 1500;
    ops.clear_counters()?;
    result
}

/// upstream: e1000_82575.c e1000_setup_copper_link_82575()
pub fn setup_copper_link_82575<I: E1000RegisterIo, P: PhyOps82571, O: LinkOps82575>(
    io: &mut I,
    phy: &mut P,
    ops: &mut O,
    mac: E1000MacType,
    sgmii: bool,
    phy_type: PhyType82575,
) -> DevResult {
    let mut ctrl = io.read_register(E1000_CTRL)? | E1000_CTRL_SLU;
    ctrl &= !(E1000_CTRL_FRCSPD | E1000_CTRL_FRCDPX);
    io.write_register(E1000_CTRL, ctrl)?;
    if matches!(
        mac,
        E1000MacType::I82580 | E1000MacType::I350 | E1000MacType::I210 | E1000MacType::I211
    ) {
        let p = io.read_register(PHY_82580_POWER_MGMT)? & !0x0000_0008;
        io.write_register(PHY_82580_POWER_MGMT, p)?;
    }
    ops.generic_serdes_setup()?;
    if sgmii {
        io.delay_us(300_000);
        phy.reset_phy()?;
    }
    match phy_type {
        PhyType82575::I210 | PhyType82575::M88 => ops.phy_setup_m88()?,
        PhyType82575::Igp3 => ops.phy_setup_igp()?,
        PhyType82575::Phy82580 => ops.phy_setup_m88()?,
        PhyType82575::None => return Err(DevError::Io),
    }
    ops.generic_copper_setup()
}

/// upstream: e1000_82575.c e1000_setup_serdes_link_82575()
pub fn setup_serdes_link_82575<I: E1000RegisterIo, N: E1000NvmAccess>(
    io: &mut I,
    nvm: &mut N,
    mac: E1000MacType,
    media: E1000MediaType,
    sgmii: bool,
    autoneg: bool,
    flow: Flow82575,
) -> DevResult {
    if media != E1000MediaType::InternalSerdes && !sgmii {
        return Ok(());
    }
    io.write_register(E1000_SCTL, E1000_SCTL_DISABLE_SERDES_LOOPBACK)?;
    let mut ext = io.read_register(E1000_CTRL_EXT)?;
    ext &= !E1000_CTRL_EXT_SDP3_DATA;
    io.write_register(E1000_CTRL_EXT, ext)?;
    let mut ctrl = io.read_register(E1000_CTRL)? | E1000_CTRL_SLU;
    if matches!(mac, E1000MacType::I82575 | E1000MacType::I82576) {
        ctrl |= E1000_CTRL_SWDPIN0 | 0x0008_0000;
    }
    let mut pcs = io.read_register(E1000_PCS_LCTL)?;
    let mut pcs_an = autoneg;
    match ext & E1000_CTRL_EXT_LINK_MODE_MASK {
        E1000_CTRL_EXT_LINK_MODE_SGMII => {
            pcs_an = true;
            pcs &= !E1000_PCS_LCTL_AN_TIMEOUT
        }
        E1000_CTRL_EXT_LINK_MODE_1000BASE_KX => pcs_an = false,
        _ => {
            if matches!(mac, E1000MacType::I82575 | E1000MacType::I82576) {
                let data = nvm
                    .read_nvm_words(0x3f, 1)?
                    .first()
                    .copied()
                    .ok_or(DevError::Io)?;
                if data & 0x0001 != 0 {
                    pcs_an = false
                }
            }
            ctrl |= E1000_CTRL_SPD_1000 | E1000_CTRL_FRCSPD | E1000_CTRL_FD | E1000_CTRL_FRCDPX;
            pcs |= E1000_PCS_LCTL_FSV_1000 | E1000_PCS_LCTL_FDV_FULL;
        }
    }
    io.write_register(E1000_CTRL, ctrl)?;
    pcs &= !(E1000_PCS_LCTL_AN_ENABLE
        | E1000_PCS_LCTL_FLV_LINK_UP
        | E1000_PCS_LCTL_FSD
        | E1000_PCS_LCTL_FORCE_LINK);
    if pcs_an {
        pcs |= E1000_PCS_LCTL_AN_ENABLE | E1000_PCS_LCTL_AN_RESTART;
        pcs &= !E1000_PCS_LCTL_FORCE_FCTRL;
        let mut adv = io.read_register(E1000_PCS_ANADV)? & !(E1000_TXCW_ASM_DIR | E1000_TXCW_PAUSE);
        match flow {
            Flow82575::Full | Flow82575::RxPause => adv |= E1000_TXCW_ASM_DIR | E1000_TXCW_PAUSE,
            Flow82575::TxPause => adv |= E1000_TXCW_ASM_DIR,
            _ => {}
        }
        io.write_register(E1000_PCS_ANADV, adv)?;
    } else {
        pcs |= E1000_PCS_LCTL_FSD | E1000_PCS_LCTL_FORCE_FCTRL;
    }
    io.write_register(E1000_PCS_LCTL, pcs)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flow82575 {
    None,
    RxPause,
    TxPause,
    Full,
}

/// upstream: e1000_82575.c e1000_get_media_type_82575()
pub fn get_media_type_82575<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    mut uses_mdio: impl FnMut() -> DevResult<bool>,
    mut set_sfp_media: impl FnMut() -> DevResult<E1000MediaType>,
    sgmii_active: &mut bool,
) -> DevResult<E1000MediaType> {
    *sgmii_active = false;
    let mut ext = io.read_register(E1000_CTRL_EXT)?;
    match ext & E1000_CTRL_EXT_LINK_MODE_MASK {
        E1000_CTRL_EXT_LINK_MODE_1000BASE_KX => Ok(E1000MediaType::InternalSerdes),
        E1000_CTRL_EXT_LINK_MODE_GMII => Ok(E1000MediaType::Copper),
        E1000_CTRL_EXT_LINK_MODE_SGMII | E1000_CTRL_EXT_LINK_MODE_PCIE_SERDES => {
            if ext & E1000_CTRL_EXT_LINK_MODE_MASK == E1000_CTRL_EXT_LINK_MODE_SGMII && uses_mdio()?
            {
                *sgmii_active = true;
                Ok(E1000MediaType::Copper)
            } else {
                match set_sfp_media() {
                    Ok(media) if media != E1000MediaType::Other => {
                        ext = (ext & !E1000_CTRL_EXT_LINK_MODE_MASK)
                            | if *sgmii_active {
                                E1000_CTRL_EXT_LINK_MODE_SGMII
                            } else {
                                E1000_CTRL_EXT_LINK_MODE_PCIE_SERDES
                            };
                        io.write_register(E1000_CTRL_EXT, ext)?;
                        Ok(media)
                    }
                    _ => {
                        if ext & E1000_CTRL_EXT_LINK_MODE_MASK == E1000_CTRL_EXT_LINK_MODE_SGMII {
                            *sgmii_active = true;
                            Ok(E1000MediaType::Copper)
                        } else {
                            Ok(E1000MediaType::InternalSerdes)
                        }
                    }
                }
            }
        }
        _ => {
            let _ = mac;
            Ok(E1000MediaType::Other)
        }
    }
}

/// upstream: e1000_82575.c e1000_set_sfp_media_type_82575()
pub fn set_sfp_media_type_82575<I: E1000RegisterIo, S: SfpEeprom82575>(
    io: &mut I,
    sfp: &mut S,
    media: &mut E1000MediaType,
    sgmii_active: &mut bool,
    module_plugged: &mut bool,
) -> DevResult {
    const IDENTIFIER: u8 = 0;
    const ETH_FLAGS: u8 = 6;
    let original = io.read_register(E1000_CTRL_EXT)?;
    io.write_register(
        E1000_CTRL_EXT,
        (original & !E1000_CTRL_EXT_SDP3_DATA) | E1000_CTRL_I2C_ENA,
    )?;
    let _ = io.read_register(E1000_STATUS)?;
    let mut ident = Err(DevError::Io);
    for _ in 0..3 {
        ident = sfp.read_sfp_byte(IDENTIFIER);
        if ident.is_ok() {
            break;
        }
        io.delay_us(100_000);
    }
    let identifier = ident?;
    let flags = sfp.read_sfp_byte(ETH_FLAGS)?;
    *module_plugged = matches!(identifier, 3 | 4);
    *sgmii_active = false;
    *media = if flags & 0x03 != 0 {
        E1000MediaType::InternalSerdes
    } else if flags & 0x30 != 0 {
        *sgmii_active = true;
        E1000MediaType::InternalSerdes
    } else if flags & 0x08 != 0 {
        *sgmii_active = true;
        E1000MediaType::Copper
    } else {
        E1000MediaType::Other
    };
    io.write_register(E1000_CTRL_EXT, original)?;
    Ok(())
}

/// upstream: e1000_82575.c e1000_valid_led_default_82575()
pub fn valid_led_default_82575<N: E1000NvmAccess>(
    nvm: &mut N,
    media: E1000MediaType,
) -> DevResult<u16> {
    let data = nvm
        .read_nvm_words(NVM_LED_WORD, 1)?
        .first()
        .copied()
        .ok_or(DevError::Io)?;
    if data == 0 || data == u16::MAX {
        Ok(if media == E1000MediaType::InternalSerdes {
            ID_LED_DEFAULT_SERDES
        } else {
            ID_LED_DEFAULT
        })
    } else {
        Ok(data)
    }
}

/// upstream: e1000_82575.c e1000_reset_init_script_82575()
pub fn reset_init_script_82575<S: ScriptIo82575>(script: &mut S, mac: E1000MacType) -> DevResult {
    if mac == E1000MacType::I82575 {
        for (reg, offset, value) in [
            (E1000_SCTL, 0, 0x0c),
            (E1000_SCTL, 1, 0x78),
            (E1000_SCTL, 0x1b, 0x23),
            (E1000_SCTL, 0x23, 0x15),
            (E1000_CCMCTL, 0x14, 0),
            (E1000_CCMCTL, 0x10, 0),
            (E1000_GIOCTL, 0, 0xec),
            (E1000_GIOCTL, 0x61, 0xdf),
            (E1000_GIOCTL, 0x34, 5),
            (E1000_GIOCTL, 0x2f, 0x81),
            (E1000_SCCTL, 2, 0x47),
            (E1000_SCCTL, 0x14, 0),
            (E1000_SCCTL, 0x10, 0),
        ] {
            script.write_8bit_ctrl_field(reg, offset, value)?;
        }
    }
    Ok(())
}

/// upstream: e1000_82575.c e1000_read_mac_addr_82575()
pub fn read_mac_addr_82575<O: LinkOps82575, F: FnMut() -> DevResult<[u8; 6]>>(
    ops: &mut O,
    mut read: F,
) -> DevResult<[u8; 6]> {
    let _ = ops;
    read()
}

/// upstream: e1000_82575.c e1000_config_collision_dist_82575()
pub fn config_collision_dist_82575<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let v = (io.read_register(E1000_TCTL_EXT)? & !0x000f_fc00) | (63 << 10);
    io.write_register(E1000_TCTL_EXT, v)?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: e1000_82575.c e1000_clear_hw_cntrs_82575()
pub fn clear_hw_cntrs_82575<I: E1000RegisterIo, B: FnMut() -> DevResult>(
    io: &mut I,
    mut base: B,
) -> DevResult {
    base()?;
    for r in [
        0x405c, 0x4060, 0x4064, 0x4068, 0x406c, 0x4070, 0x40d8, 0x40dc, 0x40e0, 0x40e4, 0x40e8,
        0x40ec, 0x4004, 0x400c, 0x4034, 0x403c, 0x40f8, 0x40fc, 0x40b4, 0x40b8, 0x40bc, 0x4100,
        0x4104, 0x4108, 0x4110, 0x4114, 0x4118, 0x411c, 0x4120, 0x4124,
    ] {
        let _ = io.read_register(r)?;
    }
    Ok(())
}

/// upstream: e1000_82575.c e1000_set_pcie_completion_timeout()
pub fn set_pcie_completion_timeout<I: E1000RegisterIo, P: super::osdep::E1000PciConfig>(
    io: &mut I,
    pci: &mut P,
) -> DevResult {
    let mut gcr = io.read_register(E1000_GCR)?;
    if gcr & E1000_GCR_CMPL_TMOUT_MASK == 0 {
        if gcr & E1000_GCR_CAP_VER2 == 0 {
            gcr |= 0x1000
        } else {
            let cap = pci.find_capability(0x10).ok_or(DevError::Io)?;
            let mut d = pci.read_config_u16(cap + 0x28).ok_or(DevError::Io)?;
            d |= 5;
            if !pci.write_config_u16(cap + 0x28, d) {
                return Err(DevError::Io);
            }
        }
    }
    gcr &= !E1000_GCR_CMPL_TMOUT_RESEND;
    io.write_register(E1000_GCR, gcr)
}

/// upstream: e1000_82575.c e1000_vmdq_set_anti_spoofing_pf()
pub fn vmdq_set_anti_spoofing_pf<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    enable: bool,
    pf: u8,
) -> DevResult {
    let reg = match mac {
        E1000MacType::I82576 => E1000_DTXSWC,
        E1000MacType::I350 | E1000MacType::I354 => E1000_TXSWC,
        _ => return Ok(()),
    };
    let mut value = io.read_register(reg)?;
    if enable {
        value |= 0x0000_ffff;
        value ^= (1u32 << pf) | (1u32 << (pf + 7));
    } else {
        value &= !0x0000_ffff;
    }
    io.write_register(reg, value)
}

/// upstream: e1000_82575.c e1000_vmdq_set_loopback_pf()
pub fn vmdq_set_loopback_pf<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    enable: bool,
) -> DevResult {
    let reg = match mac {
        E1000MacType::I82576 => E1000_DTXSWC,
        E1000MacType::I350 | E1000MacType::I354 => E1000_TXSWC,
        _ => return Ok(()),
    };
    let mut v = io.read_register(reg)?;
    if enable {
        v |= 1 << 31
    } else {
        v &= !(1 << 31)
    }
    io.write_register(reg, v)
}

/// upstream: e1000_82575.c e1000_vmdq_set_replication_pf()
pub fn vmdq_set_replication_pf<I: E1000RegisterIo>(io: &mut I, enable: bool) -> DevResult {
    let mut v = io.read_register(E1000_VT_CTL)?;
    if enable {
        v |= 1 << 30
    } else {
        v &= !(1 << 30)
    }
    io.write_register(E1000_VT_CTL, v)
}

pub trait ManagedPhy82575 {
    fn acquire(&mut self) -> DevResult;
    fn release(&mut self);
    fn read_mdic(&mut self, offset: u32) -> DevResult<u16>;
    fn write_mdic(&mut self, offset: u32, value: u16) -> DevResult;
}

/// upstream: e1000_82575.c e1000_read_phy_reg_82580()
pub fn read_phy_reg_82580<P: ManagedPhy82575>(phy: &mut P, offset: u32) -> DevResult<u16> {
    phy.acquire()?;
    let r = phy.read_mdic(offset);
    phy.release();
    r
}

/// upstream: e1000_82575.c e1000_write_phy_reg_82580()
pub fn write_phy_reg_82580<P: ManagedPhy82575>(phy: &mut P, offset: u32, value: u16) -> DevResult {
    phy.acquire()?;
    let r = phy.write_mdic(offset, value);
    phy.release();
    r
}

/// upstream: e1000_82575.c e1000_reset_mdicnfg_82580()
pub fn reset_mdicnfg_82580<I: E1000RegisterIo, N: E1000NvmAccess>(
    io: &mut I,
    nvm: &mut N,
    mac: E1000MacType,
    sgmii: bool,
    function: u8,
) -> DevResult {
    if mac != E1000MacType::I82580 || !sgmii {
        return Ok(());
    }
    let data = nvm
        .read_nvm_words(0x24 + u16::from(function) * 0x40, 1)?
        .first()
        .copied()
        .ok_or(DevError::Io)?;
    let mut v = io.read_register(E1000_MDICNFG)?;
    if data & 1 != 0 {
        v |= E1000_MDICNFG_EXT_MDIO
    }
    if data & 2 != 0 {
        v |= E1000_MDICNFG_COM_MDIO
    }
    io.write_register(E1000_MDICNFG, v)
}

/// upstream: e1000_82575.c e1000_reset_hw_82580()
pub fn reset_hw_82580<I: E1000RegisterIo, O: LinkOps82575>(
    io: &mut I,
    ops: &mut O,
    mut disable_pcie_master: impl FnMut() -> DevResult,
    mut auto_read_done: impl FnMut() -> DevResult,
    device_reset: bool,
) -> DevResult {
    let mut ctrl = io.read_register(E1000_CTRL)?;
    let _ = disable_pcie_master();
    io.write_register(E1000_IMC, u32::MAX)?;
    io.write_register(E1000_RCTL, 0)?;
    io.write_register(E1000_TCTL, E1000_TCTL_PSP)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(10_000);
    if device_reset {
        ctrl |= 0x0400_0000
    } else {
        ctrl |= E1000_CTRL_RST
    }
    io.write_register(E1000_CTRL, ctrl)?;
    io.delay_us(5_000);
    let _ = auto_read_done();
    let status = io.read_register(E1000_STATUS)?;
    io.write_register(E1000_STATUS, status | 0x0010_0000)?;
    io.write_register(E1000_IMC, u32::MAX)?;
    let _ = io.read_register(E1000_ICR)?;
    ops.generic_check_link()
}

/// upstream: e1000_82575.c e1000_rxpbs_adjust_82580()
pub const fn rxpbs_adjust_82580(data: u32) -> u16 {
    const TABLE: [u16; 11] = [36, 72, 144, 1, 2, 4, 8, 16, 35, 70, 140];
    if data < 11 { TABLE[data as usize] } else { 0 }
}

/// upstream: e1000_82575.c e1000_validate_nvm_checksum_with_offset()
pub fn validate_nvm_checksum_with_offset<N: E1000NvmAccess>(nvm: &mut N, offset: u16) -> DevResult {
    let words = nvm.read_nvm_words(offset, NVM_CHECKSUM_REG + 1)?;
    if words.iter().fold(0u16, |sum, v| sum.wrapping_add(*v)) == NVM_SUM {
        Ok(())
    } else {
        Err(DevError::Io)
    }
}

/// upstream: e1000_82575.c e1000_update_nvm_checksum_with_offset()
pub fn update_nvm_checksum_with_offset<N: E1000NvmAccess>(nvm: &mut N, offset: u16) -> DevResult {
    let words = nvm.read_nvm_words(offset, NVM_CHECKSUM_REG)?;
    let sum = words.iter().fold(0u16, |sum, v| sum.wrapping_add(*v));
    nvm.write_nvm_words(offset + NVM_CHECKSUM_REG, &[NVM_SUM.wrapping_sub(sum)])
}

/// upstream: e1000_82575.c e1000_validate_nvm_checksum_82580()
pub fn validate_nvm_checksum_82580<N: E1000NvmAccess>(nvm: &mut N) -> DevResult {
    let compat = nvm
        .read_nvm_words(3, 1)?
        .first()
        .copied()
        .ok_or(DevError::Io)?;
    let count = if compat & 0x8000 != 0 { 4 } else { 1 };
    for port in 0..count {
        validate_nvm_checksum_with_offset(nvm, port * 0x40)?;
    }
    Ok(())
}

/// upstream: e1000_82575.c e1000_update_nvm_checksum_82580()
pub fn update_nvm_checksum_82580<N: E1000NvmAccess>(nvm: &mut N) -> DevResult {
    let mut compat = nvm
        .read_nvm_words(3, 1)?
        .first()
        .copied()
        .ok_or(DevError::Io)?;
    if compat & 0x8000 == 0 {
        compat |= 0x8000;
        nvm.write_nvm_words(3, &[compat])?;
    }
    for port in 0..4 {
        update_nvm_checksum_with_offset(nvm, port * 0x40)?;
    }
    Ok(())
}

/// upstream: e1000_82575.c e1000_validate_nvm_checksum_i350()
pub fn validate_nvm_checksum_i350<N: E1000NvmAccess>(nvm: &mut N) -> DevResult {
    for port in 0..4 {
        validate_nvm_checksum_with_offset(nvm, port * 0x40)?;
    }
    Ok(())
}

/// upstream: e1000_82575.c e1000_update_nvm_checksum_i350()
pub fn update_nvm_checksum_i350<N: E1000NvmAccess>(nvm: &mut N) -> DevResult {
    for port in 0..4 {
        update_nvm_checksum_with_offset(nvm, port * 0x40)?;
    }
    Ok(())
}

/// upstream: e1000_82575.c __e1000_access_emi_reg()
pub fn access_emi_reg_82575<P: PhyOps82571>(
    phy: &mut P,
    address: u16,
    data: &mut u16,
    read: bool,
) -> DevResult {
    phy.write_phy(0x10, address)?;
    if read {
        *data = phy.read_phy(0x11)?;
        Ok(())
    } else {
        phy.write_phy(0x11, *data)
    }
}

/// upstream: e1000_82575.c e1000_read_emi_reg()
pub fn read_emi_reg_82575<P: PhyOps82571>(phy: &mut P, address: u16) -> DevResult<u16> {
    let mut value = 0;
    access_emi_reg_82575(phy, address, &mut value, true)?;
    Ok(value)
}

/// upstream: e1000_82575.c e1000_initialize_M88E1512_phy()
pub fn initialize_m88e1512_phy<P: PhyOps82571>(phy: &mut P, phy_id: u32) -> DevResult {
    if phy_id != M88E1512_ID {
        return Ok(());
    }
    for (reg, value) in [
        (0x16, 0xff),
        (0x1d, 0x214b),
        (0x1e, 0x2144),
        (0x1d, 0x0c28),
        (0x1e, 0x2146),
        (0x1d, 0xb233),
        (0x1e, 0x214d),
        (0x1d, 0xcc0c),
        (0x1e, 0x2159),
        (0x16, 0xfb),
        (0x1c, 0x000d),
        (0x16, 0x12),
        (0x14, 0x8001),
        (0x16, 0),
    ] {
        phy.write_phy(reg, value)?;
    }
    phy.reset_phy()?;
    phy.delay_us(1_000_000);
    Ok(())
}

/// upstream: e1000_82575.c e1000_initialize_M88E1543_phy()
pub fn initialize_m88e1543_phy<P: PhyOps82571>(phy: &mut P, phy_id: u32) -> DevResult {
    if phy_id != M88E1543_ID {
        return Ok(());
    }
    for (reg, value) in [
        (0x16, 0xff),
        (0x1d, 0x214b),
        (0x1e, 0x2144),
        (0x1d, 0x0c28),
        (0x1e, 0x2146),
        (0x1d, 0xb233),
        (0x1e, 0x214d),
        (0x1d, 0xdc0c),
        (0x1e, 0x2159),
        (0x16, 0xfb),
        (0x1c, 0xc00d),
        (0x16, 0x12),
        (0x14, 0x8001),
        (0x16, 1),
        (0x10, 0x9140),
        (0x16, 0),
    ] {
        phy.write_phy(reg, value)?;
    }
    phy.reset_phy()?;
    phy.delay_us(1_000_000);
    Ok(())
}

/// upstream: e1000_82575.c e1000_set_eee_i350()
pub fn set_eee_i350<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    media: E1000MediaType,
    disabled: bool,
    advertise_1g: bool,
    advertise_100m: bool,
) -> DevResult {
    if !matches!(
        mac,
        E1000MacType::I350 | E1000MacType::I354 | E1000MacType::I210 | E1000MacType::I211
    ) || media != E1000MediaType::Copper
    {
        return Ok(());
    }
    let mut ip = io.read_register(0x0e38)?;
    let mut eeer = io.read_register(0x0e30)?;
    if !disabled {
        let _eee_status = io.read_register(0x0e34)?;
        if advertise_100m {
            ip |= E1000_IPCNFG_EEE_100M_AN
        } else {
            ip &= !E1000_IPCNFG_EEE_100M_AN
        }
        if advertise_1g {
            ip |= E1000_IPCNFG_EEE_1G_AN
        } else {
            ip &= !E1000_IPCNFG_EEE_1G_AN
        }
        eeer |= E1000_EEER_TX_LPI_EN | E1000_EEER_RX_LPI_EN | E1000_EEER_LPI_FC;
    } else {
        ip &= !(E1000_IPCNFG_EEE_1G_AN | E1000_IPCNFG_EEE_100M_AN);
        eeer &= !(E1000_EEER_TX_LPI_EN | E1000_EEER_RX_LPI_EN | E1000_EEER_LPI_FC);
    }
    io.write_register(0x0e38, ip)?;
    io.write_register(0x0e30, eeer)?;
    let _ = io.read_register(0x0e38)?;
    let _ = io.read_register(0x0e30)?;
    Ok(())
}

pub trait EeeXmdio82575 {
    fn read_xmdio(&mut self, address: u16, dev: u8) -> DevResult<u16>;
    fn write_xmdio(&mut self, address: u16, dev: u8, value: u16) -> DevResult;
}

/// upstream: e1000_82575.c e1000_set_eee_i354()
pub fn set_eee_i354<P: PhyOps82571, X: EeeXmdio82575>(
    phy: &mut P,
    xmdio: &mut X,
    media: E1000MediaType,
    phy_id: u32,
    disabled: bool,
    adv_1g: bool,
    adv_100m: bool,
) -> DevResult {
    if media != E1000MediaType::Copper || !matches!(phy_id, M88E1543_ID | M88E1512_ID) {
        return Ok(());
    }
    if !disabled {
        phy.write_phy(0x16, 18)?;
        let data = phy.read_phy(0x14)? | 1;
        phy.write_phy(0x14, data)?;
        phy.write_phy(0x16, 0)?;
    }
    let mut adv = xmdio.read_xmdio(0x003c, 7)?;
    if disabled {
        adv &= !0x6
    } else {
        if adv_100m {
            adv |= E1000_EEE_ADV_100_SUPPORTED as u16
        } else {
            adv &= !E1000_EEE_ADV_100_SUPPORTED as u16
        }
        if adv_1g {
            adv |= E1000_EEE_ADV_1000_SUPPORTED as u16
        } else {
            adv &= !E1000_EEE_ADV_1000_SUPPORTED as u16
        }
    }
    xmdio.write_xmdio(0x003c, 7, adv)
}

/// upstream: e1000_82575.c e1000_get_eee_status_i354()
pub fn get_eee_status_i354<X: EeeXmdio82575>(
    xmdio: &mut X,
    media: E1000MediaType,
    phy_id: u32,
    status: &mut bool,
) -> DevResult {
    if media != E1000MediaType::Copper || !matches!(phy_id, M88E1543_ID | M88E1512_ID) {
        return Ok(());
    }
    let data = xmdio.read_xmdio(0x0001, 3)?;
    *status = data & 0x0c00 != 0;
    Ok(())
}

/// upstream: e1000_82575.c e1000_clear_vfta_i350()
pub fn clear_vfta_i350<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    for index in 0..128 {
        for _ in 0..10 {
            io.write_register(E1000_VFTA + index * 4, 0)?;
        }
        let _ = io.read_register(E1000_STATUS)?;
    }
    Ok(())
}

/// upstream: e1000_82575.c e1000_write_vfta_i350()
pub fn write_vfta_i350<I: E1000RegisterIo>(io: &mut I, offset: u32, value: u32) -> DevResult {
    for _ in 0..10 {
        io.write_register(E1000_VFTA + offset * 4, value)?;
    }
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

const I2C_SCL_OUT: u32 = 0x200;
const I2C_SDA_OUT: u32 = 0x400;
const I2C_SDA_OE_N: u32 = 0x800;
const I2C_SDA_IN: u32 = 0x1000;
const I2C_SCL_OE_N: u32 = 0x2000;
const I2C_SCL_IN: u32 = 0x4000;
const I2C_T_HD_STA: u32 = 4;
const I2C_T_LOW: u32 = 5;
const I2C_T_HIGH: u32 = 4;
const I2C_T_SU_STA: u32 = 5;
const I2C_T_SU_DATA: u32 = 1;
const I2C_T_RISE: u32 = 1;
const I2C_T_FALL: u32 = 1;
const I2C_T_SU_STO: u32 = 4;
const I2C_T_BUF: u32 = 5;
const I2C_SWFW_PHY0_SM: u16 = 2;

/// upstream: e1000_82575.c e1000_set_i2c_bb()
pub fn set_i2c_bb<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let ext = io.read_register(E1000_CTRL_EXT)? | E1000_CTRL_I2C_ENA;
    io.write_register(E1000_CTRL_EXT, ext)?;
    let _ = io.read_register(E1000_STATUS)?;
    let params = io.read_register(E1000_I2CPARAMS)? | E1000_I2CBB_EN | I2C_SDA_OE_N | I2C_SCL_OE_N;
    io.write_register(E1000_I2CPARAMS, params)?;
    let _ = io.read_register(E1000_STATUS)?;
    Ok(())
}

/// upstream: e1000_82575.c e1000_get_i2c_data()
pub const fn get_i2c_data(i2cctl: u32) -> bool {
    i2cctl & I2C_SDA_IN != 0
}

/// upstream: e1000_82575.c e1000_raise_i2c_clk()
pub fn raise_i2c_clk<I: E1000RegisterIo>(io: &mut I, i2cctl: &mut u32) -> DevResult {
    *i2cctl |= I2C_SCL_OUT;
    *i2cctl &= !I2C_SCL_OE_N;
    io.write_register(E1000_I2CPARAMS, *i2cctl)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(I2C_T_RISE);
    Ok(())
}

/// upstream: e1000_82575.c e1000_lower_i2c_clk()
pub fn lower_i2c_clk<I: E1000RegisterIo>(io: &mut I, i2cctl: &mut u32) -> DevResult {
    *i2cctl &= !I2C_SCL_OUT;
    *i2cctl &= !I2C_SCL_OE_N;
    io.write_register(E1000_I2CPARAMS, *i2cctl)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(I2C_T_FALL);
    Ok(())
}

/// upstream: e1000_82575.c e1000_set_i2c_data()
pub fn set_i2c_data<I: E1000RegisterIo>(io: &mut I, i2cctl: &mut u32, data: bool) -> DevResult {
    if data {
        *i2cctl |= I2C_SDA_OUT
    } else {
        *i2cctl &= !I2C_SDA_OUT
    }
    *i2cctl &= !I2C_SDA_OE_N;
    *i2cctl |= I2C_SCL_OE_N;
    io.write_register(E1000_I2CPARAMS, *i2cctl)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(I2C_T_RISE + I2C_T_FALL + I2C_T_SU_DATA);
    *i2cctl = io.read_register(E1000_I2CPARAMS)?;
    if data != get_i2c_data(*i2cctl) {
        Err(DevError::Io)
    } else {
        Ok(())
    }
}

/// upstream: e1000_82575.c e1000_clock_in_i2c_bit()
pub fn clock_in_i2c_bit<I: E1000RegisterIo>(io: &mut I) -> DevResult<bool> {
    let mut ctl = io.read_register(E1000_I2CPARAMS)?;
    raise_i2c_clk(io, &mut ctl)?;
    io.delay_us(I2C_T_HIGH);
    ctl = io.read_register(E1000_I2CPARAMS)?;
    let data = get_i2c_data(ctl);
    lower_i2c_clk(io, &mut ctl)?;
    io.delay_us(I2C_T_LOW);
    Ok(data)
}

/// upstream: e1000_82575.c e1000_clock_out_i2c_bit()
pub fn clock_out_i2c_bit<I: E1000RegisterIo>(io: &mut I, data: bool) -> DevResult {
    let mut ctl = io.read_register(E1000_I2CPARAMS)?;
    set_i2c_data(io, &mut ctl, data)?;
    raise_i2c_clk(io, &mut ctl)?;
    io.delay_us(I2C_T_HIGH);
    lower_i2c_clk(io, &mut ctl)?;
    io.delay_us(I2C_T_LOW);
    Ok(())
}

/// upstream: e1000_82575.c e1000_clock_in_i2c_byte()
pub fn clock_in_i2c_byte<I: E1000RegisterIo>(io: &mut I) -> DevResult<u8> {
    let mut value = 0u8;
    for bit in (0..8).rev() {
        if clock_in_i2c_bit(io)? {
            value |= 1 << bit
        }
    }
    Ok(value)
}

/// upstream: e1000_82575.c e1000_clock_out_i2c_byte()
pub fn clock_out_i2c_byte<I: E1000RegisterIo>(io: &mut I, value: u8) -> DevResult {
    for bit in (0..8).rev() {
        clock_out_i2c_bit(io, value & (1 << bit) != 0)?;
    }
    let mut ctl = io.read_register(E1000_I2CPARAMS)? | I2C_SDA_OE_N;
    io.write_register(E1000_I2CPARAMS, ctl)?;
    let _ = io.read_register(E1000_STATUS)?;
    ctl = io.read_register(E1000_I2CPARAMS)?;
    let _ = ctl;
    Ok(())
}

/// upstream: e1000_82575.c e1000_get_i2c_ack()
pub fn get_i2c_ack<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let mut ctl = io.read_register(E1000_I2CPARAMS)?;
    raise_i2c_clk(io, &mut ctl)?;
    io.delay_us(I2C_T_HIGH);
    for _ in 0..10 {
        io.delay_us(1);
        ctl = io.read_register(E1000_I2CPARAMS)?;
        if ctl & I2C_SCL_IN != 0 {
            break;
        }
    }
    if ctl & I2C_SCL_IN == 0 {
        return Err(DevError::Io);
    }
    let ack = !get_i2c_data(ctl);
    lower_i2c_clk(io, &mut ctl)?;
    io.delay_us(I2C_T_LOW);
    if ack { Ok(()) } else { Err(DevError::Io) }
}

/// upstream: e1000_82575.c e1000_i2c_start()
pub fn i2c_start<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let mut ctl = io.read_register(E1000_I2CPARAMS)?;
    set_i2c_data(io, &mut ctl, true)?;
    raise_i2c_clk(io, &mut ctl)?;
    io.delay_us(I2C_T_SU_STA);
    set_i2c_data(io, &mut ctl, false)?;
    io.delay_us(I2C_T_HD_STA);
    lower_i2c_clk(io, &mut ctl)?;
    io.delay_us(I2C_T_LOW);
    Ok(())
}

/// upstream: e1000_82575.c e1000_i2c_stop()
pub fn i2c_stop<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let mut ctl = io.read_register(E1000_I2CPARAMS)?;
    set_i2c_data(io, &mut ctl, false)?;
    raise_i2c_clk(io, &mut ctl)?;
    io.delay_us(I2C_T_SU_STO);
    set_i2c_data(io, &mut ctl, true)?;
    io.delay_us(I2C_T_BUF);
    Ok(())
}

/// upstream: e1000_82575.c e1000_i2c_bus_clear()
pub fn i2c_bus_clear<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    i2c_start(io)?;
    let mut ctl = io.read_register(E1000_I2CPARAMS)?;
    set_i2c_data(io, &mut ctl, true)?;
    for _ in 0..9 {
        raise_i2c_clk(io, &mut ctl)?;
        io.delay_us(I2C_T_HIGH);
        lower_i2c_clk(io, &mut ctl)?;
        io.delay_us(I2C_T_LOW);
    }
    i2c_start(io)?;
    i2c_stop(io)
}

/// upstream: e1000_82575.c e1000_read_i2c_byte_generic()
pub fn read_i2c_byte_generic<I: E1000RegisterIo, S: NvmSemaphore82575>(
    io: &mut I,
    sem: &mut S,
    byte_offset: u8,
    dev_addr: u8,
) -> DevResult<u8> {
    let mut retry = 1;
    while retry < 10 {
        if sem.acquire_swfw(I2C_SWFW_PHY0_SM).is_err() {
            return Err(DevError::ResourceBusy);
        }
        let result = (|| {
            i2c_start(io)?;
            clock_out_i2c_byte(io, dev_addr)?;
            get_i2c_ack(io)?;
            clock_out_i2c_byte(io, byte_offset)?;
            get_i2c_ack(io)?;
            i2c_start(io)?;
            clock_out_i2c_byte(io, dev_addr | 1)?;
            get_i2c_ack(io)?;
            let value = clock_in_i2c_byte(io)?;
            clock_out_i2c_bit(io, true)?;
            i2c_stop(io)?;
            Ok(value)
        })();
        match result {
            Ok(value) => {
                sem.release_swfw(I2C_SWFW_PHY0_SM);
                return Ok(value);
            }
            Err(error) => {
                sem.release_swfw(I2C_SWFW_PHY0_SM);
                io.delay_us(100_000);
                let _ = i2c_bus_clear(io);
                retry += 1;
                if retry >= 10 {
                    return Err(error);
                }
            }
        }
    }
    Err(DevError::Io)
}

/// upstream: e1000_82575.c e1000_write_i2c_byte_generic()
pub fn write_i2c_byte_generic<I: E1000RegisterIo, S: NvmSemaphore82575>(
    io: &mut I,
    sem: &mut S,
    byte_offset: u8,
    dev_addr: u8,
    value: u8,
) -> DevResult {
    sem.acquire_swfw(I2C_SWFW_PHY0_SM)?;
    let result = (|| {
        i2c_start(io)?;
        clock_out_i2c_byte(io, dev_addr)?;
        get_i2c_ack(io)?;
        clock_out_i2c_byte(io, byte_offset)?;
        get_i2c_ack(io)?;
        clock_out_i2c_byte(io, value)?;
        get_i2c_ack(io)?;
        i2c_stop(io)
    })();
    if result.is_err() {
        let _ = i2c_bus_clear(io);
    }
    sem.release_swfw(I2C_SWFW_PHY0_SM);
    result
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
    struct Sgmii {
        acquires: u32,
        releases: u32,
        reg: u16,
    }
    impl SgmiiPhyOps for Sgmii {
        fn acquire_phy(&mut self) -> DevResult {
            self.acquires += 1;
            Ok(())
        }
        fn release_phy(&mut self) {
            self.releases += 1
        }
        fn read_i2c_phy(&mut self, _: u8, _: u32) -> DevResult<u16> {
            Ok(self.reg)
        }
        fn write_i2c_phy(&mut self, _: u8, _: u32, v: u16) -> DevResult {
            self.reg = v;
            Ok(())
        }
        fn generic_phy_id(&mut self, _: u8) -> DevResult<(u32, u32)> {
            Ok((0x01410c30, 1))
        }
        fn delay_us(&mut self, _: u32) {}
    }
    #[test]
    fn generation_params_sgmii_access_and_pcs_status_follow_register_policy() {
        let mut io = Io::default();
        io.regs.push((E1000_EECD, 0x7800 | 0x400));
        let nvm = init_nvm_params_82575(&mut io, E1000MacType::I82575, false, false).unwrap();
        assert_eq!(nvm.config.word_size, 32768);
        assert_eq!(nvm.config.page_size, 128);
        assert_eq!(rxpbs_adjust_82580(0), 36);
        assert_eq!(rxpbs_adjust_82580(10), 140);
        assert_eq!(rxpbs_adjust_82580(11), 0);
        let mut phy = Sgmii {
            acquires: 0,
            releases: 0,
            reg: 0x55aa,
        };
        assert_eq!(read_phy_reg_sgmii_82575(&mut phy, 2, 1).unwrap(), 0x55aa);
        write_phy_reg_sgmii_82575(&mut phy, 2, 1, 0xa55a).unwrap();
        assert_eq!(phy.reg, 0xa55a);
        assert_eq!((phy.acquires, phy.releases), (2, 2));
        let mut io = Io::default();
        io.regs.push((
            E1000_PCS_LSTAT,
            E1000_PCS_LSTS_LINK_OK | E1000_PCS_LSTS_SPEED_1000 | E1000_PCS_LSTS_DUPLEX_FULL,
        ));
        let mut link = false;
        assert_eq!(
            get_pcs_speed_and_duplex_82575(&mut io, E1000MacType::I82575, &mut link).unwrap(),
            (1000, 1)
        );
        assert!(link);
    }
    #[test]
    fn bitbang_enable_and_eee_advertisement_preserve_supported_fields() {
        let mut io = Io::default();
        set_i2c_bb(&mut io).unwrap();
        assert!(
            io.writes
                .iter()
                .any(|(r, v)| *r == E1000_CTRL_EXT && v & E1000_CTRL_I2C_ENA != 0)
        );
        assert!(get_i2c_data(I2C_SDA_IN));
        assert!(!get_i2c_data(0));
        let mut eee = Io::default();
        set_eee_i350(
            &mut eee,
            E1000MacType::I350,
            E1000MediaType::Copper,
            false,
            true,
            false,
        )
        .unwrap();
        let ip = eee
            .writes
            .iter()
            .find(|(r, _)| *r == E1000_IPCNFG)
            .unwrap()
            .1;
        assert_ne!(ip & E1000_IPCNFG_EEE_1G_AN, 0);
        assert_eq!(ip & E1000_IPCNFG_EEE_100M_AN, 0);
    }
}
