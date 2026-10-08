//! Intel 82571/82572/82573/82574/82583 family operations.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_82571.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{
    api::E1000MacType,
    mac::E1000MediaType,
    nvm::{
        E1000NvmAccess, E1000NvmConfig, E1000NvmType, update_nvm_checksum_generic,
        validate_nvm_checksum_generic,
    },
    osdep::E1000RegisterIo,
    registers::*,
};

const ALL_SPEED_DUPLEX: u16 = 0x002f;
const ALL_NOT_GIG: u16 = 0x000f;
const ALL_10_SPEED: u16 = 0x0003;
const NVM_SIZE_BASE: u32 = 6;
const NVM_CFG_DONE_PORT0: u32 = 0x0004_0000;
const PHY_ID_IGP: u32 = 0x02a8_0380;
const PHY_ID_M88E1111: u32 = 0x01410cc0;
const PHY_ID_BM_R2: u32 = 0x01410cb2;
const SMART_SPEED: u16 = 0x0080;
const IGP_PM_D0_LPLU: u16 = 0x0002;
const POEMB_D0_LPLU: u32 = E1000_PHY_CTRL_D0A_LPLU;
const POEMB_D3_LPLU: u32 = E1000_PHY_CTRL_NOND0A_LPLU;
const EECD_ADDR_BITS: u32 = 0x400;
const EECD_TYPE: u32 = 0x2000;
const EECD_AUPDEN: u32 = 0x0010_0000;
const EECD_FLUPD: u32 = 0x0008_0000;
const NVM_SIZE_EX_MASK: u32 = 0x7800;
const NVM_SIZE_EX_SHIFT: u32 = 11;
const NVM_INIT_CONTROL2: u16 = 0x0f;
const NVM_ID_LED_SETTINGS: u16 = 0x04;
const NVM_EEWR_START: u32 = 1;
const NVM_RW_ADDR_SHIFT: u32 = 2;
const NVM_RW_DATA_SHIFT: u32 = 16;
const EEWR_DONE: u32 = 2;
const MDIO_OWNERSHIP_TIMEOUT: usize = 10;
const FLASH_UPDATES: usize = 2000;
const FWSM_MODE_MASK: u32 = 0x0e;
const MNGM_MASK: u16 = 0x6000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhyType82571 {
    None,
    Igp2,
    M88,
    Bm,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhyParams82571 {
    pub address: u8,
    pub reset_delay_us: u32,
    pub kind: PhyType82571,
    pub id: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NvmParams82571 {
    pub config: E1000NvmConfig,
    pub flash: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MacParams82571 {
    pub media: E1000MediaType,
    pub mta_count: u16,
    pub rar_count: u16,
    pub has_fwsm: bool,
    pub arc_valid: bool,
    pub adaptive_ifs: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flow82571 {
    None,
    RxPause,
    TxPause,
    Full,
    Default,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SerdesState {
    Down,
    AutonegProgress,
    AutonegComplete,
    ForcedUp,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SerdesLink {
    pub state: Option<SerdesState>,
    pub has_link: bool,
}

pub trait PhyOps82571 {
    fn read_phy(&mut self, reg: u16) -> DevResult<u16>;
    fn write_phy(&mut self, reg: u16, value: u16) -> DevResult;
    fn generic_phy_id(&mut self) -> DevResult<(u32, u32)>;
    fn phy_has_link(&mut self) -> DevResult<bool>;
    fn setup_igp(&mut self) -> DevResult;
    fn setup_m88(&mut self) -> DevResult;
    fn setup_copper_generic(&mut self) -> DevResult;
    fn reset_phy(&mut self) -> DevResult;
    fn config_fc(&mut self) -> DevResult;
    fn check_reset_block(&mut self) -> bool;
    fn power_down_phy(&mut self) -> DevResult;
}
pub trait FamilyOps82571 {
    fn disable_pcie_master(&mut self) -> DevResult;
    fn auto_read_done(&mut self) -> DevResult;
    fn setup_link(&mut self) -> DevResult;
    fn init_rx_addrs(&mut self, count: u16) -> DevResult;
    fn clear_vfta(&mut self) -> DevResult;
    fn init_id_led(&mut self) -> DevResult;
    fn clear_base_counters(&mut self) -> DevResult;
    fn check_alt_mac(&mut self) -> DevResult;
    fn set_rar(&mut self, address: [u8; 6], index: u32) -> DevResult;
    fn enable_tx_filtering(&mut self) -> DevResult;
    fn config_collision_dist(&mut self) -> DevResult;
    fn setup_fiber_generic(&mut self) -> DevResult;
    fn setup_copper_generic(&mut self) -> DevResult;
    fn check_mng_mode(&mut self) -> bool;
}
pub trait NvmLock82571 {
    fn get_sw_semaphore(&mut self) -> DevResult;
    fn put_sw_semaphore(&mut self);
    fn acquire_nvm_generic(&mut self) -> DevResult;
    fn release_nvm_generic(&mut self);
}
pub trait NvmWriteOps82571: E1000NvmAccess {
    fn word_size(&self) -> u16;
    fn write_spi(&mut self, offset: u16, words: &[u16]) -> DevResult;
    fn write_eewr(&mut self, offset: u16, words: &[u16]) -> DevResult;
}

/// upstream: e1000_82571.c e1000_init_phy_params_82571()
pub fn init_phy_params_82571(
    mac: E1000MacType,
    media: E1000MediaType,
    phy_id: Option<u32>,
) -> DevResult<Option<PhyParams82571>> {
    if media != E1000MediaType::Copper {
        return Ok(None);
    }
    let (kind, expected) = match mac {
        E1000MacType::I82571 | E1000MacType::I82572 => (PhyType82571::Igp2, PHY_ID_IGP),
        E1000MacType::I82573 => (PhyType82571::M88, PHY_ID_M88E1111),
        E1000MacType::I82574 | E1000MacType::I82583 => (PhyType82571::Bm, PHY_ID_BM_R2),
        _ => return Err(DevError::Io),
    };
    let id = phy_id.ok_or(DevError::Io)?;
    if id != expected {
        return Err(DevError::Io);
    }
    Ok(Some(PhyParams82571 {
        address: 1,
        reset_delay_us: 100,
        kind,
        id,
    }))
}

/// upstream: e1000_82571.c e1000_init_nvm_params_82571()
pub fn init_nvm_params_82571<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    spi_large: bool,
    spi_small: bool,
) -> DevResult<NvmParams82571> {
    let mut eecd = io.read_register(E1000_EECD)?;
    let address_bits = if spi_large {
        16
    } else if spi_small {
        8
    } else if eecd & EECD_ADDR_BITS != 0 {
        16
    } else {
        8
    };
    let page_size = if address_bits == 16 { 32 } else { 8 };
    if matches!(
        mac,
        E1000MacType::I82573 | E1000MacType::I82574 | E1000MacType::I82583
    ) && ((eecd >> 15) & 3) == 3
    {
        eecd &= !EECD_AUPDEN;
        io.write_register(E1000_EECD, eecd)?;
        return Ok(NvmParams82571 {
            config: E1000NvmConfig {
                kind: E1000NvmType::Eerd,
                word_size: 2048,
                delay_usec: 1,
                opcode_bits: 8,
                address_bits,
                page_size,
            },
            flash: true,
        });
    }
    let mut size = ((eecd & NVM_SIZE_EX_MASK) >> NVM_SIZE_EX_SHIFT) + NVM_SIZE_BASE;
    if size > 14 {
        size = 14
    }
    let word_size = 1u16.checked_shl(size).ok_or(DevError::InvalidParam)?;
    Ok(NvmParams82571 {
        config: E1000NvmConfig {
            kind: if eecd & EECD_TYPE != 0 {
                E1000NvmType::Spi
            } else {
                E1000NvmType::Spi
            },
            word_size,
            delay_usec: 1,
            opcode_bits: 8,
            address_bits,
            page_size,
        },
        flash: false,
    })
}

/// upstream: e1000_82571.c e1000_init_mac_params_82571()
pub fn init_mac_params_82571<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    device_id: u16,
) -> DevResult<(MacParams82571, bool)> {
    let media = match device_id {
        0x105f | 0x107e | 0x10a5 => E1000MediaType::Fiber,
        0x1060 | 0x10d9 | 0x10da | 0x107f => E1000MediaType::InternalSerdes,
        _ => E1000MediaType::Copper,
    };
    let has_fwsm = matches!(
        mac,
        E1000MacType::I82573 | E1000MacType::I82571 | E1000MacType::I82572
    );
    let arc_valid = if mac == E1000MacType::I82573 {
        io.read_register(E1000_FWSM)? & FWSM_MODE_MASK != 0
    } else {
        false
    };
    let clear_smbi = if matches!(mac, E1000MacType::I82571 | E1000MacType::I82572) {
        let swsm2 = io.read_register(E1000_SWSM2)?;
        if swsm2 & E1000_SWSM2_LOCK == 0 {
            io.write_register(E1000_SWSM2, swsm2 | E1000_SWSM2_LOCK)?;
            true
        } else {
            false
        }
    } else {
        true
    };
    if clear_smbi {
        let swsm = io.read_register(E1000_SWSM)?;
        io.write_register(E1000_SWSM, swsm & !E1000_SWSM_SMBI)?;
    }
    Ok((
        MacParams82571 {
            media,
            mta_count: 128,
            rar_count: E1000_RAR_ENTRIES as u16,
            has_fwsm,
            arc_valid,
            adaptive_ifs: true,
        },
        clear_smbi,
    ))
}

/// upstream: e1000_82571.c e1000_init_function_pointers_82571()
pub const fn init_function_pointers_82571() -> (bool, bool, bool) {
    (true, true, true)
}

/// upstream: e1000_82571.c e1000_get_phy_id_82571()
pub fn get_phy_id_82571<P: PhyOps82571>(phy: &mut P, mac: E1000MacType) -> DevResult<(u32, u32)> {
    match mac {
        E1000MacType::I82571 | E1000MacType::I82572 => Ok((PHY_ID_IGP, 0)),
        E1000MacType::I82573 => phy.generic_phy_id(),
        E1000MacType::I82574 | E1000MacType::I82583 => {
            let id1 = phy.read_phy(2)?;
            core::hint::spin_loop();
            let id2 = phy.read_phy(3)?;
            Ok((
                (u32::from(id1) << 16) | u32::from(id2),
                u32::from(id2 & !0xf),
            ))
        }
        _ => Err(DevError::Io),
    }
}

/// upstream: e1000_82571.c e1000_get_hw_semaphore_82574()
pub fn get_hw_semaphore_82574<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    for _ in 0..MDIO_OWNERSHIP_TIMEOUT {
        let ext = io.read_register(E1000_EXTCNF_CTRL)? | E1000_EXTCNF_CTRL_MDIO_SW_OWNERSHIP;
        io.write_register(E1000_EXTCNF_CTRL, ext)?;
        if io.read_register(E1000_EXTCNF_CTRL)? & E1000_EXTCNF_CTRL_MDIO_SW_OWNERSHIP != 0 {
            return Ok(());
        }
        io.delay_us(2_000);
    }
    let ext = io.read_register(E1000_EXTCNF_CTRL)? & !E1000_EXTCNF_CTRL_MDIO_SW_OWNERSHIP;
    io.write_register(E1000_EXTCNF_CTRL, ext)?;
    Err(DevError::Io)
}

/// upstream: e1000_82571.c e1000_put_hw_semaphore_82574()
pub fn put_hw_semaphore_82574<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let ext = io.read_register(E1000_EXTCNF_CTRL)? & !E1000_EXTCNF_CTRL_MDIO_SW_OWNERSHIP;
    io.write_register(E1000_EXTCNF_CTRL, ext)
}

/// upstream: e1000_82571.c e1000_set_d0_lplu_state_82574()
pub fn set_d0_lplu_state_82574<I: E1000RegisterIo>(io: &mut I, active: bool) -> DevResult {
    let mut data = io.read_register(E1000_PHY_CTRL)?;
    if active {
        data |= POEMB_D0_LPLU
    } else {
        data &= !POEMB_D0_LPLU
    }
    io.write_register(E1000_PHY_CTRL, data)
}

/// upstream: e1000_82571.c e1000_set_d3_lplu_state_82574()
pub fn set_d3_lplu_state_82574<I: E1000RegisterIo>(
    io: &mut I,
    active: bool,
    advertised: u16,
) -> DevResult {
    let mut data = io.read_register(E1000_PHY_CTRL)?;
    if !active {
        data &= !POEMB_D3_LPLU
    } else if matches!(advertised, ALL_SPEED_DUPLEX | ALL_NOT_GIG | ALL_10_SPEED) {
        data |= POEMB_D3_LPLU
    }
    io.write_register(E1000_PHY_CTRL, data)
}

/// upstream: e1000_82571.c e1000_get_hw_semaphore_82571()
pub fn get_hw_semaphore_82571<I: E1000RegisterIo>(
    io: &mut I,
    word_size: u16,
    smb_counter: &mut u8,
) -> DevResult {
    let sw_timeout = if *smb_counter > 2 {
        1
    } else {
        usize::from(word_size) + 1
    };
    let fw_timeout = usize::from(word_size) + 1;
    let mut acquired = false;
    for _ in 0..sw_timeout {
        if io.read_register(E1000_SWSM)? & E1000_SWSM_SMBI == 0 {
            acquired = true;
            break;
        }
        io.delay_us(50);
    }
    if !acquired {
        *smb_counter = smb_counter.saturating_add(1);
    }
    for _ in 0..fw_timeout {
        let swsm = io.read_register(E1000_SWSM)?;
        io.write_register(E1000_SWSM, swsm | E1000_SWSM_SWESMBI)?;
        if io.read_register(E1000_SWSM)? & E1000_SWSM_SWESMBI != 0 {
            return Ok(());
        }
        io.delay_us(50);
    }
    let swsm = io.read_register(E1000_SWSM)? & !E1000_SWSM_SWESMBI;
    io.write_register(E1000_SWSM, swsm)?;
    Err(DevError::ResourceBusy)
}

/// upstream: e1000_82571.c e1000_acquire_nvm_82571()
pub fn acquire_nvm_82571<I: NvmLock82571>(io: &mut I, mac: E1000MacType) -> DevResult {
    io.get_sw_semaphore()?;
    if mac != E1000MacType::I82573 {
        if let Err(error) = io.acquire_nvm_generic() {
            io.put_sw_semaphore();
            return Err(error);
        }
    }
    Ok(())
}

/// upstream: e1000_82571.c e1000_release_nvm_82571()
pub fn release_nvm_82571<I: NvmLock82571>(io: &mut I) {
    io.release_nvm_generic();
    io.put_sw_semaphore();
}

/// upstream: e1000_82571.c e1000_write_nvm_82571()
pub fn write_nvm_82571<N: NvmWriteOps82571>(
    nvm: &mut N,
    mac: E1000MacType,
    offset: u16,
    words: &[u16],
) -> DevResult {
    match mac {
        E1000MacType::I82573 | E1000MacType::I82574 | E1000MacType::I82583 => {
            nvm.write_eewr(offset, words)
        }
        E1000MacType::I82571 | E1000MacType::I82572 => nvm.write_spi(offset, words),
        _ => Err(DevError::Io),
    }
}

/// upstream: e1000_82571.c e1000_update_nvm_checksum_82571()
pub fn update_nvm_checksum_82571<I: E1000RegisterIo, N: E1000NvmAccess>(
    io: &mut I,
    nvm: &mut N,
    flash: bool,
) -> DevResult {
    update_nvm_checksum_generic(nvm)?;
    if !flash {
        return Ok(());
    }
    for _ in 0..FLASH_UPDATES {
        io.delay_us(1_000);
        if io.read_register(E1000_EECD)? & EECD_FLUPD == 0 {
            break;
        }
    }
    if io.read_register(E1000_EECD)? & EECD_FLUPD != 0 {
        return Err(DevError::Io);
    }
    if io.read_register(E1000_FLOP)? & 0xff00 == 0xdb00 {
        io.write_register(E1000_HICR, 0x40)?;
        let _ = io.read_register(E1000_STATUS)?;
        io.write_register(E1000_HICR, 0x80)?;
    }
    let eecd = io.read_register(E1000_EECD)? | EECD_FLUPD;
    io.write_register(E1000_EECD, eecd)?;
    for _ in 0..FLASH_UPDATES {
        io.delay_us(1_000);
        if io.read_register(E1000_EECD)? & EECD_FLUPD == 0 {
            return Ok(());
        }
    }
    Err(DevError::Io)
}

/// upstream: e1000_82571.c e1000_validate_nvm_checksum_82571()
pub fn validate_nvm_checksum_82571<N: NvmWriteOps82571>(nvm: &mut N, flash: bool) -> DevResult {
    if flash {
        let _ = fix_nvm_checksum_82571(nvm, true);
    }
    validate_nvm_checksum_generic(nvm)
}

/// upstream: e1000_82571.c e1000_write_nvm_eewr_82571()
pub fn write_nvm_eewr_82571<I: E1000RegisterIo>(
    io: &mut I,
    offset: u16,
    words: &[u16],
    word_size: u16,
) -> DevResult {
    if words.is_empty() || offset >= word_size || words.len() > usize::from(word_size - offset) {
        return Err(DevError::InvalidParam);
    }
    for (index, data) in words.iter().enumerate() {
        poll_eerd_eewr(io)?;
        let eewr = (u32::from(*data) << NVM_RW_DATA_SHIFT)
            | (u32::from(offset + index as u16) << NVM_RW_ADDR_SHIFT)
            | NVM_EEWR_START;
        io.write_register(E1000_EEWR, eewr)?;
        poll_eerd_eewr(io)?;
    }
    Ok(())
}

fn poll_eerd_eewr<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    for _ in 0..100_000 {
        if io.read_register(E1000_EEWR)? & EEWR_DONE != 0 {
            return Ok(());
        }
        io.delay_us(5)
    }
    Err(DevError::Io)
}

/// upstream: e1000_82571.c e1000_get_cfg_done_82571()
pub fn get_cfg_done_82571<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    for _ in 0..100 {
        if io.read_register(E1000_EEMNGCTL)? & NVM_CFG_DONE_PORT0 != 0 {
            return Ok(());
        }
        io.delay_us(1_000)
    }
    Err(DevError::Io)
}

