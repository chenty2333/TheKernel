//! Intel i210/i211 NVM, semaphore, and initialization routines.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_i210.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{api::E1000MacType, nvm::E1000NvmAccess, osdep::E1000RegisterIo, registers::*};

const SWFW_TIMEOUT: usize = 200;
const SWFW_RETRY_US: u32 = 5_000;
const NVM_WORD_TIMEOUT: usize = 100_000;
const I210_NVM_SUM: u16 = 0xbaba;
const CHECKSUM_WORD: u16 = 0x3f;
const SRWR_DATA_SHIFT: u32 = 16;
const SRWR_DONE: u32 = 2;
const NVM_CHUNK: usize = 512;
const INVM_SIZE: usize = 64;
const INVM_BASE: u32 = 0x12120;

pub trait I210NvmIo: E1000NvmAccess {
    fn read_eerd(&mut self, offset: u16, words: u16) -> DevResult<alloc::vec::Vec<u16>>;
    fn word_size(&self) -> u16;
    fn acquire_nvm(&mut self) -> DevResult;
    fn release_nvm(&mut self);
    fn update_flash(&mut self) -> DevResult;
    fn has_flash_hardware(&mut self) -> bool;
    fn init_nvm_params_82575(&mut self) -> DevResult;
    fn nvm_type(&mut self, kind: I210NvmType);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum I210NvmType {
    Flash,
    Invm,
}

pub trait I210SemaphoreOps {
    fn put_hw_semaphore(&mut self);
    fn clear_semaphore_once(&mut self) -> bool;
}

/// upstream: e1000_i210.c e1000_acquire_nvm_i210()
pub fn acquire_nvm_i210<I: E1000RegisterIo, O: I210SemaphoreOps>(
    io: &mut I,
    ops: &mut O,
    mask: u16,
    word_size: u16,
) -> DevResult {
    acquire_swfw_sync_i210(io, ops, mask, word_size)
}

/// upstream: e1000_i210.c e1000_release_nvm_i210()
pub fn release_nvm_i210<I: E1000RegisterIo, O: I210SemaphoreOps>(
    io: &mut I,
    ops: &mut O,
    mask: u16,
    word_size: u16,
) -> DevResult {
    release_swfw_sync_i210(io, ops, mask, word_size)
}

/// upstream: e1000_i210.c e1000_acquire_swfw_sync_i210()
pub fn acquire_swfw_sync_i210<I: E1000RegisterIo, O: I210SemaphoreOps>(
    io: &mut I,
    ops: &mut O,
    mask: u16,
    word_size: u16,
) -> DevResult {
    let software = u32::from(mask);
    let firmware = software << 16;
    for _ in 0..SWFW_TIMEOUT {
        if get_hw_semaphore_i210(io, ops, usize::from(word_size) + 1, false).is_err() {
            return Err(DevError::ResourceBusy);
        }
        let status = io.read_register(E1000_SW_FW_SYNC)?;
        if status & (software | firmware) == 0 {
            io.write_register(E1000_SW_FW_SYNC, status | software)?;
            ops.put_hw_semaphore();
            return Ok(());
        }
        ops.put_hw_semaphore();
        io.delay_us(SWFW_RETRY_US);
    }
    Err(DevError::ResourceBusy)
}

/// upstream: e1000_i210.c e1000_release_swfw_sync_i210()
pub fn release_swfw_sync_i210<I: E1000RegisterIo, O: I210SemaphoreOps>(
    io: &mut I,
    ops: &mut O,
    mask: u16,
    word_size: u16,
) -> DevResult {
    while get_hw_semaphore_i210(io, ops, usize::from(word_size) + 1, false).is_err() {}
    let status = io.read_register(E1000_SW_FW_SYNC)?;
    io.write_register(E1000_SW_FW_SYNC, status & !u32::from(mask))?;
    ops.put_hw_semaphore();
    Ok(())
}

/// upstream: e1000_i210.c e1000_get_hw_semaphore_i210()
pub fn get_hw_semaphore_i210<I: E1000RegisterIo, O: I210SemaphoreOps>(
    io: &mut I,
    ops: &mut O,
    timeout: usize,
    clear_once: bool,
) -> DevResult {
    let mut got_smbi = false;
    for _ in 0..timeout {
        if io.read_register(E1000_SWSM)? & E1000_SWSM_SMBI == 0 {
            got_smbi = true;
            break;
        }
        io.delay_us(50);
    }
    if !got_smbi && clear_once && ops.clear_semaphore_once() {
        ops.put_hw_semaphore();
        for _ in 0..timeout {
            if io.read_register(E1000_SWSM)? & E1000_SWSM_SMBI == 0 {
                got_smbi = true;
                break;
            }
            io.delay_us(50);
        }
    }
    if !got_smbi {
        return Err(DevError::ResourceBusy);
    }
    for _ in 0..timeout {
        let swsm = io.read_register(E1000_SWSM)?;
        io.write_register(E1000_SWSM, swsm | E1000_SWSM_SWESMBI)?;
        if io.read_register(E1000_SWSM)? & E1000_SWSM_SWESMBI != 0 {
            return Ok(());
        }
        io.delay_us(50);
    }
    ops.put_hw_semaphore();
    Err(DevError::ResourceBusy)
}

/// upstream: e1000_i210.c e1000_read_nvm_srrd_i210()
pub fn read_nvm_srrd_i210<N: I210NvmIo>(
    nvm: &mut N,
    offset: u16,
    words: u16,
) -> DevResult<alloc::vec::Vec<u16>> {
    let mut result = alloc::vec::Vec::with_capacity(words as usize);
    let mut done = 0u16;
    while done < words {
        let count = (words - done).min(NVM_CHUNK as u16);
        nvm.acquire_nvm()?;
        let read = nvm.read_eerd(offset + done, count);
        nvm.release_nvm();
        let mut chunk = read?;
        if chunk.len() != count as usize {
            return Err(DevError::Io);
        }
        result.append(&mut chunk);
        done += count;
    }
    Ok(result)
}

/// upstream: e1000_i210.c e1000_write_nvm_srwr_i210()
pub fn write_nvm_srwr_i210<I: E1000RegisterIo, N: I210NvmIo>(
    io: &mut I,
    nvm: &mut N,
    offset: u16,
    data: &[u16],
) -> DevResult {
    if data.is_empty() {
        return Err(DevError::InvalidParam);
    }
    let words = u16::try_from(data.len()).map_err(|_| DevError::InvalidParam)?;
    if offset
        .checked_add(words)
        .filter(|end| *end <= nvm.word_size())
        .is_none()
    {
        return Err(DevError::InvalidParam);
    }
    for (chunk_index, chunk) in data.chunks(NVM_CHUNK).enumerate() {
        let base = offset + (chunk_index * NVM_CHUNK) as u16;
        nvm.acquire_nvm()?;
        let result = write_nvm_srwr(io, nvm.word_size(), base, chunk);
        nvm.release_nvm();
        result?;
    }
    Ok(())
}

/// upstream: e1000_i210.c e1000_write_nvm_srwr()
pub fn write_nvm_srwr<I: E1000RegisterIo>(
    io: &mut I,
    word_size: u16,
    offset: u16,
    data: &[u16],
) -> DevResult {
    if data.is_empty() || offset >= word_size || data.len() > usize::from(word_size - offset) {
        return Err(DevError::InvalidParam);
    }
    for (index, value) in data.iter().enumerate() {
        let address = offset + index as u16;
        let request = (u32::from(address) << 2)
            | (u32::from(*value) << SRWR_DATA_SHIFT)
            | E1000_NVM_RW_REG_START;
        io.write_register(E1000_SRWR, request)?;
        let mut complete = false;
        for _ in 0..NVM_WORD_TIMEOUT {
            if io.read_register(E1000_SRWR)? & SRWR_DONE != 0 {
                complete = true;
                break;
            }
            io.delay_us(5);
        }
        if !complete {
            return Err(DevError::Io);
        }
    }
    Ok(())
}

/// upstream: e1000_i210.c e1000_read_invm_word_i210()
pub fn read_invm_word_i210<I: E1000RegisterIo>(io: &mut I, address: u8) -> DevResult<u16> {
    let mut index = 0usize;
    while index < INVM_SIZE {
        let record = io.read_register(INVM_BASE + (index as u32) * 4)?;
        let kind = record & 7;
        if kind == 0 {
            break;
        }
        if kind == 2 {
            index += 1;
        }
        if kind == 4 {
            index += 8;
        }
        if kind == 1 && ((record >> 9) & 0x7f) == u32::from(address) {
            return Ok((record >> 16) as u16);
        }
        index += 1;
    }
    Err(DevError::Io)
}

/// upstream: e1000_i210.c e1000_read_invm_i210()
pub fn read_invm_i210<I: E1000RegisterIo>(
    io: &mut I,
    offset: u16,
    words: u16,
    identity: [u16; 4],
) -> DevResult<alloc::vec::Vec<u16>> {
    if words == 0 {
        return Ok(alloc::vec::Vec::new());
    }
    let mut values = alloc::vec::Vec::with_capacity(words as usize);
    for current in offset..offset.saturating_add(words) {
        let value = match current {
            0..=2 => read_invm_word_i210(io, current as u8)?,
            0x0f => read_invm_word_i210(io, current as u8).unwrap_or(0x7243),
            0x10 => read_invm_word_i210(io, current as u8).unwrap_or(0x00c1),
            0x1c => read_invm_word_i210(io, current as u8).unwrap_or(0x0184),
            0x1d => read_invm_word_i210(io, current as u8).unwrap_or(0x200c),
            0x04 => read_invm_word_i210(io, current as u8).unwrap_or(u16::MAX),
            0x0b => identity[0],
            0x0c => identity[1],
            0x0d => identity[2],
            0x0e => identity[3],
            _ => u16::MAX,
        };
        values.push(value);
    }
    Ok(values)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct I210InvmVersion {
    pub major: u16,
    pub minor: u16,
    pub image_type: u8,
}

/// upstream: e1000_i210.c e1000_read_invm_version()
pub fn read_invm_version<I: E1000RegisterIo>(io: &mut I) -> DevResult<I210InvmVersion> {
    let mut data = [0u32; INVM_SIZE];
    for (index, value) in data.iter_mut().enumerate() {
        *value = io.read_register(INVM_BASE + index as u32 * 4)?;
    }
    let blocks = INVM_SIZE - (8 / 4);
    let mut version = None;
    for i in 1..blocks {
        let current = data[blocks - i];
        let next = data[blocks - i + 1];
        if i == 1 && current & 0x1ff8 == 0 {
            version = Some(0);
            break;
        }
        if i == 1 && current & 0x7fe000 == 0 {
            version = Some(((current & 0x1ff8) >> 3) as u16);
            break;
        }
        if ((current & 0x1ff8 == 0) && (current & 3 == 0)) || ((current & 3 != 0) && i != 1) {
            version = Some(((next & 0x7fe000) >> 13) as u16);
            break;
        }
        if current & 0x7fe000 == 0 && current & 3 == 0 {
            version = Some(((current & 0x1ff8) >> 3) as u16);
            break;
        }
    }
    let version = version.ok_or(DevError::Io)?;
    let mut image_type = None;
    for i in 1..blocks {
        let current = data[blocks - i];
        let next = data[blocks - i + 1];
        if i == 1 && current & 0x1f800000 == 0 {
            image_type = Some(0);
            break;
        }
        if (current & 3 == 0 && current & 0x1f800000 == 0) || (current & 3 != 0 && i != 1) {
            image_type = Some(((next & 0x1f800000) >> 23) as u8);
            break;
        }
    }
    Ok(I210InvmVersion {
        major: (version & 0x3f0) >> 4,
        minor: version & 0xf,
        image_type: image_type.ok_or(DevError::Io)?,
    })
}

/// upstream: e1000_i210.c e1000_validate_nvm_checksum_i210()
pub fn validate_nvm_checksum_i210<N: I210NvmIo>(nvm: &mut N) -> DevResult {
    nvm.acquire_nvm()?;
    let words = nvm.read_eerd(0, CHECKSUM_WORD + 1);
    nvm.release_nvm();
    let words = words?;
    if words.len() != usize::from(CHECKSUM_WORD + 1) {
        return Err(DevError::Io);
    }
    if words
        .iter()
        .fold(0u16, |sum, value| sum.wrapping_add(*value))
        == I210_NVM_SUM
    {
        Ok(())
    } else {
        Err(DevError::Io)
    }
}

/// upstream: e1000_i210.c e1000_update_nvm_checksum_i210()
pub fn update_nvm_checksum_i210<I: E1000RegisterIo, N: I210NvmIo>(
    io: &mut I,
    nvm: &mut N,
) -> DevResult {
    nvm.read_eerd(0, 1)?.first().ok_or(DevError::Io)?;
    nvm.acquire_nvm()?;
    let result = (|| {
        let mut checksum = 0u16;
        for offset in 0..CHECKSUM_WORD {
            checksum =
                checksum.wrapping_add(*nvm.read_eerd(offset, 1)?.first().ok_or(DevError::Io)?);
        }
        write_nvm_srwr(
            io,
            nvm.word_size(),
            CHECKSUM_WORD,
            &[I210_NVM_SUM.wrapping_sub(checksum)],
        )
    })();
    nvm.release_nvm();
    result?;
    nvm.update_flash()
}

/// upstream: e1000_i210.c e1000_get_flash_presence_i210()
pub fn get_flash_presence_i210<I: E1000RegisterIo>(io: &mut I) -> DevResult<bool> {
    Ok(io.read_register(E1000_EECD)? & E1000_EECD_FLASH_DETECTED_I210 != 0)
}

/// upstream: e1000_i210.c e1000_update_flash_i210()
pub fn update_flash_i210<I: E1000RegisterIo>(io: &mut I, attempts: usize) -> DevResult {
    pool_flash_update_done_i210(io, attempts)?;
    let eecd = io.read_register(E1000_EECD)? | E1000_EECD_FLUPD_I210;
    io.write_register(E1000_EECD, eecd)?;
    pool_flash_update_done_i210(io, attempts)
}

/// upstream: e1000_i210.c e1000_pool_flash_update_done_i210()
pub fn pool_flash_update_done_i210<I: E1000RegisterIo>(io: &mut I, attempts: usize) -> DevResult {
    for _ in 0..attempts {
        if io.read_register(E1000_EECD)? & E1000_EECD_FLUDONE_I210 != 0 {
            return Ok(());
        }
        io.delay_us(5);
    }
    Err(DevError::Io)
}

/// upstream: e1000_i210.c e1000_init_nvm_params_i210()
pub fn init_nvm_params_i210<N: I210NvmIo>(nvm: &mut N) -> DevResult {
    let result = nvm.init_nvm_params_82575();
    let has_flash = nvm.has_flash_hardware();
    nvm.nvm_type(if has_flash {
        I210NvmType::Flash
    } else {
        I210NvmType::Invm
    });
    result
}

/// upstream: e1000_i210.c e1000_init_function_pointers_i210()
pub trait I210FunctionPointerOps {
    fn init_function_pointers_82575(&mut self);
    fn install_i210_nvm_init(&mut self);
}
pub fn init_function_pointers_i210<O: I210FunctionPointerOps>(ops: &mut O) {
    ops.init_function_pointers_82575();
    ops.install_i210_nvm_init();
}

/// upstream: e1000_i210.c e1000_valid_led_default_i210()
pub fn valid_led_default_i210<N: I210NvmIo>(
    nvm: &mut N,
    offset: u16,
    serdes: bool,
) -> DevResult<u16> {
    let value = nvm
        .read_nvm_words(offset, 1)?
        .first()
        .copied()
        .ok_or(DevError::Io)?;
    if value == 0 || value == u16::MAX {
        Ok(if serdes { 0x0118 } else { 0x0819 })
    } else {
        Ok(value)
    }
}

pub trait I210PllOps {
    fn acquire_phy(&mut self) -> DevResult;
    fn release_phy(&mut self);
    fn read_phy_reg_mdic(&mut self, register: u16) -> DevResult<u16>;
    fn write_phy_reg_mdic(&mut self, register: u16, value: u16) -> DevResult;
    fn reset_internal_phy(&mut self) -> DevResult;
    fn pci_power_state(&mut self, d3: bool) -> DevResult;
    fn read_nvm_autoload(&mut self) -> DevResult<u16>;
}

/// upstream: e1000_i210.c e1000_pll_workaround_i210()
pub fn pll_workaround_i210<I: E1000RegisterIo, O: I210PllOps>(
    io: &mut I,
    ops: &mut O,
    max_tries: usize,
) -> DevResult {
    ops.acquire_phy()?;
    let mut original_mdicnfg = 0;
    let result = (|| {
        let wuc = io.read_register(0x05800)?;
        let mdicnfg = io.read_register(0x0e04)?;
        original_mdicnfg = mdicnfg;
        io.write_register(0x0e04, mdicnfg & !0x0004_0000)?;
        let nvm_word = ops.read_nvm_autoload().unwrap_or(0x202f);
        let autoload = nvm_word | 0x0010;
        let mut locked = false;
        for _ in 0..max_tries {
            ops.write_phy_reg_mdic(0x16, 0x00fc)?;
            io.delay_us(20);
            let status = ops.read_phy_reg_mdic(0x000e)?;
            io.delay_us(20);
            ops.write_phy_reg_mdic(0x16, 0)?;
            if status & 0xff != 0xff {
                locked = true;
                break;
            }
            ops.reset_internal_phy()?;
            let ext = io.read_register(E1000_CTRL_EXT)? | E1000_CTRL_EXT_PHYPDEN | 0x0004_0000;
            io.write_register(E1000_CTRL_EXT, ext)?;
            io.write_register(0x05800, 0)?;
            io.write_register(E1000_EEARBC_I210, (0x0a << 4) | (u32::from(autoload) << 16))?;
            ops.pci_power_state(true)?;
            io.delay_us(1_000);
            ops.pci_power_state(false)?;
            io.write_register(E1000_EEARBC_I210, (0x0a << 4) | (u32::from(nvm_word) << 16))?;
            io.write_register(0x05800, wuc)?;
        }
        if !locked { Err(DevError::Io) } else { Ok(()) }
    })();
    let restore = io.write_register(0x0e04, original_mdicnfg);
    ops.release_phy();
    result.and(restore)
}

/// upstream: e1000_i210.c e1000_get_cfg_done_i210()
pub fn get_cfg_done_i210<I: E1000RegisterIo>(io: &mut I, timeout_ms: usize) -> DevResult {
    for _ in 0..timeout_ms {
        if io.read_register(E1000_EEMNGCTL_I210)? & 0x0004_0000 != 0 {
            return Ok(());
        }
        io.delay_us(1_000);
    }
    // EEPROM-less silicon intentionally treats a missing config-done bit as success.
    Ok(())
}

/// upstream: e1000_i210.c e1000_init_hw_i210()
pub fn init_hw_i210<O: I210InitOps>(ops: &mut O, mac: E1000MacType, flash: bool) -> DevResult {
    if mac >= E1000MacType::I210 && !flash {
        ops.pll_workaround()?;
    }
    ops.install_cfg_done()?;
    let _ = ops.id_led_init();
    ops.init_hw_base()
}

pub trait I210InitOps {
    fn pll_workaround(&mut self) -> DevResult;
    fn install_cfg_done(&mut self) -> DevResult;
    fn id_led_init(&mut self) -> DevResult;
    fn init_hw_base(&mut self) -> DevResult;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shadow_write_checks_bounds_and_word_encoding() {
        assert!(write_nvm_srwr(&mut Mock, 4, 0, &[0x1234]).is_ok());
        assert!(write_nvm_srwr(&mut Mock, 4, 4, &[0x1234]).is_err());
        assert!(write_nvm_srwr(&mut Mock, 4, 0, &[]).is_err());
    }

    #[test]
    fn invm_walker_skips_csr_and_rsa_records_before_word_autoload() {
        let mut records = [0u32; INVM_SIZE];
        records[0] = 2;
        records[2] = 4;
        records[11] = (0xabcd << 16) | (6 << 9) | 1;
        assert_eq!(
            read_invm_word_i210(&mut InvmMock(records), 6).unwrap(),
            0xabcd
        );
    }

    struct Mock;
    impl E1000RegisterIo for Mock {
        fn read_register(&mut self, register: u32) -> DevResult<u32> {
            Ok(if register == E1000_SRWR { SRWR_DONE } else { 0 })
        }
        fn write_register(&mut self, _register: u32, _value: u32) -> DevResult {
            Ok(())
        }
        fn delay_us(&mut self, _micros: u32) {}
        fn invalid_tail_write(&mut self, _direction: &'static str) {}
    }

    struct InvmMock([u32; INVM_SIZE]);
    impl E1000RegisterIo for InvmMock {
        fn read_register(&mut self, register: u32) -> DevResult<u32> {
            if register >= INVM_BASE && register < INVM_BASE + INVM_SIZE as u32 * 4 {
                Ok(self.0[((register - INVM_BASE) / 4) as usize])
            } else {
                Ok(0)
            }
        }
        fn write_register(&mut self, _register: u32, _value: u32) -> DevResult {
            Ok(())
        }
        fn delay_us(&mut self, _micros: u32) {}
        fn invalid_tail_write(&mut self, _direction: &'static str) {}
    }
}
