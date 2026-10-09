//! Intel e1000 host-interface/manageability helpers.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_manage.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};
use super::{osdep::E1000RegisterIo, registers::*};

const HICR_EN: u32 = 0x01;
const HICR_C: u32 = 0x02;
const HICR_SV: u32 = 0x04;
const HICR_FW_RESET_ENABLE: u32 = 0x40;
const HICR_FW_RESET: u32 = 0x80;
const HICR_MEMORY_BASE_EN: u32 = 0x200;
const FWSM_MODE_MASK: u32 = 0xe;
const FWSM_MODE_SHIFT: u32 = 1;
const FWSM_FW_VALID: u32 = 0x8000;
const FWSM_HI_EN_ONLY_MODE: u32 = 4;
const MNG_IAMT_MODE: u32 = 3;
const MNG_DHCP_COMMAND_TIMEOUT: usize = 10;
const HI_COMMAND_TIMEOUT: usize = 500;
const HI_MAX_BLOCK_BYTE_LENGTH: usize = 1792;
const HI_FW_MAX_LENGTH: usize = 64 * 1024;
const HI_FW_BASE_ADDRESS: u32 = 0x10000;
const HI_FW_BLOCK_DWORD_LENGTH: usize = 256;
const MNG_DHCP_COOKIE_LENGTH: usize = 16;
const MNG_DHCP_COOKIE_OFFSET: usize = 0x6f0;
const MNG_DHCP_TX_PAYLOAD_CMD: u8 = 64;
const IAMT_SIGNATURE: u32 = 0x544d_4149;
const COOKIE_PARSING: u8 = 1;

/// Access to the host interface's dword-indexed SRAM window.
pub trait E1000ManageIo: E1000RegisterIo {
    fn read_host_dword(&mut self, index: u32) -> DevResult<u32>;
    fn write_host_dword(&mut self, index: u32, value: u32) -> DevResult;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct E1000ManagePolicy {
    pub arc_subsystem_valid: bool,
    pub asf_firmware_present: bool,
    pub has_fwsm: bool,
    pub is_82574_or_82583: bool,
    pub nvm_management_passthrough: bool,
}

/// upstream: e1000_manage.c e1000_calculate_checksum()
pub fn calculate_checksum(buffer: &[u8]) -> u8 {
    0u8.wrapping_sub(buffer.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)))
}

/// upstream: e1000_manage.c e1000_mng_enable_host_if_generic()
pub fn mng_enable_host_if_generic<I: E1000ManageIo>(io: &mut I, arc_valid: bool) -> DevResult {
    if !arc_valid { return Err(DevError::Io); }
    if io.read_register(E1000_HICR)? & HICR_EN == 0 { return Err(DevError::Io); }
    for _ in 0..MNG_DHCP_COMMAND_TIMEOUT {
        if io.read_register(E1000_HICR)? & HICR_C == 0 { return Ok(()); }
        io.delay_us(1_000);
    }
    Err(DevError::Io)
}

/// upstream: e1000_manage.c e1000_check_mng_mode_generic()
pub fn check_mng_mode_generic<I: E1000RegisterIo>(io: &mut I) -> DevResult<bool> {
    Ok((io.read_register(E1000_FWSM)? & FWSM_MODE_MASK) == (MNG_IAMT_MODE << FWSM_MODE_SHIFT))
}

/// upstream: e1000_manage.c e1000_enable_tx_pkt_filtering_generic()
pub fn enable_tx_pkt_filtering_generic<I: E1000ManageIo>(io: &mut I, arc_valid: bool) -> DevResult<bool> {
    if !check_mng_mode_generic(io)? { return Ok(false); }
    if mng_enable_host_if_generic(io, arc_valid).is_err() { return Ok(false); }
    let mut cookie = [0u8; MNG_DHCP_COOKIE_LENGTH];
    for (i, chunk) in cookie.chunks_exact_mut(4).enumerate() {
        chunk.copy_from_slice(&io.read_host_dword((MNG_DHCP_COOKIE_OFFSET / 4 + i) as u32)?.to_le_bytes());
    }
    let checksum = cookie[15];
    cookie[15] = 0;
    if u32::from_le_bytes(cookie[0..4].try_into().unwrap()) != IAMT_SIGNATURE
        || calculate_checksum(&cookie) != checksum
    { return Ok(true); }
    Ok(cookie[4] & COOKIE_PARSING != 0)
}

/// upstream: e1000_manage.c e1000_mng_write_cmd_header_generic()
pub fn mng_write_cmd_header_generic<I: E1000ManageIo>(io: &mut I, command_id: u8, command_length: u16, checksum: u8) -> DevResult {
    let mut hdr = [0u8; 8];
    hdr[0] = command_id;
    hdr[1] = checksum;
    hdr[6..8].copy_from_slice(&command_length.to_le_bytes());
    hdr[1] = calculate_checksum(&hdr);
    for (i, bytes) in hdr.chunks_exact(4).enumerate() {
        io.write_host_dword(i as u32, u32::from_le_bytes(bytes.try_into().unwrap()))?;
        let _ = io.read_register(E1000_STATUS)?;
    }
    Ok(())
}