/// upstream: e1000_82571.c e1000_set_d0_lplu_state_82571()
pub fn set_d0_lplu_state_82571<P: PhyOps82571>(
    phy: &mut P,
    active: bool,
    smart_speed: super::phy::SmartSpeedMode,
) -> DevResult {
    const PWR_MGMT: u16 = 0x19;
    const PORT_CONFIG: u16 = 0x10;
    const D0_LPLU: u16 = IGP_PM_D0_LPLU;
    if active {
        let data = phy.read_phy(PWR_MGMT)? | D0_LPLU;
        phy.write_phy(PWR_MGMT, data)?;
        let data = phy.read_phy(PORT_CONFIG)? & !SMART_SPEED;
        phy.write_phy(PORT_CONFIG, data)
    } else {
        let data = phy.read_phy(PWR_MGMT)? & !D0_LPLU;
        phy.write_phy(PWR_MGMT, data)?;
        if smart_speed != super::phy::SmartSpeedMode::Default {
            let mut value = phy.read_phy(PORT_CONFIG)?;
            if smart_speed == super::phy::SmartSpeedMode::On {
                value |= SMART_SPEED
            } else {
                value &= !SMART_SPEED
            }
            phy.write_phy(PORT_CONFIG, value)?;
        }
        Ok(())
    }
}

