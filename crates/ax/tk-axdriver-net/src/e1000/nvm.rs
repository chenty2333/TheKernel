//! Intel e1000 EEPROM/NVM protocol helpers.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_nvm.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{osdep::E1000RegisterIo, registers::*};

const NVM_POLL_WRITE: u32 = 1;
const NVM_POLL_READ: u32 = 0;
const NVM_RW_ADDR_SHIFT: u32 = 2;
const NVM_EERD_START: u32 = E1000_NVM_RW_REG_START;
const NVM_EERD_DONE: u32 = E1000_NVM_RW_REG_DONE;
const NVM_EERD_DATA_SHIFT: u32 = E1000_NVM_RW_REG_DATA;
const NVM_MAX_RETRY_SPI: usize = 5000;
const NVM_READ_OPCODE_MICROWIRE: u16 = 0x6;
const NVM_WRITE_OPCODE_MICROWIRE: u16 = 0x5;
const NVM_EWEN_OPCODE_MICROWIRE: u16 = 0x13;
const NVM_EWDS_OPCODE_MICROWIRE: u16 = 0x10;
const NVM_READ_OPCODE_SPI: u16 = 0x03;
const NVM_WRITE_OPCODE_SPI: u16 = 0x02;
const NVM_A8_OPCODE_SPI: u16 = 0x08;
const NVM_WREN_OPCODE_SPI: u16 = 0x06;
const NVM_RDSR_OPCODE_SPI: u16 = 0x05;
const NVM_STATUS_RDY_SPI: u8 = 0x01;
const NVM_CHECKSUM_REG: usize = 0x3f;
const NVM_SUM: u16 = 0xbaba;
const NVM_PBA_OFFSET_0: usize = 8;
const NVM_PBA_OFFSET_1: usize = 9;
const NVM_PBA_PTR_GUARD: u16 = 0xfafa;
const E1000_PBANUM_LENGTH: usize = 11;
const NVM_PBA_SECTION_INVALID: u16 = 0xffff;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E1000NvmType {
    Microwire,
    Spi,
    Eerd,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct E1000NvmConfig {
    pub kind: E1000NvmType,
    pub word_size: u16,
    pub delay_usec: u32,
    pub opcode_bits: u16,
    pub address_bits: u16,
    pub page_size: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E1000NvmCallback {
    NullOps,
    NullReadNvm,
    NullWriteNvm,
    NullRelease,
    ReloadGeneric,
    NullLedDefault,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct E1000NvmOps {
    pub init_params: E1000NvmCallback,
    pub acquire: E1000NvmCallback,
    pub read: E1000NvmCallback,
    pub release: E1000NvmCallback,
    pub reload: E1000NvmCallback,
    pub update: E1000NvmCallback,
    pub valid_led_default: E1000NvmCallback,
    pub validate: E1000NvmCallback,
    pub write: E1000NvmCallback,
}

/// upstream: e1000_nvm.c e1000_init_nvm_ops_generic()
pub const fn init_nvm_ops_generic() -> E1000NvmOps {
    use E1000NvmCallback::*;
    E1000NvmOps {
        init_params: NullOps,
        acquire: NullOps,
        read: NullReadNvm,
        release: NullRelease,
        reload: ReloadGeneric,
        update: NullOps,
        valid_led_default: NullLedDefault,
        validate: NullOps,
        write: NullWriteNvm,
    }
}

/// upstream: e1000_nvm.c e1000_null_read_nvm()
pub const fn null_read_nvm(_offset: u16, _words: u16) -> DevResult<u16> {
    Ok(0)
}

/// upstream: e1000_nvm.c e1000_null_nvm_generic()
pub const fn null_nvm_generic() {}

/// upstream: e1000_nvm.c e1000_null_led_default()
pub const fn null_led_default() -> DevResult<u16> {
    Ok(0)
}

/// upstream: e1000_nvm.c e1000_null_write_nvm()
pub const fn null_write_nvm(_offset: u16, _words: &[u16]) -> DevResult {
    Ok(())
}

/// upstream: e1000_nvm.c e1000_raise_eec_clk()
pub fn raise_eec_clk<I: E1000RegisterIo>(io: &mut I, eecd: &mut u32, delay_usec: u32) -> DevResult {
    *eecd |= E1000_EECD_SK;
    io.write_register(E1000_EECD, *eecd)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(delay_usec);
    Ok(())
}

/// upstream: e1000_nvm.c e1000_lower_eec_clk()
pub fn lower_eec_clk<I: E1000RegisterIo>(io: &mut I, eecd: &mut u32, delay_usec: u32) -> DevResult {
    *eecd &= !E1000_EECD_SK;
    io.write_register(E1000_EECD, *eecd)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(delay_usec);
    Ok(())
}

/// upstream: e1000_nvm.c e1000_shift_out_eec_bits()
pub fn shift_out_eec_bits<I: E1000RegisterIo>(
    io: &mut I,
    nvm: E1000NvmConfig,
    data: u16,
    count: u16,
) -> DevResult {
    if count == 0 {
        return Ok(());
    }
    let mut eecd = io.read_register(E1000_EECD)?;
    let mut mask = 1u16 << (count - 1);
    match nvm.kind {
        E1000NvmType::Microwire => eecd &= !E1000_EECD_DO,
        E1000NvmType::Spi => eecd |= E1000_EECD_DO,
        E1000NvmType::Eerd => return Err(DevError::Unsupported),
    }
    while mask != 0 {
        eecd &= !E1000_EECD_DI;
        if data & mask != 0 {
            eecd |= E1000_EECD_DI;
        }
        io.write_register(E1000_EECD, eecd)?;
        let _ = io.read_register(E1000_STATUS)?;
        io.delay_us(nvm.delay_usec);
        raise_eec_clk(io, &mut eecd, nvm.delay_usec)?;
        lower_eec_clk(io, &mut eecd, nvm.delay_usec)?;
        mask >>= 1;
    }
    eecd &= !E1000_EECD_DI;
    io.write_register(E1000_EECD, eecd)
}

/// upstream: e1000_nvm.c e1000_shift_in_eec_bits()
pub fn shift_in_eec_bits<I: E1000RegisterIo>(
    io: &mut I,
    delay_usec: u32,
    count: u16,
) -> DevResult<u16> {
    let mut eecd = io.read_register(E1000_EECD)? & !(E1000_EECD_DO | E1000_EECD_DI);
    let mut data = 0u16;
    for _ in 0..count {
        data <<= 1;
        raise_eec_clk(io, &mut eecd, delay_usec)?;
        eecd = io.read_register(E1000_EECD)?;
        eecd &= !E1000_EECD_DI;
        if eecd & E1000_EECD_DO != 0 {
            data |= 1;
        }
        lower_eec_clk(io, &mut eecd, delay_usec)?;
    }
    Ok(data)
}

/// upstream: e1000_nvm.c e1000_poll_eerd_eewr_done()
pub fn poll_eerd_eewr_done<I: E1000RegisterIo>(io: &mut I, poll_write: bool) -> DevResult {
    let register = if poll_write { E1000_EEWR } else { E1000_EERD };
    for _ in 0..100_000 {
        if io.read_register(register)? & NVM_EERD_DONE != 0 {
            return Ok(());
        }
        io.delay_us(5);
    }
    Err(DevError::Io)
}

/// upstream: e1000_nvm.c e1000_acquire_nvm_generic()
pub fn acquire_nvm_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let eecd = io.read_register(E1000_EECD)?;
    io.write_register(E1000_EECD, eecd | E1000_EECD_REQ)?;
    let _ = io.read_register(E1000_EECD)?;
    for _ in 0..E1000_NVM_GRANT_ATTEMPTS {
        if io.read_register(E1000_EECD)? & E1000_EECD_GNT != 0 {
            return Ok(());
        }
        io.delay_us(5);
    }
    let eecd = io.read_register(E1000_EECD)? & !E1000_EECD_REQ;
    io.write_register(E1000_EECD, eecd)?;
    Err(DevError::ResourceBusy)
}

pub trait E1000NvmLock {
    fn acquire_nvm(&mut self) -> DevResult;
    fn release_nvm(&mut self);
}

/// NVM access callbacks used by the PBA and checksum helpers.
pub trait E1000NvmAccess {
    fn read_nvm_words(&mut self, offset: u16, words: u16) -> DevResult<alloc::vec::Vec<u16>>;
    fn write_nvm_words(&mut self, offset: u16, words: &[u16]) -> DevResult;
    fn flash_present(&mut self) -> bool { true }
    fn read_invm_version(&mut self) -> E1000FwVersion { E1000FwVersion::default() }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct E1000FwVersion {
    pub eep_major: u16,
    pub eep_minor: u16,
    pub eep_build: u16,
    pub etrack_id: u32,
    pub or_valid: bool,
    pub or_major: u16,
    pub or_build: u16,
    pub or_patch: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct E1000Pba {
    pub words: [u16; 2],
    pub block: alloc::vec::Vec<u16>,
}

/// upstream: e1000_nvm.c e1000_read_pba_length_generic()
pub fn read_pba_length_generic<A: E1000NvmAccess>(access: &mut A) -> DevResult<usize> {
    let prefix = access.read_nvm_words(NVM_PBA_OFFSET_0 as u16, 2)?;
    if prefix.len() != 2 {
        return Err(DevError::Io);
    }
    if prefix[0] != NVM_PBA_PTR_GUARD {
        return Ok(E1000_PBANUM_LENGTH);
    }
    let len = access.read_nvm_words(prefix[1], 1)?[0];
    if len == 0 || len == u16::MAX {
        return Err(DevError::Io);
    }
    Ok((usize::from(len) * 2) - 1)
}

/// upstream: e1000_nvm.c e1000_read_pba_num_generic()
pub fn read_pba_num_generic<A: E1000NvmAccess>(access: &mut A) -> DevResult<u32> {
    let words = access.read_nvm_words(NVM_PBA_OFFSET_0 as u16, 2)?;
    if words.len() != 2 {
        return Err(DevError::Io);
    }
    if words[0] == NVM_PBA_PTR_GUARD {
        return Err(DevError::Unsupported);
    }
    Ok((u32::from(words[0]) << 16) | u32::from(words[1]))
}

/// upstream: e1000_nvm.c e1000_get_pba_block_size()
pub fn get_pba_block_size(image: &[u16]) -> DevResult<u16> {
    if image.len() <= NVM_PBA_OFFSET_1 {
        return Err(DevError::InvalidParam);
    }
    if image[NVM_PBA_OFFSET_0] != NVM_PBA_PTR_GUARD {
        return Ok(0);
    }
    let start = usize::from(image[NVM_PBA_OFFSET_1]);
    let len = *image.get(start).ok_or(DevError::InvalidParam)?;
    if len == 0 || len == u16::MAX {
        return Err(DevError::Io);
    }
    Ok(len)
}

/// upstream: e1000_nvm.c e1000_read_pba_raw()
pub fn read_pba_raw<A: E1000NvmAccess>(
    access: &mut A,
    image: Option<&[u16]>,
    max_block_size: u16,
) -> DevResult<E1000Pba> {
    let words = if let Some(image) = image {
        if image.len() <= NVM_PBA_OFFSET_1 {
            return Err(DevError::InvalidParam);
        }
        [image[NVM_PBA_OFFSET_0], image[NVM_PBA_OFFSET_1]]
    } else {
        let data = access.read_nvm_words(NVM_PBA_OFFSET_0 as u16, 2)?;
        if data.len() != 2 { return Err(DevError::Io); }
        [data[0], data[1]]
    };
    let mut pba = E1000Pba { words, block: alloc::vec::Vec::new() };
    if words[0] == NVM_PBA_PTR_GUARD {
        let size = if let Some(image) = image { get_pba_block_size(image)? } else {
            access.read_nvm_words(words[1], 1)?[0]
        };
        if size > max_block_size { return Err(DevError::InvalidParam); }
        pba.block = if let Some(image) = image {
            let start = usize::from(words[1]);
            image.get(start..start + usize::from(size)).ok_or(DevError::InvalidParam)?.to_vec()
        } else { access.read_nvm_words(words[1], size)? };
    }
    Ok(pba)
}

/// upstream: e1000_nvm.c e1000_write_pba_raw()
pub fn write_pba_raw<A: E1000NvmAccess>(
    access: &mut A,
    image: Option<&mut [u16]>,
    pba: &E1000Pba,
) -> DevResult {
    if pba.words[0] == NVM_PBA_PTR_GUARD && pba.block.is_empty() {
        return Err(DevError::InvalidParam);
    }
    if let Some(image) = image {
        if image.len() <= NVM_PBA_OFFSET_1 { return Err(DevError::InvalidParam); }
        image[NVM_PBA_OFFSET_0..=NVM_PBA_OFFSET_1].copy_from_slice(&pba.words);
        if pba.words[0] == NVM_PBA_PTR_GUARD {
            let start = usize::from(pba.words[1]);
            let end = start.checked_add(usize::from(pba.block[0])).ok_or(DevError::InvalidParam)?;
            if end > image.len() || usize::from(pba.block[0]) > pba.block.len() { return Err(DevError::InvalidParam); }
            image[start..end].copy_from_slice(&pba.block[..usize::from(pba.block[0])]);
        }
    } else {
        access.write_nvm_words(NVM_PBA_OFFSET_0 as u16, &pba.words)?;
        if pba.words[0] == NVM_PBA_PTR_GUARD {
            access.write_nvm_words(pba.words[1], &pba.block[..usize::from(pba.block[0])])?;
        }
    }
    Ok(())
}

/// upstream: e1000_nvm.c e1000_read_pba_string_generic()
pub fn read_pba_string_generic<A: E1000NvmAccess>(
    access: &mut A,
    flashless_i210: bool,
    output_size: usize,
) -> DevResult<alloc::vec::Vec<u8>> {
    if flashless_i210 { return Err(DevError::Unsupported); }
    let first = access.read_nvm_words(NVM_PBA_OFFSET_0 as u16, 2)?;
    if first.len() != 2 { return Err(DevError::Io); }
    if first[0] != NVM_PBA_PTR_GUARD {
        if output_size < E1000_PBANUM_LENGTH { return Err(DevError::InvalidParam); }
        let mut out = alloc::vec::Vec::with_capacity(E1000_PBANUM_LENGTH);
        for nibble in [first[0] >> 12, first[0] >> 8, first[0] >> 4, first[0], first[1] >> 12, first[1] >> 8] {
            out.push(if nibble & 0xf < 10 { b'0' + (nibble & 0xf) as u8 } else { b'A' + ((nibble & 0xf) - 10) as u8 });
        }
        out.push(b'-');
        for nibble in [first[1] >> 4, first[1]] {
            out.push(if nibble & 0xf < 10 { b'0' + (nibble & 0xf) as u8 } else { b'A' + ((nibble & 0xf) - 10) as u8 });
        }
        out.push(0);
        return Ok(out);
    }
    let length = access.read_nvm_words(first[1], 1)?[0];
    if length == 0 || length == NVM_PBA_SECTION_INVALID { return Err(DevError::Io); }
    let output_len = usize::from(length).checked_mul(2).and_then(|v| v.checked_sub(1)).ok_or(DevError::InvalidParam)?;
    if output_size < output_len { return Err(DevError::InvalidParam); }
    let data = access.read_nvm_words(first[1] + 1, length - 1)?;
    if data.len() != usize::from(length - 1) { return Err(DevError::Io); }
    let mut out = alloc::vec::Vec::with_capacity(output_len);
    for word in data { out.extend_from_slice(&word.to_be_bytes()); }
    out.push(0);
    Ok(out)
}

/// upstream: e1000_nvm.c e1000_read_mac_addr_generic()
pub fn read_mac_addr_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult<[u8; 6]> {
    let low = io.read_register(E1000_RA)?;
    let high = io.read_register(E1000_RA + 4)?;
    Ok([
        low as u8, (low >> 8) as u8, (low >> 16) as u8, (low >> 24) as u8,
        high as u8, (high >> 8) as u8,
    ])
}

/// upstream: e1000_nvm.c e1000_reload_nvm_generic()
pub fn reload_nvm_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    io.delay_us(10);
    let ctrl_ext = io.read_register(E1000_CTRL_EXT)? | E1000_CTRL_EXT_EE_RST;
    io.write_register(E1000_CTRL_EXT, ctrl_ext)?;
    let _ = io.read_register(E1000_CTRL_EXT)?;
    Ok(())
}

/// upstream: e1000_nvm.c e1000_get_fw_version()
pub fn get_fw_version<A: E1000NvmAccess>(
    access: &mut A,
    mac: super::api::E1000MacType,
) -> E1000FwVersion {
    use super::api::E1000MacType as M;
    const NVM_VERSION: u16 = 0x0005;
    const NVM_ETRACK_WORD: u16 = 0x0042;
    const NVM_ETRACK_HIWORD: u16 = 0x0043;
    const NVM_COMB_VER_OFF: u16 = 0x0083;
    const NVM_COMB_VER_PTR: u16 = 0x003d;
    const NVM_MAJOR_MASK: u16 = 0xf000;
    const NVM_MINOR_MASK: u16 = 0x0ff0;
    const NVM_IMAGE_ID_MASK: u16 = 0x000f;
    const NVM_COMB_VER_MASK: u16 = 0x00ff;
    const NVM_ETRACK_VALID: u16 = 0x8000;
    const NVM_NEW_DEC_MASK: u16 = 0x0f00;
    const NVM_VER_INVALID: u16 = 0xffff;
    let mut fw = E1000FwVersion::default();
    let flash_present = access.flash_present();
    if mac == M::I211 || (mac == M::I210 && !flash_present) {
        return access.read_invm_version();
    }
    let mut read = |offset: u16| access.read_nvm_words(offset, 1).ok().and_then(|v| v.first().copied()).unwrap_or(0);
    if !matches!(mac, M::I82575 | M::I82576 | M::I82580 | M::I354 | M::I210 | M::I350) {
        let _ = read(NVM_ETRACK_HIWORD);
        return fw;
    }
    let etrack_test = read(NVM_ETRACK_HIWORD);
    if matches!(mac, M::I82575 | M::I82576 | M::I82580 | M::I354)
        && etrack_test & NVM_MAJOR_MASK != NVM_ETRACK_VALID
    {
        let version = read(NVM_VERSION);
        fw.eep_major = (version & NVM_MAJOR_MASK) >> 12;
        fw.eep_minor = (version & NVM_MINOR_MASK) >> 4;
        fw.eep_build = version & NVM_IMAGE_ID_MASK;
    } else {
        if matches!(mac, M::I210 | M::I350) {
            let offset = read(NVM_COMB_VER_PTR);
            if offset != 0 && offset != NVM_VER_INVALID {
                let high = read(NVM_COMB_VER_OFF.wrapping_add(offset).wrapping_add(1));
                let low = read(NVM_COMB_VER_OFF.wrapping_add(offset));
                if high != 0 && low != 0 && high != NVM_VER_INVALID && low != NVM_VER_INVALID {
                    fw.or_valid = true;
                    fw.or_major = low >> 8;
                    fw.or_build = (low << 8) | (high >> 8);
                    fw.or_patch = high & NVM_COMB_VER_MASK;
                }
            }
        }
        let version = read(NVM_VERSION);
        fw.eep_major = (version & NVM_MAJOR_MASK) >> 12;
        let minor = if version & NVM_NEW_DEC_MASK == 0 {
            version & NVM_COMB_VER_MASK
        } else {
            (version & NVM_MINOR_MASK) >> 4
        };
        fw.eep_minor = (minor / 16) * 10 + (minor % 16);
    }
    if etrack_test & NVM_MAJOR_MASK == NVM_ETRACK_VALID {
        fw.etrack_id = (u32::from(read(NVM_ETRACK_WORD + 1)) << 16) | u32::from(read(NVM_ETRACK_WORD));
    } else if etrack_test & NVM_ETRACK_VALID == 0 {
        fw.etrack_id = (u32::from(read(NVM_ETRACK_WORD)) << 16) | u32::from(read(NVM_ETRACK_WORD + 1));
    }
    fw
}

/// upstream: e1000_nvm.c e1000_validate_nvm_checksum_generic()
pub fn validate_nvm_checksum_generic<A: E1000NvmAccess>(access: &mut A) -> DevResult {
    let words = access.read_nvm_words(0, (NVM_CHECKSUM_REG + 1) as u16)?;
    if words.len() != NVM_CHECKSUM_REG + 1 { return Err(DevError::Io); }
    let sum = words.iter().fold(0u16, |sum, word| sum.wrapping_add(*word));
    if sum == NVM_SUM { Ok(()) } else { Err(DevError::Io) }
}

/// upstream: e1000_nvm.c e1000_update_nvm_checksum_generic()
pub fn update_nvm_checksum_generic<A: E1000NvmAccess>(access: &mut A) -> DevResult {
    let words = access.read_nvm_words(0, NVM_CHECKSUM_REG as u16)?;
    if words.len() != NVM_CHECKSUM_REG { return Err(DevError::Io); }
    let sum = words.iter().fold(0u16, |sum, word| sum.wrapping_add(*word));
    access.write_nvm_words(NVM_CHECKSUM_REG as u16, &[NVM_SUM.wrapping_sub(sum)])
}

/// upstream: e1000_nvm.c e1000_standby_nvm()
pub fn standby_nvm<I: E1000RegisterIo>(io: &mut I, nvm: E1000NvmConfig) -> DevResult {
    let mut eecd = io.read_register(E1000_EECD)?;
    match nvm.kind {
        E1000NvmType::Microwire => {
            eecd &= !(E1000_EECD_CS | E1000_EECD_SK);
            io.write_register(E1000_EECD, eecd)?;
            let _ = io.read_register(E1000_STATUS)?;
            io.delay_us(nvm.delay_usec);
            raise_eec_clk(io, &mut eecd, nvm.delay_usec)?;
            eecd |= E1000_EECD_CS;
            io.write_register(E1000_EECD, eecd)?;
            let _ = io.read_register(E1000_STATUS)?;
            io.delay_us(nvm.delay_usec);
            lower_eec_clk(io, &mut eecd, nvm.delay_usec)?;
        }
        E1000NvmType::Spi => {
            eecd |= E1000_EECD_CS;
            io.write_register(E1000_EECD, eecd)?;
            let _ = io.read_register(E1000_STATUS)?;
            io.delay_us(nvm.delay_usec);
            eecd &= !E1000_EECD_CS;
            io.write_register(E1000_EECD, eecd)?;
            let _ = io.read_register(E1000_STATUS)?;
            io.delay_us(nvm.delay_usec);
        }
        E1000NvmType::Eerd => {}
    }
    Ok(())
}

/// upstream: e1000_nvm.c e1000_stop_nvm()
pub fn stop_nvm<I: E1000RegisterIo>(io: &mut I, nvm: E1000NvmConfig) -> DevResult {
    let mut eecd = io.read_register(E1000_EECD)?;
    match nvm.kind {
        E1000NvmType::Spi => {
            eecd |= E1000_EECD_CS;
            lower_eec_clk(io, &mut eecd, nvm.delay_usec)?;
        }
        E1000NvmType::Microwire => {
            eecd &= !(E1000_EECD_CS | E1000_EECD_DI);
            io.write_register(E1000_EECD, eecd)?;
            raise_eec_clk(io, &mut eecd, nvm.delay_usec)?;
            lower_eec_clk(io, &mut eecd, nvm.delay_usec)?;
        }
        E1000NvmType::Eerd => {}
    }
    Ok(())
}

/// upstream: e1000_nvm.c e1000_release_nvm_generic()
pub fn release_nvm_generic<I: E1000RegisterIo>(io: &mut I, nvm: E1000NvmConfig) -> DevResult {
    stop_nvm(io, nvm)?;
    let eecd = io.read_register(E1000_EECD)? & !E1000_EECD_REQ;
    io.write_register(E1000_EECD, eecd)
}

/// upstream: e1000_nvm.c e1000_ready_nvm_eeprom()
pub fn ready_nvm_eeprom<I: E1000RegisterIo>(io: &mut I, nvm: E1000NvmConfig) -> DevResult {
    let mut eecd = io.read_register(E1000_EECD)?;
    match nvm.kind {
        E1000NvmType::Microwire => {
            eecd &= !(E1000_EECD_DI | E1000_EECD_SK);
            io.write_register(E1000_EECD, eecd)?;
            eecd |= E1000_EECD_CS;
            io.write_register(E1000_EECD, eecd)?;
        }
        E1000NvmType::Spi => {
            eecd &= !(E1000_EECD_CS | E1000_EECD_SK);
            io.write_register(E1000_EECD, eecd)?;
            let _ = io.read_register(E1000_STATUS)?;
            io.delay_us(1);
            let mut ready = false;
            for _ in 0..NVM_MAX_RETRY_SPI {
                shift_out_eec_bits(io, nvm, NVM_RDSR_OPCODE_SPI, nvm.opcode_bits)?;
                if shift_in_eec_bits(io, nvm.delay_usec, 8)? as u8 & NVM_STATUS_RDY_SPI == 0 {
                    ready = true;
                    break;
                }
                io.delay_us(5);
                standby_nvm(io, nvm)?;
            }
            if !ready {
                return Err(DevError::Io);
            }
        }
        E1000NvmType::Eerd => {}
    }
    Ok(())
}

/// upstream: e1000_nvm.c e1000_read_nvm_eerd()
pub fn read_nvm_eerd<I: E1000RegisterIo>(
    io: &mut I,
    nvm: E1000NvmConfig,
    offset: u16,
    words: u16,
) -> DevResult<alloc::vec::Vec<u16>> {
    if words == 0 || offset >= nvm.word_size || words > nvm.word_size - offset {
        return Err(DevError::InvalidParam);
    }
    let mut data = alloc::vec::Vec::with_capacity(words as usize);
    for word in 0..words {
        let eerd = (u32::from(offset + word) << NVM_RW_ADDR_SHIFT) | NVM_EERD_START;
        io.write_register(E1000_EERD, eerd)?;
        poll_eerd_eewr_done(io, false)?;
        data.push((io.read_register(E1000_EERD)? >> NVM_EERD_DATA_SHIFT) as u16);
    }
    Ok(data)
}

/// upstream: e1000_nvm.c e1000_read_nvm_spi()
pub fn read_nvm_spi<I: E1000RegisterIo + E1000NvmLock>(
    io: &mut I,
    nvm: E1000NvmConfig,
    offset: u16,
    words: u16,
) -> DevResult<alloc::vec::Vec<u16>> {
    if words == 0 || offset >= nvm.word_size || words > nvm.word_size - offset {
        return Err(DevError::InvalidParam);
    }
    io.acquire_nvm()?;
    let result = (|| {
        ready_nvm_eeprom(io, nvm)?;
        standby_nvm(io, nvm)?;
        let opcode = NVM_READ_OPCODE_SPI
            | if nvm.address_bits == 8 && offset >= 128 {
                NVM_A8_OPCODE_SPI
            } else {
                0
            };
        shift_out_eec_bits(
            io,
            nvm,
            opcode,
            nvm.opcode_bits,
        )?;
        shift_out_eec_bits(
            io,
            nvm,
            offset * 2,
            nvm.address_bits,
        )?;
        let mut data = alloc::vec::Vec::with_capacity(words as usize);
        for _ in 0..words {
            let word = shift_in_eec_bits(io, nvm.delay_usec, 16)?;
            data.push(word.rotate_left(8));
        }
        Ok(data)
    })();
    io.release_nvm();
    result
}

/// upstream: e1000_nvm.c e1000_read_nvm_microwire()
pub fn read_nvm_microwire<I: E1000RegisterIo + E1000NvmLock>(
    io: &mut I,
    nvm: E1000NvmConfig,
    offset: u16,
    words: u16,
) -> DevResult<alloc::vec::Vec<u16>> {
    if words == 0 || offset >= nvm.word_size || words > nvm.word_size - offset {
        return Err(DevError::InvalidParam);
    }
    io.acquire_nvm()?;
    let result = (|| {
        ready_nvm_eeprom(io, nvm)?;
        let mut data = alloc::vec::Vec::with_capacity(words as usize);
        for word in 0..words {
            shift_out_eec_bits(
                io,
                nvm,
                NVM_READ_OPCODE_MICROWIRE,
                nvm.opcode_bits,
            )?;
            shift_out_eec_bits(
                io,
                nvm,
                offset + word,
                nvm.address_bits,
            )?;
            data.push(shift_in_eec_bits(io, nvm.delay_usec, 16)?);
            standby_nvm(io, nvm)?;
        }
        Ok(data)
    })();
    io.release_nvm();
    result
}

/// upstream: e1000_nvm.c e1000_write_nvm_spi()
pub fn write_nvm_spi<I: E1000RegisterIo + E1000NvmLock>(
    io: &mut I,
    nvm: E1000NvmConfig,
    offset: u16,
    data: &[u16],
) -> DevResult {
    if data.is_empty() || offset >= nvm.word_size || data.len() > (nvm.word_size - offset) as usize
    {
        return Err(DevError::InvalidParam);
    }
    let mut written = 0usize;
    while written < data.len() {
        io.acquire_nvm()?;
        let result = (|| {
            ready_nvm_eeprom(io, nvm)?;
            standby_nvm(io, nvm)?;
            shift_out_eec_bits(
                io,
                nvm,
                NVM_WREN_OPCODE_SPI,
                nvm.opcode_bits,
            )?;
            standby_nvm(io, nvm)?;
            let mut opcode = NVM_WRITE_OPCODE_SPI;
            if nvm.address_bits == 8 && offset >= 128 {
                opcode |= NVM_A8_OPCODE_SPI;
            }
            shift_out_eec_bits(
                io,
                nvm,
                opcode,
                nvm.opcode_bits,
            )?;
            shift_out_eec_bits(
                io,
                nvm,
                (offset + written as u16) * 2,
                nvm.address_bits,
            )?;
            while written < data.len() {
                shift_out_eec_bits(
                    io,
                    nvm,
                    data[written].rotate_left(8),
                    16,
                )?;
                written += 1;
                if ((usize::from(offset) + written) * 2) % usize::from(nvm.page_size) == 0 {
                    standby_nvm(io, nvm)?;
                    break;
                }
            }
            io.delay_us(10_000);
            Ok(())
        })();
        io.release_nvm();
        result?;
    }
    Ok(())
}

/// upstream: e1000_nvm.c e1000_write_nvm_microwire()
pub fn write_nvm_microwire<I: E1000RegisterIo + E1000NvmLock>(
    io: &mut I,
    nvm: E1000NvmConfig,
    offset: u16,
    data: &[u16],
) -> DevResult {
    if data.is_empty() || offset >= nvm.word_size || data.len() > (nvm.word_size - offset) as usize
    {
        return Err(DevError::InvalidParam);
    }
    io.acquire_nvm()?;
    let result = (|| {
        ready_nvm_eeprom(io, nvm)?;
        shift_out_eec_bits(
            io,
            nvm,
            NVM_EWEN_OPCODE_MICROWIRE,
            nvm.opcode_bits + 2,
        )?;
        shift_out_eec_bits(
            io,
            nvm,
            0,
            nvm.address_bits - 2,
        )?;
        standby_nvm(io, nvm)?;
        for (index, word) in data.iter().enumerate() {
            shift_out_eec_bits(
                io,
                nvm,
                NVM_WRITE_OPCODE_MICROWIRE,
                nvm.opcode_bits,
            )?;
            shift_out_eec_bits(
                io,
                nvm,
                offset + index as u16,
                nvm.address_bits,
            )?;
            shift_out_eec_bits(io, nvm, *word, 16)?;
            standby_nvm(io, nvm)?;
            let mut ready = false;
            for _ in 0..200 {
                if io.read_register(E1000_EECD)? & E1000_EECD_DO != 0 {
                    ready = true;
                    break;
                }
                io.delay_us(50);
            }
            if !ready {
                return Err(DevError::Io);
            }
            standby_nvm(io, nvm)?;
        }
        shift_out_eec_bits(
            io,
            nvm,
            NVM_EWDS_OPCODE_MICROWIRE,
            nvm.opcode_bits + 2,
        )?;
        shift_out_eec_bits(
            io,
            nvm,
            0,
            nvm.address_bits - 2,
        )?;
        Ok(())
    })();
    io.release_nvm();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemoryNvm { words: alloc::vec::Vec<u16> }
    impl E1000NvmAccess for MemoryNvm {
        fn read_nvm_words(&mut self, offset: u16, count: u16) -> DevResult<alloc::vec::Vec<u16>> {
            let start = usize::from(offset);
            let end = start.checked_add(usize::from(count)).ok_or(DevError::InvalidParam)?;
            self.words.get(start..end).map(|words| words.to_vec()).ok_or(DevError::InvalidParam)
        }
        fn write_nvm_words(&mut self, offset: u16, words: &[u16]) -> DevResult {
            let start = usize::from(offset);
            let end = start.checked_add(words.len()).ok_or(DevError::InvalidParam)?;
            self.words.get_mut(start..end).ok_or(DevError::InvalidParam)?.copy_from_slice(words);
            Ok(())
        }
    }

    struct Io {
        registers: [(u32, u32); 4],
        writes: alloc::vec::Vec<(u32, u32)>,
        delay: usize,
    }
    impl Default for Io {
        fn default() -> Self {
            Self {
                registers: [(0, 0); 4],
                writes: alloc::vec::Vec::new(),
                delay: 0,
            }
        }
    }
    impl E1000RegisterIo for Io {
        fn read_register(&mut self, register: u32) -> DevResult<u32> {
            Ok(self
                .registers
                .iter()
                .find(|(reg, _)| *reg == register)
                .map(|(_, value)| *value)
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

    #[test]
    fn generic_nvm_op_defaults_and_clock_transitions_match_source() {
        let ops = init_nvm_ops_generic();
        assert_eq!(ops.read, E1000NvmCallback::NullReadNvm);
        assert_eq!(ops.reload, E1000NvmCallback::ReloadGeneric);
        assert!(null_read_nvm(0, 1).is_ok());
        null_nvm_generic();
        assert_eq!(null_led_default().unwrap(), 0);
        assert!(null_write_nvm(0, &[1]).is_ok());

        let mut io = Io::default();
        let mut eecd = E1000_EECD_CS;
        raise_eec_clk(&mut io, &mut eecd, 3).unwrap();
        assert_eq!(eecd, E1000_EECD_CS | E1000_EECD_SK);
        lower_eec_clk(&mut io, &mut eecd, 3).unwrap();
        assert_eq!(eecd, E1000_EECD_CS);
        assert_eq!(io.delay, 6);
    }

    #[test]
    fn generic_nvm_poll_and_request_paths_are_bounded_and_release() {
        let mut io = Io::default();
        io.registers[0] = (E1000_EERD, E1000_NVM_RW_REG_DONE);
        io.registers[1] = (E1000_EECD, E1000_EECD_GNT);
        assert!(poll_eerd_eewr_done(&mut io, false).is_ok());
        assert!(acquire_nvm_generic(&mut io).is_ok());
        let config = E1000NvmConfig {
            kind: E1000NvmType::Eerd,
            word_size: 64,
            delay_usec: 1,
            opcode_bits: 8,
            address_bits: 8,
            page_size: 8,
        };
        release_nvm_generic(&mut io, config).unwrap();
        assert!(
            io.writes
                .iter()
                .any(|(reg, value)| *reg == E1000_EECD && value & E1000_EECD_REQ == 0)
        );
    }

    #[test]
    fn generic_eeprom_bit_shifting_emits_clocked_serial_stream() {
        let mut io = Io::default();
        let config = E1000NvmConfig {
            kind: E1000NvmType::Microwire,
            word_size: 64,
            delay_usec: 2,
            opcode_bits: 3,
            address_bits: 6,
            page_size: 8,
        };
        shift_out_eec_bits(&mut io, config, 0b101, 3).unwrap();
        assert!(io.writes.len() >= 10);
        assert_eq!(io.delay, 3 * (2 + 2 + 2));
    }

    #[test]
    fn generic_pba_and_checksum_helpers_follow_nvm_word_layout() {
        let mut nvm = MemoryNvm { words: alloc::vec![0; 64] };
        nvm.words[8] = 0x1234;
        nvm.words[9] = 0x5678;
        assert_eq!(read_pba_num_generic(&mut nvm).unwrap(), 0x1234_5678);
        assert_eq!(read_pba_length_generic(&mut nvm).unwrap(), E1000_PBANUM_LENGTH);
        nvm.words[8] = NVM_PBA_PTR_GUARD;
        nvm.words[9] = 20;
        nvm.words[20] = 3;
        nvm.words[21..23].copy_from_slice(&[0x4142, 0x4344]);
        assert_eq!(read_pba_length_generic(&mut nvm).unwrap(), 5);
        assert_eq!(read_pba_string_generic(&mut nvm, false, 5).unwrap(), b"ABCD\0");
        assert!(validate_nvm_checksum_generic(&mut nvm).is_err());
        nvm.words[..NVM_CHECKSUM_REG].fill(0);
        update_nvm_checksum_generic(&mut nvm).unwrap();
        validate_nvm_checksum_generic(&mut nvm).unwrap();
    }

    #[test]
    fn raw_pba_round_trips_nvm_image_block() {
        let mut nvm = MemoryNvm { words: alloc::vec![0; 64] };
        nvm.words[8..10].copy_from_slice(&[NVM_PBA_PTR_GUARD, 24]);
        nvm.words[24..27].copy_from_slice(&[3, 0x1234, 0xabcd]);
        let pba = read_pba_raw(&mut nvm, None, 16).unwrap();
        assert_eq!(pba.block, [3, 0x1234, 0xabcd]);
        let mut image = alloc::vec![0; 64];
        write_pba_raw(&mut nvm, Some(&mut image), &pba).unwrap();
        assert_eq!(image[24..27], [3, 0x1234, 0xabcd]);
        assert_eq!(get_pba_block_size(&image).unwrap(), 3);
    }
}
