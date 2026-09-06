//! Immutable block storage backed by a bootloader-owned module.

extern crate alloc;

use alloc::vec::Vec;

use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};

use crate::BlockDriverOps;

/// One private, RAM-backed replacement for a module block.
///
/// The vector stays deliberately sparse: unchanged blocks continue to read
/// directly from the boot module, while filesystem metadata/data writes get a
/// private copy for this boot only.
struct OverlayBlock {
    block_id: u64,
    bytes: Vec<u8>,
}

/// Block device exposing a bootloader-supplied image with a sparse RAM COW
/// layer.  The module itself remains immutable and can therefore stay in its
/// boot-reserved physical range for the lifetime of the guest.
///
/// The caller must keep `bytes` mapped and reserved for the lifetime of the
/// device. The module is deliberately read-only boot media.
pub struct BootModuleBlockDevice {
    bytes: &'static [u8],
    block_size: usize,
    overlay: Vec<OverlayBlock>,
}

impl BootModuleBlockDevice {
    /// Creates a block device over an immutable module.
    pub fn new(bytes: &'static [u8], block_size: usize) -> Result<Self, DevError> {
        if block_size == 0 || bytes.len() % block_size != 0 {
            return Err(DevError::InvalidParam);
        }
        Ok(Self {
            bytes,
            block_size,
            overlay: Vec::new(),
        })
    }

    fn overlay_block(&self, block_id: u64) -> Option<&[u8]> {
        self.overlay
            .iter()
            .find(|entry| entry.block_id == block_id)
            .map(|entry| entry.bytes.as_slice())
    }

    fn write_one_block(&mut self, block_id: u64, bytes: &[u8]) -> DevResult {
        debug_assert_eq!(bytes.len(), self.block_size);
        if let Some(entry) = self
            .overlay
            .iter_mut()
            .find(|entry| entry.block_id == block_id)
        {
            entry.bytes.copy_from_slice(bytes);
            return Ok(());
        }
        let source = self.byte_range(block_id, self.block_size)?;
        let mut copy = Vec::new();
        copy.try_reserve_exact(self.block_size)
            .map_err(|_| DevError::NoMemory)?;
        copy.extend_from_slice(&self.bytes[source]);
        copy.copy_from_slice(bytes);
        self.overlay
            .try_reserve(1)
            .map_err(|_| DevError::NoMemory)?;
        self.overlay.push(OverlayBlock {
            block_id,
            bytes: copy,
        });
        Ok(())
    }

    fn byte_range(&self, block_id: u64, len: usize) -> Result<core::ops::Range<usize>, DevError> {
        if len == 0 || len % self.block_size != 0 {
            return Err(DevError::InvalidParam);
        }
        let blocks = len / self.block_size;
        let start_block = usize::try_from(block_id).map_err(|_| DevError::Io)?;
        let end_block = start_block.checked_add(blocks).ok_or(DevError::Io)?;
        let start = start_block
            .checked_mul(self.block_size)
            .ok_or(DevError::Io)?;
        let end = end_block.checked_mul(self.block_size).ok_or(DevError::Io)?;
        if end > self.bytes.len() {
            return Err(DevError::Io);
        }
        Ok(start..end)
    }
}

impl BaseDriverOps for BootModuleBlockDevice {
    fn device_name(&self) -> &str {
        "boot-module-rootfs"
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::Block
    }
}

impl BlockDriverOps for BootModuleBlockDevice {
    fn num_blocks(&self) -> u64 {
        (self.bytes.len() / self.block_size) as u64
    }

    fn block_size(&self) -> usize {
        self.block_size
    }

    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DevResult {
        let range = self.byte_range(block_id, buf.len())?;
        let first = usize::try_from(block_id).map_err(|_| DevError::Io)?;
        for (index, output) in buf.chunks_exact_mut(self.block_size).enumerate() {
            let current = first.checked_add(index).ok_or(DevError::Io)? as u64;
            if let Some(overlay) = self.overlay_block(current) {
                output.copy_from_slice(overlay);
            } else {
                let start = range.start + index * self.block_size;
                output.copy_from_slice(&self.bytes[start..start + self.block_size]);
            }
        }
        Ok(())
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DevResult {
        let _ = self.byte_range(block_id, buf.len())?;
        for (index, input) in buf.chunks_exact(self.block_size).enumerate() {
            let current = block_id.checked_add(index as u64).ok_or(DevError::Io)?;
            self.write_one_block(current, input)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> DevResult {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static MODULE: [u8; 1024] = [0x5a; 1024];

    #[test]
    fn reads_are_checked_and_writes_are_private_to_the_boot() {
        let mut dev = BootModuleBlockDevice::new(&MODULE, 512).unwrap();
        let mut block = [0; 512];
        assert!(dev.read_block(1, &mut block).is_ok());
        assert_eq!(block, [0x5a; 512]);
        assert!(matches!(dev.read_block(2, &mut block), Err(DevError::Io)));
        assert!(matches!(
            dev.read_block(u64::MAX, &mut block),
            Err(DevError::Io)
        ));
        assert!(matches!(
            dev.read_block(0, &mut []),
            Err(DevError::InvalidParam)
        ));
        dev.write_block(0, &[0xa5; 512]).unwrap();
        dev.read_block(0, &mut block).unwrap();
        assert_eq!(block, [0xa5; 512]);
        // The second block still aliases the immutable module bytes.
        dev.read_block(1, &mut block).unwrap();
        assert_eq!(block, [0x5a; 512]);
    }
}