/// upstream: e1000_82571.c e1000_reset_hw_82571()
pub fn reset_hw_82571<I: E1000RegisterIo, O: FamilyOps82571>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
    flash: bool,
    media: E1000MediaType,
    laa: &mut bool,
    address: [u8; 6],
    serdes: &mut SerdesLink,
) -> DevResult {
    let _ = ops.disable_pcie_master();
    io.write_register(E1000_IMC, u32::MAX)?;
    io.write_register(E1000_RCTL, 0)?;
    let tctl = io.read_register(E1000_TCTL)? & !E1000_TCTL_EN;
    io.write_register(E1000_TCTL, tctl)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(10_000);
    let has_sem = matches!(
        mac,
        E1000MacType::I82573 | E1000MacType::I82574 | E1000MacType::I82583
    );
    let sem_result = if has_sem {
        get_hw_semaphore_82574(io)
    } else {
        Ok(())
    };
    let ctrl = io.read_register(E1000_CTRL)?;
    io.write_register(E1000_CTRL, ctrl | E1000_CTRL_RST)?;
    if has_sem && sem_result.is_ok() {
        let _ = put_hw_semaphore_82574(io);
    }
    if flash {
        io.delay_us(10);
        let ext = io.read_register(E1000_CTRL_EXT)? | E1000_CTRL_EXT_EE_RST;
        io.write_register(E1000_CTRL_EXT, ext)?;
        let _ = io.read_register(E1000_STATUS)?;
    }
    ops.auto_read_done()?;
    match mac {
        E1000MacType::I82571 | E1000MacType::I82572 => {
            let eecd = io.read_register(E1000_EECD)? & !(E1000_EECD_REQ | E1000_EECD_GNT);
            io.write_register(E1000_EECD, eecd)?;
        }
        E1000MacType::I82573 | E1000MacType::I82574 | E1000MacType::I82583 => io.delay_us(25_000),
        _ => {}
    }
    io.write_register(E1000_IMC, u32::MAX)?;
    let _ = io.read_register(E1000_ICR)?;
    if mac == E1000MacType::I82571 {
        ops.check_alt_mac()?;
        *laa = true;
        ops.set_rar(address, 13)?;
    }
    let _ = tctl;
    if media == E1000MediaType::InternalSerdes {
        *serdes = SerdesLink {
            state: Some(SerdesState::Down),
            has_link: false,
        };
    }
    Ok(())
}

