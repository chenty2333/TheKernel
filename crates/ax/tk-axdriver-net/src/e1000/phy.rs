//! Shared Intel e1000 PHY operations.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_phy.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{
    mac::{E1000PhyRegisterIo, FlowControlMode, config_collision_dist_generic},
    osdep::E1000RegisterIo,
    registers::*,
};

const PHY_ID1: u8 = 0x02;
const PHY_ID2: u8 = 0x03;
const PHY_REVISION_MASK: u16 = 0x000f;
const M88E1000_PHY_GEN_CONTROL: u8 = 0x1b;
const E1000_BLK_PHY_RESET: u8 = 12;
const MAX_PHY_REG_ADDRESS: u32 = 0x1f;
const MDIC_DATA_MASK: u32 = 0xffff;
const NWAY_AR_10T_HD_CAPS: u16 = 0x0020;
const NWAY_AR_10T_FD_CAPS: u16 = 0x0040;
const NWAY_AR_100TX_HD_CAPS: u16 = 0x0080;
const NWAY_AR_100TX_FD_CAPS: u16 = 0x0100;
const CR_1000T_HD_CAPS: u16 = 0x0100;
const CR_1000T_FD_CAPS: u16 = 0x0200;
const ADVERTISE_10_HALF: u16 = 0x0001;
const ADVERTISE_10_FULL: u16 = 0x0002;
const ADVERTISE_100_HALF: u16 = 0x0004;
const ADVERTISE_100_FULL: u16 = 0x0008;
const ADVERTISE_1000_HALF: u16 = 0x0010;
const ADVERTISE_1000_FULL: u16 = 0x0020;
const ALL_SPEED_DUPLEX: u16 = ADVERTISE_10_HALF
    | ADVERTISE_10_FULL
    | ADVERTISE_100_HALF
    | ADVERTISE_100_FULL
    | ADVERTISE_1000_HALF
    | ADVERTISE_1000_FULL;
const ALL_NOT_GIG: u16 =
    ADVERTISE_10_HALF | ADVERTISE_10_FULL | ADVERTISE_100_HALF | ADVERTISE_100_FULL;
