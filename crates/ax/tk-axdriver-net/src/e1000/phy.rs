//! Shared Intel e1000 PHY operations.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_phy.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::DevResult;

use super::{mac::E1000PhyRegisterIo, osdep::E1000RegisterIo, registers::*};

const PHY_ID1: u8 = 0x02;
const PHY_ID2: u8 = 0x03;
const PHY_REVISION_MASK: u16 = 0x000f;
const M88E1000_PHY_GEN_CONTROL: u8 = 0x1b;
const E1000_BLK_PHY_RESET: u8 = 12;

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
        phy: [u16; 4],
        phy_writes: alloc::vec::Vec<(u8, u16)>,
        delay: usize,
    }
    impl E1000RegisterIo for Io {
        fn read_register(&mut self, register: u32) -> DevResult<u32> {
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
}