/// upstream: e1000_82571.c e1000_initialize_hw_bits_82571()
pub fn initialize_hw_bits_82571<I: E1000RegisterIo>(io: &mut I, mac: E1000MacType) -> DevResult {
    for q in 0..2 {
        let off = 0x3828 + q * 0x100;
        let reg = io.read_register(off)? | (1 << 22);
        io.write_register(off, reg)?;
    }
    let tarc0 = 0x3840;
    let mut reg = io.read_register(tarc0)? & !(0xf << 27);
    match mac {
        E1000MacType::I82571 | E1000MacType::I82572 => {
            reg |= (1 << 23) | (1 << 24) | (1 << 25) | (1 << 26)
        }
        E1000MacType::I82574 | E1000MacType::I82583 => reg |= 1 << 26,
        _ => {}
    }
    io.write_register(tarc0, reg)?;
    if matches!(mac, E1000MacType::I82571 | E1000MacType::I82572) {
        let mut t = io.read_register(0x3940)?;
        t &= !((1 << 29) | (1 << 30));
        t |= (1 << 22) | (1 << 24) | (1 << 25) | (1 << 26);
        if io.read_register(E1000_TCTL)? & E1000_TCTL_MULR != 0 {
            t &= !(1 << 28)
        } else {
            t |= 1 << 28
        }
        io.write_register(0x3940, t)?;
    }
    if matches!(
        mac,
        E1000MacType::I82573 | E1000MacType::I82574 | E1000MacType::I82583
    ) {
        let c = io.read_register(E1000_CTRL)? & !(1 << 29);
        io.write_register(E1000_CTRL, c)?;
        let e = io.read_register(E1000_CTRL_EXT)? & !(1 << 23) | (1 << 22);
        io.write_register(E1000_CTRL_EXT, e)?;
    }
    if mac == E1000MacType::I82571 {
        let v = io.read_register(0x100c)? | 1;
        io.write_register(0x100c, v)?;
        let e = io.read_register(E1000_CTRL_EXT)? & !0x1000;
        io.write_register(E1000_CTRL_EXT, e)?;
    }
    if matches!(mac, E1000MacType::I82571 | E1000MacType::I82572) {
        let r = io.read_register(0x5008)? | 0x300;
        io.write_register(0x5008, r)?;
        let ext = io.read_register(E1000_CTRL_EXT)? & !E1000_CTRL_EXT_DMA_DYN_CLK_EN;
        io.write_register(E1000_CTRL_EXT, ext)?;
    }
    if matches!(
        mac,
        E1000MacType::I82571 | E1000MacType::I82572 | E1000MacType::I82573
    ) {
        let rfctl =
            io.read_register(E1000_RFCTL)? | E1000_RFCTL_IPV6_EX_DIS | E1000_RFCTL_NEW_IPV6_EXT_DIS;
        io.write_register(E1000_RFCTL, rfctl)?;
    }
    if matches!(mac, E1000MacType::I82574 | E1000MacType::I82583) {
        let g = io.read_register(E1000_GCR)? | (1 << 22);
        io.write_register(E1000_GCR, g)?;
        let g2 = io.read_register(E1000_GCR2)? | 1;
        io.write_register(E1000_GCR2, g2)?;
    }
    Ok(())
}