/// upstream: e1000_manage.c e1000_mng_host_if_write_generic()
pub fn mng_host_if_write_generic<I: E1000ManageIo>(io: &mut I, buffer: &[u8], offset: u16, sum: &mut u8) -> DevResult {
    let end = usize::from(offset).checked_add(buffer.len()).ok_or(DevError::InvalidParam)?;
    if buffer.is_empty() || end > 0x6f8 { return Err(DevError::InvalidParam); }
    let mut pos = 0usize;
    let mut byte_offset = usize::from(offset);
    if byte_offset & 3 != 0 {
        let index = (byte_offset / 4) as u32;
        let mut word = io.read_host_dword(index)?.to_le_bytes();
        let start = byte_offset & 3;
        let n = core::cmp::min(4 - start, buffer.len());
        word[start..start + n].copy_from_slice(&buffer[..n]);
        for b in &buffer[..n] { *sum = sum.wrapping_add(*b); }
        io.write_host_dword(index, u32::from_le_bytes(word))?;
        pos += n;
        byte_offset += n;
    }
    while pos < buffer.len() {
        let n = core::cmp::min(4, buffer.len() - pos);
        let mut word = [0u8; 4];
        word[..n].copy_from_slice(&buffer[pos..pos + n]);
        for b in &word { *sum = sum.wrapping_add(*b); }
        io.write_host_dword((byte_offset / 4) as u32, u32::from_le_bytes(word))?;
        pos += n;
        byte_offset += n;
    }
    Ok(())
}

/// upstream: e1000_manage.c e1000_mng_write_dhcp_info_generic()
pub fn mng_write_dhcp_info_generic<I: E1000ManageIo>(io: &mut I, arc_valid: bool, buffer: &[u8]) -> DevResult {
    mng_enable_host_if_generic(io, arc_valid)?;
    let mut checksum = 0u8;
    mng_host_if_write_generic(io, buffer, 8, &mut checksum)?;
    mng_write_cmd_header_generic(io, MNG_DHCP_TX_PAYLOAD_CMD, buffer.len() as u16, checksum)?;
    let hicr = io.read_register(E1000_HICR)?;
    io.write_register(E1000_HICR, hicr | HICR_C)
}

/// upstream: e1000_manage.c e1000_enable_mng_pass_thru()
pub fn enable_mng_pass_thru<I: E1000RegisterIo>(io: &mut I, policy: E1000ManagePolicy) -> DevResult<bool> {
    if !policy.asf_firmware_present { return Ok(false); }
    let manc = io.read_register(E1000_MANC)?;
    if manc & 0x0002_0000 == 0 { return Ok(false); }
    let factps = io.read_register(E1000_FACTPS)?;
    if policy.has_fwsm {
        let fwsm = io.read_register(E1000_FWSM)?;
        return Ok(factps & 0x2000_0000 == 0 && (fwsm & FWSM_MODE_MASK) == (2 << FWSM_MODE_SHIFT));
    }
    if policy.is_82574_or_82583 {
        return Ok(factps & 0x2000_0000 == 0 && policy.nvm_management_passthrough);
    }
    Ok(manc & 1 != 0 && manc & 2 == 0)
}

/// upstream: e1000_manage.c e1000_host_interface_command()
pub fn host_interface_command<I: E1000ManageIo>(io: &mut I, policy: E1000ManagePolicy, buffer: &mut [u8]) -> DevResult {
    if !policy.arc_subsystem_valid || !policy.asf_firmware_present { return Ok(()); }
    if buffer.is_empty() || buffer.len() & 3 != 0 || buffer.len() > HI_MAX_BLOCK_BYTE_LENGTH { return Err(DevError::InvalidParam); }
    let hicr = io.read_register(E1000_HICR)?;
    if hicr & HICR_EN == 0 { return Err(DevError::Io); }
    for (i, bytes) in buffer.chunks_exact(4).enumerate() { io.write_host_dword(i as u32, u32::from_le_bytes(bytes.try_into().unwrap()))?; }
    io.write_register(E1000_HICR, hicr | HICR_C)?;
    for _ in 0..HI_COMMAND_TIMEOUT {
        if io.read_register(E1000_HICR)? & HICR_C == 0 { break; }
        io.delay_us(1_000);
    }
    let status = io.read_register(E1000_HICR)?;
    if status & HICR_C != 0 || status & HICR_SV == 0 { return Err(DevError::Io); }
    for (i, bytes) in buffer.chunks_exact_mut(4).enumerate() { bytes.copy_from_slice(&io.read_host_dword(i as u32)?.to_le_bytes()); }
    Ok(())
}

