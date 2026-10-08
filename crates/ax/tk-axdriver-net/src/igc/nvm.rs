//! FreeBSD IGC shared EEPROM, SPI and NVM helpers.
//!
//! Translated from FreeBSD `sys/dev/igc/igc_nvm.c`, commit
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright 2021 Intel Corp; Copyright 2021 Rubicon Communications, LLC (Netgate).

use super::api::{IgcApiCallback, IgcHardware};

const EECD: u32 = 0x00010;
const EERD: u32 = 0x12014;
const EEWR: u32 = 0x12018;
const CTRL_EXT: u32 = 0x00018;
const RAL0: u32 = 0x05400;
const RAH0: u32 = 0x05404;
const EECD_SK: u32 = 0x01;
const EECD_CS: u32 = 0x02;
const EECD_DI: u32 = 0x04;
const EECD_DO: u32 = 0x08;
const EECD_REQ: u32 = 0x40;
const EECD_GNT: u32 = 0x80;
const CTRL_EXT_EE_RST: u32 = 0x2000;
const NVM_RW_ADDR_SHIFT: u32 = 2;
const NVM_RW_REG_DATA: u32 = 16;
const NVM_RW_REG_DONE: u32 = 2;
const NVM_RW_REG_START: u32 = 1;
const NVM_CHECKSUM_REG: u16 = 0x3f;
const NVM_SUM: u16 = 0xbaba;
const NVM_PBA_OFFSET_0: u16 = 8;
const NVM_PBA_OFFSET_1: u16 = 9;
const NVM_PBA_PTR_GUARD: u16 = 0xfafa;
const NVM_PBANUM_LENGTH: usize = 11;
const NVM_RDSR_OPCODE_SPI: u16 = 0x05;
const NVM_WREN_OPCODE_SPI: u16 = 0x06;
const NVM_WRITE_OPCODE_SPI: u16 = 0x02;
const NVM_A8_OPCODE_SPI: u16 = 0x08;
const NVM_STATUS_RDY_SPI: u8 = 0x01;
const NVM_MAX_RETRY_SPI: u16 = 5000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NvmKind {
    Spi,
    Other,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NvmInfo {
    pub kind: NvmKind,
    pub word_size: u32,
    pub opcode_bits: u16,
    pub delay_usec: u32,
    pub page_size: u16,
    pub address_bits: u16,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NvmError {
    Bounds,
    Timeout,
    NoSpace,
    InvalidPba,
    Io,
    Sync,
}

pub trait IgcNvmIo {
    fn read_reg(&mut self, reg: u32) -> u32;
    fn write_reg(&mut self, reg: u32, value: u32);
    fn write_flush(&mut self);
    fn delay_us(&mut self, us: u32);
    fn delay_ms(&mut self, ms: u32);
    fn nvm_info(&self) -> NvmInfo;
    fn acquire_nvm(&mut self) -> Result<(), NvmError>;
    fn release_nvm(&mut self);
    fn read_nvm(&mut self, offset: u16, words: u16, data: &mut [u16]) -> Result<(), NvmError>;
    fn write_nvm(&mut self, offset: u16, words: u16, data: &[u16]) -> Result<(), NvmError>;
    fn mac_type_i225(&self) -> bool;
    fn debug(&mut self, _message: &'static str) {}
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FirmwareVersion {
    pub eep_major: u8,
    pub eep_minor: u8,
    pub or_valid: bool,
    pub or_major: u16,
    pub or_build: u16,
    pub or_patch: u16,
    pub etrack_id: u32,
}

// upstream: igc_nvm.c igc_init_nvm_ops_generic()
pub fn igc_init_nvm_ops_generic(hw: &mut IgcHardware) {
    hw.nvm_ops = super::api::IgcNvmOps {
        init_params: Some(IgcApiCallback::NvmInitParamsGeneric),
        acquire: Some(IgcApiCallback::NvmNullGeneric),
        read: Some(IgcApiCallback::NvmNullRead),
        release: Some(IgcApiCallback::NvmNullGeneric),
        reload: Some(IgcApiCallback::NvmReloadGeneric),
        update: Some(IgcApiCallback::NvmNullGeneric),
        validate: Some(IgcApiCallback::NvmNullGeneric),
        write: Some(IgcApiCallback::NvmNullWrite),
    };
}

// upstream: igc_nvm.c igc_null_read_nvm()
pub fn igc_null_read_nvm(_offset: u16, _words: u16, _data: &mut [u16]) -> Result<(), NvmError> {
    Ok(())
}
// upstream: igc_nvm.c igc_null_nvm_generic()
pub fn igc_null_nvm_generic() {}
// upstream: igc_nvm.c igc_null_write_nvm()
pub fn igc_null_write_nvm(_offset: u16, _words: u16, _data: &[u16]) -> Result<(), NvmError> {
    Ok(())
}

// upstream: igc_nvm.c igc_raise_eec_clk()
pub fn igc_raise_eec_clk<I: IgcNvmIo>(io: &mut I, eecd: &mut u32) {
    *eecd |= EECD_SK;
    io.write_reg(EECD, *eecd);
    io.write_flush();
    io.delay_us(io.nvm_info().delay_usec);
}
// upstream: igc_nvm.c igc_lower_eec_clk()
pub fn igc_lower_eec_clk<I: IgcNvmIo>(io: &mut I, eecd: &mut u32) {
    *eecd &= !EECD_SK;
    io.write_reg(EECD, *eecd);
    io.write_flush();
    io.delay_us(io.nvm_info().delay_usec);
}
// upstream: igc_nvm.c igc_shift_out_eec_bits()
pub fn igc_shift_out_eec_bits<I: IgcNvmIo>(io: &mut I, data: u16, count: u16) {
    if count == 0 {
        return;
    }
    let info = io.nvm_info();
    let mut eecd = io.read_reg(EECD);
    if info.kind == NvmKind::Spi {
        eecd |= EECD_DO;
    }
    let mut mask = 1u16 << (count - 1);
    while mask != 0 {
        eecd &= !EECD_DI;
        if data & mask != 0 {
            eecd |= EECD_DI;
        }
        io.write_reg(EECD, eecd);
        io.write_flush();
        io.delay_us(info.delay_usec);
        igc_raise_eec_clk(io, &mut eecd);
        igc_lower_eec_clk(io, &mut eecd);
        mask >>= 1;
    }
    eecd &= !EECD_DI;
    io.write_reg(EECD, eecd);
}
// upstream: igc_nvm.c igc_shift_in_eec_bits()
pub fn igc_shift_in_eec_bits<I: IgcNvmIo>(io: &mut I, count: u16) -> u16 {
    let mut eecd = io.read_reg(EECD) & !(EECD_DO | EECD_DI);
    let mut data = 0u16;
    for _ in 0..count {
        data <<= 1;
        igc_raise_eec_clk(io, &mut eecd);
        eecd = io.read_reg(EECD);
        eecd &= !EECD_DI;
        if eecd & EECD_DO != 0 {
            data |= 1;
        }
        igc_lower_eec_clk(io, &mut eecd);
    }
    data
}
// upstream: igc_nvm.c igc_poll_eerd_eewr_done()
pub fn igc_poll_eerd_eewr_done<I: IgcNvmIo>(io: &mut I, poll_write: bool) -> Result<(), NvmError> {
    let reg = if poll_write { EEWR } else { EERD };
    for _ in 0..100_000 {
        if io.read_reg(reg) & NVM_RW_REG_DONE != 0 {
            return Ok(());
        }
        io.delay_us(5);
    }
    Err(NvmError::Timeout)
}
// upstream: igc_nvm.c igc_acquire_nvm_generic()
pub fn igc_acquire_nvm_generic<I: IgcNvmIo>(io: &mut I) -> Result<(), NvmError> {
    let mut eecd = io.read_reg(EECD);
    io.write_reg(EECD, eecd | EECD_REQ);
    eecd = io.read_reg(EECD);
    let mut timeout = 1000;
    while timeout != 0 {
        if eecd & EECD_GNT != 0 {
            break;
        }
        io.delay_us(5);
        eecd = io.read_reg(EECD);
        timeout -= 1;
    }
    if timeout == 0 {
        io.write_reg(EECD, eecd & !EECD_REQ);
        io.debug("Could not acquire NVM grant");
        return Err(NvmError::Timeout);
    }
    Ok(())
}

// upstream: igc_nvm.c igc_standby_nvm()
pub fn igc_standby_nvm<I: IgcNvmIo>(io: &mut I) {
    if io.nvm_info().kind != NvmKind::Spi {
        return;
    }
    let mut eecd = io.read_reg(EECD) | EECD_CS;
    io.write_reg(EECD, eecd);
    io.write_flush();
    io.delay_us(io.nvm_info().delay_usec);
    eecd &= !EECD_CS;
    io.write_reg(EECD, eecd);
    io.write_flush();
    io.delay_us(io.nvm_info().delay_usec);
}
// upstream: igc_nvm.c igc_stop_nvm()
pub fn igc_stop_nvm<I: IgcNvmIo>(io: &mut I) {
    if io.nvm_info().kind == NvmKind::Spi {
        let mut eecd = io.read_reg(EECD) | EECD_CS;
        igc_lower_eec_clk(io, &mut eecd);
    }
}
// upstream: igc_nvm.c igc_release_nvm_generic()
pub fn igc_release_nvm_generic<I: IgcNvmIo>(io: &mut I) {
    igc_stop_nvm(io);
    let eecd = io.read_reg(EECD) & !EECD_REQ;
    io.write_reg(EECD, eecd);
}

// upstream: igc_nvm.c igc_ready_nvm_eeprom()
pub fn igc_ready_nvm_eeprom<I: IgcNvmIo>(io: &mut I) -> Result<(), NvmError> {
    if io.nvm_info().kind != NvmKind::Spi {
        return Ok(());
    }
    let eecd = io.read_reg(EECD) & !(EECD_CS | EECD_SK);
    io.write_reg(EECD, eecd);
    io.write_flush();
    io.delay_us(1);
    let opcode_bits = io.nvm_info().opcode_bits;
    let mut timeout = NVM_MAX_RETRY_SPI;
    while timeout != 0 {
        igc_shift_out_eec_bits(io, NVM_RDSR_OPCODE_SPI, opcode_bits);
        let spi_status = igc_shift_in_eec_bits(io, 8) as u8;
        if spi_status & NVM_STATUS_RDY_SPI == 0 {
            break;
        }
        io.delay_us(5);
        igc_standby_nvm(io);
        timeout -= 1;
    }
    if timeout == 0 {
        return Err(NvmError::Timeout);
    }
    Ok(())
}

// upstream: igc_nvm.c igc_read_nvm_eerd()
pub fn igc_read_nvm_eerd<I: IgcNvmIo>(
    io: &mut I,
    offset: u16,
    words: u16,
    data: &mut [u16],
) -> Result<(), NvmError> {
    let info = io.nvm_info();
    if u32::from(offset) >= info.word_size
        || u32::from(words) > info.word_size - u32::from(offset)
        || words == 0
        || usize::from(words) > data.len()
    {
        return Err(NvmError::Bounds);
    }
    for i in 0..words {
        let eerd = ((u32::from(offset) + u32::from(i)) << NVM_RW_ADDR_SHIFT) + NVM_RW_REG_START;
        io.write_reg(EERD, eerd);
        igc_poll_eerd_eewr_done(io, false)?;
        data[usize::from(i)] = (io.read_reg(EERD) >> NVM_RW_REG_DATA) as u16;
    }
    Ok(())
}

// upstream: igc_nvm.c igc_write_nvm_spi()
pub fn igc_write_nvm_spi<I: IgcNvmIo>(
    io: &mut I,
    offset: u16,
    data: &[u16],
) -> Result<(), NvmError> {
    let info = io.nvm_info();
    let words = data.len();
    if u32::from(offset) >= info.word_size
        || words as u32 > info.word_size - u32::from(offset)
        || words == 0
        || info.page_size == 0
    {
        return Err(NvmError::Bounds);
    }
    let mut widx = 0usize;
    while widx < words {
        io.acquire_nvm()?;
        if let Err(error) = igc_ready_nvm_eeprom(io) {
            io.release_nvm();
            return Err(error);
        }
        igc_standby_nvm(io);
        igc_shift_out_eec_bits(io, NVM_WREN_OPCODE_SPI, info.opcode_bits);
        igc_standby_nvm(io);
        let mut write_opcode = NVM_WRITE_OPCODE_SPI;
        if info.address_bits == 8 && offset >= 128 {
            write_opcode |= NVM_A8_OPCODE_SPI;
        }
        igc_shift_out_eec_bits(io, write_opcode, info.opcode_bits);
        igc_shift_out_eec_bits(
            io,
            (u32::from(offset) + widx as u32).wrapping_mul(2) as u16,
            info.address_bits,
        );
        while widx < words {
            igc_shift_out_eec_bits(io, data[widx].rotate_left(8), 16);
            widx += 1;
            if (((usize::from(offset) + widx) * 2) % usize::from(info.page_size)) == 0 {
                igc_standby_nvm(io);
                break;
            }
        }
        io.delay_ms(10);
        io.release_nvm();
    }
    Ok(())
}

fn read_word<I: IgcNvmIo>(io: &mut I, offset: u16) -> Result<u16, NvmError> {
    let mut word = [0u16; 1];
    io.read_nvm(offset, 1, &mut word)?;
    Ok(word[0])
}

// upstream: igc_nvm.c igc_read_pba_string_generic()
pub fn igc_read_pba_string_generic<I: IgcNvmIo>(
    io: &mut I,
    output: &mut [u8],
) -> Result<usize, NvmError> {
    let nvm_data = read_word(io, NVM_PBA_OFFSET_0)?;
    let mut pba_ptr = read_word(io, NVM_PBA_OFFSET_1)?;
    if nvm_data != NVM_PBA_PTR_GUARD {
        if output.len() < NVM_PBANUM_LENGTH {
            return Err(NvmError::NoSpace);
        }
        let nibbles = [
            ((nvm_data >> 12) & 0xf) as u8,
            ((nvm_data >> 8) & 0xf) as u8,
            ((nvm_data >> 4) & 0xf) as u8,
            (nvm_data & 0xf) as u8,
            ((pba_ptr >> 12) & 0xf) as u8,
            ((pba_ptr >> 8) & 0xf) as u8,
            b'-',
            0,
            ((pba_ptr >> 4) & 0xf) as u8,
            (pba_ptr & 0xf) as u8,
        ];
        output[..10].copy_from_slice(&nibbles);
        for (idx, byte) in output[..10].iter_mut().enumerate() {
            if idx != 6 {
                *byte = if *byte < 0xa {
                    *byte + b'0'
                } else {
                    *byte + b'A' - 10
                };
            }
        }
        output[10] = 0;
        return Ok(10);
    }
    let length = read_word(io, pba_ptr)?;
    if length == 0xffff || length == 0 {
        return Err(NvmError::InvalidPba);
    }
    let output_size = usize::from(length).saturating_mul(2).saturating_sub(1);
    if output.len() < output_size {
        return Err(NvmError::NoSpace);
    }
    pba_ptr = pba_ptr.wrapping_add(1);
    let word_count = length - 1;
    for offset in 0..word_count {
        let value = read_word(io, pba_ptr.wrapping_add(offset))?;
        output[usize::from(offset) * 2] = (value >> 8) as u8;
        output[usize::from(offset) * 2 + 1] = value as u8;
    }
    let byte_count = usize::from(word_count) * 2;
    output[byte_count] = 0;
    Ok(byte_count)
}

// upstream: igc_nvm.c igc_read_mac_addr_generic()
pub fn igc_read_mac_addr_generic<I: IgcNvmIo>(io: &mut I) -> [u8; 6] {
    let rar_high = io.read_reg(RAH0);
    let rar_low = io.read_reg(RAL0);
    let mut address = [0u8; 6];
    for i in 0..4 {
        address[i] = (rar_low >> (i * 8)) as u8;
    }
    for i in 0..2 {
        address[i + 4] = (rar_high >> (i * 8)) as u8;
    }
    address
}

// upstream: igc_nvm.c igc_validate_nvm_checksum_generic()
pub fn igc_validate_nvm_checksum_generic<I: IgcNvmIo>(io: &mut I) -> Result<(), NvmError> {
    let mut checksum = 0u16;
    for i in 0..=NVM_CHECKSUM_REG {
        checksum = checksum.wrapping_add(read_word(io, i)?);
    }
    if checksum != NVM_SUM {
        return Err(NvmError::Io);
    }
    Ok(())
}

// upstream: igc_nvm.c igc_update_nvm_checksum_generic()
pub fn igc_update_nvm_checksum_generic<I: IgcNvmIo>(io: &mut I) -> Result<(), NvmError> {
    let mut checksum = 0u16;
    for i in 0..NVM_CHECKSUM_REG {
        checksum = checksum.wrapping_add(read_word(io, i)?);
    }
    io.write_nvm(NVM_CHECKSUM_REG, 1, &[NVM_SUM.wrapping_sub(checksum)])
}

// upstream: igc_nvm.c igc_reload_nvm_generic()
pub fn igc_reload_nvm_generic<I: IgcNvmIo>(io: &mut I) {
    io.delay_us(10);
    let ctrl_ext = io.read_reg(CTRL_EXT) | CTRL_EXT_EE_RST;
    io.write_reg(CTRL_EXT, ctrl_ext);
    io.write_flush();
}

// upstream: igc_nvm.c igc_get_fw_version()
pub fn igc_get_fw_version<I: IgcNvmIo>(io: &mut I) -> FirmwareVersion {
    const NVM_VERSION: u16 = 0x0005;
    const NVM_ETRACK_WORD: u16 = 0x0042;
    const NVM_ETRACK_HIWORD: u16 = 0x0043;
    const NVM_COMB_VER_PTR: u16 = 0x003d;
    const NVM_COMB_VER_OFF: u16 = 0x0083;
    const NVM_MAJOR_MASK: u16 = 0xf000;
    const NVM_MINOR_MASK: u16 = 0x0ff0;
    const NVM_COMB_VER_MASK: u16 = 0x00ff;
    const NVM_MAJOR_SHIFT: u16 = 12;
    const NVM_MINOR_SHIFT: u16 = 4;
    const NVM_COMB_VER_SHIFT: u16 = 8;
    const NVM_VER_INVALID: u16 = 0xffff;
    const NVM_ETRACK_SHIFT: u32 = 16;
    const NVM_ETRACK_VALID: u16 = 0x8000;
    const NVM_NEW_DEC_MASK: u16 = 0x0f00;
    const NVM_HEX_CONV: u16 = 16;
    const NVM_HEX_TENS: u16 = 10;
    let mut fw = FirmwareVersion::default();
    let etrack_test = read_word(io, NVM_ETRACK_HIWORD).unwrap_or(0);
    if !io.mac_type_i225() {
        return fw;
    }
    let comb_offset = read_word(io, NVM_COMB_VER_PTR).unwrap_or(0);
    if comb_offset != 0 && comb_offset != NVM_VER_INVALID {
        let eeprom_verh = read_word(
            io,
            NVM_COMB_VER_OFF.wrapping_add(comb_offset).wrapping_add(1),
        )
        .unwrap_or(0);
        let eeprom_verl = read_word(io, NVM_COMB_VER_OFF.wrapping_add(comb_offset)).unwrap_or(0);
        if eeprom_verh != 0
            && eeprom_verl != 0
            && eeprom_verh != NVM_VER_INVALID
            && eeprom_verl != NVM_VER_INVALID
        {
            fw.or_valid = true;
            fw.or_major = eeprom_verl >> NVM_COMB_VER_SHIFT;
            fw.or_build = (eeprom_verl << NVM_COMB_VER_SHIFT) | (eeprom_verh >> NVM_COMB_VER_SHIFT);
            fw.or_patch = eeprom_verh & NVM_COMB_VER_MASK;
        }
    }
    let fw_version = read_word(io, NVM_VERSION).unwrap_or(0);
    fw.eep_major = ((fw_version & NVM_MAJOR_MASK) >> NVM_MAJOR_SHIFT) as u8;
    let eeprom_verl = if fw_version & NVM_NEW_DEC_MASK == 0 {
        fw_version & NVM_COMB_VER_MASK
    } else {
        (fw_version & NVM_MINOR_MASK) >> NVM_MINOR_SHIFT
    };
    let q = eeprom_verl / NVM_HEX_CONV;
    let hval = q * NVM_HEX_TENS;
    fw.eep_minor = (hval + (eeprom_verl % NVM_HEX_CONV)) as u8;
    if etrack_test & NVM_MAJOR_MASK == NVM_ETRACK_VALID {
        let eeprom_verl = read_word(io, NVM_ETRACK_WORD).unwrap_or(0);
        let eeprom_verh = read_word(io, NVM_ETRACK_WORD + 1).unwrap_or(0);
        fw.etrack_id = (u32::from(eeprom_verh) << NVM_ETRACK_SHIFT) | u32::from(eeprom_verl);
    } else if etrack_test & NVM_ETRACK_VALID == 0 {
        let eeprom_verh = read_word(io, NVM_ETRACK_WORD).unwrap_or(0);
        let eeprom_verl = read_word(io, NVM_ETRACK_WORD + 1).unwrap_or(0);
        fw.etrack_id = (u32::from(eeprom_verh) << NVM_ETRACK_SHIFT) | u32::from(eeprom_verl);
    }
    fw
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    #[derive(Default)]
    struct Fake {
        regs: Vec<(u32, u32)>,
        words: Vec<u16>,
        delays: Vec<u32>,
        released: usize,
    }
    impl Fake {
        fn get(&self, reg: u32) -> u32 {
            self.regs
                .iter()
                .rev()
                .find(|v| v.0 == reg)
                .map_or(0, |v| v.1)
        }
        fn set(&mut self, reg: u32, value: u32) {
            if let Some(v) = self.regs.iter_mut().find(|v| v.0 == reg) {
                v.1 = value
            } else {
                self.regs.push((reg, value))
            }
        }
    }
    impl IgcNvmIo for Fake {
        fn read_reg(&mut self, reg: u32) -> u32 {
            let value = self.get(reg);
            if reg == EERD && value & NVM_RW_REG_START != 0 {
                let index = ((value >> NVM_RW_ADDR_SHIFT) & 0x3fff) as usize;
                value
                    | NVM_RW_REG_DONE
                    | (u32::from(*self.words.get(index).unwrap_or(&0)) << NVM_RW_REG_DATA)
            } else {
                value
            }
        }
        fn write_reg(&mut self, reg: u32, value: u32) {
            self.set(reg, value)
        }
        fn write_flush(&mut self) {}
        fn delay_us(&mut self, us: u32) {
            self.delays.push(us)
        }
        fn delay_ms(&mut self, ms: u32) {
            self.delays.push(ms * 1000)
        }
        fn nvm_info(&self) -> NvmInfo {
            NvmInfo {
                kind: NvmKind::Spi,
                word_size: self.words.len() as u32,
                opcode_bits: 8,
                delay_usec: 1,
                page_size: 8,
                address_bits: 8,
            }
        }
        fn acquire_nvm(&mut self) -> Result<(), NvmError> {
            Ok(())
        }
        fn release_nvm(&mut self) {
            self.released += 1
        }
        fn read_nvm(&mut self, offset: u16, words: u16, data: &mut [u16]) -> Result<(), NvmError> {
            let start = usize::from(offset);
            let n = usize::from(words);
            let Some(src) = self.words.get(start..start + n) else {
                return Err(NvmError::Bounds);
            };
            data[..n].copy_from_slice(src);
            Ok(())
        }
        fn write_nvm(&mut self, offset: u16, words: u16, data: &[u16]) -> Result<(), NvmError> {
            let start = usize::from(offset);
            let n = usize::from(words);
            if start + n > self.words.len() || n > data.len() {
                return Err(NvmError::Bounds);
            }
            self.words[start..start + n].copy_from_slice(&data[..n]);
            Ok(())
        }
        fn mac_type_i225(&self) -> bool {
            true
        }
    }
    #[test]
    fn eerd_read_pba_mac_and_checksum_paths_preserve_source_encoding() {
        let mut fake = Fake {
            words: alloc::vec![0;128],
            ..Fake::default()
        };
        fake.words[8] = 0x1234;
        fake.words[9] = 0x5678;
        let mut data = [0u16; 2];
        igc_read_nvm_eerd(&mut fake, 8, 2, &mut data).unwrap();
        assert_eq!(data, [0x1234, 0x5678]);
        let mut pba = [0u8; 16];
        let len = igc_read_pba_string_generic(&mut fake, &mut pba).unwrap();
        assert_eq!(len, 10);
        assert_eq!(&pba[..10], b"123456-078");
        fake.set(RAL0, 0x4433_2211);
        fake.set(RAH0, 0x0000_6655);
        assert_eq!(
            igc_read_mac_addr_generic(&mut fake),
            [0x11, 0x22, 0x33, 0x44, 0x55, 0x66]
        );
        let sum = fake.words[..usize::from(NVM_CHECKSUM_REG)]
            .iter()
            .copied()
            .fold(0u16, u16::wrapping_add);
        fake.words[usize::from(NVM_CHECKSUM_REG)] = NVM_SUM.wrapping_sub(sum);
        assert!(igc_validate_nvm_checksum_generic(&mut fake).is_ok());
        igc_update_nvm_checksum_generic(&mut fake).unwrap();
        assert_eq!(
            fake.words[usize::from(NVM_CHECKSUM_REG)],
            NVM_SUM.wrapping_sub(sum)
        );
    }
    #[test]
    fn null_ops_and_eeprom_grant_timeout_match_default_semantics() {
        let mut empty = [];
        assert!(igc_null_read_nvm(0, 0, &mut empty).is_ok());
        assert!(igc_null_write_nvm(0, 0, &[]).is_ok());
        igc_null_nvm_generic();
        let mut fake = Fake::default();
        assert!(matches!(
            igc_acquire_nvm_generic(&mut fake),
            Err(NvmError::Timeout)
        ));
        assert_eq!(fake.get(EECD) & EECD_REQ, 0);
    }
}