/// upstream: e1000_82571.c e1000_init_hw_82571()
pub fn init_hw_82571<I: E1000RegisterIo, O: FamilyOps82571>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
    laa: bool,
    mta_count: u16,
    rar_count: u16,
) -> DevResult {
    initialize_hw_bits_82571(io, mac)?;
    let _ = ops.init_id_led();
    ops.clear_vfta()?;
    ops.init_rx_addrs(if laa {
        rar_count.saturating_sub(1)
    } else {
        rar_count
    })?;
    for i in 0..mta_count {
        io.write_register(E1000_MTA + u32::from(i) * 4, 0)?;
    }
    let link = ops.setup_link();
    let mut data = (io.read_register(0x3828)? & !E1000_TXDCTL_WTHRESH)
        | E1000_TXDCTL_FULL_TX_DESC_WB
        | E1000_TXDCTL_COUNT_DESC;
    io.write_register(0x3828, data)?;
    if mac == E1000MacType::I82573 {
        let _ = ops.enable_tx_filtering();
    }
    if matches!(mac, E1000MacType::I82574 | E1000MacType::I82583) {
        let g = io.read_register(E1000_GCR)? & !0x0800_0000;
        io.write_register(E1000_GCR, g)?;
    } else {
        data = (io.read_register(0x3928)? & !E1000_TXDCTL_WTHRESH)
            | E1000_TXDCTL_FULL_TX_DESC_WB
            | E1000_TXDCTL_COUNT_DESC;
        io.write_register(0x3928, data)?;
    }
    clear_hw_cntrs_82571(io, || ops.clear_base_counters())?;
    link
}