/// upstream: e1000_manage.c e1000_load_firmware()
pub fn load_firmware<I: E1000ManageIo>(io: &mut I, supported_generation: bool, buffer: &[u8]) -> DevResult {
    if !supported_generation { return Err(DevError::Unsupported); }
    let mut hicr = io.read_register(E1000_HICR)?;
    if hicr & HICR_EN == 0 || hicr & HICR_MEMORY_BASE_EN == 0 { return Err(DevError::Unsupported); }
    if buffer.is_empty() || buffer.len() & 3 != 0 || buffer.len() > HI_FW_MAX_LENGTH { return Err(DevError::InvalidParam); }
    let mut icr = io.read_register(E1000_ICR_V2)?;
    hicr |= HICR_FW_RESET_ENABLE;
    io.write_register(E1000_HICR, hicr)?;
    hicr |= HICR_FW_RESET;
    io.write_register(E1000_HICR, hicr)?;
    let _ = io.read_register(E1000_STATUS)?;
    for _ in 0..HI_COMMAND_TIMEOUT * 2 {
        icr = io.read_register(E1000_ICR_V2)?;
        if icr & 0x0000_2000 != 0 { break; }
        io.delay_us(1_000);
    }
    if icr & 0x0000_2000 == 0 { return Err(DevError::Io); }
    let mut fwsm = 0;
    for _ in 0..HI_COMMAND_TIMEOUT {
        fwsm = io.read_register(E1000_FWSM)?;
        if fwsm & FWSM_FW_VALID != 0 && ((fwsm & FWSM_MODE_MASK) >> FWSM_MODE_SHIFT) == FWSM_HI_EN_ONLY_MODE { break; }
        io.delay_us(1_000);
    }
    if fwsm & FWSM_FW_VALID == 0 || ((fwsm & FWSM_MODE_MASK) >> FWSM_MODE_SHIFT) != FWSM_HI_EN_ONLY_MODE { return Err(DevError::Io); }
    for (i, bytes) in buffer.chunks_exact(4).enumerate() {
        if i % HI_FW_BLOCK_DWORD_LENGTH == 0 {
            io.write_register(E1000_HIBBA, HI_FW_BASE_ADDRESS + ((HI_FW_BLOCK_DWORD_LENGTH * 4) * (i / HI_FW_BLOCK_DWORD_LENGTH)) as u32)?;
        }
        io.write_host_dword((i % HI_FW_BLOCK_DWORD_LENGTH) as u32, u32::from_le_bytes(bytes.try_into().unwrap()))?;
    }
    hicr = io.read_register(E1000_HICR)?;
    io.write_register(E1000_HICR, hicr | HICR_C)?;
    for _ in 0..HI_COMMAND_TIMEOUT {
        if io.read_register(E1000_HICR)? & HICR_C == 0 { return Ok(()); }
        io.delay_us(1_000);
    }
    Err(DevError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Io { regs: alloc::vec::Vec<(u32, u32)>, host: alloc::vec::Vec<u32>, delay: u64 }
    impl Default for Io { fn default() -> Self { Self { regs: alloc::vec::Vec::new(), host: alloc::vec![0; 512], delay: 0 } } }
    impl E1000RegisterIo for Io {
        fn read_register(&mut self, reg: u32) -> DevResult<u32> {
            Ok(self.regs.iter().find(|(r, _)| *r == reg).map(|(_, v)| *v).unwrap_or(0))
        }
        fn write_register(&mut self, reg: u32, value: u32) -> DevResult {
            if let Some((_, old)) = self.regs.iter_mut().find(|(r, _)| *r == reg) { *old = value; }
            else { self.regs.push((reg, value)); }
            Ok(())
        }
        fn delay_us(&mut self, us: u32) { self.delay += u64::from(us); }
        fn invalid_tail_write(&mut self, _: &'static str) {}
    }
    impl E1000ManageIo for Io {
        fn read_host_dword(&mut self, index: u32) -> DevResult<u32> { self.host.get(index as usize).copied().ok_or(DevError::InvalidParam) }
        fn write_host_dword(&mut self, index: u32, value: u32) -> DevResult {
            *self.host.get_mut(index as usize).ok_or(DevError::InvalidParam)? = value;
            Ok(())
        }
    }

    #[test]
    fn management_checksum_and_unaligned_host_write_match_byte_layout() {
        assert_eq!(calculate_checksum(&[1, 2, 3]), 250);
        let mut io = Io::default();
        io.host[0] = u32::from_le_bytes([0xaa, 0xbb, 0xcc, 0xdd]);
        let mut sum = 0;
        mng_host_if_write_generic(&mut io, &[1, 2, 3, 4, 5], 1, &mut sum).unwrap();
        assert_eq!(io.host[0].to_le_bytes(), [0xaa, 1, 2, 3]);
        assert_eq!(io.host[1].to_le_bytes(), [4, 5, 0, 0]);
        assert_eq!(sum, 15);
    }

    #[test]
    fn management_command_checks_arc_and_host_enable() {
        let mut io = Io::default();
        assert!(mng_enable_host_if_generic(&mut io, false).is_err());
        io.write_register(E1000_HICR, HICR_EN).unwrap();
        assert!(mng_enable_host_if_generic(&mut io, true).is_ok());
        io.write_register(E1000_FWSM, MNG_IAMT_MODE << FWSM_MODE_SHIFT).unwrap();
        assert!(check_mng_mode_generic(&mut io).unwrap());
    }
}
