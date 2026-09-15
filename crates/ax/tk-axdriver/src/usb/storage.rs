//! USB mass-storage Bulk-Only Transport, transparent SCSI command set, LUN 0.
use axdriver_base::{BaseDriverOps, DeviceType};
use axdriver_block::BlockDriverOps;
use crab_usb::usb_if::{descriptor::EndpointType, transfer::Direction};

use super::*;

pub struct UsbBlock {
    state: Mutex<Storage>,
    blocks: u64,
    block_size: usize,
}
struct Storage {
    host: Arc<Host>,
    _owner: DeviceOwner,
    input: crab_usb::EndpointHandle,
    output: crab_usb::EndpointHandle,
    tag: u32,
    failed: bool,
}
impl Storage {
    fn command(&mut self, cdb: &[u8], data: &mut [u8], input: bool) -> DevResult {
        if self.failed {
            return Err(DevError::Io);
        }
        let result = self.command_inner(cdb, data, input);
        // A transport error requires BOT reset recovery before another CBW.
        // Keep this instance failed rather than issuing commands out of phase.
        if result.is_err() {
            self.failed = true;
        }
        match result? {
            true => Ok(()),
            false => Err(DevError::Io),
        }
    }
    fn command_inner(&mut self, cdb: &[u8], data: &mut [u8], input: bool) -> DevResult<bool> {
        if cdb.is_empty() || cdb.len() > 16 || data.len() > u32::MAX as usize {
            return Err(DevError::InvalidParam);
        }
        self.tag = self.tag.wrapping_add(1);
        let mut cbw = [0_u8; 31];
        cbw[..4].copy_from_slice(&0x43425355_u32.to_le_bytes());
        cbw[4..8].copy_from_slice(&self.tag.to_le_bytes());
        cbw[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
        cbw[12] = if input { 0x80 } else { 0 };
        cbw[14] = cdb.len() as u8;
        cbw[15..15 + cdb.len()].copy_from_slice(cdb);
        if self
            .host
            .transfer(&self.output, TransferRequest::bulk_out(&cbw))?
            != cbw.len()
        {
            return Err(DevError::Io);
        }
        let transferred = if data.is_empty() {
            0
        } else if input {
            self.host
                .transfer(&self.input, TransferRequest::bulk_in(data))?
        } else {
            self.host
                .transfer(&self.output, TransferRequest::bulk_out(data))?
        };
        let mut csw = [0; 13];
        if self
            .host
            .transfer(&self.input, TransferRequest::bulk_in(&mut csw))?
            != csw.len()
        {
            return Err(DevError::Io);
        }
        validate_csw(&csw, self.tag, data.len(), transferred)
    }
}
fn validate_csw(csw: &[u8; 13], tag: u32, requested: usize, actual: usize) -> DevResult<bool> {
    let residue = u32::from_le_bytes(csw[8..12].try_into().unwrap()) as usize;
    if u32::from_le_bytes(csw[..4].try_into().unwrap()) != 0x53425355
        || u32::from_le_bytes(csw[4..8].try_into().unwrap()) != tag
        || csw[12] > 1
        || residue > requested
        || actual > requested
    {
        return Err(DevError::Io);
    }
    // A valid command-failed CSW can report unprocessed bytes. BOT remains
    // synchronized and permits REQUEST SENSE; this is not a phase error.
    if csw[12] == 1 {
        return Ok(false);
    }
    if residue != 0 || actual != requested {
        return Err(DevError::Io);
    }
    Ok(true)
}

impl UsbBlock {
    pub(super) fn new(
        host: Arc<Host>,
        device: Device,
        session: InterfaceSession,
        interface: &InterfaceDescriptor,
    ) -> DevResult<Self> {
        let endpoint = |direction| {
            let ep = interface
                .endpoints
                .iter()
                .find(|ep| ep.transfer_type == EndpointType::Bulk && ep.direction == direction)
                .ok_or(DevError::Unsupported)?;
            session.endpoint(ep.address).map_err(|_| DevError::Io)
        };
        let input = endpoint(Direction::In)?;
        let output = endpoint(Direction::Out)?;
        let mut state = Storage {
            host,
            _owner: DeviceOwner {
                _device: device,
                _session: session,
            },
            input,
            output,
            tag: 0,
            failed: false,
        };
        // Clear power-on/reset unit attention before commands with a data
        // phase. TEST UNIT READY failures still leave BOT at the next CBW.
        let mut ready = false;
        for _ in 0..3 {
            if state.command_inner(&[0; 6], &mut [], false)? {
                ready = true;
                break;
            }
            let mut sense = [0; 18];
            state.command(&[0x03, 0, 0, 0, 18, 0], &mut sense, true)?;
            axhal::time::busy_wait(Duration::from_millis(100));
        }
        if !ready {
            return Err(DevError::Io);
        }
        let mut capacity = [0; 8];
        state.command(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], &mut capacity, true)?;
        let last = u32::from_be_bytes(capacity[..4].try_into().unwrap());
        let block_size = u32::from_be_bytes(capacity[4..].try_into().unwrap()) as usize;
        if last == u32::MAX || !block_size.is_power_of_two() || !(512..=4096).contains(&block_size)
        {
            return Err(DevError::Unsupported);
        }
        info!(
            "USB mass storage: {} blocks of {} bytes",
            last as u64 + 1,
            block_size
        );
        Ok(Self {
            state: Mutex::new(state),
            blocks: last as u64 + 1,
            block_size,
        })
    }
    fn validate_range(&self, block: u64, length: usize) -> DevResult {
        if !length.is_multiple_of(self.block_size)
            || block
                .checked_add((length / self.block_size) as u64)
                .is_none_or(|end| end > self.blocks)
        {
            return Err(DevError::InvalidParam);
        }
        Ok(())
    }
    fn io(&mut self, block: u64, data: &mut [u8], input: bool) -> DevResult {
        self.validate_range(block, data.len())?;
        let mut block = block;
        // Bounded requests avoid oversized xHCI transfer descriptors.
        for chunk in data.chunks_mut(64 * 1024) {
            let count = (chunk.len() / self.block_size) as u16;
            let mut cdb = [0; 10];
            cdb[0] = if input { 0x28 } else { 0x2a };
            cdb[2..6].copy_from_slice(&(block as u32).to_be_bytes());
            cdb[7..9].copy_from_slice(&count.to_be_bytes());
            self.state.get_mut().command(&cdb, chunk, input)?;
            block += count as u64;
        }
        Ok(())
    }
}
impl BaseDriverOps for UsbBlock {
    fn device_name(&self) -> &str {
        "USB mass storage"
    }
    fn device_type(&self) -> DeviceType {
        DeviceType::Block
    }
}
impl BlockDriverOps for UsbBlock {
    fn num_blocks(&self) -> u64 {
        self.blocks
    }
    fn block_size(&self) -> usize {
        self.block_size
    }
    fn read_block(&mut self, block: u64, data: &mut [u8]) -> DevResult {
        self.io(block, data, true)
    }
    fn write_block(&mut self, block: u64, data: &[u8]) -> DevResult {
        self.validate_range(block, data.len())?;
        let mut block = block;
        for chunk in data.chunks(64 * 1024) {
            self.io(block, &mut chunk.to_vec(), false)?;
            block += (chunk.len() / self.block_size) as u64;
        }
        Ok(())
    }
    fn flush(&mut self) -> DevResult {
        self.state
            .get_mut()
            .command(&[0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0], &mut [], false)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_stale_failed_and_short_status() {
        let mut status = [0; 13];
        status[..4].copy_from_slice(&0x53425355_u32.to_le_bytes());
        status[4..8].copy_from_slice(&7_u32.to_le_bytes());
        assert!(validate_csw(&status, 7, 512, 512).unwrap());
        assert!(validate_csw(&status, 8, 512, 512).is_err());
        assert!(validate_csw(&status, 7, 512, 511).is_err());
        status[12] = 1;
        assert!(!validate_csw(&status, 7, 512, 512).unwrap());
        status[8..12].copy_from_slice(&512_u32.to_le_bytes());
        assert!(!validate_csw(&status, 7, 512, 0).unwrap());
        status[8..12].copy_from_slice(&513_u32.to_le_bytes());
        assert!(validate_csw(&status, 7, 512, 0).is_err());
        status[12] = 2;
        assert!(validate_csw(&status, 7, 512, 512).is_err());
    }
}