/// upstream: e1000_82571.c e1000_clear_vfta_82571()
pub fn clear_vfta_82571<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    vlan_id: u16,
) -> DevResult {
    let mut off = 0;
    let mut keep = 0u32;
    if matches!(
        mac,
        E1000MacType::I82573 | E1000MacType::I82574 | E1000MacType::I82583
    ) && vlan_id != 0
    {
        off = (u32::from(vlan_id) >> 5) & 0x7f;
        keep = 1u32 << (vlan_id & 0x1f);
    }
    for index in 0..128 {
        io.write_register(E1000_VFTA + index * 4, if index == off { keep } else { 0 })?;
        let _ = io.read_register(E1000_STATUS)?;
    }
    Ok(())
}

/// upstream: e1000_82571.c e1000_check_mng_mode_82574()
pub fn check_mng_mode_82574<N: E1000NvmAccess>(nvm: &mut N) -> bool {
    nvm.read_nvm_words(NVM_INIT_CONTROL2, 1)
        .ok()
        .and_then(|w| w.first().copied())
        .map(|d| d & MNGM_MASK != 0)
        .unwrap_or(false)
}

/// upstream: e1000_82571.c e1000_led_on_82574()
pub fn led_on_82574<I: E1000RegisterIo>(io: &mut I, mode2: u32) -> DevResult {
    let mut ctrl = mode2;
    if io.read_register(E1000_STATUS)? & E1000_STATUS_LU == 0 {
        for i in 0..4 {
            if (mode2 >> (i * 8)) & 0xff == 0x0e {
                ctrl |= E1000_LEDCTL_LED0_IVRT << (i * 8)
            }
        }
    }
    io.write_register(E1000_LEDCTL, ctrl)
}

/// upstream: e1000_82571.c e1000_check_phy_82574()
pub fn check_phy_82574<P: PhyOps82571>(phy: &mut P) -> bool {
    let receive = match phy.read_phy(0x15) {
        Ok(v) => v,
        Err(_) => return false,
    };
    if receive != u16::MAX {
        return false;
    }
    phy.read_phy(0x0a)
        .map(|status| status & 0x00ff == 0x00ff)
        .unwrap_or(false)
}

/// upstream: e1000_82571.c e1000_setup_link_82571()
pub fn setup_link_82571<O: FamilyOps82571>(
    ops: &mut O,
    mac: E1000MacType,
    requested: &mut Flow82571,
) -> DevResult {
    if matches!(
        mac,
        E1000MacType::I82573 | E1000MacType::I82574 | E1000MacType::I82583
    ) && *requested == Flow82571::Default
    {
        *requested = Flow82571::Full;
    }
    ops.setup_link()
}

/// upstream: e1000_82571.c e1000_setup_copper_link_82571()
pub fn setup_copper_link_82571<I: E1000RegisterIo, P: PhyOps82571>(
    io: &mut I,
    phy: &mut P,
    kind: PhyType82571,
) -> DevResult {
    let mut ctrl = io.read_register(E1000_CTRL)? | E1000_CTRL_SLU;
    ctrl &= !(E1000_CTRL_FRCSPD | E1000_CTRL_FRCDPX);
    io.write_register(E1000_CTRL, ctrl)?;
    match kind {
        PhyType82571::M88 | PhyType82571::Bm => phy.setup_m88()?,
        PhyType82571::Igp2 => phy.setup_igp()?,
        PhyType82571::None => return Err(DevError::Io),
    }
    phy.setup_copper_generic()
}

