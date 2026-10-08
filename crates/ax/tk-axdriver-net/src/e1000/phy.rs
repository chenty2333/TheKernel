//! Shared Intel e1000 PHY operations.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_phy.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{
    mac::{E1000PhyRegisterIo, FlowControlMode},
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
const MII_AUTONEG_ADV: u8 = 0x04;
const MII_1000T_CTRL: u8 = 0x09;
const IGP01E1000_PHY_PAGE_SELECT: u32 = 0x1f;
const MAX_PHY_MULTI_PAGE_REG: u32 = 0x0f;
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

pub trait E1000PhyMdicOps {
    fn read_mdic(&mut self, offset: u32) -> DevResult<u16>;
    fn write_mdic(&mut self, offset: u32, data: u16) -> DevResult;
    fn acquire(&mut self) -> DevResult;
    fn release(&mut self);
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
        phy: [u16; 16],
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
    }
    impl E1000RegisterIo for Io {
        fn read_register(&mut self, register: u32) -> DevResult<u32> {
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
}
