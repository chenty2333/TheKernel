//! Original bounded, polling ACPI embedded-controller byte protocol.
use crate::{BAD_PARAMETER, Status, TIME};
pub trait Io {
    fn status(&mut self) -> Result<u8, Status>;
    fn read_data(&mut self) -> Result<u8, Status>;
    fn command(&mut self, value: u8) -> Result<(), Status>;
    fn write_data(&mut self, value: u8) -> Result<(), Status>;
    fn now_us(&self) -> u64;
    fn stall_us(&mut self, micros: u32);
}
fn wait(io: &mut impl Io, mask: u8, set: bool) -> Result<(), Status> {
    let start = io.now_us();
    loop {
        if (io.status()? & mask != 0) == set {
            return Ok(());
        }
        if io.now_us().wrapping_sub(start) >= 100_000 {
            return Err(TIME);
        }
        io.stall_us(10);
    }
}
pub fn read(io: &mut impl Io, address: u8) -> Result<u8, Status> {
    wait(io, 2, false)?;
    io.command(0x80)?;
    wait(io, 2, false)?;
    io.write_data(address)?;
    wait(io, 1, true)?;
    io.read_data()
}
pub fn write(io: &mut impl Io, address: u8, value: u8) -> Result<(), Status> {
    wait(io, 2, false)?;
    io.command(0x81)?;
    wait(io, 2, false)?;
    io.write_data(address)?;
    wait(io, 2, false)?;
    io.write_data(value)?;
    wait(io, 2, false)
}
pub fn query(io: &mut impl Io) -> Result<Option<u8>, Status> {
    if io.status()? & 0x20 == 0 {
        return Ok(None);
    }
    wait(io, 2, false)?;
    io.command(0x84)?;
    wait(io, 1, true)?;
    let query = io.read_data()?;
    Ok((query != 0).then_some(query))
}
pub fn transfer(
    io: &mut impl Io,
    write_op: bool,
    address: u64,
    width: u32,
    value: &mut u64,
) -> Result<(), Status> {
    if !matches!(width, 8 | 16 | 32 | 64)
        || address
            .checked_add(u64::from(width / 8))
            .is_none_or(|end| end > 256)
    {
        return Err(BAD_PARAMETER);
    }
    if !write_op {
        *value = 0;
    }
    for i in 0..width / 8 {
        let addr = (address + u64::from(i)) as u8;
        if write_op {
            write(io, addr, (*value >> (i * 8)) as u8)?;
        } else {
            *value |= u64::from(read(io, addr)?) << (i * 8);
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Mock {
        clock: u64,
        busy: bool,
        writes: alloc::vec::Vec<u8>,
    }
    impl Io for Mock {
        fn status(&mut self) -> Result<u8, Status> {
            Ok(if self.busy { 2 } else { 0x21 })
        }
        fn read_data(&mut self) -> Result<u8, Status> {
            Ok(0x42)
        }
        fn command(&mut self, v: u8) -> Result<(), Status> {
            self.writes.push(v);
            Ok(())
        }
        fn write_data(&mut self, v: u8) -> Result<(), Status> {
            self.writes.push(v);
            Ok(())
        }
        fn now_us(&self) -> u64 {
            self.clock
        }
        fn stall_us(&mut self, v: u32) {
            self.clock += u64::from(v);
        }
    }
    #[test]
    fn byte_protocol_and_bounds() {
        let mut m = Mock {
            clock: 0,
            busy: false,
            writes: alloc::vec::Vec::new(),
        };
        assert_eq!(read(&mut m, 5), Ok(0x42));
        assert_eq!(m.writes, [0x80, 5]);
        m.writes.clear();
        write(&mut m, 6, 7).unwrap();
        assert_eq!(m.writes, [0x81, 6, 7]);
        assert_eq!(query(&mut m), Ok(Some(0x42)));
        let mut v = 0;
        assert_eq!(transfer(&mut m, false, 255, 16, &mut v), Err(BAD_PARAMETER));
    }
    #[test]
    fn bad_controller_times_out() {
        let mut m = Mock {
            clock: 0,
            busy: true,
            writes: alloc::vec::Vec::new(),
        };
        assert_eq!(read(&mut m, 0), Err(TIME));
        assert!(m.writes.is_empty());
        assert_eq!(m.clock, 100_000);
    }
}