/// upstream: e1000_82571.c e1000_setup_fiber_serdes_link_82571()
pub fn setup_fiber_serdes_link_82571<I: E1000RegisterIo, O: FamilyOps82571>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
) -> DevResult {
    if matches!(mac, E1000MacType::I82571 | E1000MacType::I82572) {
        io.write_register(E1000_SCTL, 0x0400)?;
    }
    ops.setup_fiber_generic()
}

/// upstream: e1000_82571.c e1000_check_for_serdes_link_82571()
pub fn check_for_serdes_link_82571<I: E1000RegisterIo, P: PhyOps82571>(
    io: &mut I,
    phy: &mut P,
    link: &mut SerdesLink,
    txcw: u32,
) -> DevResult {
    const SYNCH: u32 = 0x4000_0000;
    const INVALID: u32 = 0x0800_0000;
    const C: u32 = 0x2000_0000;
    const ANE: u32 = 0x8000_0000;
    const RETRIES: usize = 5;
    let ctrl = io.read_register(E1000_CTRL)?;
    let status = io.read_register(E1000_STATUS)?;
    let _ = io.read_register(E1000_RXCW)?;
    io.delay_us(10);
    let rxcw = io.read_register(E1000_RXCW)?;
    if rxcw & SYNCH != 0 && rxcw & INVALID == 0 {
        match link.state.unwrap_or(SerdesState::Down) {
            SerdesState::AutonegComplete => {
                if status & E1000_STATUS_LU == 0 {
                    link.state = Some(SerdesState::AutonegProgress);
                    link.has_link = false
                } else {
                    link.has_link = true
                }
            }
            SerdesState::ForcedUp => {
                if rxcw & C != 0 {
                    io.write_register(E1000_TXCW, txcw)?;
                    io.write_register(E1000_CTRL, ctrl & !E1000_CTRL_SLU)?;
                    link.state = Some(SerdesState::AutonegProgress);
                    link.has_link = false
                } else {
                    link.has_link = true
                }
            }
            SerdesState::AutonegProgress => {
                if rxcw & C != 0 {
                    if status & E1000_STATUS_LU != 0 {
                        link.state = Some(SerdesState::AutonegComplete);
                        link.has_link = true
                    } else {
                        link.state = Some(SerdesState::Down);
                        link.has_link = false
                    }
                } else {
                    io.write_register(E1000_TXCW, txcw & !ANE)?;
                    io.write_register(E1000_CTRL, ctrl | E1000_CTRL_SLU | E1000_CTRL_FD)?;
                    phy.config_fc()?;
                    link.state = Some(SerdesState::ForcedUp);
                    link.has_link = true
                }
            }
            SerdesState::Down => {
                io.write_register(E1000_TXCW, txcw)?;
                io.write_register(E1000_CTRL, ctrl & !E1000_CTRL_SLU)?;
                link.state = Some(SerdesState::AutonegProgress);
                link.has_link = false;
            }
        }
    } else if rxcw & SYNCH == 0 {
        link.has_link = false;
        link.state = Some(SerdesState::Down)
    } else {
        let mut i = 0;
        while i < RETRIES {
            io.delay_us(10);
            let sample = io.read_register(E1000_RXCW)?;
            if sample & SYNCH != 0 && sample & C != 0 {
                i += 1;
                continue;
            }
            if sample & INVALID != 0 {
                link.has_link = false;
                link.state = Some(SerdesState::Down);
                break;
            }
            i += 1;
        }
        if i == RETRIES {
            let value = io.read_register(E1000_TXCW)? | ANE;
            io.write_register(E1000_TXCW, value)?;
            link.state = Some(SerdesState::AutonegProgress);
            link.has_link = false;
        }
    }
    Ok(())
}

/// upstream: e1000_82571.c e1000_valid_led_default_82571()
pub fn valid_led_default_82571<N: E1000NvmAccess>(
    nvm: &mut N,
    mac: E1000MacType,
) -> DevResult<u16> {
    let mut data = nvm
        .read_nvm_words(NVM_ID_LED_SETTINGS, 1)?
        .first()
        .copied()
        .ok_or(DevError::Io)?;
    if matches!(
        mac,
        E1000MacType::I82573 | E1000MacType::I82574 | E1000MacType::I82583
    ) {
        if data == 0xf746 {
            data = 0x1811
        }
    } else if data == 0 || data == u16::MAX {
        data = 0x8911
    }
    Ok(data)
}

/// upstream: e1000_82571.c e1000_get_laa_state_82571()
pub const fn get_laa_state_82571(mac: E1000MacType, laa: bool) -> bool {
    matches!(mac, E1000MacType::I82571) && laa
}

/// upstream: e1000_82571.c e1000_set_laa_state_82571()
pub fn set_laa_state_82571<O: FamilyOps82571>(
    ops: &mut O,
    mac: E1000MacType,
    laa: &mut bool,
    state: bool,
    address: [u8; 6],
    rar_count: u16,
) -> DevResult {
    if mac != E1000MacType::I82571 {
        return Ok(());
    }
    *laa = state;
    if state {
        ops.set_rar(address, u32::from(rar_count - 1))?;
    }
    Ok(())
}

