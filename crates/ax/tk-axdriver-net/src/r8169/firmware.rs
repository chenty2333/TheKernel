//! Runtime Realtek PHY bytecode, not firmware embedded in the ELF.
//! Format/opcode facts: Linux 7.2.3 r8169_firmware.c. Bounded original VM.
use super::{indirect as i, regs::Bus};
use crate::{DevError, DevResult};
const MAX_BYTES: usize = 64 * 1024;
pub struct Firmware<'a> {
    code: &'a [u8],
}
impl<'a> Firmware<'a> {
    pub fn parse(bytes: &'a [u8]) -> DevResult<Self> {
        if bytes.len() < 4 || bytes.len() > MAX_BYTES {
            return Err(DevError::InvalidParam);
        }
        let code = if bytes[..4] == [0; 4] {
            if bytes.len() < 45 || bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0 {
                return Err(DevError::InvalidParam);
            }
            let start = u32::from_le_bytes(bytes[36..40].try_into().unwrap()) as usize;
            let count = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
            let end = count
                .checked_mul(4)
                .and_then(|n| start.checked_add(n))
                .ok_or(DevError::InvalidParam)?;
            if start < 45 {
                return Err(DevError::InvalidParam);
            }
            bytes.get(start..end).ok_or(DevError::InvalidParam)?
        } else {
            bytes
        };
        if code.is_empty() || !code.len().is_multiple_of(4) {
            return Err(DevError::InvalidParam);
        }
        let firmware = Self { code };
        let size = firmware.len();
        for pc in 0..size {
            let (op, reg, data) = firmware.instruction(pc);
            let valid = match op {
                0 | 1 | 2 | 7 | 8 | 12 | 14 => true,
                4 => data <= 1,
                3 => reg <= pc,
                9 => pc + 2 < size,
                10 | 11 | 13 => pc + 1 + reg < size,
                _ => false,
            };
            if !valid {
                return Err(DevError::InvalidParam);
            }
        }
        Ok(firmware)
    }
    fn len(&self) -> usize {
        self.code.len() / 4
    }
    fn instruction(&self, pc: usize) -> (u32, usize, u16) {
        let word = u32::from_le_bytes(self.code[pc * 4..pc * 4 + 4].try_into().unwrap());
        (word >> 28, ((word >> 16) & 0xfff) as usize, word as u16)
    }
    pub fn execute(&self, bus: &mut impl Bus) -> DevResult {
        let mut pc = 0;
        let mut previous = 0u16;
        let mut reads = 0u32;
        let mut steps = 0usize;
        let mut delays = 0u32;
        let mut mac = false;
        let mut base = 0xa400u16;
        while pc < self.len() {
            steps += 1;
            if steps > 100_000 {
                log::warn!("\x013R8169_FW_BUDGET_FAILED pc={pc} steps={steps}");
                return Err(DevError::Io);
            }
            let (op, reg, data) = self.instruction(pc);
            let mut next = pc + 1;
            let operation = (|| -> DevResult {
                match op {
                    0 => {
                        previous = access(bus, mac, &mut base, reg, None)?;
                        reads += 1;
                    }
                    1 => previous |= data,
                    2 => previous &= data,
                    3 => next = pc - reg,
                    4 => mac = data != 0,
                    7 => reads = 0,
                    8 => {
                        access(bus, mac, &mut base, reg, Some(data))?;
                    }
                    9 => {
                        if reads == u32::from(data) {
                            next += 1;
                        }
                    }
                    10 => {
                        if previous == data {
                            next += reg;
                        }
                    }
                    11 => {
                        if previous != data {
                            next += reg;
                        }
                    }
                    12 => {
                        access(bus, mac, &mut base, reg, Some(previous))?;
                    }
                    13 => next += reg,
                    14 => {
                        delays += u32::from(data);
                        if delays > 5000 {
                            return Err(DevError::Io);
                        }
                        bus.delay_us(u32::from(data) * 1000);
                    }
                    _ => return Err(DevError::Unsupported),
                }
                Ok(())
            })();
            if let Err(error) = operation {
                log::warn!(
                    "\x013R8169_FW_INSTRUCTION_FAILED pc={pc} opcode={op:#x} reg={reg:#x} \
                     data={data:#06x} base={base:#06x} mac={mac} steps={steps} delay_ms={delays} \
                     error={error:?}"
                );
                return Err(error);
            }
            pc = next;
        }
        Ok(())
    }
}
fn access(
    bus: &mut impl Bus,
    mac: bool,
    base: &mut u16,
    reg: usize,
    value: Option<u16>,
) -> DevResult<u16> {
    if reg == 31 {
        if let Some(data) = value {
            *base = if !mac && data == 0 {
                0xa400
            } else {
                data.checked_mul(16).ok_or(DevError::InvalidParam)?
            };
        }
        return Ok(if !mac && *base == 0xa400 {
            0
        } else {
            *base >> 4
        });
    }
    let offset = if mac {
        reg
    } else {
        let index = if *base == 0xa400 {
            reg
        } else {
            reg.checked_sub(16).ok_or(DevError::InvalidParam)?
        };
        index.checked_mul(2).ok_or(DevError::InvalidParam)?
    };
    let address = usize::from(*base)
        .checked_add(offset)
        .and_then(|v| u16::try_from(v).ok())
        .ok_or(DevError::InvalidParam)?;
    match (mac, value) {
        (true, Some(data)) => i::mac_write(bus, address, data)?,
        (false, Some(data)) => i::phy_write(bus, address, data)?,
        (true, None) => return i::mac_read(bus, address),
        (false, None) => return i::phy_read(bus, address),
    }
    Ok(0)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn bytes(code: &[u32]) -> std::vec::Vec<u8> {
        code.iter().flat_map(|v| v.to_le_bytes()).collect()
    }
    #[test]
    fn malformed_header_opcode_jump_and_end_are_rejected_before_writes() {
        for code in [
            &[0x60000000][..],
            &[0x30010000],
            &[0xd0010000],
            &[0x40000002],
        ] {
            assert!(Firmware::parse(&bytes(code)).is_err());
        }
        for length in 0..45 {
            assert!(Firmware::parse(&std::vec![0; length]).is_err());
        }
    }
    #[test]
    fn checked_header_and_optional_runtime_file() {
        let mut image = std::vec![0u8; 128];
        image[36..40].copy_from_slice(&128u32.to_le_bytes());
        image[40..44].copy_from_slice(&2u32.to_le_bytes());
        image.extend_from_slice(&bytes(&[0x801f0a43, 0x80100055]));
        image[44] = 0u8.wrapping_sub(image.iter().fold(0u8, |a, b| a.wrapping_add(*b)));
        let mut bus = super::super::fake::FakeBus::h8168();
        Firmware::parse(&image).unwrap().execute(&mut bus).unwrap();
        image[44] ^= 1;
        assert!(Firmware::parse(&image).is_err());
        // Optional extra coverage against separately supplied rootfs data.
        // The normal host suite above has deterministic, license-free input.
        if let Ok(path) = std::env::var("THEKERNEL_TEST_RTL8168_FIRMWARE") {
            let data = std::fs::read(path).unwrap();
            let mut bus = super::super::fake::FakeBus::h8168();
            Firmware::parse(&data).unwrap().execute(&mut bus).unwrap();
            super::super::h8168::configure_phy(&mut bus).unwrap();
        }
    }
    #[test]
    fn interpreter_handles_phy_mac_pages_and_previous_data() {
        let code = bytes(&[
            0x801f0a43, 0x80100055, 0x00100000, 0x10000080, 0xc0100000, 0x40000001, 0x801f0fc2,
            0x80000033,
        ]);
        let mut bus = super::super::fake::FakeBus::h8168();
        Firmware::parse(&code).unwrap().execute(&mut bus).unwrap();
        assert_eq!(i::phy_read(&mut bus, 0xa430).unwrap(), 0xd5);
        assert_eq!(i::mac_read(&mut bus, 0xfc20).unwrap(), 0x33);
    }
    #[test]
    fn backward_loops_delays_and_bad_page_arithmetic_are_bounded() {
        for code in [&[0x30000000][..], &[0xe000ffff], &[0x801f0fff, 0x80000000]] {
            let mut bus = super::super::fake::FakeBus::h8168();
            assert!(
                Firmware::parse(&bytes(code))
                    .unwrap()
                    .execute(&mut bus)
                    .is_err()
            );
        }
    }
}