const ALL_10_SPEED: u16 = ADVERTISE_10_HALF | ADVERTISE_10_FULL;
const MII_AUTONEG_ADV: u8 = 0x04;
const MII_1000T_CTRL: u8 = 0x09;
const PHY_CONTROL: u8 = 0x00;
const MII_CR_AUTO_NEG_EN: u16 = 0x1000;
const MII_CR_RESTART_AUTO_NEG: u16 = 0x0200;
const MII_CR_FULL_DUPLEX: u16 = 0x0100;
const MII_CR_SPEED_1000: u16 = 0x0040;
const MII_CR_SPEED_100: u16 = 0x2000;
const MII_CR_RESET: u16 = 0x8000;
const MII_CR_POWER_DOWN: u16 = 0x0800;
const ALL_HALF_DUPLEX: u16 = ADVERTISE_10_HALF | ADVERTISE_100_HALF;
const ALL_100_SPEED: u16 = ADVERTISE_100_HALF | ADVERTISE_100_FULL;
const CR_1000T_MS_ENABLE: u16 = 0x1000;
const CR_1000T_MS_VALUE: u16 = 0x0800;
const PHY_STATUS: u8 = 0x01;
const PHY_AUTO_NEG_LIMIT: usize = 45;
const MII_SR_LINK_STATUS: u16 = 0x0004;
const MII_SR_AUTONEG_COMPLETE: u16 = 0x0020;
const M88E1000_PHY_SPEC_STATUS: u8 = 0x11;
const M88E1000_PSSR_MDIX: u16 = 0x0040;
const M88E1000_PSSR_SPEED: u16 = 0xc000;
const M88E1000_PSSR_1000MBS: u16 = 0x8000;
const IGP01E1000_PSSR_MDIX: u16 = 0x0800;
const PHY_1000T_STATUS: u32 = 0x0a;
const SR_1000T_REMOTE_RX_STATUS: u16 = 0x1000;
const SR_1000T_LOCAL_RX_STATUS: u16 = 0x2000;
const IFE_PSC_AUTO_POLARITY_DISABLE: u16 = 0x0010;
const IFE_PMC_MDIX_STATUS: u16 = 0x0020;
const M88E1000_PSSR_REV_POLARITY: u16 = 0x0002;
const M88E1000_PSSR_DOWNSHIFT: u16 = 0x0020;
const M88E1000_PSSR_CABLE_LENGTH: u16 = 0x0380;
const M88E1000_PSSR_CABLE_LENGTH_SHIFT: u32 = 7;
const IGP01E1000_PHY_LINK_HEALTH: u8 = 0x13;
const IGP01E1000_PLHR_SS_DOWNGRADE: u16 = 0x8000;
const CABLE_LENGTH_UNDEFINED: u16 = 0xff;
const IGP02E1000_AGC_LENGTH_SHIFT: u32 = 9;
const IGP02E1000_AGC_LENGTH_MASK: u16 = 0x7f;
const IGP02E1000_AGC_RANGE: u16 = 15;
const IGP02E1000_PHY_AGC_REGS: [u32; 4] = [0x11b1, 0x12b1, 0x14b1, 0x18b1];
const I210_PHY_ID: u32 = 0x0141_0c00;
const M88E1543_PHY_ID: u32 = 0x0141_0ea0;
const M88E1512_PHY_ID: u32 = 0x0141_0dd0;
const M88E1340M_PHY_ID: u32 = 0x0141_0df0;
const I347AT4_PHY_ID: u32 = 0x0141_0dc0;
const M88E1112_PHY_ID: u32 = 0x0141_0c90;
const I347AT4_PCDL: u32 = 0x10;
const I347AT4_PCDC: u32 = 0x15;
const I347AT4_PAGE_SELECT: u32 = 0x16;
const I347AT4_PCDC_CABLE_LENGTH_UNIT: u16 = 0x0400;
const M88E1112_VCT_DSP_DISTANCE: u32 = 0x1a;
const GS40G_PAGE_SHIFT: u32 = 16;
const IGP01E1000_PHY_PORT_STATUS: u32 = 0x11;
const IGP01E1000_PHY_PCS_INIT_REG: u32 = 0x00b4;
const IGP01E1000_PSSR_SPEED_MASK: u16 = 0xc000;
const IGP01E1000_PSSR_SPEED_1000MBPS: u16 = 0xc000;
const IGP01E1000_PHY_POLARITY_MASK: u16 = 0x0078;
const IGP01E1000_PSSR_POLARITY_REVERSED: u16 = 0x0002;
const IFE_PHY_EXTENDED_STATUS_CONTROL: u8 = 0x10;
const IFE_PHY_SPECIAL_CONTROL: u8 = 0x11;
const IFE_PESC_POLARITY_REVERSED: u16 = 0x0100;
const IFE_PSC_FORCE_POLARITY: u16 = 0x0020;
const IFE_PHY_MDIX_CONTROL: u8 = 0x1c;
const IFE_PMC_FORCE_MDIX: u16 = 0x0040;
const IFE_PMC_AUTO_MDIX: u16 = 0x0080;
const I82577_PHY_STATUS_2: u8 = 26;
const I82577_PHY_STATUS2_REV_POLARITY: u16 = 0x0400;
const I82577_CFG_REG: u8 = 22;
const I82577_CFG_ASSERT_CRS_ON_TX: u16 = 1 << 15;
const I82577_CFG_ENABLE_DOWNSHIFT: u16 = 3 << 10;
const I82577_PHY_CTRL_2: u8 = 18;
const I82577_PHY_CTRL2_MANUAL_MDIX: u16 = 0x0200;
const I82577_PHY_CTRL2_AUTO_MDI_MDIX: u16 = 0x0400;
const I82577_PHY_CTRL2_MDIX_CFG_MASK: u16 = 0x0600;
const M88E1000_PHY_SPEC_CTRL: u8 = 0x10;
const M88E1000_EXT_PHY_SPEC_CTRL: u8 = 0x14;
const M88E1000_PSCR_POLARITY_REVERSAL: u16 = 0x0002;
const M88E1000_PSCR_MDI_MANUAL_MODE: u16 = 0x0000;
const M88E1000_PSCR_MDIX_MANUAL_MODE: u16 = 0x0020;
const M88E1000_PSCR_AUTO_X_1000T: u16 = 0x0040;
const M88E1000_PSCR_AUTO_X_MODE: u16 = 0x0060;
const M88E1000_PSCR_ASSERT_CRS_ON_TX: u16 = 0x0800;
const M88E1000_EPSCR_MASTER_DOWNSHIFT_MASK: u16 = 0x0c00;
const M88E1000_EPSCR_MASTER_DOWNSHIFT_1X: u16 = 0;
const M88E1000_EPSCR_SLAVE_DOWNSHIFT_MASK: u16 = 0x0300;
const M88E1000_EPSCR_SLAVE_DOWNSHIFT_1X: u16 = 0x0100;
const M88E1000_EPSCR_TX_CLK_25: u16 = 0x0070;
const M88EC018_EPSCR_DOWNSHIFT_COUNTER_MASK: u16 = 0x0e00;
const M88EC018_EPSCR_DOWNSHIFT_COUNTER_5X: u16 = 0x0800;
const I82578_EPSCR_DOWNSHIFT_ENABLE: u16 = 0x0020;
const I82578_EPSCR_DOWNSHIFT_COUNTER_MASK: u16 = 0x001c;
const BME1000_PSCR_ENABLE_DOWNSHIFT: u16 = 0x0800;
const BME1000_E_PHY_ID_R2: u32 = 0x0141_0cb1;
const M88E1111_I_PHY_ID: u32 = 0x0141_0cc0;
const I347AT4_E_PHY_ID: u32 = 0x0141_0dc0;
const M88E1340M_E_PHY_ID: u32 = 0x0141_0df0;
const M88E1112_E_PHY_ID: u32 = 0x0141_0c90;
const I210_I_PHY_ID: u32 = 0x0141_0c00;
const M88E1543_E_PHY_ID: u32 = 0x0141_0ea0;
const M88E1512_E_PHY_ID: u32 = 0x0141_0dd0;
const M88E1000_PHY_PAGE_SELECT: u8 = 0x1d;
const I347AT4_PSCR_DOWNSHIFT_ENABLE: u16 = 0x0800;
const I347AT4_PSCR_DOWNSHIFT_MASK: u16 = 0x7000;
const I347AT4_PSCR_DOWNSHIFT_6X: u16 = 0x5000;
const IGP01E1000_PHY_PORT_CONFIG: u8 = 0x10;
const IGP01E1000_PHY_PORT_CTRL: u8 = 0x12;
const IGP02E1000_PHY_POWER_MGMT: u8 = 0x19;
const IGP01E1000_PSCR_AUTO_MDIX: u16 = 0x1000;
const IGP01E1000_PSCR_FORCE_MDI_MDIX: u16 = 0x2000;
const IGP01E1000_PSCFR_SMART_SPEED: u16 = 0x0080;
const IGP02E1000_PM_D3_LPLU: u16 = 0x0004;
const PHY_FORCE_LIMIT: u32 = 20;
const IGP01E1000_PHY_PAGE_SELECT: u32 = 0x1f;
const MAX_PHY_MULTI_PAGE_REG: u32 = 0x0f;
const KMRNCTRLSTA_OFFSET: u32 = 0x001f_0000;
const KMRNCTRLSTA_OFFSET_SHIFT: u32 = 16;
const KMRNCTRLSTA_REN: u32 = 0x0020_0000;
const NWAY_AR_PAUSE: u16 = 0x0400;
const NWAY_AR_ASM_DIR: u16 = 0x0800;
const I2CCMD_TIMEOUT: u32 = E1000_I2CCMD_PHY_TIMEOUT;
const SFP_DIAG_BASE: u16 = 0x0100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E1000PhyCallback {
    NullOps,
    NullSetPage,
    NullReadReg,
    NullRelease,
    NullLpluState,
    NullWriteReg,
    ReadI2cByteNull,
    WriteI2cByteNull,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct E1000PhyOps {
    pub init_params: E1000PhyCallback,
    pub acquire: E1000PhyCallback,
    pub check_polarity: E1000PhyCallback,
    pub check_reset_block: E1000PhyCallback,
    pub commit: E1000PhyCallback,
    pub force_speed_duplex: E1000PhyCallback,
    pub get_cfg_done: E1000PhyCallback,
    pub get_cable_length: E1000PhyCallback,
    pub get_info: E1000PhyCallback,
    pub set_page: E1000PhyCallback,
    pub read_reg: E1000PhyCallback,
    pub read_reg_locked: E1000PhyCallback,
    pub read_reg_page: E1000PhyCallback,
    pub release: E1000PhyCallback,
    pub reset: E1000PhyCallback,
    pub set_d0_lplu_state: E1000PhyCallback,
    pub set_d3_lplu_state: E1000PhyCallback,
    pub write_reg: E1000PhyCallback,
    pub write_reg_locked: E1000PhyCallback,
    pub write_reg_page: E1000PhyCallback,
    pub power_up: E1000PhyCallback,
    pub power_down: E1000PhyCallback,
    pub read_i2c_byte: E1000PhyCallback,
    pub write_i2c_byte: E1000PhyCallback,
    pub cfg_on_link_up: E1000PhyCallback,
}

/// upstream: e1000_phy.c e1000_init_phy_ops_generic()
pub const fn init_phy_ops_generic() -> E1000PhyOps {
    use E1000PhyCallback::*;
    E1000PhyOps {
        init_params: NullOps,
        acquire: NullOps,
        check_polarity: NullOps,
        check_reset_block: NullOps,
        commit: NullOps,
        force_speed_duplex: NullOps,
        get_cfg_done: NullOps,
        get_cable_length: NullOps,
        get_info: NullOps,
        set_page: NullSetPage,
        read_reg: NullReadReg,
        read_reg_locked: NullReadReg,
        read_reg_page: NullReadReg,
        release: NullRelease,
        reset: NullOps,
        set_d0_lplu_state: NullLpluState,
        set_d3_lplu_state: NullLpluState,
        write_reg: NullWriteReg,
        write_reg_locked: NullWriteReg,
        write_reg_page: NullWriteReg,
        power_up: NullRelease,
        power_down: NullRelease,
        read_i2c_byte: ReadI2cByteNull,
        write_i2c_byte: WriteI2cByteNull,
        cfg_on_link_up: NullOps,
    }
}

/// upstream: e1000_phy.c e1000_null_set_page()
pub const fn null_set_page() -> DevResult {
    Ok(())
}

/// upstream: e1000_phy.c e1000_null_read_reg()
pub const fn null_read_reg() -> DevResult<u16> {
    Ok(0)
}

/// upstream: e1000_phy.c e1000_null_phy_generic()
pub const fn null_phy_generic() {}

/// upstream: e1000_phy.c e1000_null_lplu_state()
pub const fn null_lplu_state(_active: bool) -> DevResult {
    Ok(())
}

/// upstream: e1000_phy.c e1000_null_write_reg()
pub const fn null_write_reg(_offset: u32, _data: u16) -> DevResult {
    Ok(())
}

/// upstream: e1000_phy.c e1000_read_i2c_byte_null()
pub const fn read_i2c_byte_null(_byte_offset: u8, _device_address: u8) -> DevResult<u8> {
    Ok(0)
}

/// upstream: e1000_phy.c e1000_write_i2c_byte_null()
pub const fn write_i2c_byte_null(_byte_offset: u8, _device_address: u8, _data: u8) -> DevResult {
    Ok(())
}

/// upstream: e1000_phy.c e1000_check_reset_block_generic()
pub fn check_reset_block_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult<bool> {
    Ok(io.read_register(E1000_MANC)? & E1000_MANC_BLK_PHY_RST_ON_IDE != 0)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PhyIdentity {
    pub id: u32,
    pub revision: u32,
}

/// upstream: e1000_phy.c e1000_get_phy_id()
pub fn get_phy_id<I: E1000PhyRegisterIo>(
    io: &mut I,
    read_callback_installed: bool,
) -> DevResult<PhyIdentity> {
    if !read_callback_installed {
        return Ok(PhyIdentity::default());
    }
    let mut identity = PhyIdentity::default();
    for _ in 0..2 {
        let id1 = io.read_phy_register(PHY_ID1)?;
        identity.id = u32::from(id1) << 16;
        io.delay_us(20);
        let id2 = io.read_phy_register(PHY_ID2)?;
        identity.id |= u32::from(id2 & PHY_REVISION_MASK);
        identity.revision = u32::from(id2 & !PHY_REVISION_MASK);
        if identity.id != 0 && identity.id != u32::from(PHY_REVISION_MASK) {
            return Ok(identity);
        }
    }
    Ok(identity)
}

/// upstream: e1000_phy.c e1000_phy_reset_dsp_generic()
pub fn phy_reset_dsp_generic<I: E1000PhyRegisterIo>(
    io: &mut I,
    write_callback_installed: bool,
) -> DevResult {
    if !write_callback_installed {
        return Ok(());
    }
    io.write_phy_register(M88E1000_PHY_GEN_CONTROL, 0x00c1)?;
    io.write_phy_register(M88E1000_PHY_GEN_CONTROL, 0)
}

/// upstream: e1000_phy.c e1000_disable_phy_retry_mechanism()
pub fn disable_phy_retry_mechanism(current: &mut u32) -> u32 {
    let original = *current;
    *current = 0;
    original
}

/// upstream: e1000_phy.c e1000_enable_phy_retry_mechanism()
pub fn enable_phy_retry_mechanism(current: &mut u32, original: u32) {
    *current = original;
}

/// upstream: e1000_phy.c e1000_read_phy_reg_mdic()
pub fn read_phy_reg_mdic<I: E1000RegisterIo>(
    io: &mut I,
    phy_address: u8,
    retry_count: u32,
    pch2lan: bool,
    offset: u32,
) -> DevResult<u16> {
    if offset > MAX_PHY_REG_ADDRESS {
        return Err(DevError::InvalidParam);
    }
    for retry in 0..=retry_count {
        let command = (offset << E1000_MDIC_REG_SHIFT)
            | (u32::from(phy_address) << E1000_MDIC_PHY_SHIFT)
            | E1000_MDIC_OP_READ;
        io.write_register(E1000_MDIC, command)?;
        let mut mdic = 0;
        for _ in 0..(E1000_GEN_POLL_TIMEOUT * 3) {
            io.delay_us(50);
            mdic = io.read_register(E1000_MDIC)?;
            if mdic & E1000_MDIC_READY != 0 {
                break;
            }
        }
        let success = mdic & E1000_MDIC_READY != 0
            && mdic & E1000_MDIC_ERROR == 0
            && ((mdic & E1000_MDIC_REG_MASK) >> E1000_MDIC_REG_SHIFT) == offset;
        if pch2lan {
            io.delay_us(100);
        }
        if success {
            return Ok((mdic & MDIC_DATA_MASK) as u16);
        }
        if retry != retry_count {
            io.delay_us(10_000);
        }
    }
    Err(DevError::Io)
}

/// upstream: e1000_phy.c e1000_write_phy_reg_mdic()
pub fn write_phy_reg_mdic<I: E1000RegisterIo>(
    io: &mut I,
    phy_address: u8,
    retry_count: u32,
    pch2lan: bool,
    offset: u32,
    data: u16,
) -> DevResult {
    if offset > MAX_PHY_REG_ADDRESS {
        return Err(DevError::InvalidParam);
    }
    for retry in 0..=retry_count {
        let command = u32::from(data)
            | (offset << E1000_MDIC_REG_SHIFT)
            | (u32::from(phy_address) << E1000_MDIC_PHY_SHIFT)
            | E1000_MDIC_OP_WRITE;
        io.write_register(E1000_MDIC, command)?;
        let mut mdic = 0;
        for _ in 0..(E1000_GEN_POLL_TIMEOUT * 3) {
            io.delay_us(50);
            mdic = io.read_register(E1000_MDIC)?;
            if mdic & E1000_MDIC_READY != 0 {
                break;
            }
        }
        let success = mdic & E1000_MDIC_READY != 0
            && mdic & E1000_MDIC_ERROR == 0
            && ((mdic & E1000_MDIC_REG_MASK) >> E1000_MDIC_REG_SHIFT) == offset;
        if pch2lan {
            io.delay_us(100);
        }
        if success {
            return Ok(());
        }
        if retry != retry_count {
            io.delay_us(10_000);
        }
    }
    Err(DevError::Io)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutonegConfig {
    pub advertised: u16,
    pub mask: u16,
    pub flow_control: FlowControlMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MasterSlaveMode {
    Auto,
    ForceMaster,
    ForceSlave,
}

/// upstream: e1000_phy.c e1000_set_master_slave_mode()
pub fn set_master_slave_mode<I: E1000PhyRegisterIo>(
    io: &mut I,
    requested: MasterSlaveMode,
) -> DevResult<MasterSlaveMode> {
    let mut control = io.read_phy_register(MII_1000T_CTRL)?;
    let original = if control & CR_1000T_MS_ENABLE == 0 {
        MasterSlaveMode::Auto
    } else if control & CR_1000T_MS_VALUE != 0 {
        MasterSlaveMode::ForceMaster
    } else {
        MasterSlaveMode::ForceSlave
    };
    match requested {
        MasterSlaveMode::ForceMaster => control |= CR_1000T_MS_ENABLE | CR_1000T_MS_VALUE,
        MasterSlaveMode::ForceSlave => {
            control |= CR_1000T_MS_ENABLE;
            control &= !CR_1000T_MS_VALUE;
        }
        MasterSlaveMode::Auto => control &= !CR_1000T_MS_ENABLE,
    }
    io.write_phy_register(MII_1000T_CTRL, control)?;
    Ok(original)
}

/// upstream: e1000_phy.c e1000_copper_link_setup_82577()
pub fn copper_link_setup_82577<I, R>(
    io: &mut I,
    reset_82580: bool,
    mdix: u8,
    master_slave: MasterSlaveMode,
    mut reset_phy: R,
) -> DevResult<MasterSlaveMode>
where
    I: E1000PhyRegisterIo,
    R: FnMut() -> DevResult,
{
    if reset_82580 {
        reset_phy()?;
    }
    let cfg = io.read_phy_register(I82577_CFG_REG)?
        | I82577_CFG_ASSERT_CRS_ON_TX
        | I82577_CFG_ENABLE_DOWNSHIFT;
    io.write_phy_register(I82577_CFG_REG, cfg)?;
    let mut control = io.read_phy_register(I82577_PHY_CTRL_2)?;
    control &= !I82577_PHY_CTRL2_MDIX_CFG_MASK;
    match mdix {
        1 => {}
        2 => control |= I82577_PHY_CTRL2_MANUAL_MDIX,
        _ => control |= I82577_PHY_CTRL2_AUTO_MDI_MDIX,
    }
    io.write_phy_register(I82577_PHY_CTRL_2, control)?;
    set_master_slave_mode(io, master_slave)
}

/// upstream: e1000_phy.c e1000_copper_link_setup_m88()
pub fn copper_link_setup_m88<I, C>(
    io: &mut I,
    phy_type: E1000PhyType,
    phy_id: u32,
    revision: u8,
    mdix: u8,
    disable_polarity_correction: bool,
    mut commit: C,
) -> DevResult
where
    I: E1000PhyRegisterIo,
    C: FnMut() -> DevResult,
{
    let mut phy_data = io.read_phy_register(M88E1000_PHY_SPEC_CTRL)?;
    if phy_type != E1000PhyType::Bm {
        phy_data |= M88E1000_PSCR_ASSERT_CRS_ON_TX;
    }
    phy_data &= !M88E1000_PSCR_AUTO_X_MODE;
    phy_data |= match mdix {
        1 => M88E1000_PSCR_MDI_MANUAL_MODE,
        2 => M88E1000_PSCR_MDIX_MANUAL_MODE,
        3 => M88E1000_PSCR_AUTO_X_1000T,
        _ => M88E1000_PSCR_AUTO_X_MODE,
    };
    phy_data &= !M88E1000_PSCR_POLARITY_REVERSAL;
    if disable_polarity_correction {
        phy_data |= M88E1000_PSCR_POLARITY_REVERSAL;
    }
    if phy_type == E1000PhyType::Bm {
        if phy_id == BME1000_E_PHY_ID_R2 {
            phy_data &= !BME1000_PSCR_ENABLE_DOWNSHIFT;
            io.write_phy_register(M88E1000_PHY_SPEC_CTRL, phy_data)?;
            commit()?;
        }
        phy_data |= BME1000_PSCR_ENABLE_DOWNSHIFT;
    }
    io.write_phy_register(M88E1000_PHY_SPEC_CTRL, phy_data)?;
    if phy_type == E1000PhyType::M88 && revision < 4 && phy_id != BME1000_E_PHY_ID_R2 {
        let mut extended = io.read_phy_register(M88E1000_EXT_PHY_SPEC_CTRL)?;
        extended |= M88E1000_EPSCR_TX_CLK_25;
        if revision == 2 && phy_id == M88E1111_I_PHY_ID {
            extended &= !M88EC018_EPSCR_DOWNSHIFT_COUNTER_MASK;
            extended |= M88EC018_EPSCR_DOWNSHIFT_COUNTER_5X;
        } else {
            extended &=
                !(M88E1000_EPSCR_MASTER_DOWNSHIFT_MASK | M88E1000_EPSCR_SLAVE_DOWNSHIFT_MASK);
            extended |= M88E1000_EPSCR_MASTER_DOWNSHIFT_1X | M88E1000_EPSCR_SLAVE_DOWNSHIFT_1X;
        }
        io.write_phy_register(M88E1000_EXT_PHY_SPEC_CTRL, extended)?;
    }
    if phy_type == E1000PhyType::Bm && phy_id == BME1000_E_PHY_ID_R2 {
        io.write_phy_register(29, 0x0003)?;
        io.write_phy_register(30, 0)?;
    }
    commit()?;
    if phy_type == E1000PhyType::I82578 {
        let mut extended = io.read_phy_register(M88E1000_EXT_PHY_SPEC_CTRL)?;
        extended |= I82578_EPSCR_DOWNSHIFT_ENABLE;
        extended &= !I82578_EPSCR_DOWNSHIFT_COUNTER_MASK;
        io.write_phy_register(M88E1000_EXT_PHY_SPEC_CTRL, extended)?;
    }
    Ok(())
}

/// upstream: e1000_phy.c e1000_copper_link_setup_m88_gen2()
pub fn copper_link_setup_m88_gen2<I, C>(
    io: &mut I,
    phy_id: u32,
    mdix: u8,
    disable_polarity_correction: bool,
    master_slave: MasterSlaveMode,
    mut commit: C,
) -> DevResult<MasterSlaveMode>
where
    I: E1000PhyRegisterIo,
    C: FnMut() -> DevResult,
{
    let mut control = io.read_phy_register(M88E1000_PHY_SPEC_CTRL)?;
    control &= !M88E1000_PSCR_AUTO_X_MODE;
    match mdix {
        1 => control |= M88E1000_PSCR_MDI_MANUAL_MODE,
        2 => control |= M88E1000_PSCR_MDIX_MANUAL_MODE,
        3 if phy_id != M88E1112_PHY_ID => control |= M88E1000_PSCR_AUTO_X_1000T,
        _ => control |= M88E1000_PSCR_AUTO_X_MODE,
    }
    control &= !M88E1000_PSCR_POLARITY_REVERSAL;
    if disable_polarity_correction {
        control |= M88E1000_PSCR_POLARITY_REVERSAL;
    }
    if phy_id == M88E1543_PHY_ID {
        control &= !I347AT4_PSCR_DOWNSHIFT_ENABLE;
        io.write_phy_register(M88E1000_PHY_SPEC_CTRL, control)?;
        commit()?;
    }
    control &= !I347AT4_PSCR_DOWNSHIFT_MASK;
    control |= I347AT4_PSCR_DOWNSHIFT_6X | I347AT4_PSCR_DOWNSHIFT_ENABLE;
    io.write_phy_register(M88E1000_PHY_SPEC_CTRL, control)?;
    commit()?;
    set_master_slave_mode(io, master_slave)
}

/// upstream: e1000_phy.c e1000_copper_link_setup_igp()
pub fn copper_link_setup_igp<I, R, L, D>(
    io: &mut I,
    phy_is_igp: bool,
    mdix: u8,
    autoneg: bool,
    autoneg_advertised: u16,
    set_d0_lplu_callback: bool,
    mut reset_phy: R,
    mut set_d3_lplu: L,
    mut set_d0_lplu: D,
) -> DevResult<Option<MasterSlaveMode>>
where
    I: E1000PhyRegisterIo,
    R: FnMut() -> DevResult,
    L: FnMut(bool) -> DevResult,
    D: FnMut(bool) -> DevResult,
{
    reset_phy()?;
    io.delay_us(100_000);
    if phy_is_igp {
        set_d3_lplu(false)?;
    }
    if set_d0_lplu_callback {
        set_d0_lplu(false)?;
    }
    let mut control = io.read_phy_register(IGP01E1000_PHY_PORT_CTRL)?;
    control &= !IGP01E1000_PSCR_AUTO_MDIX;
    match mdix {
        1 => control &= !IGP01E1000_PSCR_FORCE_MDI_MDIX,
        2 => control |= IGP01E1000_PSCR_FORCE_MDI_MDIX,
        _ => control |= IGP01E1000_PSCR_AUTO_MDIX,
    }
    io.write_phy_register(IGP01E1000_PHY_PORT_CTRL, control)?;
    if !autoneg {
        return Ok(None);
    }
    if autoneg_advertised == ADVERTISE_1000_FULL {
        let port_config =
            io.read_phy_register(IGP01E1000_PHY_PORT_CONFIG)? & !IGP01E1000_PSCFR_SMART_SPEED;
        io.write_phy_register(IGP01E1000_PHY_PORT_CONFIG, port_config)?;
        let control_1000 = io.read_phy_register(MII_1000T_CTRL)? & !CR_1000T_MS_ENABLE;
        io.write_phy_register(MII_1000T_CTRL, control_1000)?;
    }
    set_master_slave_mode(io, MasterSlaveMode::Auto).map(Some)
}

/// upstream: e1000_phy.c e1000_setup_copper_link_generic()
pub fn setup_copper_link_generic<I, F, C>(
    io: &mut I,
    autoneg: bool,
    config: &mut AutonegConfig,
    wait_to_complete: bool,
    read_callback_installed: bool,
    link_status_pending: &mut bool,
    mut force_speed_duplex: F,
    mut configure_flow_control: C,
) -> DevResult<bool>
where
    I: E1000PhyRegisterIo,
    F: FnMut() -> DevResult,
    C: FnMut() -> DevResult,
{
    if autoneg {
        copper_link_autoneg(
            io,
            config,
            wait_to_complete,
            read_callback_installed,
            link_status_pending,
        )?;
    } else {
        force_speed_duplex()?;
    }
    let link = phy_has_link_generic(io, 10, 10, read_callback_installed)?;
    if link {
        config_collision_dist_generic(io)?;
        configure_flow_control()?;
    }
    Ok(link)
}

/// upstream: e1000_phy.c e1000_phy_force_speed_duplex_igp()
pub fn phy_force_speed_duplex_igp<I: E1000PhyRegisterIo>(
    io: &mut I,
    forced_speed_duplex: u16,
    wait_to_complete: bool,
    flow_control: &mut FlowControlMode,
) -> DevResult {
    let mut phy_control = io.read_phy_register(PHY_CONTROL)?;
    phy_control = phy_force_speed_duplex_setup(io, flow_control, forced_speed_duplex, phy_control)?;
    io.write_phy_register(PHY_CONTROL, phy_control)?;
    let port_control = io.read_phy_register(IGP01E1000_PHY_PORT_CTRL)?
        & !IGP01E1000_PSCR_AUTO_MDIX
        & !IGP01E1000_PSCR_FORCE_MDI_MDIX;
    io.write_phy_register(IGP01E1000_PHY_PORT_CTRL, port_control)?;
    io.delay_us(1);
    if wait_to_complete {
        let _ = phy_has_link_generic(io, PHY_FORCE_LIMIT, 100_000, true)?;
        let _ = phy_has_link_generic(io, PHY_FORCE_LIMIT, 100_000, true)?;
    }
    Ok(())
}

/// upstream: e1000_phy.c e1000_phy_force_speed_duplex_m88()
pub fn phy_force_speed_duplex_m88<I, L, C>(
    io: &mut I,
    phy_type: E1000PhyType,
    phy_id: u32,
    forced_speed_duplex: u16,
    wait_to_complete: bool,
    flow_control: &mut FlowControlMode,
    mut poll_link: L,
    mut commit: C,
) -> DevResult
where
    I: E1000PhyRegisterIo,
    L: FnMut(u32, u32) -> DevResult<bool>,
    C: FnMut() -> DevResult,
{
    if phy_type != E1000PhyType::I210 {
        let control = io.read_phy_register(M88E1000_PHY_SPEC_CTRL)? & !M88E1000_PSCR_AUTO_X_MODE;
        io.write_phy_register(M88E1000_PHY_SPEC_CTRL, control)?;
    }
    let phy_control = io.read_phy_register(PHY_CONTROL)?;
    let phy_control =
        phy_force_speed_duplex_setup(io, flow_control, forced_speed_duplex, phy_control)?;
    io.write_phy_register(PHY_CONTROL, phy_control)?;
    commit()?;
    if wait_to_complete {
        let link = poll_link(PHY_FORCE_LIMIT, 100_000)?;
        if !link {
            let reset_dsp = match phy_id {
                I347AT4_E_PHY_ID | M88E1340M_E_PHY_ID | M88E1112_E_PHY_ID | M88E1543_E_PHY_ID
                | M88E1512_E_PHY_ID | I210_I_PHY_ID => false,
                _ => phy_type == E1000PhyType::M88,
            };
            if reset_dsp {
                io.write_phy_register(M88E1000_PHY_PAGE_SELECT, 0x001d)?;
                phy_reset_dsp_generic(io, true)?;
            }
        }
        let _ = poll_link(PHY_FORCE_LIMIT, 100_000)?;
    }
    if phy_type != E1000PhyType::M88
        || matches!(
            phy_id,
            I347AT4_E_PHY_ID
                | M88E1340M_E_PHY_ID
                | M88E1112_E_PHY_ID
                | I210_I_PHY_ID
                | M88E1543_E_PHY_ID
                | M88E1512_E_PHY_ID
        )
    {
        return Ok(());
    }
    let extended = io.read_phy_register(M88E1000_EXT_PHY_SPEC_CTRL)? | M88E1000_EPSCR_TX_CLK_25;
    io.write_phy_register(M88E1000_EXT_PHY_SPEC_CTRL, extended)
}

/// upstream: e1000_phy.c e1000_phy_force_speed_duplex_ife()
pub fn phy_force_speed_duplex_ife<I: E1000PhyRegisterIo>(
    io: &mut I,
    forced_speed_duplex: u16,
    wait_to_complete: bool,
    flow_control: &mut FlowControlMode,
) -> DevResult {
    let phy_control = io.read_phy_register(PHY_CONTROL)?;
    let phy_control =
        phy_force_speed_duplex_setup(io, flow_control, forced_speed_duplex, phy_control)?;
    io.write_phy_register(PHY_CONTROL, phy_control)?;
    let mdix =
        io.read_phy_register(IFE_PHY_MDIX_CONTROL)? & !IFE_PMC_AUTO_MDIX & !IFE_PMC_FORCE_MDIX;
    io.write_phy_register(IFE_PHY_MDIX_CONTROL, mdix)?;
    io.delay_us(1);
    if wait_to_complete {
        let _ = phy_has_link_generic(io, PHY_FORCE_LIMIT, 100_000, true)?;
        let _ = phy_has_link_generic(io, PHY_FORCE_LIMIT, 100_000, true)?;
    }
    Ok(())
}

pub trait E1000PhyMdicOps: E1000RegisterIo {
    fn read_mdic(&mut self, offset: u32) -> DevResult<u16>;
    fn write_mdic(&mut self, offset: u32, data: u16) -> DevResult;
    fn acquire(&mut self) -> DevResult;
    fn release(&mut self);
    fn set_phy_address(&mut self, _address: u8) {}
}

/// upstream: e1000_phy.c e1000_get_cfg_done_generic()
pub fn get_cfg_done_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    io.delay_us(10_000);
    Ok(())
}

/// upstream: e1000_phy.c e1000_phy_init_script_igp3()
pub fn phy_init_script_igp3<I: E1000PhyMdicOps>(io: &mut I) -> DevResult {
    const SCRIPT: &[(u32, u16)] = &[
        (0x2f5b, 0x9018),
        (0x2f52, 0x0000),
        (0x2fb1, 0x8b24),
        (0x2fb2, 0xf8f0),
        (0x2010, 0x10b0),
        (0x2011, 0x0000),
        (0x20dd, 0x249a),
        (0x20de, 0x00d3),
        (0x28b4, 0x04ce),
        (0x2f70, 0x29e4),
        (0x0000, 0x0140),
        (0x1f30, 0x1606),
        (0x1f31, 0xb814),
        (0x1f35, 0x002a),
        (0x1f3e, 0x0067),
        (0x1f54, 0x0065),
        (0x1f55, 0x002a),
        (0x1f56, 0x002a),
        (0x1f72, 0x3fb0),
        (0x1f76, 0xc0ff),
        (0x1f77, 0x1dec),
        (0x1f78, 0xf9ef),
        (0x1f79, 0x0210),
        (0x1895, 0x0003),
        (0x1796, 0x0008),
        (0x1798, 0xd008),
        (0x1898, 0xd918),
        (0x187a, 0x0800),
        (0x0019, 0x008d),
        (0x001b, 0x2080),
        (0x0014, 0x0045),
        (0x0000, 0x1340),
    ];
    for (register, value) in SCRIPT {
        let _ = io.write_mdic(*register, *value);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E1000PhyType {
    Unknown,
    M88,
    Igp,
    Igp2,
    Gg82563,
    Igp3,
    Ife,
    Bm,
    I82578,
    I82577,
    I82579,
    I217,
    I82580,
    I210,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CablePolarity {
    Normal,
    Reversed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PhyDiagnostics {
    pub cable_polarity: Option<CablePolarity>,
    pub speed_downgraded: bool,
    pub min_cable_length: u16,
    pub max_cable_length: u16,
    pub cable_length: u16,
    pub polarity_correction: bool,
    pub is_mdix: bool,
    pub local_rx: ReceiverStatus,
    pub remote_rx: ReceiverStatus,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReceiverStatus {
    #[default]
    Undefined,
    Ok,
    NotOk,
}

/// upstream: e1000_phy.c e1000_check_downshift_generic()
pub fn check_downshift_generic<I: E1000PhyRegisterIo>(
    io: &mut I,
    phy_type: E1000PhyType,
    diagnostics: &mut PhyDiagnostics,
) -> DevResult {
    let (register, mask) = match phy_type {
        E1000PhyType::I210
        | E1000PhyType::M88
        | E1000PhyType::Gg82563
        | E1000PhyType::Bm
        | E1000PhyType::I82578 => (u32::from(M88E1000_PHY_SPEC_STATUS), M88E1000_PSSR_DOWNSHIFT),
        E1000PhyType::Igp | E1000PhyType::Igp2 | E1000PhyType::Igp3 => (
            u32::from(IGP01E1000_PHY_LINK_HEALTH),
            IGP01E1000_PLHR_SS_DOWNGRADE,
        ),
        _ => {
            diagnostics.speed_downgraded = false;
            return Ok(());
        }
    };
    diagnostics.speed_downgraded = io.read_phy_register(register as u8)? & mask != 0;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmartSpeedMode {
    Default,
    On,
    Off,
}

/// upstream: e1000_phy.c e1000_set_d3_lplu_state_generic()
pub fn set_d3_lplu_state_generic<I: E1000PhyRegisterIo>(
    io: &mut I,
    active: bool,
    autoneg_advertised: u16,
    smart_speed: SmartSpeedMode,
    read_callback_installed: bool,
) -> DevResult {
    if !read_callback_installed {
        return Ok(());
    }
    let mut power = io.read_phy_register(IGP02E1000_PHY_POWER_MGMT)?;
    if !active {
        power &= !IGP02E1000_PM_D3_LPLU;
        io.write_phy_register(IGP02E1000_PHY_POWER_MGMT, power)?;
        if matches!(smart_speed, SmartSpeedMode::On | SmartSpeedMode::Off) {
            let mut config = io.read_phy_register(IGP01E1000_PHY_PORT_CONFIG)?;
            if smart_speed == SmartSpeedMode::On {
                config |= IGP01E1000_PSCFR_SMART_SPEED;
            } else {
                config &= !IGP01E1000_PSCFR_SMART_SPEED;
            }
            io.write_phy_register(IGP01E1000_PHY_PORT_CONFIG, config)?;
        }
    } else if matches!(
        autoneg_advertised,
        ALL_SPEED_DUPLEX | ALL_NOT_GIG | ALL_10_SPEED
    ) {
        power |= IGP02E1000_PM_D3_LPLU;
        io.write_phy_register(IGP02E1000_PHY_POWER_MGMT, power)?;
        let config =
            io.read_phy_register(IGP01E1000_PHY_PORT_CONFIG)? & !IGP01E1000_PSCFR_SMART_SPEED;
        io.write_phy_register(IGP01E1000_PHY_PORT_CONFIG, config)?;
    }
    Ok(())
}

/// upstream: e1000_phy.c e1000_check_polarity_m88()
pub fn check_polarity_m88<I: E1000PhyRegisterIo>(
    io: &mut I,
    diagnostics: &mut PhyDiagnostics,
) -> DevResult {
    let data = io.read_phy_register(M88E1000_PHY_SPEC_STATUS)?;
    diagnostics.cable_polarity = Some(if data & M88E1000_PSSR_REV_POLARITY != 0 {
        CablePolarity::Reversed
    } else {
        CablePolarity::Normal
    });
    Ok(())
}

/// upstream: e1000_phy.c e1000_check_polarity_igp()
pub fn check_polarity_igp<F>(mut read_phy: F, diagnostics: &mut PhyDiagnostics) -> DevResult
where
    F: FnMut(u32) -> DevResult<u16>,
{
    let status = read_phy(IGP01E1000_PHY_PORT_STATUS)?;
    let (register, mask) = if status & IGP01E1000_PSSR_SPEED_MASK == IGP01E1000_PSSR_SPEED_1000MBPS
    {
        (IGP01E1000_PHY_PCS_INIT_REG, IGP01E1000_PHY_POLARITY_MASK)
    } else {
        (
            IGP01E1000_PHY_PORT_STATUS,
            IGP01E1000_PSSR_POLARITY_REVERSED,
        )
    };
    diagnostics.cable_polarity = Some(if read_phy(register)? & mask != 0 {
        CablePolarity::Reversed
    } else {
        CablePolarity::Normal
    });
    Ok(())
}

/// upstream: e1000_phy.c e1000_check_polarity_ife()
pub fn check_polarity_ife<I: E1000PhyRegisterIo>(
    io: &mut I,
    polarity_correction: bool,
    diagnostics: &mut PhyDiagnostics,
) -> DevResult {
    let (register, mask) = if polarity_correction {
        (IFE_PHY_EXTENDED_STATUS_CONTROL, IFE_PESC_POLARITY_REVERSED)
    } else {
        (IFE_PHY_SPECIAL_CONTROL, IFE_PSC_FORCE_POLARITY)
    };
    diagnostics.cable_polarity = Some(if io.read_phy_register(register)? & mask != 0 {
        CablePolarity::Reversed
    } else {
        CablePolarity::Normal
    });
    Ok(())
}

/// upstream: e1000_phy.c e1000_check_polarity_82577()
pub fn check_polarity_82577<I: E1000PhyRegisterIo>(
    io: &mut I,
    diagnostics: &mut PhyDiagnostics,
) -> DevResult {
    diagnostics.cable_polarity = Some(
        if io.read_phy_register(I82577_PHY_STATUS_2)? & I82577_PHY_STATUS2_REV_POLARITY != 0 {
            CablePolarity::Reversed
        } else {
            CablePolarity::Normal
        },
    );
    Ok(())
}

/// upstream: e1000_phy.c e1000_get_cable_length_m88()
pub fn get_cable_length_m88<I: E1000PhyRegisterIo>(
    io: &mut I,
    diagnostics: &mut PhyDiagnostics,
) -> DevResult {
    const TABLE: [u16; 7] = [0, 50, 80, 110, 140, 140, CABLE_LENGTH_UNDEFINED];
    let status = io.read_phy_register(M88E1000_PHY_SPEC_STATUS)?;
    let index =
        ((status & M88E1000_PSSR_CABLE_LENGTH) >> M88E1000_PSSR_CABLE_LENGTH_SHIFT) as usize;
    if index >= TABLE.len() - 1 {
        return Err(DevError::Io);
    }
    diagnostics.min_cable_length = TABLE[index];
    diagnostics.max_cable_length = TABLE[index + 1];
    diagnostics.cable_length = (diagnostics.min_cable_length + diagnostics.max_cable_length) / 2;
    Ok(())
}

/// upstream: e1000_phy.c e1000_get_cable_length_igp_2()
pub fn get_cable_length_igp_2<F>(mut read_phy: F, diagnostics: &mut PhyDiagnostics) -> DevResult
where
    F: FnMut(u32) -> DevResult<u16>,
{
    const TABLE: [u16; 113] = [
        0, 0, 0, 0, 0, 0, 0, 0, 3, 5, 8, 11, 13, 16, 18, 21, 0, 0, 0, 3, 6, 10, 13, 16, 19, 23, 26,
        29, 32, 35, 38, 41, 6, 10, 14, 18, 22, 26, 30, 33, 37, 41, 44, 48, 51, 54, 58, 61, 21, 26,
        31, 35, 40, 44, 49, 53, 57, 61, 65, 68, 72, 75, 79, 82, 40, 45, 51, 56, 61, 66, 70, 75, 79,
        83, 87, 91, 94, 98, 101, 104, 60, 66, 72, 77, 82, 87, 92, 96, 100, 104, 108, 111, 114, 117,
        119, 121, 83, 89, 95, 100, 105, 109, 113, 116, 119, 122, 124, 104, 109, 114, 118, 121, 124,
    ];
    let mut agc_sum = 0u16;
    let mut min_index = TABLE.len() - 1;
    let mut max_index = 0usize;
    for register in IGP02E1000_PHY_AGC_REGS {
        let data = read_phy(register)?;
        let index = usize::from((data >> IGP02E1000_AGC_LENGTH_SHIFT) & IGP02E1000_AGC_LENGTH_MASK);
        if index >= TABLE.len() || index == 0 {
            return Err(DevError::Io);
        }
        if TABLE[min_index] > TABLE[index] {
            min_index = index;
        }
        if TABLE[max_index] < TABLE[index] {
            max_index = index;
        }
        agc_sum = agc_sum.saturating_add(TABLE[index]);
    }
    agc_sum = agc_sum.saturating_sub(TABLE[min_index] + TABLE[max_index]) / 2;
    diagnostics.min_cable_length = agc_sum.saturating_sub(IGP02E1000_AGC_RANGE);
    diagnostics.max_cable_length = agc_sum + IGP02E1000_AGC_RANGE;
    diagnostics.cable_length = (diagnostics.min_cable_length + diagnostics.max_cable_length) / 2;
    Ok(())
}

/// upstream: e1000_phy.c e1000_get_cable_length_m88_gen2()
pub fn get_cable_length_m88_gen2<R, W>(
    mut read_phy: R,
    mut write_phy: W,
    phy_id: u32,
    phy_address: u8,
    diagnostics: &mut PhyDiagnostics,
) -> DevResult
where
    R: FnMut(u32) -> DevResult<u16>,
    W: FnMut(u32, u16) -> DevResult,
{
    const M88_TABLE: [u16; 7] = [0, 50, 80, 110, 140, 140, CABLE_LENGTH_UNDEFINED];
    match phy_id {
        I210_PHY_ID => {
            let length_reg = (0x7 << GS40G_PAGE_SHIFT) | (I347AT4_PCDL + u32::from(phy_address));
            let length = read_phy(length_reg)?;
            let control = read_phy((0x7 << GS40G_PAGE_SHIFT) | I347AT4_PCDC)?;
            let meters = if control & I347AT4_PCDC_CABLE_LENGTH_UNIT == 0 {
                length / 100
            } else {
                length
            };
            diagnostics.min_cable_length = meters;
            diagnostics.max_cable_length = meters;
            diagnostics.cable_length = meters;
        }
        M88E1543_PHY_ID | M88E1512_PHY_ID | M88E1340M_PHY_ID | I347AT4_PHY_ID => {
            let original_page = read_phy(I347AT4_PAGE_SELECT)?;
            write_phy(I347AT4_PAGE_SELECT, 7)?;
            let length = read_phy(I347AT4_PCDL + u32::from(phy_address))?;
            let control = read_phy(I347AT4_PCDC)?;
            let meters = if control & I347AT4_PCDC_CABLE_LENGTH_UNIT == 0 {
                length / 100
            } else {
                length
            };
            diagnostics.min_cable_length = meters;
            diagnostics.max_cable_length = meters;
            diagnostics.cable_length = meters;
            write_phy(I347AT4_PAGE_SELECT, original_page)?;
        }
        M88E1112_PHY_ID => {
            let original_page = read_phy(I347AT4_PAGE_SELECT)?;
            write_phy(I347AT4_PAGE_SELECT, 5)?;
            let status = read_phy(M88E1112_VCT_DSP_DISTANCE)?;
            let index = ((status & M88E1000_PSSR_CABLE_LENGTH) >> M88E1000_PSSR_CABLE_LENGTH_SHIFT)
                as usize;
            if index >= M88_TABLE.len() - 1 {
                return Err(DevError::Io);
            }
            diagnostics.min_cable_length = M88_TABLE[index];
            diagnostics.max_cable_length = M88_TABLE[index + 1];
            diagnostics.cable_length =
                (diagnostics.min_cable_length + diagnostics.max_cable_length) / 2;
            write_phy(I347AT4_PAGE_SELECT, original_page)?;
        }
        _ => return Err(DevError::Unsupported),
    }
    Ok(())
}

/// upstream: e1000_phy.c e1000_get_phy_info_m88()
pub fn get_phy_info_m88<I, C>(
    io: &mut I,
    link_up: bool,
    mut get_cable_length: C,
    diagnostics: &mut PhyDiagnostics,
) -> DevResult
where
    I: E1000PhyRegisterIo,
    C: FnMut() -> DevResult,
{
    if !link_up {
        return Err(DevError::InvalidParam);
    }
    let control = io.read_phy_register(M88E1000_PHY_SPEC_CTRL)?;
    diagnostics.polarity_correction = control & M88E1000_PSCR_POLARITY_REVERSAL != 0;
    check_polarity_m88(io, diagnostics)?;
    let status = io.read_phy_register(M88E1000_PHY_SPEC_STATUS)?;
    diagnostics.is_mdix = status & M88E1000_PSSR_MDIX != 0;
    if status & M88E1000_PSSR_SPEED == M88E1000_PSSR_1000MBS {
        get_cable_length()?;
        let receiver = io.read_phy_register(PHY_1000T_STATUS as u8)?;
        diagnostics.local_rx = if receiver & SR_1000T_LOCAL_RX_STATUS != 0 {
            ReceiverStatus::Ok
        } else {
            ReceiverStatus::NotOk
        };
        diagnostics.remote_rx = if receiver & SR_1000T_REMOTE_RX_STATUS != 0 {
            ReceiverStatus::Ok
        } else {
            ReceiverStatus::NotOk
        };
    } else {
        diagnostics.cable_length = CABLE_LENGTH_UNDEFINED;
        diagnostics.local_rx = ReceiverStatus::Undefined;
        diagnostics.remote_rx = ReceiverStatus::Undefined;
    }
    Ok(())
}

/// upstream: e1000_phy.c e1000_get_phy_info_igp()
pub fn get_phy_info_igp<F, C>(
    mut read_phy: F,
    link_up: bool,
    mut get_cable_length: C,
    diagnostics: &mut PhyDiagnostics,
) -> DevResult
where
    F: FnMut(u32) -> DevResult<u16>,
    C: FnMut() -> DevResult,
{
    if !link_up {
        return Err(DevError::InvalidParam);
    }
    diagnostics.polarity_correction = true;
    check_polarity_igp(&mut read_phy, diagnostics)?;
    let status = read_phy(u32::from(IGP01E1000_PHY_PORT_STATUS))?;
    diagnostics.is_mdix = status & IGP01E1000_PSSR_MDIX != 0;
    if status & IGP01E1000_PSSR_SPEED_MASK == IGP01E1000_PSSR_SPEED_1000MBPS {
        get_cable_length()?;
        let receiver = read_phy(PHY_1000T_STATUS)?;
        diagnostics.local_rx = if receiver & SR_1000T_LOCAL_RX_STATUS != 0 {
            ReceiverStatus::Ok
        } else {
            ReceiverStatus::NotOk
        };
        diagnostics.remote_rx = if receiver & SR_1000T_REMOTE_RX_STATUS != 0 {
            ReceiverStatus::Ok
        } else {
            ReceiverStatus::NotOk
        };
    } else {
        diagnostics.cable_length = CABLE_LENGTH_UNDEFINED;
        diagnostics.local_rx = ReceiverStatus::Undefined;
        diagnostics.remote_rx = ReceiverStatus::Undefined;
    }
    Ok(())
}

/// upstream: e1000_phy.c e1000_get_phy_info_ife()
pub fn get_phy_info_ife<F>(
    mut read_phy: F,
    link_up: bool,
    diagnostics: &mut PhyDiagnostics,
) -> DevResult
where
    F: FnMut(u32) -> DevResult<u16>,
{
    if !link_up {
        return Err(DevError::InvalidParam);
    }
    let control = read_phy(u32::from(IFE_PHY_SPECIAL_CONTROL))?;
    diagnostics.polarity_correction = control & IFE_PSC_AUTO_POLARITY_DISABLE == 0;
    if diagnostics.polarity_correction {
        let data = read_phy(u32::from(IFE_PHY_EXTENDED_STATUS_CONTROL))?;
        diagnostics.cable_polarity = Some(if data & IFE_PESC_POLARITY_REVERSED != 0 {
            CablePolarity::Reversed
        } else {
            CablePolarity::Normal
        });
    } else {
        diagnostics.cable_polarity = Some(if control & IFE_PSC_FORCE_POLARITY != 0 {
            CablePolarity::Reversed
        } else {
            CablePolarity::Normal
        });
    }
    diagnostics.is_mdix = read_phy(u32::from(IFE_PHY_MDIX_CONTROL))? & IFE_PMC_MDIX_STATUS != 0;
    diagnostics.cable_length = CABLE_LENGTH_UNDEFINED;
    diagnostics.local_rx = ReceiverStatus::Undefined;
    diagnostics.remote_rx = ReceiverStatus::Undefined;
    Ok(())
}

/// upstream: e1000_phy.c e1000_phy_force_speed_duplex_setup()
pub fn phy_force_speed_duplex_setup<I: E1000RegisterIo>(
    io: &mut I,
    flow_control: &mut FlowControlMode,
    forced_speed_duplex: u16,
    mut phy_control: u16,
) -> DevResult<u16> {
    *flow_control = FlowControlMode::None;
    let mut control = io.read_register(E1000_CTRL)?;
    control |= E1000_CTRL_FRCSPD | E1000_CTRL_FRCDPX;
    control &= !E1000_CTRL_SPD_SEL;
    control &= !E1000_CTRL_ASDE;
    phy_control &= !MII_CR_AUTO_NEG_EN;
    if forced_speed_duplex & ALL_HALF_DUPLEX != 0 {
        control &= !E1000_CTRL_FD;
        phy_control &= !MII_CR_FULL_DUPLEX;
    } else {
        control |= E1000_CTRL_FD;
        phy_control |= MII_CR_FULL_DUPLEX;
    }
    if forced_speed_duplex & ALL_100_SPEED != 0 {
        control |= E1000_CTRL_SPD_100;
        phy_control |= MII_CR_SPEED_100;
        phy_control &= !MII_CR_SPEED_1000;
    } else {
        control &= !(E1000_CTRL_SPD_1000 | E1000_CTRL_SPD_100);
        phy_control &= !(MII_CR_SPEED_1000 | MII_CR_SPEED_100);
    }
    config_collision_dist_generic(io)?;
    io.write_register(E1000_CTRL, control)?;
    Ok(phy_control)
}

/// upstream: e1000_phy.c e1000_phy_sw_reset_generic()
pub fn phy_sw_reset_generic<I: E1000PhyRegisterIo>(
    io: &mut I,
    read_callback_installed: bool,
) -> DevResult {
    if !read_callback_installed {
        return Ok(());
    }
    let control = io.read_phy_register(PHY_CONTROL)? | MII_CR_RESET;
    io.write_phy_register(PHY_CONTROL, control)?;
    io.delay_us(1);
    Ok(())
}

/// upstream: e1000_phy.c e1000_phy_hw_reset_generic()
pub fn phy_hw_reset_generic<I: E1000PhyMdicOps>(
    io: &mut I,
    reset_blocked: bool,
    reset_delay_us: u32,
) -> DevResult {
    if reset_blocked {
        return Ok(());
    }
    io.acquire()?;
    let control = io.read_register(E1000_CTRL)?;
    io.write_register(E1000_CTRL, control | E1000_CTRL_PHY_RST)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(reset_delay_us);
    io.write_register(E1000_CTRL, control)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(150);
    io.release();
    get_cfg_done_generic(io)
}

/// upstream: e1000_phy.c e1000_power_up_phy_copper()
pub fn power_up_phy_copper<I: E1000PhyRegisterIo>(io: &mut I) {
    if let Ok(control) = io.read_phy_register(PHY_CONTROL) {
        let _ = io.write_phy_register(PHY_CONTROL, control & !MII_CR_POWER_DOWN);
    }
}

/// upstream: e1000_phy.c e1000_power_down_phy_copper()
pub fn power_down_phy_copper<I: E1000PhyRegisterIo>(io: &mut I) {
    if let Ok(control) = io.read_phy_register(PHY_CONTROL) {
        let _ = io.write_phy_register(PHY_CONTROL, control | MII_CR_POWER_DOWN);
        io.delay_us(1000);
    }
}

/// upstream: e1000_phy.c e1000_get_phy_type_from_id()
pub const fn get_phy_type_from_id(phy_id: u32) -> E1000PhyType {
    match phy_id {
        0x0141_0c30 | 0x0141_0c50 | 0x0141_0cc0 | 0x0141_0c20 | 0x0141_0ea0 | 0x0141_0dd0
        | 0x0141_0dc0 | 0x0141_0c90 | 0x0141_0df0 => E1000PhyType::M88,
        0x02a8_0380 => E1000PhyType::Igp2,
        0x0141_0ca0 => E1000PhyType::Gg82563,
        0x02a8_0390 => E1000PhyType::Igp3,
        0x02a8_0330 | 0x02a8_0320 | 0x02a8_0310 => E1000PhyType::Ife,
        0x0141_0cb0 | 0x0141_0cb1 => E1000PhyType::Bm,
        0x004d_d040 => E1000PhyType::I82578,
        0x0154_0050 => E1000PhyType::I82577,
        0x0154_0090 => E1000PhyType::I82579,
        0x0154_00a0 => E1000PhyType::I217,
        0x0154_03a0 => E1000PhyType::I82580,
        0x0141_0c00 => E1000PhyType::I210,
        _ => E1000PhyType::Unknown,
    }
}

/// upstream: e1000_phy.c e1000_determine_phy_address()
pub fn determine_phy_address<I: E1000PhyMdicOps>(io: &mut I) -> DevResult<(u8, u32, E1000PhyType)> {
    for address in 0..8u8 {
        io.set_phy_address(address);
        for _ in 0..10 {
            let id1 = io.read_mdic(u32::from(PHY_ID1))?;
            io.delay_us(20);
            let id2 = io.read_mdic(u32::from(PHY_ID2))?;
            let phy_id = (u32::from(id1) << 16) | u32::from(id2 & !PHY_REVISION_MASK);
            let phy_type = get_phy_type_from_id(phy_id);
            if phy_type != E1000PhyType::Unknown {
                return Ok((address, phy_id, phy_type));
            }
            io.delay_us(1000);
        }
    }
    Err(DevError::Io)
}

/// upstream: e1000_phy.c e1000_read_phy_reg_m88()
pub fn read_phy_reg_m88<I: E1000PhyMdicOps>(
    io: &mut I,
    offset: u32,
    acquire_installed: bool,
) -> DevResult<u16> {
    if !acquire_installed {
        return Ok(0);
    }
    io.acquire()?;
    let result = io.read_mdic(offset & MAX_PHY_REG_ADDRESS);
    io.release();
    result
}

/// upstream: e1000_phy.c e1000_write_phy_reg_m88()
pub fn write_phy_reg_m88<I: E1000PhyMdicOps>(
    io: &mut I,
    offset: u32,
    data: u16,
    acquire_installed: bool,
) -> DevResult {
    if !acquire_installed {
        return Ok(());
    }
    io.acquire()?;
    let result = io.write_mdic(offset & MAX_PHY_REG_ADDRESS, data);
    io.release();
    result
}

/// upstream: e1000_phy.c e1000_set_page_igp()
pub fn set_page_igp<I: E1000PhyMdicOps>(io: &mut I, page: u16) -> DevResult<u8> {
    io.write_mdic(IGP01E1000_PHY_PAGE_SELECT, page)?;
    Ok(1)
}

/// upstream: e1000_phy.c __e1000_read_phy_reg_igp()
pub fn read_phy_reg_igp_internal<I: E1000PhyMdicOps>(
    io: &mut I,
    offset: u32,
    locked: bool,
    acquire_installed: bool,
) -> DevResult<u16> {
    if !locked {
        if !acquire_installed {
            return Ok(0);
        }
        io.acquire()?;
    }
    let result = (|| {
        if offset > MAX_PHY_MULTI_PAGE_REG {
            io.write_mdic(IGP01E1000_PHY_PAGE_SELECT, offset as u16)?;
        }
        io.read_mdic(offset & MAX_PHY_REG_ADDRESS)
    })();
    if !locked {
        io.release();
    }
    result
}

/// upstream: e1000_phy.c e1000_read_phy_reg_igp()
pub fn read_phy_reg_igp<I: E1000PhyMdicOps>(
    io: &mut I,
    offset: u32,
    acquire_installed: bool,
) -> DevResult<u16> {
    read_phy_reg_igp_internal(io, offset, false, acquire_installed)
}

/// upstream: e1000_phy.c e1000_read_phy_reg_igp_locked()
pub fn read_phy_reg_igp_locked<I: E1000PhyMdicOps>(io: &mut I, offset: u32) -> DevResult<u16> {
    read_phy_reg_igp_internal(io, offset, true, true)
}

/// upstream: e1000_phy.c __e1000_write_phy_reg_igp()
pub fn write_phy_reg_igp_internal<I: E1000PhyMdicOps>(
    io: &mut I,
    offset: u32,
    data: u16,
    locked: bool,
    acquire_installed: bool,
) -> DevResult {
    if !locked {
        if !acquire_installed {
            return Ok(());
        }
        io.acquire()?;
    }
    let result = (|| {
        if offset > MAX_PHY_MULTI_PAGE_REG {
            io.write_mdic(IGP01E1000_PHY_PAGE_SELECT, offset as u16)?;
        }
        io.write_mdic(offset & MAX_PHY_REG_ADDRESS, data)
    })();
    if !locked {
        io.release();
    }
    result
}

/// upstream: e1000_phy.c e1000_write_phy_reg_igp()
pub fn write_phy_reg_igp<I: E1000PhyMdicOps>(
    io: &mut I,
    offset: u32,
    data: u16,
    acquire_installed: bool,
) -> DevResult {
    write_phy_reg_igp_internal(io, offset, data, false, acquire_installed)
}

/// upstream: e1000_phy.c e1000_write_phy_reg_igp_locked()
pub fn write_phy_reg_igp_locked<I: E1000PhyMdicOps>(
    io: &mut I,
    offset: u32,
    data: u16,
) -> DevResult {
    write_phy_reg_igp_internal(io, offset, data, true, true)
}

/// upstream: e1000_phy.c __e1000_read_kmrn_reg()
pub fn read_kmrn_reg_internal<I: E1000PhyMdicOps>(
    io: &mut I,
    offset: u32,
    locked: bool,
    acquire_installed: bool,
) -> DevResult<u16> {
    if !locked {
        if !acquire_installed {
            return Ok(0);
        }
        io.acquire()?;
    }
    let result = (|| {
        let command = ((offset << KMRNCTRLSTA_OFFSET_SHIFT) & KMRNCTRLSTA_OFFSET) | KMRNCTRLSTA_REN;
        io.write_register(E1000_KMRNCTRLSTA, command)?;
        let _ = io.read_register(E1000_STATUS)?;
        io.delay_us(2);
        Ok(io.read_register(E1000_KMRNCTRLSTA)? as u16)
    })();
    if !locked {
        io.release();
    }
    result
}

/// upstream: e1000_phy.c e1000_read_kmrn_reg_generic()
pub fn read_kmrn_reg_generic<I: E1000PhyMdicOps>(
    io: &mut I,
    offset: u32,
    acquire_installed: bool,
) -> DevResult<u16> {
    read_kmrn_reg_internal(io, offset, false, acquire_installed)
}

/// upstream: e1000_phy.c e1000_read_kmrn_reg_locked()
pub fn read_kmrn_reg_locked<I: E1000PhyMdicOps>(io: &mut I, offset: u32) -> DevResult<u16> {
    read_kmrn_reg_internal(io, offset, true, true)
}

/// upstream: e1000_phy.c __e1000_write_kmrn_reg()
pub fn write_kmrn_reg_internal<I: E1000PhyMdicOps>(
    io: &mut I,
    offset: u32,
    data: u16,
    locked: bool,
    acquire_installed: bool,
) -> DevResult {
    if !locked {
        if !acquire_installed {
            return Ok(());
        }
        io.acquire()?;
    }
    let result = (|| {
        let value = ((offset << KMRNCTRLSTA_OFFSET_SHIFT) & KMRNCTRLSTA_OFFSET) | u32::from(data);
        io.write_register(E1000_KMRNCTRLSTA, value)?;
        let _ = io.read_register(E1000_STATUS)?;
        io.delay_us(2);
        Ok(())
    })();
    if !locked {
        io.release();
    }
    result
}

/// upstream: e1000_phy.c e1000_write_kmrn_reg_generic()
pub fn write_kmrn_reg_generic<I: E1000PhyMdicOps>(
    io: &mut I,
    offset: u32,
    data: u16,
    acquire_installed: bool,
) -> DevResult {
    write_kmrn_reg_internal(io, offset, data, false, acquire_installed)
}

/// upstream: e1000_phy.c e1000_write_kmrn_reg_locked()
pub fn write_kmrn_reg_locked<I: E1000PhyMdicOps>(io: &mut I, offset: u32, data: u16) -> DevResult {
    write_kmrn_reg_internal(io, offset, data, true, true)
}

/// upstream: e1000_phy.c e1000_phy_setup_autoneg()
pub fn phy_setup_autoneg<I: E1000PhyRegisterIo>(
    io: &mut I,
    config: &mut AutonegConfig,
) -> DevResult {
    config.advertised &= config.mask;
    let mut advertisement = io.read_phy_register(MII_AUTONEG_ADV)?;
    let mut gigabit_control = 0;
    if config.mask & ADVERTISE_1000_FULL != 0 {
        gigabit_control = io.read_phy_register(MII_1000T_CTRL)?;
    }
    advertisement &= !(NWAY_AR_100TX_FD_CAPS
        | NWAY_AR_100TX_HD_CAPS
        | NWAY_AR_10T_FD_CAPS
        | NWAY_AR_10T_HD_CAPS);
    gigabit_control &= !(CR_1000T_HD_CAPS | CR_1000T_FD_CAPS);
    if config.advertised & ADVERTISE_10_HALF != 0 {
        advertisement |= NWAY_AR_10T_HD_CAPS;
    }
    if config.advertised & ADVERTISE_10_FULL != 0 {
        advertisement |= NWAY_AR_10T_FD_CAPS;
    }
    if config.advertised & ADVERTISE_100_HALF != 0 {
        advertisement |= NWAY_AR_100TX_HD_CAPS;
    }
    if config.advertised & ADVERTISE_100_FULL != 0 {
        advertisement |= NWAY_AR_100TX_FD_CAPS;
    }
    // The upstream source deliberately never advertises 1000BASE-T half duplex.
    let _denied_gigabit_half = config.advertised & ADVERTISE_1000_HALF;
    if config.advertised & ADVERTISE_1000_FULL != 0 {
        gigabit_control |= CR_1000T_FD_CAPS;
    }
    match config.flow_control {
        FlowControlMode::None => advertisement &= !(NWAY_AR_ASM_DIR | NWAY_AR_PAUSE),
        FlowControlMode::RxPause => advertisement |= NWAY_AR_ASM_DIR | NWAY_AR_PAUSE,
        FlowControlMode::TxPause => {
            advertisement |= NWAY_AR_ASM_DIR;
            advertisement &= !NWAY_AR_PAUSE;
        }
        FlowControlMode::Full => advertisement |= NWAY_AR_ASM_DIR | NWAY_AR_PAUSE,
    }
    io.write_phy_register(MII_AUTONEG_ADV, advertisement)?;
    if config.mask & ADVERTISE_1000_FULL != 0 {
        io.write_phy_register(MII_1000T_CTRL, gigabit_control)?;
    }
    Ok(())
}

/// upstream: e1000_phy.c e1000_copper_link_autoneg()
pub fn copper_link_autoneg<I: E1000PhyRegisterIo>(
    io: &mut I,
    config: &mut AutonegConfig,
    wait_to_complete: bool,
    read_callback_installed: bool,
    link_status_pending: &mut bool,
) -> DevResult {
    config.advertised &= config.mask;
    if config.advertised == 0 {
        config.advertised = config.mask;
    }
    phy_setup_autoneg(io, config)?;
    let control = io.read_phy_register(PHY_CONTROL)?;
    io.write_phy_register(
        PHY_CONTROL,
        control | MII_CR_AUTO_NEG_EN | MII_CR_RESTART_AUTO_NEG,
    )?;
    if wait_to_complete {
        wait_autoneg(io, read_callback_installed)?;
    }
    *link_status_pending = true;
    Ok(())
}

/// upstream: e1000_phy.c e1000_wait_autoneg()
pub fn wait_autoneg<I: E1000PhyRegisterIo>(io: &mut I, read_callback_installed: bool) -> DevResult {
    if !read_callback_installed {
        return Ok(());
    }
    for _ in (0..PHY_AUTO_NEG_LIMIT).rev() {
        let _ = io.read_phy_register(PHY_STATUS)?;
        let status = io.read_phy_register(PHY_STATUS)?;
        if status & MII_SR_AUTONEG_COMPLETE != 0 {
            break;
        }
        io.delay_us(100_000);
    }
    Ok(())
}

/// upstream: e1000_phy.c e1000_phy_has_link_generic()
pub fn phy_has_link_generic<I: E1000PhyRegisterIo>(
    io: &mut I,
    iterations: u32,
    interval_us: u32,
    read_callback_installed: bool,
) -> DevResult<bool> {
    if !read_callback_installed {
        return Ok(false);
    }
    for _ in 0..iterations {
        if io.read_phy_register(PHY_STATUS).is_err() {
            io.delay_us(interval_us);
        }
        let status = io.read_phy_register(PHY_STATUS)?;
        if status & MII_SR_LINK_STATUS != 0 {
            return Ok(true);
        }
        io.delay_us(interval_us);
    }
    Ok(false)
}

fn i2c_wait<I: E1000RegisterIo>(io: &mut I) -> DevResult<u32> {
    for _ in 0..I2CCMD_TIMEOUT {
        io.delay_us(50);
        let command = io.read_register(E1000_I2CCMD)?;
        if command & E1000_I2CCMD_READY != 0 {
            if command & E1000_I2CCMD_ERROR != 0 {
                return Err(DevError::Io);
            }
            return Ok(command);
        }
    }
    Err(DevError::Io)
}

/// upstream: e1000_phy.c e1000_read_phy_reg_i2c()
pub fn read_phy_reg_i2c<I: E1000RegisterIo>(
    io: &mut I,
    phy_address: u8,
    offset: u32,
) -> DevResult<u16> {
    let command = (offset << E1000_I2CCMD_REG_ADDR_SHIFT)
        | (u32::from(phy_address) << E1000_I2CCMD_PHY_ADDR_SHIFT)
        | E1000_I2CCMD_OPCODE_READ;
    io.write_register(E1000_I2CCMD, command)?;
    let response = i2c_wait(io)?;
    Ok((((response >> 8) & 0x00ff) | ((response << 8) & 0xff00)) as u16)
}

/// upstream: e1000_phy.c e1000_write_phy_reg_i2c()
pub fn write_phy_reg_i2c<I: E1000RegisterIo>(
    io: &mut I,
    phy_address: u8,
    offset: u32,
    data: u16,
) -> DevResult {
    if phy_address == 0 || phy_address > 7 {
        return Err(DevError::InvalidParam);
    }
    let swapped = data.rotate_left(8);
    let command = (offset << E1000_I2CCMD_REG_ADDR_SHIFT)
        | (u32::from(phy_address) << E1000_I2CCMD_PHY_ADDR_SHIFT)
        | u32::from(swapped);
    io.write_register(E1000_I2CCMD, command)?;
    i2c_wait(io).map(|_| ())
}

/// upstream: e1000_phy.c e1000_read_sfp_data_byte()
pub fn read_sfp_data_byte<I: E1000RegisterIo>(io: &mut I, offset: u16) -> DevResult<u8> {
    if offset > SFP_DIAG_BASE + 255 {
        return Err(DevError::InvalidParam);
    }
    io.write_register(
        E1000_I2CCMD,
        (u32::from(offset) << E1000_I2CCMD_REG_ADDR_SHIFT) | E1000_I2CCMD_OPCODE_READ,
    )?;
    Ok((i2c_wait(io)? & 0xff) as u8)
}

/// upstream: e1000_phy.c e1000_write_sfp_data_byte()
pub fn write_sfp_data_byte<I: E1000RegisterIo>(io: &mut I, offset: u16, data: u8) -> DevResult {
    if offset > SFP_DIAG_BASE + 255 {
        return Err(DevError::InvalidParam);
    }
    let command = (u32::from(offset) << E1000_I2CCMD_REG_ADDR_SHIFT) | E1000_I2CCMD_OPCODE_READ;
    io.write_register(E1000_I2CCMD, command)?;
    let old_word = i2c_wait(io)?;
    let word = (old_word & 0xff00) | u32::from(data);
    io.write_register(
        E1000_I2CCMD,
        (u32::from(offset) << E1000_I2CCMD_REG_ADDR_SHIFT) | word,
    )?;
    i2c_wait(io).map(|_| ())
}

/// The source represents the reset-block status as a positive driver code.
pub const fn reset_block_error_code(blocked: bool) -> Result<(), u8> {
    if blocked {
        Err(E1000_BLK_PHY_RESET)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use axdriver_base::DevError;

    use super::*;

    #[derive(Default)]
    struct Io {
        registers: [(u32, u32); 2],
        writes: alloc::vec::Vec<(u32, u32)>,
        phy: [u16; 32],
        phy_writes: alloc::vec::Vec<(u8, u16)>,
        delay: usize,
        mdic_data: u16,
        mdic_error: bool,
        i2c_data: u16,
        i2c_error: bool,
        mdic_reads: alloc::vec::Vec<u32>,
        mdic_writes: alloc::vec::Vec<(u32, u16)>,
        locks: usize,
        unlocks: usize,
        kmrn_data: u16,
        phy_address: u8,
        use_phy_ids: bool,
        phy_ids: [(u16, u16); 8],
    }
    impl E1000RegisterIo for Io {
        fn read_register(&mut self, register: u32) -> DevResult<u32> {
            if register == E1000_KMRNCTRLSTA {
                let command = self
                    .writes
                    .iter()
                    .rev()
                    .find_map(|(reg, value)| (*reg == E1000_KMRNCTRLSTA).then_some(*value))
                    .unwrap_or(0);
                return Ok((command & KMRNCTRLSTA_OFFSET) | u32::from(self.kmrn_data));
            }
            if register == E1000_MDIC {
                let command = self
                    .writes
                    .iter()
                    .rev()
                    .find_map(|(reg, value)| (*reg == E1000_MDIC).then_some(*value))
                    .unwrap_or(0);
                return Ok((command & E1000_MDIC_REG_MASK)
                    | E1000_MDIC_READY
                    | if self.mdic_error { E1000_MDIC_ERROR } else { 0 }
                    | u32::from(self.mdic_data));
            }
            if register == E1000_I2CCMD {
                let command = self
                    .writes
                    .iter()
                    .rev()
                    .find_map(|(reg, value)| (*reg == E1000_I2CCMD).then_some(*value))
                    .unwrap_or(0);
                let data = if command & E1000_I2CCMD_OPCODE_READ != 0 {
                    u32::from(self.i2c_data)
                } else {
                    command & 0xffff
                };
                return Ok((command & 0xffff_0000)
                    | E1000_I2CCMD_READY
                    | if self.i2c_error {
                        E1000_I2CCMD_ERROR
                    } else {
                        0
                    }
                    | data);
            }
            Ok(self
                .registers
                .iter()
                .find(|(reg, _)| *reg == register)
                .map(|(_, val)| *val)
                .unwrap_or(0))
        }
        fn write_register(&mut self, register: u32, value: u32) -> DevResult {
            self.writes.push((register, value));
            Ok(())
        }
        fn delay_us(&mut self, delay: u32) {
            self.delay += delay as usize;
        }
        fn invalid_tail_write(&mut self, _: &'static str) {}
    }
    impl E1000PhyRegisterIo for Io {
        fn read_phy_register(&mut self, register: u8) -> DevResult<u16> {
            self.phy
                .get(register as usize)
                .copied()
                .ok_or(DevError::InvalidParam)
        }
        fn write_phy_register(&mut self, register: u8, value: u16) -> DevResult {
            self.phy_writes.push((register, value));
            Ok(())
        }
    }
    impl E1000PhyMdicOps for Io {
        fn read_mdic(&mut self, offset: u32) -> DevResult<u16> {
            self.mdic_reads.push(offset);
            if self.use_phy_ids {
                let (id1, id2) = self.phy_ids[self.phy_address as usize];
                return match offset {
                    value if value == u32::from(PHY_ID1) => Ok(id1),
                    value if value == u32::from(PHY_ID2) => Ok(id2),
                    _ => Ok(self.mdic_data),
                };
            }
            Ok(self.mdic_data)
        }
        fn write_mdic(&mut self, offset: u32, data: u16) -> DevResult {
            self.mdic_writes.push((offset, data));
            Ok(())
        }
        fn acquire(&mut self) -> DevResult {
            self.locks += 1;
            Ok(())
        }
        fn release(&mut self) {
            self.unlocks += 1;
        }
        fn set_phy_address(&mut self, address: u8) {
            self.phy_address = address;
        }
    }

    #[test]
    fn generic_phy_ops_defaults_and_noops_match_source() {
        let ops = init_phy_ops_generic();
        assert_eq!(ops.read_reg, E1000PhyCallback::NullReadReg);
        assert_eq!(ops.read_i2c_byte, E1000PhyCallback::ReadI2cByteNull);
        assert!(null_set_page().is_ok());
        assert_eq!(null_read_reg().unwrap(), 0);
        null_phy_generic();
        assert!(null_lplu_state(true).is_ok());
        assert!(null_write_reg(1, 2).is_ok());
        assert_eq!(read_i2c_byte_null(0, 0).unwrap(), 0);
        assert!(write_i2c_byte_null(0, 0, 0).is_ok());
    }

    #[test]
    fn generic_phy_identity_reset_and_retry_adapters_preserve_order() {
        let mut io = Io::default();
        io.phy[PHY_ID1 as usize] = 0x0141;
        io.phy[PHY_ID2 as usize] = 0x0cc2;
        assert_eq!(
            get_phy_id(&mut io, true).unwrap(),
            PhyIdentity {
                id: 0x0141_0002,
                revision: 0x0cc0
            }
        );
        assert_eq!(io.delay, 20);
        assert!(phy_reset_dsp_generic(&mut io, true).is_ok());
        assert_eq!(
            io.phy_writes,
            [
                (M88E1000_PHY_GEN_CONTROL, 0xc1),
                (M88E1000_PHY_GEN_CONTROL, 0)
            ]
        );
        let mut retries = 4;
        let saved = disable_phy_retry_mechanism(&mut retries);
        assert_eq!(retries, 0);
        enable_phy_retry_mechanism(&mut retries, saved);
        assert_eq!(retries, 4);
        assert_eq!(reset_block_error_code(true), Err(E1000_BLK_PHY_RESET));
    }

    #[test]
    fn generic_mdic_transactions_validate_echo_error_retry_and_pch_delay() {
        let mut io = Io {
            mdic_data: 0x1234,
            ..Io::default()
        };
        assert_eq!(read_phy_reg_mdic(&mut io, 1, 0, true, 2).unwrap(), 0x1234);
        assert_eq!(io.writes[0].0, E1000_MDIC);
        assert!(io.writes[0].1 & E1000_MDIC_OP_READ != 0);
        assert_eq!(io.delay, 150);
        write_phy_reg_mdic(&mut io, 1, 0, false, 3, 0xabcd).unwrap();
        assert!(io.writes.last().unwrap().1 & E1000_MDIC_OP_WRITE != 0);
        assert!(write_phy_reg_mdic(&mut io, 1, 0, false, 0x20, 0).is_err());
        io.mdic_error = true;
        assert!(read_phy_reg_mdic(&mut io, 1, 1, false, 2).is_err());
    }

    #[test]
    fn generic_phy_autoneg_programs_speed_pause_and_gigabit_words() {
        let mut io = Io::default();
        io.phy[MII_AUTONEG_ADV as usize] = 0xffff;
        io.phy[MII_1000T_CTRL as usize] = 0xffff;
        let mut config = AutonegConfig {
            advertised: ADVERTISE_10_HALF
                | ADVERTISE_100_FULL
                | ADVERTISE_1000_HALF
                | ADVERTISE_1000_FULL,
            mask: u16::MAX,
            flow_control: FlowControlMode::TxPause,
        };
        phy_setup_autoneg(&mut io, &mut config).unwrap();
        assert_eq!(
            config.advertised,
            ADVERTISE_10_HALF | ADVERTISE_100_FULL | ADVERTISE_1000_HALF | ADVERTISE_1000_FULL
        );
        assert_eq!(io.phy_writes[0], (MII_AUTONEG_ADV, 0xfb3f));
        assert_eq!(io.phy_writes[1], (MII_1000T_CTRL, 0xfeff));
    }

    #[test]
    fn generic_i2c_phy_and_sfp_transfers_preserve_byte_order_and_bounds() {
        let mut io = Io {
            i2c_data: 0x3412,
            ..Io::default()
        };
        assert_eq!(read_phy_reg_i2c(&mut io, 1, 4).unwrap(), 0x1234);
        assert_eq!(
            io.writes[0].1,
            (4 << E1000_I2CCMD_REG_ADDR_SHIFT)
                | (1 << E1000_I2CCMD_PHY_ADDR_SHIFT)
                | E1000_I2CCMD_OPCODE_READ
        );
        write_phy_reg_i2c(&mut io, 1, 5, 0x1234).unwrap();
        assert_eq!(io.writes.last().unwrap().1 & 0xffff, 0x3412);
        assert!(write_phy_reg_i2c(&mut io, 0, 5, 0).is_err());
        assert_eq!(read_sfp_data_byte(&mut io, 0x1ff).unwrap(), 0x12);
        write_sfp_data_byte(&mut io, 0x10, 0xcd).unwrap();
        assert_eq!(io.writes.last().unwrap().1 & 0xffff, 0x34cd);
        assert!(read_sfp_data_byte(&mut io, 0x200).is_err());
        io.i2c_error = true;
        assert!(read_phy_reg_i2c(&mut io, 1, 1).is_err());
    }

    #[test]
    fn generic_m88_and_igp_helpers_map_pages_and_release_locks() {
        let mut io = Io {
            mdic_data: 0x1234,
            ..Io::default()
        };
        assert_eq!(read_phy_reg_m88(&mut io, 0x23, true).unwrap(), 0x1234);
        assert_eq!(io.mdic_reads, [3]);
        assert_eq!((io.locks, io.unlocks), (1, 1));
        write_phy_reg_m88(&mut io, 0x24, 0xabcd, true).unwrap();
        assert_eq!(io.mdic_writes, [(4, 0xabcd)]);

        io.mdic_writes.clear();
        assert_eq!(read_phy_reg_igp(&mut io, 0x21, true).unwrap(), 0x1234);
        assert_eq!(io.mdic_writes, [(IGP01E1000_PHY_PAGE_SELECT, 0x21)]);
        assert_eq!(io.mdic_reads.last(), Some(&1));
        assert_eq!(set_page_igp(&mut io, 4).unwrap(), 1);
        assert_eq!(
            io.mdic_writes.last(),
            Some(&(IGP01E1000_PHY_PAGE_SELECT, 4))
        );
        let before = io.locks;
        write_phy_reg_igp_locked(&mut io, 0x22, 0x55aa).unwrap();
        assert_eq!(io.locks, before);
        assert_eq!(
            io.mdic_writes[io.mdic_writes.len() - 2],
            (IGP01E1000_PHY_PAGE_SELECT, 0x22)
        );
        assert_eq!(io.mdic_writes.last(), Some(&(2, 0x55aa)));
    }

    #[test]
    fn generic_kmrn_read_write_preserve_acquire_flush_delay_order() {
        let mut io = Io {
            kmrn_data: 0x55aa,
            ..Io::default()
        };
        assert_eq!(read_kmrn_reg_generic(&mut io, 3, true).unwrap(), 0x55aa);
        assert_eq!((io.locks, io.unlocks), (1, 1));
        assert_eq!(io.delay, 2);
        assert!(io.writes[0].1 & KMRNCTRLSTA_REN != 0);
        write_kmrn_reg_generic(&mut io, 5, 0x1234, true).unwrap();
        assert_eq!(
            io.writes.last().unwrap().1,
            (5 << KMRNCTRLSTA_OFFSET_SHIFT) | 0x1234
        );
        let locks = io.locks;
        write_kmrn_reg_locked(&mut io, 7, 0x4321).unwrap();
        assert_eq!(io.locks, locks);
    }

    #[test]
    fn generic_phy_polling_reads_sticky_status_twice_and_bounds_wait() {
        let mut linked = Io::default();
        linked.phy[PHY_STATUS as usize] = MII_SR_LINK_STATUS | MII_SR_AUTONEG_COMPLETE;
        assert!(phy_has_link_generic(&mut linked, 10, 25, true).unwrap());
        assert!(wait_autoneg(&mut linked, true).is_ok());
        assert_eq!(linked.delay, 0);

        let mut down = Io::default();
        assert!(!phy_has_link_generic(&mut down, 2, 10, true).unwrap());
        assert_eq!(down.delay, 20);
        wait_autoneg(&mut down, true).unwrap();
        assert_eq!(down.delay, 4_500_020);
    }

    #[test]
    fn generic_copper_autoneg_and_master_slave_setup_preserve_register_policy() {
        let mut io = Io::default();
        io.phy[MII_AUTONEG_ADV as usize] = 0xffff;
        io.phy[PHY_CONTROL as usize] = 0x8000;
        let mut config = AutonegConfig {
            advertised: 0,
            mask: ADVERTISE_10_HALF,
            flow_control: FlowControlMode::None,
        };
        let mut pending = false;
        copper_link_autoneg(&mut io, &mut config, true, true, &mut pending).unwrap();
        assert_eq!(config.advertised, ADVERTISE_10_HALF);
        assert!(pending);
        assert_eq!(
            io.phy_writes.last(),
            Some(&(
                PHY_CONTROL,
                0x8000 | MII_CR_AUTO_NEG_EN | MII_CR_RESTART_AUTO_NEG
            ))
        );

        io.phy[MII_1000T_CTRL as usize] = CR_1000T_MS_ENABLE | CR_1000T_MS_VALUE;
        let original = set_master_slave_mode(&mut io, MasterSlaveMode::ForceSlave).unwrap();
        assert_eq!(original, MasterSlaveMode::ForceMaster);
        assert_eq!(
            io.phy_writes.last(),
            Some(&(MII_1000T_CTRL, CR_1000T_MS_ENABLE))
        );
    }

    #[test]
    fn generic_phy_type_scan_and_igp3_script_preserve_source_table() {
        assert_eq!(get_phy_type_from_id(0x0141_0c30), E1000PhyType::M88);
        assert_eq!(get_phy_type_from_id(0x02a8_0390), E1000PhyType::Igp3);
        assert_eq!(get_phy_type_from_id(0), E1000PhyType::Unknown);
        let mut io = Io::default();
        io.use_phy_ids = true;
        io.phy_ids[3] = (0x0141, 0x0cc2);
        assert_eq!(
            determine_phy_address(&mut io).unwrap(),
            (3, 0x0141_0cc0, E1000PhyType::M88)
        );
        assert_eq!(io.phy_address, 3);

        let mut script = Io::default();
        phy_init_script_igp3(&mut script).unwrap();
        assert_eq!(script.mdic_writes.len(), 32);
        assert_eq!(script.mdic_writes[0], (0x2f5b, 0x9018));
        assert_eq!(script.mdic_writes[31], (0x0000, 0x1340));
        let mut delay = Io::default();
        get_cfg_done_generic(&mut delay).unwrap();
        assert_eq!(delay.delay, 10_000);
    }

    #[test]
    fn generic_cable_polarity_downshift_and_length_decode() {
        let mut io = Io::default();
        io.phy[M88E1000_PHY_SPEC_STATUS as usize] = M88E1000_PSSR_REV_POLARITY
            | M88E1000_PSSR_DOWNSHIFT
            | (3 << M88E1000_PSSR_CABLE_LENGTH_SHIFT);
        let mut diagnostics = PhyDiagnostics::default();
        check_polarity_m88(&mut io, &mut diagnostics).unwrap();
        assert_eq!(diagnostics.cable_polarity, Some(CablePolarity::Reversed));
        check_downshift_generic(&mut io, E1000PhyType::M88, &mut diagnostics).unwrap();
        assert!(diagnostics.speed_downgraded);
        get_cable_length_m88(&mut io, &mut diagnostics).unwrap();
        assert_eq!(
            (
                diagnostics.min_cable_length,
                diagnostics.max_cable_length,
                diagnostics.cable_length
            ),
            (110, 140, 125)
        );
        check_downshift_generic(&mut io, E1000PhyType::Unknown, &mut diagnostics).unwrap();
        assert!(!diagnostics.speed_downgraded);

        io.phy[IGP01E1000_PHY_LINK_HEALTH as usize] = IGP01E1000_PLHR_SS_DOWNGRADE;
        check_downshift_generic(&mut io, E1000PhyType::Igp2, &mut diagnostics).unwrap();
        assert!(diagnostics.speed_downgraded);

        let mut diagnostics = PhyDiagnostics::default();
        check_polarity_igp(
            |register| {
                Ok(if register == IGP01E1000_PHY_PORT_STATUS {
                    IGP01E1000_PSSR_SPEED_1000MBPS
                } else {
                    IGP01E1000_PHY_POLARITY_MASK
                })
            },
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(diagnostics.cable_polarity, Some(CablePolarity::Reversed));
        let mut io = Io::default();
        io.phy[IFE_PHY_EXTENDED_STATUS_CONTROL as usize] = IFE_PESC_POLARITY_REVERSED;
        check_polarity_ife(&mut io, true, &mut diagnostics).unwrap();
        assert_eq!(diagnostics.cable_polarity, Some(CablePolarity::Reversed));
        io.phy[I82577_PHY_STATUS_2 as usize] = I82577_PHY_STATUS2_REV_POLARITY;
        check_polarity_82577(&mut io, &mut diagnostics).unwrap();
        assert_eq!(diagnostics.cable_polarity, Some(CablePolarity::Reversed));
    }

    #[test]
    fn generic_d3_lplu_and_smartspeed_states_are_mutually_exclusive() {
        let mut io = Io::default();
        io.phy[IGP02E1000_PHY_POWER_MGMT as usize] = 0x8000;
        io.phy[IGP01E1000_PHY_PORT_CONFIG as usize] = IGP01E1000_PSCFR_SMART_SPEED;
        set_d3_lplu_state_generic(&mut io, true, ALL_NOT_GIG, SmartSpeedMode::Default, true)
            .unwrap();
        assert_eq!(
            io.phy_writes[0],
            (IGP02E1000_PHY_POWER_MGMT, 0x8000 | IGP02E1000_PM_D3_LPLU)
        );
        assert_eq!(io.phy_writes[1], (IGP01E1000_PHY_PORT_CONFIG, 0));
        let writes = io.phy_writes.len();
        set_d3_lplu_state_generic(
            &mut io,
            true,
            ADVERTISE_100_FULL,
            SmartSpeedMode::Default,
            true,
        )
        .unwrap();
        assert_eq!(io.phy_writes.len(), writes);
        set_d3_lplu_state_generic(&mut io, false, ADVERTISE_100_FULL, SmartSpeedMode::On, true)
            .unwrap();
        assert_eq!(
            io.phy_writes.last(),
            Some(&(IGP01E1000_PHY_PORT_CONFIG, IGP01E1000_PSCFR_SMART_SPEED))
        );
    }

    #[test]
    fn generic_igp2_cable_length_uses_trimmed_four_channel_agc_average() {
        let mut diagnostics = PhyDiagnostics::default();
        let mut registers = alloc::vec::Vec::new();
        get_cable_length_igp_2(
            |register| {
                registers.push(register);
                let index = match register {
                    0x11b1 => 20,
                    0x12b1 => 40,
                    0x14b1 => 60,
                    _ => 80,
                };
                Ok((index as u16) << IGP02E1000_AGC_LENGTH_SHIFT)
            },
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(registers, IGP02E1000_PHY_AGC_REGS);
        assert_eq!(
            (
                diagnostics.min_cable_length,
                diagnostics.max_cable_length,
                diagnostics.cable_length
            ),
            (33, 63, 48)
        );
        assert!(get_cable_length_igp_2(|_| Ok(0), &mut diagnostics).is_err());
    }

    #[test]
    fn generic_m88_gen2_cable_length_supports_i210_meters_and_paged_legacy() {
        let mut diagnostics = PhyDiagnostics::default();
        let mut writes = alloc::vec::Vec::new();
        get_cable_length_m88_gen2(
            |register| {
                Ok(
                    if register == ((0x7 << GS40G_PAGE_SHIFT) | (I347AT4_PCDL + 1)) {
                        250
                    } else {
                        0
                    },
                )
            },
            |register, value| {
                writes.push((register, value));
                Ok(())
            },
            I210_PHY_ID,
            1,
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(
            (diagnostics.min_cable_length, diagnostics.max_cable_length),
            (2, 2)
        );

        writes.clear();
        get_cable_length_m88_gen2(
            |register| {
                Ok(match register {
                    I347AT4_PAGE_SELECT => 3,
                    value if value == I347AT4_PCDL + 2 => 25,
                    I347AT4_PCDC => I347AT4_PCDC_CABLE_LENGTH_UNIT,
                    _ => 0,
                })
            },
            |register, value| {
                writes.push((register, value));
                Ok(())
            },
            M88E1543_PHY_ID,
            2,
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(diagnostics.cable_length, 25);
        assert_eq!(writes, [(I347AT4_PAGE_SELECT, 7), (I347AT4_PAGE_SELECT, 3)]);

        writes.clear();
        get_cable_length_m88_gen2(
            |register| {
                Ok(if register == I347AT4_PAGE_SELECT {
                    4
                } else {
                    3 << M88E1000_PSSR_CABLE_LENGTH_SHIFT
                })
            },
            |register, value| {
                writes.push((register, value));
                Ok(())
            },
            M88E1112_PHY_ID,
            0,
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(diagnostics.cable_length, 125);
        assert_eq!(writes, [(I347AT4_PAGE_SELECT, 5), (I347AT4_PAGE_SELECT, 4)]);
    }

    #[test]
    fn generic_force_speed_duplex_updates_mac_phy_and_disables_flow_control() {
        let mut io = Io::default();
        let mut flow = FlowControlMode::Full;
        let phy = phy_force_speed_duplex_setup(
            &mut io,
            &mut flow,
            ADVERTISE_100_FULL,
            MII_CR_AUTO_NEG_EN | MII_CR_SPEED_1000,
        )
        .unwrap();
        assert_eq!(flow, FlowControlMode::None);
        assert_eq!(phy, MII_CR_FULL_DUPLEX | MII_CR_SPEED_100);
        assert_eq!(
            io.writes.last(),
            Some(&(
                E1000_CTRL,
                E1000_CTRL_FRCSPD | E1000_CTRL_FRCDPX | E1000_CTRL_FD | E1000_CTRL_SPD_100
            ))
        );
    }

    #[test]
    fn generic_igp_force_speed_duplex_disables_mdix_and_rechecks_link() {
        let mut io = Io::default();
        io.phy[PHY_CONTROL as usize] = MII_CR_AUTO_NEG_EN;
        io.phy[IGP01E1000_PHY_PORT_CTRL as usize] = 0xffff;
        io.phy[PHY_STATUS as usize] = MII_SR_LINK_STATUS;
        let mut flow = FlowControlMode::Full;
        phy_force_speed_duplex_igp(&mut io, ADVERTISE_100_FULL, true, &mut flow).unwrap();
        assert_eq!(flow, FlowControlMode::None);
        assert_eq!(
            io.phy_writes
                .iter()
                .find(|(reg, _)| *reg == IGP01E1000_PHY_PORT_CTRL)
                .unwrap()
                .1,
            0xffff & !IGP01E1000_PSCR_AUTO_MDIX & !IGP01E1000_PSCR_FORCE_MDI_MDIX
        );
        assert_eq!(io.delay, 1);
    }

    #[test]
    fn generic_m88_force_speed_commits_resets_dsp_and_restores_tx_clock() {
        let mut io = Io::default();
        io.phy[PHY_CONTROL as usize] = MII_CR_AUTO_NEG_EN;
        let mut flow = FlowControlMode::Full;
        let mut polls = 0;
        let mut commits = 0;
        phy_force_speed_duplex_m88(
            &mut io,
            E1000PhyType::M88,
            0x0141_0c30,
            ADVERTISE_100_FULL,
            true,
            &mut flow,
            |iterations, interval| {
                assert_eq!((iterations, interval), (PHY_FORCE_LIMIT, 100_000));
                polls += 1;
                Ok(polls > 1)
            },
            || {
                commits += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(polls, 2);
        assert_eq!(commits, 1);
        assert!(io.phy_writes.contains(&(M88E1000_PHY_PAGE_SELECT, 0x1d)));
        assert!(io.phy_writes.contains(&(M88E1000_PHY_GEN_CONTROL, 0xc1)));
        assert_eq!(
            io.phy_writes.last().unwrap(),
            &(M88E1000_EXT_PHY_SPEC_CTRL, M88E1000_EPSCR_TX_CLK_25)
        );
        assert_eq!(flow, FlowControlMode::None);
    }

    #[test]
    fn generic_phy_info_m88_igp_and_ife_follow_link_speed_branches() {
        let mut m88 = Io::default();
        m88.phy[M88E1000_PHY_SPEC_CTRL as usize] = M88E1000_PSCR_POLARITY_REVERSAL;
        m88.phy[M88E1000_PHY_SPEC_STATUS as usize] = M88E1000_PSSR_MDIX | M88E1000_PSSR_1000MBS;
        m88.phy[PHY_1000T_STATUS as usize] = SR_1000T_LOCAL_RX_STATUS;
        let mut diagnostics = PhyDiagnostics::default();
        let mut cable = false;
        get_phy_info_m88(
            &mut m88,
            true,
            || {
                cable = true;
                Ok(())
            },
            &mut diagnostics,
        )
        .unwrap();
        assert!(cable);
        assert!(diagnostics.polarity_correction);
        assert!(diagnostics.is_mdix);
        assert_eq!(diagnostics.local_rx, ReceiverStatus::Ok);
        assert_eq!(diagnostics.remote_rx, ReceiverStatus::NotOk);

        let mut regs = [0u16; 0x200];
        regs[IGP01E1000_PHY_PORT_STATUS as usize] =
            IGP01E1000_PSSR_SPEED_1000MBPS | IGP01E1000_PSSR_MDIX;
        regs[IGP01E1000_PHY_PCS_INIT_REG as usize] = IGP01E1000_PHY_POLARITY_MASK;
        regs[PHY_1000T_STATUS as usize] = SR_1000T_REMOTE_RX_STATUS;
        let mut diagnostics = PhyDiagnostics::default();
        get_phy_info_igp(
            |register| Ok(regs[register as usize]),
            true,
            || Ok(()),
            &mut diagnostics,
        )
        .unwrap();
        assert!(diagnostics.is_mdix);
        assert_eq!(diagnostics.local_rx, ReceiverStatus::NotOk);
        assert_eq!(diagnostics.remote_rx, ReceiverStatus::Ok);

        let mut diagnostics = PhyDiagnostics::default();
        get_phy_info_ife(
            |register| {
                Ok(match register {
                    value if value == u32::from(IFE_PHY_SPECIAL_CONTROL) => {
                        IFE_PSC_FORCE_POLARITY | IFE_PSC_AUTO_POLARITY_DISABLE
                    }
                    value if value == u32::from(IFE_PHY_MDIX_CONTROL) => IFE_PMC_MDIX_STATUS,
                    _ => 0,
                })
            },
            true,
            &mut diagnostics,
        )
        .unwrap();
        assert!(!diagnostics.polarity_correction);
        assert_eq!(diagnostics.cable_polarity, Some(CablePolarity::Reversed));
        assert!(diagnostics.is_mdix);
        assert_eq!(diagnostics.cable_length, CABLE_LENGTH_UNDEFINED);
    }

    #[test]
    fn generic_ife_force_speed_disables_autocrossover_and_rechecks_link() {
        let mut io = Io::default();
        io.phy[PHY_CONTROL as usize] = MII_CR_AUTO_NEG_EN;
        io.phy[IFE_PHY_MDIX_CONTROL as usize] = 0xffff;
        io.phy[PHY_STATUS as usize] = MII_SR_LINK_STATUS;
        let mut flow = FlowControlMode::Full;
        phy_force_speed_duplex_ife(&mut io, ADVERTISE_100_FULL, true, &mut flow).unwrap();
        assert_eq!(flow, FlowControlMode::None);
        assert_eq!(
            io.phy_writes
                .iter()
                .find(|(reg, _)| *reg == IFE_PHY_MDIX_CONTROL)
                .unwrap()
                .1,
            0xffff & !IFE_PMC_AUTO_MDIX & !IFE_PMC_FORCE_MDIX
        );
        assert_eq!(io.delay, 1);
    }

    #[test]
    fn generic_phy_reset_and_copper_power_transitions_preserve_delays() {
        let mut io = Io::default();
        io.phy[PHY_CONTROL as usize] = 0x1000;
        phy_sw_reset_generic(&mut io, true).unwrap();
        assert_eq!(
            io.phy_writes.last(),
            Some(&(PHY_CONTROL, 0x1000 | MII_CR_RESET))
        );
        assert_eq!(io.delay, 1);
        power_down_phy_copper(&mut io);
        assert_eq!(
            io.phy_writes.last(),
            Some(&(PHY_CONTROL, 0x1000 | MII_CR_POWER_DOWN))
        );
        assert_eq!(io.delay, 1001);
        power_up_phy_copper(&mut io);
        assert_eq!(io.phy_writes.last(), Some(&(PHY_CONTROL, 0x1000)));

        let mut hw = Io::default();
        phy_hw_reset_generic(&mut hw, true, 5000).unwrap();
        assert!(hw.writes.is_empty());
        phy_hw_reset_generic(&mut hw, false, 5000).unwrap();
        assert_eq!(hw.writes[0], (E1000_CTRL, E1000_CTRL_PHY_RST));
        assert_eq!(hw.writes[1], (E1000_CTRL, 0));
        assert_eq!(hw.delay, 15_150);
        assert_eq!((hw.locks, hw.unlocks), (1, 1));
    }

    #[test]
    fn generic_82577_link_setup_programs_crs_downshift_mdix_and_master_slave() {
        let mut io = Io::default();
        io.phy[I82577_CFG_REG as usize] = 0x0001;
        io.phy[I82577_PHY_CTRL_2 as usize] = 0xffff;
        io.phy[MII_1000T_CTRL as usize] = CR_1000T_MS_ENABLE;
        let mut reset = false;
        let original =
            copper_link_setup_82577(&mut io, true, 2, MasterSlaveMode::ForceMaster, || {
                reset = true;
                Ok(())
            })
            .unwrap();
        assert!(reset);
        assert_eq!(original, MasterSlaveMode::ForceSlave);
        assert_eq!(
            io.phy_writes[0],
            (
                I82577_CFG_REG,
                0x0001 | I82577_CFG_ASSERT_CRS_ON_TX | I82577_CFG_ENABLE_DOWNSHIFT
            )
        );
        assert_eq!(
            io.phy_writes[1],
            (
                I82577_PHY_CTRL_2,
                (0xffff & !I82577_PHY_CTRL2_MDIX_CFG_MASK) | I82577_PHY_CTRL2_MANUAL_MDIX
            )
        );
        assert_eq!(
            io.phy_writes[2],
            (MII_1000T_CTRL, CR_1000T_MS_ENABLE | CR_1000T_MS_VALUE)
        );
    }

    #[test]
    fn generic_m88_copper_setup_preserves_mdi_polarity_and_revision_workaround() {
        let mut io = Io::default();
        io.phy[M88E1000_PHY_SPEC_CTRL as usize] = 0;
        io.phy[M88E1000_EXT_PHY_SPEC_CTRL as usize] = 0xffff;
        let mut commits = 0;
        copper_link_setup_m88(
            &mut io,
            E1000PhyType::M88,
            M88E1111_I_PHY_ID,
            2,
            3,
            true,
            || {
                commits += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(io.phy_writes[0], (M88E1000_PHY_SPEC_CTRL, 0x0842));
        let extended = io.phy_writes[1].1;
        assert_eq!(
            extended & M88E1000_EPSCR_TX_CLK_25,
            M88E1000_EPSCR_TX_CLK_25
        );
        assert_eq!(
            extended & M88EC018_EPSCR_DOWNSHIFT_COUNTER_MASK,
            M88EC018_EPSCR_DOWNSHIFT_COUNTER_5X
        );
        assert_eq!(commits, 1);
    }

    #[test]
    fn generic_m88_gen2_setup_commits_downshift_and_master_slave_policy() {
        let mut io = Io::default();
        io.phy[M88E1000_PHY_SPEC_CTRL as usize] = 0xffff;
        let mut commits = 0;
        copper_link_setup_m88_gen2(
            &mut io,
            M88E1543_PHY_ID,
            2,
            true,
            MasterSlaveMode::ForceMaster,
            || {
                commits += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(commits, 2);
        assert_eq!(io.phy_writes[0].1 & I347AT4_PSCR_DOWNSHIFT_ENABLE, 0);
        assert_eq!(
            io.phy_writes[1].1 & (I347AT4_PSCR_DOWNSHIFT_MASK | I347AT4_PSCR_DOWNSHIFT_ENABLE),
            I347AT4_PSCR_DOWNSHIFT_6X | I347AT4_PSCR_DOWNSHIFT_ENABLE
        );
        assert_eq!(
            io.phy_writes.last().unwrap(),
            &(MII_1000T_CTRL, CR_1000T_MS_ENABLE | CR_1000T_MS_VALUE)
        );
    }

    #[test]
    fn generic_igp_setup_resets_disables_lplu_and_configures_mdix() {
        let mut io = Io::default();
        io.phy[MII_1000T_CTRL as usize] = CR_1000T_MS_ENABLE | CR_1000T_MS_VALUE;
        let events = core::cell::Cell::new(0u8);
        let mode = copper_link_setup_igp(
            &mut io,
            true,
            2,
            true,
            ADVERTISE_1000_FULL,
            true,
            || {
                events.set(events.get() | 1);
                Ok(())
            },
            |active| {
                assert!(!active);
                events.set(events.get() | 2);
                Ok(())
            },
            |active| {
                assert!(!active);
                events.set(events.get() | 4);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(mode, Some(MasterSlaveMode::ForceMaster));
        assert_eq!(events.get(), 7);
        assert_eq!(io.delay, 100_000);
        assert_eq!(io.phy_writes[0].0, IGP01E1000_PHY_PORT_CTRL);
        assert_ne!(io.phy_writes[0].1 & IGP01E1000_PSCR_FORCE_MDI_MDIX, 0);
        assert_eq!(io.phy_writes[1], (IGP01E1000_PHY_PORT_CONFIG, 0));
        assert_eq!(io.phy_writes[2], (MII_1000T_CTRL, CR_1000T_MS_VALUE));
        assert_eq!(io.phy_writes[3], (MII_1000T_CTRL, CR_1000T_MS_VALUE));
    }

    #[test]
    fn generic_copper_link_setup_runs_autoneg_or_forced_path_then_checks_link() {
        let mut io = Io::default();
        io.phy[PHY_STATUS as usize] = MII_SR_LINK_STATUS;
        let mut config = AutonegConfig {
            advertised: ADVERTISE_10_FULL,
            mask: ADVERTISE_10_FULL,
            flow_control: FlowControlMode::Full,
        };
        let mut force_called = false;
        let mut fc_called = false;
        let mut link_status_pending = false;
        assert!(
            setup_copper_link_generic(
                &mut io,
                true,
                &mut config,
                true,
                true,
                &mut link_status_pending,
                || {
                    force_called = true;
                    Ok(())
                },
                || {
                    fc_called = true;
                    Ok(())
                },
            )
            .unwrap()
        );
        assert!(!force_called);
        assert!(fc_called);
        assert!(link_status_pending);
        assert!(
            io.phy_writes
                .iter()
                .any(|(register, value)| *register == PHY_CONTROL
                    && value & MII_CR_RESTART_AUTO_NEG != 0)
        );
    }
}