/// upstream: e1000_82571.c e1000_fix_nvm_checksum_82571()
pub fn fix_nvm_checksum_82571<N: NvmWriteOps82571>(nvm: &mut N, flash: bool) -> DevResult {
    if !flash {
        return Ok(());
    }
    let flag = nvm
        .read_nvm_words(0x10, 1)?
        .first()
        .copied()
        .ok_or(DevError::Io)?;
    if flag & 0x10 == 0 {
        let mut data = nvm
            .read_nvm_words(0x23, 1)?
            .first()
            .copied()
            .ok_or(DevError::Io)?;
        if data & 0x8000 == 0 {
            data |= 0x8000;
            nvm.write_nvm_words(0x23, &[data])?;
            update_nvm_checksum_generic(nvm)?;
        }
    }
    Ok(())
}

/// upstream: e1000_82571.c e1000_read_mac_addr_82571()
pub fn read_mac_addr_82571<O: FamilyOps82571, F: FnMut() -> DevResult<[u8; 6]>>(
    ops: &mut O,
    mac: E1000MacType,
    mut read_generic: F,
) -> DevResult<[u8; 6]> {
    if mac == E1000MacType::I82571 {
        ops.check_alt_mac()?;
    }
    read_generic()
}

/// upstream: e1000_82571.c e1000_power_down_phy_copper_82571()
pub fn power_down_phy_copper_82571<P: PhyOps82571, O: FamilyOps82571>(
    phy: &mut P,
    ops: &mut O,
) -> DevResult {
    if !ops.check_mng_mode() && !phy.check_reset_block() {
        phy.power_down_phy()?;
    }
    Ok(())
}

/// upstream: e1000_82571.c e1000_clear_hw_cntrs_82571()
pub fn clear_hw_cntrs_82571<I: E1000RegisterIo, B: FnMut() -> DevResult>(
    io: &mut I,
    mut clear_base: B,
) -> DevResult {
    clear_base()?;
    for r in [
        0x405c, 0x4060, 0x4064, 0x4068, 0x406c, 0x4070, 0x40d8, 0x40dc, 0x40e0, 0x40e4, 0x40e8,
        0x40ec, 0x4004, 0x400c, 0x4034, 0x403c, 0x40f8, 0x40fc, 0x40b4, 0x40b8, 0x40bc, 0x1400,
        0x4104, 0x4108, 0x40cc, 0x40d0, 0x40d4, 0x4120,
    ] {
        let _ = io.read_register(r)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Io {
        regs: alloc::vec::Vec<(u32, u32)>,
        writes: alloc::vec::Vec<(u32, u32)>,
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
                *x = v;
            } else {
                self.regs.push((r, v));
            }
            Ok(())
        }
        fn delay_us(&mut self, _: u32) {}
        fn invalid_tail_write(&mut self, _: &'static str) {}
    }
    struct Nvm;
    impl E1000NvmAccess for Nvm {
        fn read_nvm_words(&mut self, _: u16, _: u16) -> DevResult<alloc::vec::Vec<u16>> {
            Ok(alloc::vec![MNGM_MASK])
        }
        fn write_nvm_words(&mut self, _: u16, _: &[u16]) -> DevResult {
            Ok(())
        }
    }
    #[test]
    fn nvm_geometry_flash_update_and_link_power_policy_match_family() {
        let mut io = Io::default();
        io.regs.push((E1000_EECD, 0x7800 | EECD_ADDR_BITS));
        let nvm = init_nvm_params_82571(&mut io, E1000MacType::I82571, false, false).unwrap();
        assert_eq!(nvm.config.word_size, 0x4000);
        assert_eq!(nvm.config.address_bits, 16);
        assert!(!nvm.flash);

        let mut io = Io::default();
        io.regs.push((E1000_EECD, (3 << 15) | EECD_AUPDEN));
        let nvm = init_nvm_params_82571(&mut io, E1000MacType::I82573, false, false).unwrap();
        assert!(nvm.flash);
        assert_eq!(nvm.config.word_size, 2048);
        assert!(
            io.writes
                .iter()
                .any(|(r, v)| *r == E1000_EECD && v & EECD_AUPDEN == 0)
        );

        let mut io = Io::default();
        set_d3_lplu_state_82574(&mut io, true, ALL_SPEED_DUPLEX).unwrap();
        assert!(io.writes[0].1 & POEMB_D3_LPLU != 0);
        set_d3_lplu_state_82574(&mut io, false, 0).unwrap();
        assert!(io.writes.last().unwrap().1 & POEMB_D3_LPLU == 0);

        let (params, clear) =
            init_mac_params_82571(&mut Io::default(), E1000MacType::I82571, 0x105f).unwrap();
        assert_eq!(params.media, E1000MediaType::Fiber);
        assert!(clear);
        assert!(check_mng_mode_82574(&mut Nvm));
    }
}
