//! Linux i2c-dev ioctl ABI values and x86_64 wire layouts.
//! The ioctl numbers/layouts follow Linux's stable I2C UAPI; controller
//! behavior is implemented by the translated FreeBSD ig4 iicbus backend.

pub const I2C_RETRIES: u32 = 0x0701;
pub const I2C_TIMEOUT: u32 = 0x0702;
pub const I2C_SLAVE: u32 = 0x0703;
pub const I2C_TENBIT: u32 = 0x0704;
pub const I2C_FUNCS: u32 = 0x8008_0705;
pub const I2C_SLAVE_FORCE: u32 = 0x0706;
pub const I2C_RDWR: u32 = 0xc010_0707;
pub const I2C_PEC: u32 = 0x0708;
pub const I2C_SMBUS: u32 = 0xc010_0720;
pub const I2C_RDWR_IOCTL_MAX_MSGS: usize = 42;
pub const I2C_M_RD: u16 = 0x0001;
pub const I2C_M_NOSTART: u16 = 0x4000;
pub const I2C_M_STOP: u16 = 0x8000;
pub const I2C_FUNC_I2C: u64 = 0x0000_0001;
pub const I2C_FUNC_SMBUS_READ_BYTE: u64 = 0x0002_0000;
pub const I2C_FUNC_SMBUS_WRITE_BYTE: u64 = 0x0004_0000;
pub const I2C_FUNC_SMBUS_READ_BYTE_DATA: u64 = 0x0008_0000;
pub const I2C_FUNC_SMBUS_WRITE_BYTE_DATA: u64 = 0x0010_0000;
pub const I2C_FUNC_SMBUS_READ_WORD_DATA: u64 = 0x0020_0000;
pub const I2C_FUNC_SMBUS_WRITE_WORD_DATA: u64 = 0x0040_0000;
pub const I2C_FUNC_SMBUS_READ_BLOCK_DATA: u64 = 0x0100_0000;
pub const I2C_FUNC_SMBUS_WRITE_BLOCK_DATA: u64 = 0x0200_0000;
pub const I2C_FUNC_SMBUS_READ_I2C_BLOCK: u64 = 0x0400_0000;
pub const I2C_FUNC_SMBUS_WRITE_I2C_BLOCK: u64 = 0x0800_0000;
pub const I2C_SMBUS_BLOCK_MAX: usize = 32;

pub const I2C_SMBUS_QUICK: u32 = 0;
pub const I2C_SMBUS_BYTE: u32 = 1;
pub const I2C_SMBUS_BYTE_DATA: u32 = 2;
pub const I2C_SMBUS_WORD_DATA: u32 = 3;
pub const I2C_SMBUS_PROC_CALL: u32 = 4;
pub const I2C_SMBUS_BLOCK_DATA: u32 = 5;
pub const I2C_SMBUS_I2C_BLOCK_BROKEN: u32 = 6;
pub const I2C_SMBUS_BLOCK_PROC_CALL: u32 = 7;
pub const I2C_SMBUS_I2C_BLOCK_DATA: u32 = 8;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct I2cMsg {
    pub address: u16,
    pub flags: u16,
    pub length: u16,
    pub buffer: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct I2cRdwrIoctlData {
    pub messages: u64,
    pub count: u32,
    pub reserved: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct I2cSmbusIoctlData {
    pub read_write: u8,
    pub command: u8,
    pub size: u32,
    pub data: u64,
}

const _: () = {
    assert!(core::mem::size_of::<I2cMsg>() == 16);
    assert!(core::mem::offset_of!(I2cMsg, buffer) == 8);
    assert!(core::mem::size_of::<I2cRdwrIoctlData>() == 16);
    assert!(core::mem::size_of::<I2cSmbusIoctlData>() == 16);
};

pub const fn functions() -> u64 {
    I2C_FUNC_I2C
        | I2C_FUNC_SMBUS_READ_BYTE
        | I2C_FUNC_SMBUS_WRITE_BYTE
        | I2C_FUNC_SMBUS_READ_BYTE_DATA
        | I2C_FUNC_SMBUS_WRITE_BYTE_DATA
        | I2C_FUNC_SMBUS_READ_WORD_DATA
        | I2C_FUNC_SMBUS_WRITE_WORD_DATA
        | I2C_FUNC_SMBUS_READ_I2C_BLOCK
        | I2C_FUNC_SMBUS_WRITE_I2C_BLOCK
}
