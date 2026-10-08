//! Intel ICH/PCH integrated MAC flash, PHY, and power-management support.
//!
//! Translated from FreeBSD sys/dev/e1000/e1000_ich8lan.c revision
//! c2b7fe4a9e94a0edba9dd2772874928b565c4f9e (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{
    api::E1000MacType, chip82571::PhyOps82571, chip82575::LinkOps82575, mac::E1000MediaType,
    nvm::E1000NvmAccess, osdep::E1000RegisterIo, registers::*,
};

const SW_FLAG_TIMEOUT: usize = 100;
const PHY_CFG_TIMEOUT: usize = 100;
const HSFSTS: u32 = 4;
const HSFCTL: u32 = 6;
const HSFSTS_FDONE: u16 = 1;
const HSFSTS_FCERR: u16 = 2;
const HSFSTS_DAEL: u16 = 4;
const HSFSTS_FCINPROG: u16 = 0x20;
const HSFSTS_FLDESVALID: u16 = 0x4000;
const FLCTL_FGO: u16 = 1;
const FLCTL_CYCLE_SHIFT: u16 = 1;
const FLCTL_DBC_SHIFT: u16 = 8;
const FLASH_READ_TIMEOUT: usize = 10_000_000;
const FLASH_CYCLE_READ: u16 = 0;
const FLASH_CYCLE_WRITE: u16 = 2;
const FLASH_CYCLE_ERASE: u16 = 3;
const NVM_SIG_WORD: u32 = 0x13;
const NVM_SIG_MASK: u8 = 0xc0;
const NVM_SIG_VALUE: u8 = 0x80;
const ICH_NVM_WORD_SIZE: usize = 2048;
const NVM_CHECKSUM_REG: u16 = 0x3f;
const NVM_SUM: u16 = 0xbaba;
const FLASH_FADDR: u32 = 8;
const FLASH_FDATA0: u32 = 0x10;
const FLASH_WRITE_TIMEOUT: usize = 100_000;
const FLASH_ERASE_TIMEOUT: usize = 200_000;
const FLASH_REPEAT: usize = 10;
const ICH8_COUNTERS: &[u32] = &[
    0x04000, 0x04004, 0x04008, 0x0400c, 0x04010, 0x04014, 0x04018, 0x0401c, 0x04020, 0x04024,
    0x04028, 0x0402c, 0x04030, 0x04034, 0x04038, 0x0403c, 0x04040, 0x04044, 0x04048, 0x0404c,
    0x04050, 0x04054, 0x04058, 0x0405c, 0x04060, 0x04064, 0x04068, 0x0406c, 0x04070, 0x04074,
    0x04078, 0x0407c, 0x04080, 0x04084, 0x04088, 0x0408c, 0x04090, 0x04094, 0x04098, 0x0409c,
    0x040a0, 0x040a4, 0x040a8, 0x040ac, 0x040b0, 0x040b4, 0x040b8, 0x040bc, 0x040c0, 0x040c4,
    0x040c8, 0x040cc, 0x040d0, 0x040d4, 0x040d8, 0x040dc,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ich8NvmInfo {
    pub word_size: u16,
    pub flash_bank_size: u32,
    pub active_bank: u8,
    pub shadow_words: u16,
    pub has_shadow: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShadowWord {
    pub value: u16,
    pub modified: bool,
}
pub struct Ich8ShadowNvm {
    pub info: Ich8NvmInfo,
    pub shadow: alloc::vec::Vec<ShadowWord>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ich8PhyKind {
    Unknown,
    Igp3,
    Ife,
    Bm,
    Hv82577,
    Hv82578,
    Hv82579,
    I217,
    M88,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ich8MacParams {
    pub media: E1000MediaType,
    pub rar_count: u16,
    pub mta_count: u16,
    pub has_fwsm: bool,
    pub adaptive_ifs: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ich8BusInfo {
    pub pci_express: bool,
    pub single_port: bool,
}
pub trait Ich8FlashIo {
    fn flash_read16(&mut self, offset: u32) -> DevResult<u16>;
    fn flash_write16(&mut self, offset: u32, value: u16) -> DevResult;
    fn flash_read32(&mut self, offset: u32) -> DevResult<u32>;
    fn flash_write32(&mut self, offset: u32, value: u32) -> DevResult;
    fn delay_us(&mut self, usec: u32);
}
pub trait Ich8NvmOps: E1000NvmAccess {
    fn write_nvm_flash(&mut self, offset: u16, words: &[u16]) -> DevResult;
    fn flash_bank_size(&self) -> u32;
    fn flash_active_bank(&self) -> u8;
}

/// upstream: e1000_ich8lan.c e1000_acquire_swflag_ich8lan()
pub fn acquire_swflag_ich8lan<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    for _ in 0..PHY_CFG_TIMEOUT {
        if io.read_register(E1000_EXTCNF_CTRL)? & E1000_EXTCNF_CTRL_SWFLAG == 0 {
            break;
        }
        io.delay_us(1000)
    }
    let mut reg = io.read_register(E1000_EXTCNF_CTRL)?;
    if reg & E1000_EXTCNF_CTRL_SWFLAG != 0 {
        return Err(DevError::ResourceBusy);
    }
    reg |= E1000_EXTCNF_CTRL_SWFLAG;
    io.write_register(E1000_EXTCNF_CTRL, reg)?;
    for _ in 0..SW_FLAG_TIMEOUT {
        reg = io.read_register(E1000_EXTCNF_CTRL)?;
        if reg & E1000_EXTCNF_CTRL_SWFLAG != 0 {
            return Ok(());
        }
        io.delay_us(1000)
    }
    reg &= !E1000_EXTCNF_CTRL_SWFLAG;
    io.write_register(E1000_EXTCNF_CTRL, reg)?;
    Err(DevError::ResourceBusy)
}

/// upstream: e1000_ich8lan.c e1000_release_swflag_ich8lan()
pub fn release_swflag_ich8lan<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    let reg = io.read_register(E1000_EXTCNF_CTRL)?;
    if reg & E1000_EXTCNF_CTRL_SWFLAG != 0 {
        io.write_register(E1000_EXTCNF_CTRL, reg & !E1000_EXTCNF_CTRL_SWFLAG)?
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_acquire_nvm_ich8lan()
pub const fn acquire_nvm_ich8lan() -> DevResult {
    Ok(())
}
/// upstream: e1000_ich8lan.c e1000_release_nvm_ich8lan()
pub const fn release_nvm_ich8lan() {}

/// upstream: e1000_ich8lan.c e1000_check_mng_mode_ich8lan()
pub fn check_mng_mode_ich8lan<I: E1000RegisterIo>(io: &mut I) -> DevResult<bool> {
    let fwsm = io.read_register(E1000_FWSM)?;
    Ok(fwsm & 0x0000_8000 != 0 && (fwsm & 0xe) == (3 << 1))
}

/// upstream: e1000_ich8lan.c e1000_check_mng_mode_pchlan()
pub fn check_mng_mode_pchlan<I: E1000RegisterIo>(io: &mut I) -> DevResult<bool> {
    let fwsm = io.read_register(E1000_FWSM)?;
    Ok(fwsm & 0x0000_8000 != 0 && fwsm & (3 << 1) != 0)
}

/// upstream: e1000_ich8lan.c e1000_check_reset_block_ich8lan()
pub fn check_reset_block_ich8lan<I: E1000RegisterIo>(io: &mut I) -> DevResult<bool> {
    for _ in 0..31 {
        if io.read_register(E1000_FWSM)? & 0x0000_0040 != 0 {
            return Ok(false);
        }
        io.delay_us(10_000)
    }
    Ok(true)
}

/// upstream: e1000_ich8lan.c e1000_get_cfg_done_ich8lan()
pub fn get_cfg_done_ich8lan<I: E1000RegisterIo>(io: &mut I, function: u8) -> DevResult {
    let mask = match function {
        0 => 0x40000,
        1 => 0x80000,
        2 => 0x100000,
        _ => 0x200000,
    };
    for _ in 0..PHY_CFG_TIMEOUT {
        if io.read_register(E1000_EEMNGCTL)? & mask != 0 {
            return Ok(());
        }
        io.delay_us(1000)
    }
    Err(DevError::Io)
}

/// upstream: e1000_ich8lan.c e1000_valid_nvm_bank_detect_ich8lan()
pub fn valid_nvm_bank_detect_ich8lan<I: Ich8FlashIo, R: E1000RegisterIo>(
    flash: &mut I,
    regs: &mut R,
    mac: E1000MacType,
    nvm: Ich8NvmInfo,
) -> DevResult<u8> {
    let bank_size = if matches!(
        mac,
        E1000MacType::PchSpt
            | E1000MacType::PchCnp
            | E1000MacType::PchTgp
            | E1000MacType::PchAdp
            | E1000MacType::PchMtp
            | E1000MacType::PchPtp
            | E1000MacType::PchNvp
    ) {
        nvm.flash_bank_size
    } else {
        nvm.flash_bank_size * 2
    };
    if matches!(mac, E1000MacType::Ich8Lan | E1000MacType::Ich9Lan) {
        let eecd = regs.read_register(E1000_EECD)?;
        if eecd & 0xc000 == 0xc000 {
            return Ok(if eecd & 0x4000 != 0 { 1 } else { 0 });
        }
    }
    let offset = if matches!(
        mac,
        E1000MacType::PchSpt
            | E1000MacType::PchCnp
            | E1000MacType::PchTgp
            | E1000MacType::PchAdp
            | E1000MacType::PchMtp
            | E1000MacType::PchPtp
            | E1000MacType::PchNvp
    ) {
        NVM_SIG_WORD
    } else {
        NVM_SIG_WORD * 2 + 1
    };
    for bank in 0..2 {
        let address = offset + bank * bank_size;
        let value = if address & 1 == 0 {
            (flash.flash_read32(address & !3)? >> ((address & 2) * 8)) as u8
        } else {
            (flash.flash_read32(address & !3)? >> 8) as u8
        };
        if value & NVM_SIG_MASK == NVM_SIG_VALUE {
            return Ok(bank as u8);
        }
    }
    Err(DevError::Io)
}

/// upstream: e1000_ich8lan.c e1000_flash_cycle_init_ich8lan()
pub fn flash_cycle_init_ich8lan<I: Ich8FlashIo>(flash: &mut I, mac: E1000MacType) -> DevResult {
    let mut status = flash.flash_read16(HSFSTS)?;
    if status & HSFSTS_FLDESVALID == 0 {
        return Err(DevError::Unsupported);
    }
    status |= HSFSTS_FCERR | HSFSTS_DAEL;
    flash.flash_write16(HSFSTS, status)?;
    if status & HSFSTS_FCINPROG == 0 {
        flash.flash_write16(HSFSTS, status | HSFSTS_FDONE)?;
        return Ok(());
    }
    for _ in 0..FLASH_READ_TIMEOUT {
        status = flash.flash_read16(HSFSTS)?;
        if status & HSFSTS_FCINPROG == 0 {
            flash.flash_write16(HSFSTS, status | HSFSTS_FDONE)?;
            return Ok(());
        }
    }
    let _ = mac;
    Err(DevError::ResourceBusy)
}

/// upstream: e1000_ich8lan.c e1000_flash_cycle_ich8lan()
pub fn flash_cycle_ich8lan<I: Ich8FlashIo>(
    flash: &mut I,
    cycle: u16,
    byte_count: u8,
    timeout: usize,
) -> DevResult {
    let mut ctl = flash.flash_read16(HSFCTL)?;
    ctl &= !(3 << FLCTL_CYCLE_SHIFT);
    ctl |= cycle << FLCTL_CYCLE_SHIFT;
    ctl &= !(3 << FLCTL_DBC_SHIFT);
    ctl |= u16::from(byte_count.saturating_sub(1)) << FLCTL_DBC_SHIFT;
    ctl |= FLCTL_FGO;
    flash.flash_write16(HSFCTL, ctl)?;
    for _ in 0..timeout {
        let status = flash.flash_read16(HSFSTS)?;
        if status & HSFSTS_FCINPROG == 0 {
            return if status & (HSFSTS_FCERR | HSFSTS_DAEL) == 0 {
                Ok(())
            } else {
                Err(DevError::Io)
            };
        }
    }
    Err(DevError::Io)
}

/// upstream: e1000_ich8lan.c e1000_read_flash_dword_ich8lan()
pub fn read_flash_dword_ich8lan<I: Ich8FlashIo>(flash: &mut I, offset: u32) -> DevResult<u32> {
    flash_cycle_init_ich8lan(flash, E1000MacType::Ich8Lan)?;
    flash.flash_write16(HSFCTL, ((offset >> 2) & 0x3fff) as u16)?;
    flash_cycle_ich8lan(flash, FLASH_CYCLE_READ, 4, FLASH_READ_TIMEOUT)?;
    flash.flash_read32(offset & !3)
}

/// upstream: e1000_ich8lan.c e1000_read_flash_word_ich8lan()
pub fn read_flash_word_ich8lan<I: Ich8FlashIo>(flash: &mut I, offset: u32) -> DevResult<u16> {
    let data = read_flash_dword_ich8lan(flash, offset)?;
    Ok((data >> ((offset & 2) * 8)) as u16)
}

/// upstream: e1000_ich8lan.c e1000_read_flash_byte_ich8lan()
pub fn read_flash_byte_ich8lan<I: Ich8FlashIo>(flash: &mut I, offset: u32) -> DevResult<u8> {
    let data = read_flash_dword_ich8lan(flash, offset)?;
    Ok((data >> ((offset & 3) * 8)) as u8)
}

/// upstream: e1000_ich8lan.c e1000_read_flash_data_ich8lan()
pub fn read_flash_data_ich8lan<I: Ich8FlashIo>(
    flash: &mut I,
    offset: u32,
    size: u8,
) -> DevResult<u16> {
    match size {
        1 => Ok(u16::from(read_flash_byte_ich8lan(flash, offset)?)),
        2 => read_flash_word_ich8lan(flash, offset),
        _ => Err(DevError::InvalidParam),
    }
}

/// upstream: e1000_ich8lan.c e1000_read_flash_data32_ich8lan()
pub fn read_flash_data32_ich8lan<I: Ich8FlashIo>(flash: &mut I, offset: u32) -> DevResult<u32> {
    read_flash_dword_ich8lan(flash, offset)
}

/// upstream: e1000_ich8lan.c e1000_write_flash_data32_ich8lan()
pub fn write_flash_data32_ich8lan<I: Ich8FlashIo>(
    flash: &mut I,
    offset: u32,
    value: u32,
) -> DevResult {
    for _ in 0..FLASH_REPEAT {
        flash_cycle_init_ich8lan(flash, E1000MacType::Ich8Lan)?;
        flash.flash_write16(
            HSFCTL,
            (FLASH_CYCLE_WRITE << FLCTL_CYCLE_SHIFT) | (3 << FLCTL_DBC_SHIFT),
        )?;
        flash.flash_write32(FLASH_FADDR, offset)?;
        flash.flash_write32(FLASH_FDATA0, value)?;
        if flash_cycle_ich8lan(flash, FLASH_CYCLE_WRITE, 4, FLASH_WRITE_TIMEOUT).is_ok() {
            return Ok(());
        }
    }
    Err(DevError::Io)
}

/// upstream: e1000_ich8lan.c e1000_write_flash_data_ich8lan()
pub fn write_flash_data_ich8lan<I: Ich8FlashIo>(
    flash: &mut I,
    offset: u32,
    size: u8,
    data: u16,
    pch_spt: bool,
) -> DevResult {
    if (pch_spt && size != 4) || (!pch_spt && !(1..=2).contains(&size)) {
        return Err(DevError::InvalidParam);
    }
    let value = if size == 1 {
        u32::from(data & 0xff)
    } else {
        u32::from(data)
    };
    for _ in 0..FLASH_REPEAT {
        flash_cycle_init_ich8lan(flash, E1000MacType::Ich8Lan)?;
        flash.flash_write16(
            HSFCTL,
            (FLASH_CYCLE_WRITE << FLCTL_CYCLE_SHIFT) | (u16::from(size - 1) << FLCTL_DBC_SHIFT),
        )?;
        flash.flash_write32(FLASH_FADDR, offset)?;
        flash.flash_write32(FLASH_FDATA0, value)?;
        if flash_cycle_ich8lan(flash, FLASH_CYCLE_WRITE, size, FLASH_WRITE_TIMEOUT).is_ok() {
            return Ok(());
        }
    }
    Err(DevError::Io)
}

/// upstream: e1000_ich8lan.c e1000_write_flash_byte_ich8lan()
pub fn write_flash_byte_ich8lan<I: Ich8FlashIo>(
    flash: &mut I,
    offset: u32,
    value: u8,
) -> DevResult {
    write_flash_data_ich8lan(flash, offset, 1, u16::from(value), false)
}

/// upstream: e1000_ich8lan.c e1000_retry_write_flash_dword_ich8lan()
pub fn retry_write_flash_dword_ich8lan<I: Ich8FlashIo>(
    flash: &mut I,
    word_offset: u32,
    value: u32,
) -> DevResult {
    let offset = word_offset.checked_mul(2).ok_or(DevError::InvalidParam)?;
    for _ in 0..100 {
        if write_flash_data32_ich8lan(flash, offset, value).is_ok() {
            return Ok(());
        }
        flash_delay(flash, 100)?;
    }
    Err(DevError::Io)
}

/// upstream: e1000_ich8lan.c e1000_retry_write_flash_byte_ich8lan()
pub fn retry_write_flash_byte_ich8lan<I: Ich8FlashIo>(
    flash: &mut I,
    offset: u32,
    value: u8,
) -> DevResult {
    for _ in 0..100 {
        if write_flash_byte_ich8lan(flash, offset, value).is_ok() {
            return Ok(());
        }
        flash_delay(flash, 100)?;
    }
    Err(DevError::Io)
}

fn flash_delay<I: Ich8FlashIo>(flash: &mut I, usec: u32) -> DevResult {
    flash.delay_us(usec);
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_erase_flash_bank_ich8lan()
pub fn erase_flash_bank_ich8lan<I: Ich8FlashIo>(
    flash: &mut I,
    base: u32,
    bank_size: u32,
    bank: u32,
    sector_size: u32,
) -> DevResult {
    let sectors = if sector_size == 256 {
        bank_size / 256
    } else {
        1
    };
    let size = if sectors == 0 {
        return Err(DevError::InvalidParam);
    } else {
        sector_size
    };
    for index in 0..sectors {
        let addr = base + bank * bank_size + index * size;
        let mut done = false;
        for _ in 0..FLASH_REPEAT {
            flash_cycle_init_ich8lan(flash, E1000MacType::Ich8Lan)?;
            flash.flash_write16(HSFCTL, FLASH_CYCLE_ERASE << FLCTL_CYCLE_SHIFT)?;
            flash.flash_write32(FLASH_FADDR, addr)?;
            if flash_cycle_ich8lan(flash, FLASH_CYCLE_ERASE, 0, FLASH_ERASE_TIMEOUT).is_ok() {
                done = true;
                break;
            }
        }
        if !done {
            return Err(DevError::Io);
        }
    }
    Ok(())
}

fn nvm_read_words<I: Ich8FlashIo>(
    flash: &mut I,
    info: Ich8NvmInfo,
    active_bank: u8,
    offset: u16,
    words: u16,
    shadow: &[ShadowWord],
) -> DevResult<alloc::vec::Vec<u16>> {
    if words == 0 || offset >= info.word_size || words > info.word_size - offset {
        return Err(DevError::InvalidParam);
    }
    let start = if active_bank == 0 {
        0
    } else {
        info.flash_bank_size
    };
    let mut out = alloc::vec::Vec::with_capacity(words as usize);
    for i in 0..words {
        let index = usize::from(offset + i);
        let value = if info.has_shadow && shadow.get(index).map(|w| w.modified).unwrap_or(false) {
            shadow[index].value
        } else {
            read_flash_word_ich8lan(flash, start + u32::from(offset + i) * 2)?
        };
        out.push(value)
    }
    Ok(out)
}

/// upstream: e1000_ich8lan.c e1000_read_nvm_ich8lan()
pub fn read_nvm_ich8lan<I: Ich8FlashIo>(
    flash: &mut I,
    info: Ich8NvmInfo,
    offset: u16,
    words: u16,
    shadow: &[ShadowWord],
) -> DevResult<alloc::vec::Vec<u16>> {
    nvm_read_words(flash, info, info.active_bank, offset, words, shadow)
}

/// upstream: e1000_ich8lan.c e1000_read_nvm_spt()
pub fn read_nvm_spt<I: Ich8FlashIo>(
    flash: &mut I,
    info: Ich8NvmInfo,
    offset: u16,
    words: u16,
    shadow: &[ShadowWord],
) -> DevResult<alloc::vec::Vec<u16>> {
    nvm_read_words(flash, info, info.active_bank, offset, words, shadow)
}

/// upstream: e1000_ich8lan.c e1000_write_nvm_ich8lan()
pub fn write_nvm_ich8lan(
    info: Ich8NvmInfo,
    offset: u16,
    words: &[u16],
    shadow: &mut [ShadowWord],
) -> DevResult {
    if words.is_empty()
        || offset >= info.word_size
        || words.len() > usize::from(info.word_size - offset)
    {
        return Err(DevError::InvalidParam);
    }
    for (i, value) in words.iter().enumerate() {
        let entry = shadow
            .get_mut(usize::from(offset) + i)
            .ok_or(DevError::InvalidParam)?;
        entry.modified = true;
        entry.value = *value;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_update_nvm_checksum_spt()
pub fn update_nvm_checksum_spt<N: Ich8NvmOps, F: Ich8FlashIo>(
    nvm: &mut N,
    flash: &mut F,
    shadow: &mut [ShadowWord],
    info: Ich8NvmInfo,
) -> DevResult {
    update_nvm_checksum_generic(nvm)?;
    if !info.has_shadow {
        return Ok(());
    }
    commit_shadow_nvm(nvm, flash, shadow, info, true)
}

/// upstream: e1000_ich8lan.c e1000_update_nvm_checksum_ich8lan()
pub fn update_nvm_checksum_ich8lan<N: Ich8NvmOps, F: Ich8FlashIo>(
    nvm: &mut N,
    flash: &mut F,
    shadow: &mut [ShadowWord],
    info: Ich8NvmInfo,
) -> DevResult {
    update_nvm_checksum_generic(nvm)?;
    if !info.has_shadow {
        return Ok(());
    }
    commit_shadow_nvm(nvm, flash, shadow, info, false)
}

fn update_nvm_checksum_generic<N: E1000NvmAccess>(nvm: &mut N) -> DevResult {
    let words = nvm.read_nvm_words(0, NVM_CHECKSUM_REG)?;
    let sum = words.iter().fold(0u16, |sum, v| sum.wrapping_add(*v));
    nvm.write_nvm_words(NVM_CHECKSUM_REG, &[NVM_SUM.wrapping_sub(sum)])
}

fn commit_shadow_nvm<N: Ich8NvmOps, F: Ich8FlashIo>(
    nvm: &mut N,
    flash: &mut F,
    shadow: &mut [ShadowWord],
    info: Ich8NvmInfo,
    spt: bool,
) -> DevResult {
    let active = nvm.flash_active_bank();
    let target = 1 - active;
    erase_flash_bank_ich8lan(
        flash,
        0,
        info.flash_bank_size,
        target.into(),
        if spt { 4096 } else { 4096 },
    )?;
    for offset in (0..usize::from(info.word_size)).step_by(if spt { 2 } else { 1 }) {
        if spt && offset + 1 < usize::from(info.word_size) {
            let mut pair = nvm_read_words(flash, info, active, offset as u16, 2, shadow)?;
            for j in 0..2 {
                if shadow.get(offset + j).is_some_and(|v| v.modified) {
                    pair[j] = shadow[offset + j].value
                }
            }
            if offset == (NVM_SIG_WORD as usize) - 1 {
                pair[1] |= 0xc000;
            }
            let dword = u32::from(pair[0]) | (u32::from(pair[1]) << 16);
            retry_write_flash_dword_ich8lan(
                flash,
                (offset as u32 + u32::from(target) * info.flash_bank_size) / 2,
                dword,
            )?;
        } else {
            let value = if shadow.get(offset).is_some_and(|v| v.modified) {
                shadow[offset].value
            } else {
                nvm_read_words(flash, info, active, offset as u16, 1, shadow)?[0]
            };
            retry_write_flash_byte_ich8lan(
                flash,
                (u32::from(target) * info.flash_bank_size) + (offset as u32) * 2,
                value as u8,
            )?;
            retry_write_flash_byte_ich8lan(
                flash,
                (u32::from(target) * info.flash_bank_size) + (offset as u32) * 2 + 1,
                (value >> 8) as u8,
            )?;
        }
    }
    for entry in shadow.iter_mut() {
        entry.modified = false;
        entry.value = u16::MAX;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_validate_nvm_checksum_ich8lan()
pub fn validate_nvm_checksum_ich8lan<N: E1000NvmAccess>(
    nvm: &mut N,
    mac: E1000MacType,
) -> DevResult {
    let (word, mask) = if matches!(
        mac,
        E1000MacType::PchLpt
            | E1000MacType::PchSpt
            | E1000MacType::PchCnp
            | E1000MacType::PchTgp
            | E1000MacType::PchAdp
            | E1000MacType::PchMtp
            | E1000MacType::PchPtp
            | E1000MacType::PchNvp
    ) {
        (3, 1)
    } else {
        (0x19, 0x40)
    };
    let mut data = nvm
        .read_nvm_words(word, 1)?
        .first()
        .copied()
        .ok_or(DevError::Io)?;
    if data & mask == 0 {
        if mac < E1000MacType::PchTgp {
            data |= mask;
            nvm.write_nvm_words(word, &[data])?;
            let _ = update_nvm_checksum_generic(nvm)?;
        } else if mac == E1000MacType::PchTgp {
            return Ok(());
        }
    }
    validate_nvm_checksum_generic(nvm)
}

fn validate_nvm_checksum_generic<N: E1000NvmAccess>(nvm: &mut N) -> DevResult {
    let words = nvm.read_nvm_words(0, NVM_CHECKSUM_REG + 1)?;
    if words.iter().fold(0u16, |sum, v| sum.wrapping_add(*v)) == NVM_SUM {
        Ok(())
    } else {
        Err(DevError::Io)
    }
}

/// upstream: e1000_ich8lan.c e1000_init_phy_params_ich8lan()
pub fn init_phy_params_ich8lan(media: E1000MediaType, phy_id: u32) -> DevResult<Ich8PhyKind> {
    if media != E1000MediaType::Copper {
        return Ok(Ich8PhyKind::Unknown);
    }
    match phy_id {
        0x02a80380 | 0x02a80390 | 0x02a80391 => Ok(Ich8PhyKind::Igp3),
        0x02a80100 | 0x02a80110 | 0x02a80120 => Ok(Ich8PhyKind::Ife),
        0x01410cb0 => Ok(Ich8PhyKind::Bm),
        _ => Err(DevError::Io),
    }
}

/// upstream: e1000_ich8lan.c e1000_init_phy_params_pchlan()
pub fn init_phy_params_pchlan(phy_id: u32) -> DevResult<Ich8PhyKind> {
    match phy_id {
        0x01540000 | 0x01540010 | 0x01540020 => Ok(Ich8PhyKind::Hv82577),
        0x01540030 => Ok(Ich8PhyKind::Hv82579),
        0x01540040 => Ok(Ich8PhyKind::I217),
        _ => Err(DevError::Io),
    }
}

/// upstream: e1000_ich8lan.c e1000_init_nvm_params_ich8lan()
pub fn init_nvm_params_ich8lan<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    gfpreg: u32,
) -> DevResult<Ich8NvmInfo> {
    let bank_size = if matches!(
        mac,
        E1000MacType::PchSpt
            | E1000MacType::PchCnp
            | E1000MacType::PchTgp
            | E1000MacType::PchAdp
            | E1000MacType::PchMtp
            | E1000MacType::PchPtp
            | E1000MacType::PchNvp
    ) {
        let total = (((io.read_register(E1000_STRAP)? >> 1) & 0x1f) + 1) * 4096;
        (total / 2) / 2
    } else {
        let start = gfpreg & 0x1fff;
        let end = ((gfpreg >> 16) & 0x1fff) + 1;
        if end <= start {
            return Err(DevError::Io);
        }
        (((end - start) << 12) / 2) / 2
    };
    Ok(Ich8NvmInfo {
        word_size: ICH_NVM_WORD_SIZE as u16,
        flash_bank_size: bank_size,
        active_bank: 0,
        shadow_words: ICH_NVM_WORD_SIZE as u16,
        has_shadow: true,
    })
}

/// upstream: e1000_ich8lan.c e1000_init_mac_params_ich8lan()
pub fn init_mac_params_ich8lan(mac: E1000MacType, _fwsm: u32) -> Ich8MacParams {
    let rar = match mac {
        E1000MacType::Ich8Lan => 7,
        E1000MacType::Pch2Lan => 11,
        E1000MacType::PchLpt
        | E1000MacType::PchSpt
        | E1000MacType::PchCnp
        | E1000MacType::PchTgp
        | E1000MacType::PchAdp
        | E1000MacType::PchMtp
        | E1000MacType::PchPtp
        | E1000MacType::PchNvp => 12,
        _ => 10,
    };
    Ich8MacParams {
        media: E1000MediaType::Copper,
        rar_count: rar,
        mta_count: 32,
        has_fwsm: true,
        adaptive_ifs: true,
    }
}

/// upstream: e1000_ich8lan.c e1000_init_function_pointers_ich8lan()
pub const fn init_function_pointers_ich8lan() -> (bool, bool, bool) {
    (true, true, true)
}

/// upstream: e1000_ich8lan.c e1000_get_bus_info_ich8lan()
pub const fn get_bus_info_ich8lan() -> Ich8BusInfo {
    Ich8BusInfo {
        pci_express: true,
        single_port: true,
    }
}

/// upstream: e1000_ich8lan.c e1000_setup_link_ich8lan()
pub fn setup_link_ich8lan<O: LinkOps82575>(ops: &mut O, requested: &mut u8) -> DevResult {
    if *requested == 0 {
        *requested = 3
    }
    ops.generic_check_link()
}

/// upstream: e1000_ich8lan.c e1000_setup_copper_link_ich8lan()
pub fn setup_copper_link_ich8lan<I: E1000RegisterIo, O: LinkOps82575>(
    io: &mut I,
    ops: &mut O,
) -> DevResult {
    let mut ctrl = io.read_register(E1000_CTRL)? | E1000_CTRL_SLU;
    ctrl &= !(E1000_CTRL_FRCSPD | E1000_CTRL_FRCDPX);
    io.write_register(E1000_CTRL, ctrl)?;
    ops.generic_copper_setup()
}

/// upstream: e1000_ich8lan.c e1000_setup_copper_link_pch_lpt()
pub fn setup_copper_link_pch_lpt<I: E1000RegisterIo, O: LinkOps82575>(
    io: &mut I,
    ops: &mut O,
    eee_disabled: bool,
) -> DevResult {
    setup_copper_link_ich8lan(io, ops)?;
    if !eee_disabled {
        let _ = io.read_register(E1000_EEER)?;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_get_link_up_info_ich8lan()
pub fn get_link_up_info_ich8lan<O: LinkOps82575>(ops: &mut O) -> DevResult<(u16, u16)> {
    ops.generic_copper_speed()
}

/// upstream: e1000_ich8lan.c e1000_check_for_copper_link_ich8lan()
pub fn check_for_copper_link_ich8lan<O: LinkOps82575>(ops: &mut O) -> DevResult {
    ops.generic_check_copper()
}

/// upstream: e1000_ich8lan.c e1000_rar_set_pch2lan()
pub fn rar_set_pch2lan<I: E1000RegisterIo>(
    io: &mut I,
    address: [u8; 6],
    index: u32,
    rar_count: u32,
) -> DevResult {
    let low = u32::from(address[0])
        | (u32::from(address[1]) << 8)
        | (u32::from(address[2]) << 16)
        | (u32::from(address[3]) << 24);
    let mut high = u32::from(address[4]) | (u32::from(address[5]) << 8);
    if low != 0 || high != 0 {
        high |= E1000_RAH_AV
    }
    if index == 0 {
        io.write_register(E1000_RA, low)?;
        let _ = io.read_register(E1000_STATUS)?;
        io.write_register(E1000_RA + 4, high)?;
        let _ = io.read_register(E1000_STATUS)?;
        return Ok(());
    }
    if index >= rar_count {
        return Err(DevError::InvalidParam);
    }
    acquire_swflag_ich8lan(io)?;
    let base = 0x5438 + (index - 1) * 8;
    io.write_register(base, low)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.write_register(base + 4, high)?;
    let _ = io.read_register(E1000_STATUS)?;
    release_swflag_ich8lan(io)?;
    if io.read_register(base)? == low && io.read_register(base + 4)? == high {
        Ok(())
    } else {
        Err(DevError::ResourceBusy)
    }
}

/// upstream: e1000_ich8lan.c e1000_rar_set_pch_lpt()
pub fn rar_set_pch_lpt<I: E1000RegisterIo>(
    io: &mut I,
    address: [u8; 6],
    index: u32,
    rar_count: u32,
    wlock_mac: u32,
) -> DevResult {
    let low = u32::from(address[0])
        | (u32::from(address[1]) << 8)
        | (u32::from(address[2]) << 16)
        | (u32::from(address[3]) << 24);
    let mut high = u32::from(address[4]) | (u32::from(address[5]) << 8);
    if low != 0 || high != 0 {
        high |= E1000_RAH_AV
    }
    if index == 0 {
        io.write_register(E1000_RA, low)?;
        let _ = io.read_register(E1000_STATUS)?;
        io.write_register(E1000_RA + 4, high)?;
        let _ = io.read_register(E1000_STATUS)?;
        return Ok(());
    }
    if index >= rar_count || wlock_mac == 1 || (wlock_mac != 0 && index > wlock_mac) {
        return Err(DevError::ResourceBusy);
    }
    acquire_swflag_ich8lan(io)?;
    let base = 0x5408 + (index - 1) * 8;
    io.write_register(base, low)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.write_register(base + 4, high)?;
    let _ = io.read_register(E1000_STATUS)?;
    release_swflag_ich8lan(io)?;
    if io.read_register(base)? == low && io.read_register(base + 4)? == high {
        Ok(())
    } else {
        Err(DevError::ResourceBusy)
    }
}

/// upstream: e1000_ich8lan.c e1000_update_mc_addr_list_pch2lan()
pub fn update_mc_addr_list_pch2lan<F: FnMut() -> DevResult>(
    mut generic_update: F,
    phy_wakeup_access: DevResult,
    mut write_phy_mta: impl FnMut(u32, u16) -> DevResult,
    mta_shadow: &[u32],
) -> DevResult {
    generic_update()?;
    phy_wakeup_access?;
    for (index, value) in mta_shadow.iter().enumerate() {
        write_phy_mta((index as u32) * 2, *value as u16)?;
        write_phy_mta((index as u32) * 2 + 1, (*value >> 16) as u16)?;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_write_smbus_addr()
pub fn write_smbus_addr<I: E1000RegisterIo, P: PhyOps82571>(io: &mut I, phy: &mut P) -> DevResult {
    let strap = io.read_register(E1000_STRAP)?;
    let mut data = phy.read_phy(0x0d)?;
    data &= !0x007f;
    data |= ((strap & 0x0000_7f00) >> 8) as u16;
    data |= 0x8080;
    let freq = (strap & 0x0000_0300) >> 8;
    if freq != 0 {
        data = (data & !0x0600) | (((freq - 1) as u16 & 1) << 9) | (((freq - 1) as u16 & 2) << 9);
    }
    phy.write_phy(0x0d, data)
}

/// upstream: e1000_ich8lan.c e1000_sw_lcd_config_ich8lan()
pub fn sw_lcd_config_ich8lan<N: E1000NvmAccess, P: PhyOps82571>(
    nvm: &mut N,
    phy: &mut P,
    base_dword: u32,
    count: u32,
) -> DevResult {
    let mut page = 0u16;
    let start = (base_dword * 2) as u16;
    for i in 0..count {
        let regs = nvm.read_nvm_words(start + (i as u16) * 2, 2)?;
        if regs.len() != 2 {
            return Err(DevError::Io);
        }
        let value = regs[0];
        let addr = regs[1];
        if addr == 0x1f {
            page = value;
            continue;
        }
        phy.write_phy(page | (addr & 0x1f), value)?;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_phy_is_accessible_pchlan()
pub fn phy_is_accessible_pchlan<P: PhyOps82571>(phy: &mut P, expected_id: Option<u32>) -> bool {
    for _ in 0..2 {
        let id = phy.generic_phy_id();
        if let Ok((value, revision)) = id {
            let current = value | (revision & 0xf);
            if value != 0 && value != u32::MAX {
                return expected_id
                    .map(|expected| expected == current)
                    .unwrap_or(true);
            }
        }
    }
    false
}

/// upstream: e1000_ich8lan.c e1000_toggle_lanphypc_pch_lpt()
pub fn toggle_lanphypc_pch_lpt<I: E1000RegisterIo>(io: &mut I, mac: E1000MacType) -> DevResult {
    let mut ext = io.read_register(E1000_FEXTNVM3)? & !0x000000f0;
    ext |= 0x00000050;
    io.write_register(E1000_FEXTNVM3, ext)?;
    let mut ctrl = io.read_register(E1000_CTRL)? | 0x00000020;
    ctrl &= !0x00000010;
    io.write_register(E1000_CTRL, ctrl)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(1000);
    ctrl &= !0x00000020;
    io.write_register(E1000_CTRL, ctrl)?;
    let _ = io.read_register(E1000_STATUS)?;
    if mac < E1000MacType::PchLpt {
        io.delay_us(50_000)
    } else {
        for _ in 0..20 {
            if io.read_register(E1000_CTRL_EXT)? & E1000_CTRL_EXT_LPCD != 0 {
                break;
            }
            io.delay_us(5_000)
        }
        io.delay_us(30_000)
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_reconfigure_k1_exit_timeout()
pub fn reconfigure_k1_exit_timeout<P: PhyOps82571>(
    io: &mut impl E1000RegisterIo,
    phy: &mut P,
    mac: E1000MacType,
) -> DevResult {
    if !matches!(
        mac,
        E1000MacType::PchMtp | E1000MacType::PchPtp | E1000MacType::PchNvp
    ) {
        return Ok(());
    }
    let reg = io.read_register(E1000_FEXTNVM12)?;
    io.write_register(E1000_FEXTNVM12, (reg & !0x0000_0300) | 0x0000_0200)?;
    io.delay_us(1_000);
    let timeout = phy.read_phy(0x19)? & !0x0f00;
    phy.write_phy(0x19, timeout | 0x0f00)
}

/// upstream: e1000_ich8lan.c e1000_gate_hw_phy_config_ich8lan()
pub fn gate_hw_phy_config_ich8lan<I: E1000RegisterIo>(io: &mut I, gate: bool) -> DevResult {
    let mut reg = io.read_register(E1000_EXTCNF_CTRL)?;
    if gate {
        reg |= 0x00000001
    } else {
        reg &= !0x00000001
    }
    io.write_register(E1000_EXTCNF_CTRL, reg)
}

/// upstream: e1000_ich8lan.c e1000_set_d0_lplu_state_ich8lan()
pub fn set_d0_lplu_state_ich8lan<I: E1000RegisterIo, P: PhyOps82571>(
    io: &mut I,
    phy: &mut P,
    phy_kind: Ich8PhyKind,
    mac: E1000MacType,
    active: bool,
    smart: super::phy::SmartSpeedMode,
) -> DevResult {
    if phy_kind == Ich8PhyKind::Ife {
        return Ok(());
    }
    let mut ctrl = io.read_register(E1000_PHY_CTRL)?;
    if active {
        ctrl |= E1000_PHY_CTRL_D0A_LPLU;
        io.write_register(E1000_PHY_CTRL, ctrl)?;
        if phy_kind != Ich8PhyKind::Igp3 {
            return Ok(());
        }
        if mac == E1000MacType::Ich8Lan {
            let _ = gig_downshift_workaround_ich8lan(phy);
        }
        let port = phy.read_phy(0x10)? & !0x0080;
        phy.write_phy(0x10, port)
    } else {
        ctrl &= !E1000_PHY_CTRL_D0A_LPLU;
        io.write_register(E1000_PHY_CTRL, ctrl)?;
        if phy_kind != Ich8PhyKind::Igp3 {
            return Ok(());
        }
        if smart != super::phy::SmartSpeedMode::Default {
            let mut port = phy.read_phy(0x10)?;
            if smart == super::phy::SmartSpeedMode::On {
                port |= 0x0080
            } else {
                port &= !0x0080
            }
            phy.write_phy(0x10, port)?;
        }
        Ok(())
    }
}

/// upstream: e1000_ich8lan.c e1000_set_d3_lplu_state_ich8lan()
pub fn set_d3_lplu_state_ich8lan<I: E1000RegisterIo, P: PhyOps82571>(
    io: &mut I,
    phy: &mut P,
    phy_kind: Ich8PhyKind,
    mac: E1000MacType,
    active: bool,
    advertised: u16,
    smart: super::phy::SmartSpeedMode,
) -> DevResult {
    let mut ctrl = io.read_register(E1000_PHY_CTRL)?;
    if !active {
        ctrl &= !E1000_PHY_CTRL_NOND0A_LPLU;
        io.write_register(E1000_PHY_CTRL, ctrl)?;
        if phy_kind != Ich8PhyKind::Igp3 {
            return Ok(());
        }
        if smart != super::phy::SmartSpeedMode::Default {
            let mut p = phy.read_phy(0x10)?;
            if smart == super::phy::SmartSpeedMode::On {
                p |= 0x80
            } else {
                p &= !0x80
            }
            phy.write_phy(0x10, p)?;
        }
        return Ok(());
    }
    if !matches!(advertised, 0x2f | 0x0f | 0x03) {
        return Ok(());
    }
    ctrl |= E1000_PHY_CTRL_NOND0A_LPLU;
    io.write_register(E1000_PHY_CTRL, ctrl)?;
    if phy_kind == Ich8PhyKind::Igp3 {
        if mac == E1000MacType::Ich8Lan {
            let _ = gig_downshift_workaround_ich8lan(phy);
        }
        let p = phy.read_phy(0x10)? & !0x80;
        phy.write_phy(0x10, p)?;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_set_lplu_state_pchlan()
pub fn set_lplu_state_pchlan<I: E1000RegisterIo>(
    io: &mut I,
    active: bool,
    advertised: u16,
) -> DevResult {
    let mut value = io.read_register(E1000_PHY_CTRL)?;
    if active && matches!(advertised, 0x2f | 0x0f | 0x03) {
        value |= E1000_PHY_CTRL_NOND0A_LPLU
    } else {
        value &= !E1000_PHY_CTRL_NOND0A_LPLU
    }
    io.write_register(E1000_PHY_CTRL, value)
}

/// upstream: e1000_ich8lan.c e1000_calc_rx_da_crc()
pub fn calc_rx_da_crc(mac: [u8; 6]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in mac {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask)
        }
    }
    !crc
}

/// upstream: e1000_ich8lan.c e1000_ltr2ns()
pub fn ltr2ns(value: u16, scale: u8) -> DevResult<u64> {
    let multiplier = match scale {
        0 => 1u64,
        1 => 32,
        2 => 1024,
        3 => 32768,
        4 => 1_048_576,
        5 => 33_554_432,
        _ => return Err(DevError::InvalidParam),
    };
    Ok(u64::from(value) * multiplier)
}

/// upstream: e1000_ich8lan.c e1000_clear_hw_cntrs_ich8lan()
pub fn clear_hw_cntrs_ich8lan<I: E1000RegisterIo>(io: &mut I) -> DevResult {
    for register in ICH8_COUNTERS {
        let _ = io.read_register(*register)?;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_lan_init_done_ich8lan()
pub fn lan_init_done_ich8lan<I: E1000RegisterIo>(io: &mut I, timeout: usize) -> DevResult {
    let mut complete = false;
    for _ in 0..timeout {
        if io.read_register(E1000_STATUS)? & E1000_STATUS_LAN_INIT_DONE != 0 {
            complete = true;
            break;
        }
        io.delay_us(100)
    }
    let status = io.read_register(E1000_STATUS)?;
    io.write_register(E1000_STATUS, status & !E1000_STATUS_LAN_INIT_DONE)?;
    if complete { Ok(()) } else { Err(DevError::Io) }
}

/// upstream: e1000_ich8lan.c e1000_valid_led_default_ich8lan()
pub fn valid_led_default_ich8lan<N: E1000NvmAccess>(nvm: &mut N, word: u16) -> DevResult<u16> {
    let value = nvm
        .read_nvm_words(word, 1)?
        .first()
        .copied()
        .ok_or(DevError::Io)?;
    Ok(if value == 0 || value == u16::MAX {
        0x0f0f
    } else {
        value
    })
}

/// upstream: e1000_ich8lan.c e1000_power_down_phy_copper_ich8lan()
pub fn power_down_phy_copper_ich8lan<P: PhyOps82571>(phy: &mut P) -> DevResult {
    phy.power_down_phy()
}

pub trait Ich8LedOps {
    fn setup_led(&mut self) -> DevResult;
    fn cleanup_led(&mut self) -> DevResult;
    fn led_on(&mut self) -> DevResult;
    fn led_off(&mut self) -> DevResult;
}

pub trait Ich8PhyRegOps {
    fn read_reg(&mut self, reg: u16) -> DevResult<u16>;
    fn write_reg(&mut self, reg: u16, value: u16) -> DevResult;
    fn read_reg_page(&mut self, page: u16, reg: u16) -> DevResult<u16>;
    fn read_reg_locked(&mut self, reg: u16) -> DevResult<u16>;
    fn write_reg_locked(&mut self, reg: u16, value: u16) -> DevResult;
    fn acquire(&mut self) -> DevResult;
    fn release(&mut self);
    fn write_reg_page(&mut self, page: u16, reg: u16, value: u16) -> DevResult;
    fn wakeup_reg_access(&mut self, enable: bool, saved: &mut u16) -> DevResult;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Ich8LedState {
    pub ledctl_default: u32,
    pub ledctl_mode1: u32,
    pub ledctl_mode2: u32,
    pub phy_ife: bool,
}

/// upstream: e1000_ich8lan.c __e1000_access_emi_reg_locked()
pub fn access_emi_reg_locked<P: Ich8PhyRegOps>(
    phy: &mut P,
    address: u16,
    data: &mut u16,
    read: bool,
) -> DevResult {
    phy.write_reg_locked(0x10, address)?;
    if read {
        *data = phy.read_reg_locked(0x11)?;
    } else {
        phy.write_reg_locked(0x11, *data)?;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_read_emi_reg_locked()
pub fn read_emi_reg_locked<P: Ich8PhyRegOps>(phy: &mut P, address: u16) -> DevResult<u16> {
    let mut data = 0;
    access_emi_reg_locked(phy, address, &mut data, true)?;
    Ok(data)
}

/// upstream: e1000_ich8lan.c e1000_write_emi_reg_locked()
pub fn write_emi_reg_locked<P: Ich8PhyRegOps>(phy: &mut P, address: u16, data: u16) -> DevResult {
    let mut value = data;
    access_emi_reg_locked(phy, address, &mut value, false)
}

/// upstream: e1000_ich8lan.c e1000_copy_rx_addrs_to_phy_ich8lan()
pub fn copy_rx_addrs_to_phy_ich8lan<I: E1000RegisterIo, P: Ich8PhyRegOps>(
    io: &mut I,
    phy: &mut P,
    rar_count: u16,
) -> DevResult {
    phy.acquire()?;
    let mut saved = 0;
    let result = (|| {
        phy.wakeup_reg_access(true, &mut saved)?;
        for index in 0..rar_count {
            let low = io.read_register(E1000_RA + u32::from(index) * 8)?;
            let high = io.read_register(E1000_RA + 4 + u32::from(index) * 8)?;
            let base = 16 + index * 4;
            phy.write_reg_page(769, base, low as u16)?;
            phy.write_reg_page(769, base + 1, (low >> 16) as u16)?;
            phy.write_reg_page(769, base + 2, high as u16)?;
            phy.write_reg_page(769, base + 3, ((high & E1000_RAH_AV) >> 16) as u16)?;
        }
        Ok(())
    })();
    let restore = phy.wakeup_reg_access(false, &mut saved);
    phy.release();
    result.and(restore)
}

/// upstream: e1000_ich8lan.c e1000_id_led_init_pchlan()
pub fn id_led_init_pchlan<I: E1000RegisterIo, N: E1000NvmAccess>(
    io: &mut I,
    nvm: &mut N,
    word: u16,
) -> DevResult<Ich8LedState> {
    let data = valid_led_default_ich8lan(nvm, word)?;
    let default = io.read_register(0x0e00)?;
    let mut mode1 = default;
    let mut mode2 = default;
    for index in 0u16..4 {
        let code = (data >> (index * 4)) & 0xf;
        let shift = u32::from(index) * 5;
        let on = 2u32;
        let off = 2u32 | 8;
        if matches!(code, 4 | 5 | 6) {
            mode1 = (mode1 & !(0x1f << shift)) | (on << shift);
        }
        if matches!(code, 7 | 8 | 9) {
            mode1 = (mode1 & !(0x1f << shift)) | (off << shift);
        }
        if matches!(code, 2 | 5 | 8) {
            mode2 = (mode2 & !(0x1f << shift)) | (on << shift);
        }
        if matches!(code, 3 | 6 | 9) {
            mode2 = (mode2 & !(0x1f << shift)) | (off << shift);
        }
    }
    Ok(Ich8LedState {
        ledctl_default: default,
        ledctl_mode1: mode1,
        ledctl_mode2: mode2,
        phy_ife: false,
    })
}

/// upstream: e1000_ich8lan.c e1000_igp3_phy_powerdown_workaround_ich8lan()
pub fn igp3_phy_powerdown_workaround_ich8lan<I: E1000RegisterIo, P: Ich8PhyRegOps>(
    io: &mut I,
    phy: &mut P,
    phy_kind: Ich8PhyKind,
    mac: E1000MacType,
) -> DevResult {
    if phy_kind != Ich8PhyKind::Igp3 {
        return Ok(());
    }
    for attempt in 0..2 {
        let control = io.read_register(E1000_PHY_CTRL)? | 0x0000_0c00;
        io.write_register(E1000_PHY_CTRL, control)?;
        if mac == E1000MacType::Ich8Lan {
            if let Ok(value) = phy.read_reg(0x19) {
                let _ = phy.write_reg(0x19, value | 0x0200);
            }
        }
        let value = phy.read_reg(0x0c)? & !0x0700;
        phy.write_reg(0x0c, value | 0x0100)?;
        if phy.read_reg(0x0c)? & 0x0700 == 0x0100 || attempt != 0 {
            break;
        }
        let ctrl = io.read_register(E1000_CTRL)?;
        io.write_register(E1000_CTRL, ctrl | 0x0000_8000)?;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_lv_phy_workarounds_ich8lan()
pub fn lv_phy_workarounds_ich8lan<P: Ich8PhyRegOps>(phy: &mut P, mac: E1000MacType) -> DevResult {
    if mac != E1000MacType::Pch2Lan {
        return Ok(());
    }
    set_mdio_slow_mode_hv(phy)?;
    phy.acquire()?;
    let result = write_emi_reg_locked(phy, 0x16, 0x0034)
        .and_then(|_| write_emi_reg_locked(phy, 0x17, 0x0005));
    phy.release();
    result
}

/// upstream: e1000_ich8lan.c e1000_set_eee_pchlan()
pub fn set_eee_pchlan<P: Ich8PhyRegOps>(
    phy: &mut P,
    phy_kind: Ich8PhyKind,
    disabled: bool,
    lp_advertisement: &mut u16,
) -> DevResult {
    let (lpa, pcs_status, advertisement) = match phy_kind {
        Ich8PhyKind::Hv82579 => (0x0040, 0x0042, 0x003c),
        Ich8PhyKind::I217 => (0x00a0, 0x00a2, 0x009c),
        _ => return Ok(()),
    };
    phy.acquire()?;
    let result = (|| {
        let mut lpi = phy.read_reg_locked(0x0e)? & !0x0007;
        if !disabled {
            *lp_advertisement = read_emi_reg_locked(phy, lpa)?;
            let adv = read_emi_reg_locked(phy, advertisement)?;
            if adv & *lp_advertisement & 0x0002 != 0 {
                lpi |= 0x0004;
            }
            if adv & *lp_advertisement & 0x0001 != 0 {
                if phy.read_reg_locked(0x05)? & 0x0100 != 0 {
                    lpi |= 0x0002;
                } else {
                    *lp_advertisement &= !1;
                }
            }
        }
        if phy_kind == Ich8PhyKind::Hv82579 {
            let pll = read_emi_reg_locked(phy, 0x0019)? & !0x0001;
            write_emi_reg_locked(phy, 0x0019, pll)?;
        }
        let _ = read_emi_reg_locked(phy, pcs_status)?;
        phy.write_reg_locked(0x0e, lpi)
    })();
    phy.release();
    result
}

/// upstream: e1000_ich8lan.c e1000_oem_bits_config_ich8lan()
pub fn oem_bits_config_ich8lan<I: E1000RegisterIo, P: Ich8PhyRegOps>(
    io: &mut I,
    phy: &mut P,
    mac: E1000MacType,
    d0_state: bool,
    reset_blocked: bool,
) -> DevResult {
    if mac < E1000MacType::PchLan {
        return Ok(());
    }
    phy.acquire()?;
    let result = (|| {
        if mac == E1000MacType::PchLan
            && io.read_register(E1000_EXTCNF_CTRL)? & E1000_EXTCNF_CTRL_OEM_WRITE_ENABLE != 0
        {
            return Ok(());
        }
        if io.read_register(E1000_FEXTNVM)? & (1 << 27) == 0 {
            return Ok(());
        }
        let mac_ctrl = io.read_register(E1000_PHY_CTRL)?;
        let mut oem = phy.read_reg_locked(0x6019)? & !(0x0040 | 0x0004);
        if d0_state {
            if mac_ctrl & 0x40 != 0 {
                oem |= 0x0040;
            }
            if mac_ctrl & E1000_PHY_CTRL_D0A_LPLU != 0 {
                oem |= 0x0004;
            }
        } else {
            if mac_ctrl & (0x40 | 0x08) != 0 {
                oem |= 0x0040;
            }
            if mac_ctrl & (E1000_PHY_CTRL_D0A_LPLU | E1000_PHY_CTRL_NOND0A_LPLU) != 0 {
                oem |= 0x0004;
            }
        }
        if (d0_state || mac != E1000MacType::PchLan) && !reset_blocked {
            oem |= 0x0400;
        }
        phy.write_reg_locked(0x6019, oem)
    })();
    phy.release();
    result
}

pub trait Ich8LifecycleCallbacks {
    fn init_phy_workarounds(&mut self) -> DevResult;
    fn configure_smbus(&mut self) -> DevResult;
    fn disable_phy(&mut self) -> DevResult;
    fn enable_phy(&mut self) -> DevResult;
    fn set_oem_bits(&mut self, d0: bool) -> DevResult;
    fn reset_phy(&mut self) -> DevResult;
    fn acquire_phy(&mut self) -> DevResult;
    fn release_phy(&mut self);
    fn set_i217_power_good_bits(&mut self, resume: bool) -> DevResult;
    fn check_link(&mut self) -> DevResult<bool>;
    fn read_kmrn_diag(&mut self) -> DevResult<u16>;
    fn reset_phy_delay(&mut self, millis: u32) -> DevResult;
    fn sw_lcd_config(&mut self) -> DevResult;
    fn gate_phy_config(&mut self, gate: bool) -> DevResult;
    fn set_lpi_update_timer(&mut self) -> DevResult;
}

/// upstream: e1000_ich8lan.c e1000_post_phy_reset_ich8lan()
pub fn post_phy_reset_ich8lan<O: Ich8LifecycleCallbacks>(
    ops: &mut O,
    mac: E1000MacType,
    reset_blocked: bool,
    firmware_managed: bool,
) -> DevResult {
    if reset_blocked {
        return Ok(());
    }
    ops.reset_phy_delay(10)?;
    match mac {
        E1000MacType::PchLan => ops.init_phy_workarounds()?,
        E1000MacType::Pch2Lan => ops.init_phy_workarounds()?,
        _ => {}
    }
    ops.sw_lcd_config()?;
    ops.set_oem_bits(true)?;
    if mac == E1000MacType::Pch2Lan && !firmware_managed {
        ops.reset_phy_delay(10)?;
        ops.gate_phy_config(false)?;
    }
    if mac == E1000MacType::Pch2Lan {
        ops.acquire_phy()?;
        let result = ops.set_lpi_update_timer();
        ops.release_phy();
        result?;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_platform_pm_pch_lpt()
pub fn platform_pm_pch_lpt<I: E1000RegisterIo>(
    io: &mut I,
    link: bool,
    speed_mbps: u16,
    max_frame_size: u32,
    rx_buffer_kb: u32,
    max_snoop: u16,
    max_nosnoop: u16,
) -> DevResult<i32> {
    let mut encoded = 0u16;
    let mut high_water = 0i32;
    if link {
        if max_frame_size == 0 || speed_mbps == 0 {
            return Err(DevError::InvalidParam);
        }
        let raw =
            (i64::from(rx_buffer_kb) * 1024 - 2 * i64::from(max_frame_size)).max(0) * 8 * 1000
                / i64::from(speed_mbps);
        let mut value = raw as u64;
        let mut scale = 0u8;
        while value > 0x3ff {
            scale += 1;
            value = value.div_ceil(32);
        }
        if scale > 5 {
            return Err(DevError::InvalidParam);
        }
        encoded = (u16::from(scale) << 10) | value as u16;
        let platform = max_snoop.max(max_nosnoop);
        let platform_ns = ltr2ns(platform & 0x03ff, ((platform >> 10) & 7) as u8)?;
        let encoded_ns = ltr2ns(encoded & 0x03ff, ((encoded >> 10) & 7) as u8)?;
        let latency = if encoded_ns > platform_ns {
            encoded = platform;
            platform_ns
        } else {
            encoded_ns
        };
        if latency != 0 {
            high_water = (i64::from(rx_buffer_kb)
                - (latency as i64 * i64::from(speed_mbps) / 8_000_000))
                as i32;
        }
        if !(0..=0x1f).contains(&high_water) {
            return Err(DevError::InvalidParam);
        }
    }
    let ltr = u32::from(encoded)
        | (u32::from(encoded) << 16)
        | (u32::from(link) << 15)
        | (u32::from(link) << 31)
        | (1 << 30);
    io.write_register(0x000f8, ltr)?;
    let svt = io.read_register(0x000f4)?;
    io.write_register(0x000f4, (svt & !0x1f) | high_water as u32)?;
    let svcr = io.read_register(E1000_SVCR)? | 0x0000_1001;
    io.write_register(E1000_SVCR, svcr)?;
    Ok(high_water)
}

pub trait Ich8JumboOps {
    fn set_rx_filter_crc(&mut self, enabled: bool) -> DevResult;
    fn copy_rx_addrs_to_phy(&mut self) -> DevResult;
    fn set_kmrn_jumbo_mode(&mut self, enabled: bool) -> DevResult;
    fn configure_phy_jumbo(&mut self, enabled: bool) -> DevResult;
}

/// upstream: e1000_ich8lan.c e1000_lv_jumbo_workaround_ich8lan()
pub fn lv_jumbo_workaround_ich8lan<I: E1000RegisterIo, O: Ich8JumboOps>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
    enable: bool,
    rar_count: u16,
) -> DevResult {
    if mac < E1000MacType::Pch2Lan {
        return Ok(());
    }
    let mut phy = io.read_register(E1000_PHY_CTRL)?;
    io.write_register(E1000_PHY_CTRL, phy | (1 << 14))?;
    if enable {
        for index in 0..rar_count {
            let high = io.read_register(E1000_RA + 4 + u32::from(index) * 8)?;
            if high & E1000_RAH_AV == 0 {
                continue;
            }
            let low = io.read_register(E1000_RA + u32::from(index) * 8)?;
            let address = [
                low as u8,
                (low >> 8) as u8,
                (low >> 16) as u8,
                (low >> 24) as u8,
                high as u8,
                (high >> 8) as u8,
            ];
            io.write_register(0x0b000 + u32::from(index) * 4, calc_rx_da_crc(address))?;
        }
        ops.copy_rx_addrs_to_phy()?;
        let debug = io.read_register(0x05b00)?;
        io.write_register(0x05b00, (debug & !(1 << 14)) | (7 << 15))?;
        ops.set_rx_filter_crc(true)?;
    } else {
        let debug = io.read_register(0x05b00)?;
        io.write_register(0x05b00, debug & !(0xf << 14))?;
        ops.set_rx_filter_crc(false)?;
    }
    ops.set_kmrn_jumbo_mode(enable)?;
    ops.configure_phy_jumbo(enable)?;
    phy = io.read_register(E1000_PHY_CTRL)?;
    io.write_register(E1000_PHY_CTRL, phy & !(1 << 14))
}

/// upstream: e1000_ich8lan.c e1000_resume_workarounds_pchlan()
pub fn resume_workarounds_pchlan<O: Ich8LifecycleCallbacks>(
    ops: &mut O,
    mac: E1000MacType,
    phy: Ich8PhyKind,
) -> DevResult {
    if mac < E1000MacType::Pch2Lan {
        return Ok(());
    }
    ops.init_phy_workarounds()?;
    if phy == Ich8PhyKind::I217 {
        ops.acquire_phy()?;
        let result = ops.set_i217_power_good_bits(true);
        ops.release_phy();
        result?;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_suspend_workarounds_ich8lan()
pub fn suspend_workarounds_ich8lan<I: E1000RegisterIo, O: Ich8LifecycleCallbacks>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
    phy: Ich8PhyKind,
) -> DevResult {
    let mut ctrl = io.read_register(E1000_PHY_CTRL)? | 0x40;
    if phy == Ich8PhyKind::I217 {
        if ops.acquire_phy().is_ok() {
            let result = ops.set_i217_power_good_bits(false);
            ops.release_phy();
            result?;
        }
    }
    io.write_register(E1000_PHY_CTRL, ctrl)?;
    if mac == E1000MacType::Ich8Lan {
        if let Ok(v) = io.read_register(E1000_PHY_CTRL) {
            ctrl = v | 0x08;
            io.write_register(E1000_PHY_CTRL, ctrl)?;
        }
    }
    if mac >= E1000MacType::PchLan {
        ops.set_oem_bits(false)?;
        if mac == E1000MacType::PchLan {
            ops.reset_phy()?;
        }
        ops.acquire_phy()?;
        let result = ops.configure_smbus();
        ops.release_phy();
        result?;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_kmrn_lock_loss_workaround_ich8lan()
pub fn kmrn_lock_loss_workaround_ich8lan<I: E1000RegisterIo, O: Ich8LifecycleCallbacks>(
    io: &mut I,
    ops: &mut O,
    enabled: bool,
) -> DevResult {
    if !enabled || !ops.check_link()? {
        return Ok(());
    }
    for _ in 0..10 {
        let _ = ops.read_kmrn_diag()?;
        let status = ops.read_kmrn_diag()?;
        if status & 0x2000 == 0 {
            return Ok(());
        }
        ops.reset_phy()?;
        ops.reset_phy_delay(5)?;
    }
    let ctrl = io.read_register(E1000_PHY_CTRL)? | 0x48;
    io.write_register(E1000_PHY_CTRL, ctrl)?;
    Err(DevError::Io)
}

pub trait Ich8LifecycleOps {
    fn disable_pcie_master(&mut self) -> DevResult;
    fn read_nvm_k1(&mut self) -> DevResult<bool>;
    fn phy_reset_blocked(&mut self) -> bool;
    fn acquire_phy(&mut self) -> DevResult;
    fn release_phy(&mut self);
    fn get_cfg_done(&mut self) -> DevResult;
    fn post_phy_reset(&mut self) -> DevResult;
    fn init_rx_addrs(&mut self) -> DevResult;
    fn setup_link(&mut self) -> DevResult;
    fn set_no_snoop(&mut self, value: u32) -> DevResult;
    fn id_led_init(&mut self) -> DevResult;
    fn read_pci_vendor_id(&mut self) -> DevResult<u16>;
    fn reconfigure_k1_exit_timeout(&mut self) -> DevResult;
}

/// upstream: e1000_ich8lan.c e1000_reset_hw_ich8lan()
pub fn reset_hw_ich8lan<I: E1000RegisterIo, O: Ich8LifecycleOps>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
) -> DevResult {
    let _ = ops.disable_pcie_master();
    io.write_register(E1000_IMC, u32::MAX)?;
    io.write_register(E1000_RCTL, 0)?;
    io.write_register(E1000_TCTL, E1000_TCTL_PSP)?;
    let _ = io.read_register(E1000_STATUS)?;
    io.delay_us(10_000);
    if mac == E1000MacType::Ich8Lan {
        io.write_register(E1000_PBA, E1000_PBA_8K)?;
        io.write_register(E1000_PBS, 16)?;
    }
    let mut ctrl = io.read_register(E1000_CTRL)?;
    if !ops.phy_reset_blocked() {
        ctrl |= E1000_CTRL_PHY_RST;
        if mac == E1000MacType::Pch2Lan && io.read_register(E1000_FWSM)? & 0x0000_8000 == 0 {
            gate_hw_phy_config_ich8lan(io, true)?;
        }
    }
    let swflag = acquire_swflag_ich8lan(io);
    let pci_id = ops.read_pci_vendor_id()?;
    io.write_register(E1000_STRAP, u32::from(pci_id))?;
    io.write_register(E1000_CTRL, ctrl | 0x0400_0000)?;
    io.delay_us(20_000);
    let pci_id = ops.read_pci_vendor_id()?;
    io.write_register(E1000_STRAP, u32::from(pci_id))?;
    if mac == E1000MacType::Pch2Lan {
        let value = io.read_register(E1000_FEXTNVM3)?;
        io.write_register(E1000_FEXTNVM3, (value & !0x0000_0300) | 0x0000_0200)?;
    }
    if ctrl & E1000_CTRL_PHY_RST != 0 {
        ops.get_cfg_done()?;
        ops.post_phy_reset()?;
    }
    if mac == E1000MacType::PchLan {
        io.write_register(0x03f18, 0x6565_6565)?;
    }
    io.write_register(E1000_IMC, u32::MAX)?;
    let _ = io.read_register(E1000_ICR)?;
    let k = io.read_register(0x03008)?;
    io.write_register(0x03008, k | 0x0000_0100)?;
    if mac >= E1000MacType::PchPtp && mac < E1000MacType::I82575 {
        let value = io.read_register(E1000_CTRL_EXT)?;
        io.write_register(E1000_CTRL_EXT, value & !0x0000_0020)?;
    }
    if swflag.is_ok() {
        release_swflag_ich8lan(io)?;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_init_hw_ich8lan()
pub fn init_hw_ich8lan<I: E1000RegisterIo, O: Ich8LifecycleOps>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
) -> DevResult {
    initialize_hw_bits_ich8lan(io, mac)?;
    if matches!(
        mac,
        E1000MacType::PchMtp | E1000MacType::PchPtp | E1000MacType::PchNvp
    ) {
        ops.acquire_phy()?;
        let result = ops.reconfigure_k1_exit_timeout();
        ops.release_phy();
        result?;
    }
    let _ = ops.id_led_init();
    ops.init_rx_addrs()?;
    for index in 0..128 {
        io.write_register(0x05200 + index * 4, 0)?;
    }
    if mac == E1000MacType::PchLan {
        let value = io.read_register(0x05b00)?;
        io.write_register(0x05b00, value & !0x0000_0040)?;
    }
    let result = ops.setup_link();
    for txdctl in [0x03828, 0x04028] {
        let mut value = io.read_register(txdctl)?;
        value = (value & !E1000_TXDCTL_WTHRESH) | E1000_TXDCTL_FULL_TX_DESC_WB;
        value = (value & !E1000_TXDCTL_PTHRESH) | E1000_TXDCTL_MAX_TX_DESC_PREFETCH;
        io.write_register(txdctl, value)?;
    }
    ops.set_no_snoop(if mac == E1000MacType::Ich8Lan {
        0x0000_000f
    } else {
        !0x0000_000f
    })?;
    if mac >= E1000MacType::PchTgp {
        let debug = io.read_register(0x05b00)?;
        io.write_register(0x05b00, debug | (1 << 12))?;
    }
    let ctrl_ext = io.read_register(E1000_CTRL_EXT)? | E1000_CTRL_EXT_RO_DIS;
    io.write_register(E1000_CTRL_EXT, ctrl_ext)?;
    clear_hw_cntrs_ich8lan(io)?;
    result
}

/// upstream: e1000_ich8lan.c e1000_cleanup_led_ich8lan()
pub fn cleanup_led_ich8lan<I: E1000RegisterIo, P: Ich8PhyRegOps>(
    io: &mut I,
    phy: &mut P,
    state: Ich8LedState,
) -> DevResult {
    if state.phy_ife {
        return phy.write_reg(0x1b, 0);
    }
    io.write_register(0x0e00, state.ledctl_default)
}

/// upstream: e1000_ich8lan.c e1000_led_on_ich8lan()
pub fn led_on_ich8lan<I: E1000RegisterIo, P: Ich8PhyRegOps>(
    io: &mut I,
    phy: &mut P,
    state: Ich8LedState,
) -> DevResult {
    if state.phy_ife {
        return phy.write_reg(0x1b, 0x400);
    }
    io.write_register(0x0e00, state.ledctl_mode2)
}

/// upstream: e1000_ich8lan.c e1000_led_off_ich8lan()
pub fn led_off_ich8lan<I: E1000RegisterIo, P: Ich8PhyRegOps>(
    io: &mut I,
    phy: &mut P,
    state: Ich8LedState,
) -> DevResult {
    if state.phy_ife {
        return phy.write_reg(0x1b, 0x800);
    }
    io.write_register(0x0e00, state.ledctl_mode1)
}

/// upstream: e1000_ich8lan.c e1000_set_mdio_slow_mode_hv()
pub fn set_mdio_slow_mode_hv<P: Ich8PhyRegOps>(phy: &mut P) -> DevResult {
    let value = phy.read_reg_page(769, 16)?;
    phy.write_reg_page(769, 16, value | 0x0400)
}

/// upstream: e1000_ich8lan.c e1000_k1_workaround_lv()
pub fn k1_workaround_lv<I: E1000RegisterIo, P: Ich8PhyRegOps>(
    io: &mut I,
    phy: &mut P,
    mac: E1000MacType,
) -> DevResult {
    if mac != E1000MacType::Pch2Lan {
        return Ok(());
    }
    let status = phy.read_reg(0x11)?;
    if status & 0x2400 == 0x2400 {
        if status & 0x0c00 != 0 {
            let pm = phy.read_reg(0x13)?;
            phy.write_reg(0x13, pm & !0x0001)?;
        } else {
            let reg = io.read_register(E1000_FEXTNVM4)?;
            io.write_register(E1000_FEXTNVM4, (reg & !0x0000_0f00) | 0x0000_0800)?;
        }
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_set_obff_timer_pch_lpt()
pub fn set_obff_timer_pch_lpt<I: E1000RegisterIo>(io: &mut I, itr: u32) -> DevResult {
    let timer = (itr & 0x000f_ffff).saturating_mul(256) / 1000;
    if timer > 0x000f_ffff {
        return Err(DevError::InvalidParam);
    }
    let value = io.read_register(E1000_SVCR)?;
    io.write_register(E1000_SVCR, (value & !0xffff_0000) | (timer << 16))
}

/// upstream: e1000_ich8lan.c e1000_set_kmrn_lock_loss_workaround_ich8lan()
pub fn set_kmrn_lock_loss_workaround_ich8lan(mac: E1000MacType, enabled: bool, state: &mut bool) {
    if mac == E1000MacType::Ich8Lan {
        *state = enabled;
    }
}

/// upstream: e1000_ich8lan.c e1000_initialize_hw_bits_ich8lan()
pub fn initialize_hw_bits_ich8lan<I: E1000RegisterIo>(io: &mut I, mac: E1000MacType) -> DevResult {
    let mut value = io.read_register(E1000_CTRL_EXT)? | (1 << 22);
    if mac >= E1000MacType::PchLan {
        value |= 1 << 20;
    }
    io.write_register(E1000_CTRL_EXT, value)?;
    for txdctl in [0x03828, 0x04028] {
        let v = io.read_register(txdctl)? | (1 << 22);
        io.write_register(txdctl, v)?;
    }
    let mut tarc0 = io.read_register(0x03840)?;
    if mac == E1000MacType::Ich8Lan {
        tarc0 |= (1 << 28) | (1 << 29);
    }
    io.write_register(
        0x03840,
        tarc0 | (1 << 23) | (1 << 24) | (1 << 26) | (1 << 27),
    )?;
    let mut tarc1 = io.read_register(0x03940)?;
    if io.read_register(E1000_TCTL)? & (1 << 20) != 0 {
        tarc1 &= !(1 << 28);
    } else {
        tarc1 |= 1 << 28;
    }
    io.write_register(0x03940, tarc1 | (1 << 24) | (1 << 26) | (1 << 30))?;
    if mac == E1000MacType::Ich8Lan {
        let status = io.read_register(E1000_STATUS)?;
        io.write_register(E1000_STATUS, status & !(1 << 31))?;
    }
    let mut rfctl = io.read_register(E1000_RFCTL)? | E1000_RFCTL_NFSW_DIS | E1000_RFCTL_NFSR_DIS;
    if mac == E1000MacType::Ich8Lan {
        rfctl |= E1000_RFCTL_IPV6_EX_DIS | E1000_RFCTL_NEW_IPV6_EXT_DIS;
    }
    io.write_register(E1000_RFCTL, rfctl)?;
    if mac >= E1000MacType::PchLpt {
        let ecc = io.read_register(E1000_PBECCSTS)? | E1000_PBECCSTS_ECC_ENABLE;
        io.write_register(E1000_PBECCSTS, ecc)?;
        let ctrl = io.read_register(E1000_CTRL)? | 0x0008_0000;
        io.write_register(E1000_CTRL, ctrl)?;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_setup_led_pchlan()
pub fn setup_led_pchlan<O: Ich8LedOps>(ops: &mut O) -> DevResult {
    ops.setup_led()
}
/// upstream: e1000_ich8lan.c e1000_cleanup_led_pchlan()
pub fn cleanup_led_pchlan<O: Ich8LedOps>(ops: &mut O) -> DevResult {
    ops.cleanup_led()
}
/// upstream: e1000_ich8lan.c e1000_led_on_pchlan()
pub fn led_on_pchlan<O: Ich8LedOps>(ops: &mut O) -> DevResult {
    ops.led_on()
}
/// upstream: e1000_ich8lan.c e1000_led_off_pchlan()
pub fn led_off_pchlan<O: Ich8LedOps>(ops: &mut O) -> DevResult {
    ops.led_off()
}

pub trait Ich8WorkaroundOps {
    fn init_phy_script(&mut self) -> DevResult;
    fn init_base(&mut self) -> DevResult;
    fn init_rx_addrs(&mut self, count: u16) -> DevResult;
    fn setup_link(&mut self) -> DevResult;
    fn id_led_init(&mut self) -> DevResult;
    fn check_mng_mode(&mut self) -> bool;
    fn power_down_phy(&mut self) -> DevResult;
    fn read_phy_locked(&mut self, reg: u16) -> DevResult<u16>;
    fn write_phy_locked(&mut self, reg: u16, value: u16) -> DevResult;
    fn acquire_swflag_for_init(&mut self) -> DevResult;
    fn release_swflag_after_init(&mut self);
    fn reset_is_blocked(&mut self) -> bool;
    fn reconfigure_k1_for_init(&mut self) -> DevResult;
}

/// upstream: e1000_ich8lan.c e1000_init_phy_workarounds_pchlan()
pub fn init_phy_workarounds_pchlan<I: E1000RegisterIo, O: Ich8WorkaroundOps>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
    managed: bool,
    mut disable_ulp: impl FnMut(bool) -> DevResult,
    mut phy_accessible: impl FnMut() -> bool,
) -> DevResult {
    gate_hw_phy_config_ich8lan(io, true)?;
    let _ = disable_ulp(true);
    ops.acquire_swflag_for_init()?;
    if matches!(
        mac,
        E1000MacType::PchMtp | E1000MacType::PchPtp | E1000MacType::PchNvp
    ) {
        let _ = ops.reconfigure_k1_for_init();
    }
    if !phy_accessible() {
        let ext = io.read_register(E1000_CTRL_EXT)? | 0x00000800;
        io.write_register(E1000_CTRL_EXT, ext)?;
        io.delay_us(50_000);
        if !phy_accessible() {
            if managed {
                return Err(DevError::ResourceBusy);
            }
            toggle_lanphypc_pch_lpt(io, mac)?;
            if !phy_accessible() {
                return Err(DevError::Io);
            }
        }
    }
    ops.release_swflag_after_init();
    if ops.reset_is_blocked() {
        return Err(DevError::ResourceBusy);
    }
    ops.init_phy_script()?;
    if matches!(
        mac,
        E1000MacType::PchMtp | E1000MacType::PchPtp | E1000MacType::PchNvp
    ) {
        ops.reconfigure_k1_for_init()?;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_phy_hw_reset_ich8lan()
pub fn phy_hw_reset_ich8lan<I: E1000RegisterIo, O: Ich8WorkaroundOps>(
    io: &mut I,
    ops: &mut O,
    mac: E1000MacType,
    reset_allowed: bool,
) -> DevResult {
    if !reset_allowed {
        return Err(DevError::ResourceBusy);
    }
    if matches!(
        mac,
        E1000MacType::PchLpt
            | E1000MacType::PchSpt
            | E1000MacType::PchCnp
            | E1000MacType::PchTgp
            | E1000MacType::PchAdp
            | E1000MacType::PchMtp
            | E1000MacType::PchPtp
            | E1000MacType::PchNvp
    ) {
        toggle_lanphypc_pch_lpt(io, mac)?;
    }
    ops.init_phy_script()
}

/// upstream: e1000_ich8lan.c e1000_disable_ulp_lpt_lp()
pub fn disable_ulp_lpt_lp<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    device_id: u16,
    force: bool,
    mut phy_disable: impl FnMut() -> DevResult,
) -> DevResult {
    if mac < E1000MacType::PchLpt || matches!(device_id, 0x155a | 0x1559 | 0x15a0 | 0x15a1) {
        return Ok(());
    }
    let fwsm = io.read_register(E1000_FWSM)?;
    if fwsm & 0x00008000 != 0 {
        let mut h2me = io.read_register(E1000_H2ME)?;
        if force {
            h2me = (h2me & !0x2) | 0x4;
            io.write_register(E1000_H2ME, h2me)?;
        }
        for _ in 0..250 {
            if io.read_register(E1000_FWSM)? & 0x00001000 == 0 {
                break;
            }
            io.delay_us(10_000)
        }
        let mut h = io.read_register(E1000_H2ME)?;
        if force {
            h &= !0x4
        } else {
            h &= !0x2
        }
        io.write_register(E1000_H2ME, h)?;
        return Ok(());
    }
    phy_disable()
}

/// upstream: e1000_ich8lan.c e1000_enable_ulp_lpt_lp()
pub fn enable_ulp_lpt_lp<I: E1000RegisterIo>(
    io: &mut I,
    mac: E1000MacType,
    to_sx: bool,
    mut phy_enable: impl FnMut() -> DevResult,
) -> DevResult {
    if mac < E1000MacType::PchLpt {
        return Ok(());
    }
    if io.read_register(E1000_FWSM)? & 0x00008000 != 0 {
        let mut h = io.read_register(E1000_H2ME)?;
        h |= 0x2;
        if to_sx {
            h |= 0x4
        }
        io.write_register(E1000_H2ME, h)?;
        for _ in 0..250 {
            if io.read_register(E1000_FWSM)? & 0x00001000 != 0 {
                return Ok(());
            }
            io.delay_us(10_000)
        }
        Err(DevError::Io)
    } else {
        phy_enable()
    }
}

/// upstream: e1000_ich8lan.c e1000_configure_k0s_lpt()
pub fn configure_k0s_lpt<P: PhyOps82571>(
    phy: &mut P,
    entry_latency: u8,
    min_time: u8,
) -> DevResult {
    if entry_latency > 3 || min_time > 4 {
        return Err(DevError::InvalidParam);
    }
    let mut value = phy.read_phy(0x1b)?;
    value = (value & !0x000f) | (u16::from(min_time) << 0);
    phy.write_phy(0x1b, value)
}

/// upstream: e1000_ich8lan.c e1000_configure_k1_ich8lan()
pub fn configure_k1_ich8lan<P: PhyOps82571>(phy: &mut P, enable: bool) -> DevResult {
    let mut v = phy.read_phy(0x19)?;
    if enable {
        v |= 1
    } else {
        v &= !1
    }
    phy.write_phy(0x19, v)
}

/// upstream: e1000_ich8lan.c e1000_k1_workaround_lpt_lp()
pub fn k1_workaround_lpt_lp<I: E1000RegisterIo, P: PhyOps82571>(
    io: &mut I,
    phy: &mut P,
    link: bool,
) -> DevResult {
    let mut f = io.read_register(E1000_FEXTNVM6)?;
    let status = io.read_register(E1000_STATUS)?;
    if link && status & 0x00000080 != 0 {
        let reg = phy.read_phy(0x19)?;
        phy.write_phy(0x19, reg & !1)?;
        phy.delay_us(10);
        f |= 0x100;
        io.write_register(E1000_FEXTNVM6, f)?;
        phy.write_phy(0x19, reg)?;
    } else {
        f &= !0x100;
        if status & 0x00000040 != 0 {
            let mut inband = phy.read_phy(0x1a)?;
            inband = (inband & !0x03ff) | if status & 0x00000040 != 0 { 5 } else { 50 };
            phy.write_phy(0x1a, inband)?;
        }
        io.write_register(E1000_FEXTNVM6, f)?;
    }
    Ok(())
}

/// upstream: e1000_ich8lan.c e1000_k1_gig_workaround_hv()
pub fn k1_gig_workaround_hv<I: E1000RegisterIo, P: PhyOps82571>(
    io: &mut I,
    phy: &mut P,
    link: bool,
) -> DevResult {
    let status = io.read_register(E1000_STATUS)?;
    if link && status & 0x80 != 0 {
        let mut k = phy.read_phy(0x19)?;
        phy.write_phy(0x19, k & !1)?;
        phy.delay_us(10);
        k |= 1;
        phy.write_phy(0x19, k)
    } else {
        Ok(())
    }
}

/// upstream: e1000_ich8lan.c e1000_gig_downshift_workaround_ich8lan()
pub fn gig_downshift_workaround_ich8lan<P: PhyOps82571>(phy: &mut P) -> DevResult {
    let value = phy.read_phy(0x19)? | 0x200;
    phy.write_phy(0x19, value)
}

/// upstream: e1000_ich8lan.c e1000_hv_phy_workarounds_ich8lan()
pub fn hv_phy_workarounds_ich8lan<P: Ich8PhyRegOps>(
    phy: &mut P,
    mac: E1000MacType,
    kind: Ich8PhyKind,
    revision: u8,
) -> DevResult {
    if mac != E1000MacType::PchLan {
        return Ok(());
    }
    if kind == Ich8PhyKind::Hv82577 {
        set_mdio_slow_mode_hv(phy)?;
    }
    if (kind == Ich8PhyKind::Hv82577 && matches!(revision, 1 | 2))
        || (kind == Ich8PhyKind::Hv82578 && revision == 1)
    {
        phy.write_reg_page(769, 25, 0x4431)?;
        phy.write_reg_page(770, 16, 0xa204)?;
    }
    if kind == Ich8PhyKind::Hv82578 && revision < 2 {
        phy.write_reg(0, 0x8000)?;
        phy.write_reg(0, 0x3140)?;
    }
    phy.acquire()?;
    let result = (|| {
        phy.write_reg_page(0, 31, 0)?;
        let port = phy.read_reg_locked(0x10)?;
        phy.write_reg_locked(0x10, port & 0x00ff)?;
        write_emi_reg_locked(phy, 0x0034, 0x0034)
    })();
    phy.release();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ltr_units_follow_five_bit_scale_steps() {
        assert_eq!(ltr2ns(7, 0).unwrap(), 7);
        assert_eq!(ltr2ns(7, 1).unwrap(), 224);
        assert_eq!(ltr2ns(7, 5).unwrap(), 234_881_024);
        assert!(ltr2ns(1, 6).is_err());
    }

    #[test]
    fn mac_generation_order_tracks_pch_gates() {
        assert!(E1000MacType::PchLan >= E1000MacType::Ich10Lan);
        assert!(E1000MacType::Pch2Lan < E1000MacType::PchLpt);
        assert!(E1000MacType::PchTgp < E1000MacType::I82575);
    }

    #[test]
    fn receive_address_crc_uses_ethernet_polynomial() {
        assert_eq!(calc_rx_da_crc([0, 0, 0, 0, 0, 0]), 0xb1c2_a1a3);
    }
}
