//! Shared queue-backed GPT partition view; never rediscover/reset the controller.
use alloc::{boxed::Box, format, string::String, vec::Vec};

use crate::{AxBlockDevice, SharedBlockDevice, StaticBlockDevice, prelude::*};
pub struct PartitionBlock {
    parent: SharedBlockDevice,
    start: u64,
    blocks: u64,
    sector: usize,
    name: String,
    read_only: bool,
}
impl PartitionBlock {
    fn range(&self, block: u64, len: usize) -> DevResult<u64> {
        if !len.is_multiple_of(self.sector)
            || block > self.blocks
            || (len / self.sector) as u64 > self.blocks - block
        {
            return Err(DevError::InvalidParam);
        }
        self.start.checked_add(block).ok_or(DevError::InvalidParam)
    }
    pub fn read_only(&self) -> bool {
        self.read_only
    }
}
impl BaseDriverOps for PartitionBlock {
    fn device_name(&self) -> &str {
        &self.name
    }
    fn device_type(&self) -> DeviceType {
        DeviceType::Block
    }
}
impl BlockDriverOps for PartitionBlock {
    fn num_blocks(&self) -> u64 {
        self.blocks
    }
    fn block_size(&self) -> usize {
        self.sector
    }
    fn read_block(&mut self, block: u64, buf: &mut [u8]) -> DevResult {
        let lba = self.range(block, buf.len())?;
        self.parent.lock().read_block(lba, buf)
    }
    fn write_block(&mut self, block: u64, buf: &[u8]) -> DevResult {
        if self.read_only {
            return Err(DevError::Unsupported);
        }
        let lba = self.range(block, buf.len())?;
        self.parent.lock().write_block(lba, buf)
    }
    fn flush(&mut self) -> DevResult {
        self.parent.lock().flush()
    }
}
/// Scan once while the discovered parent is idle. Keep the parent queue alive
/// in each view, enforce relative bounds and inherit immutable hardware RO.
pub fn discover_gpt_partitions(
    parent: &SharedBlockDevice,
    name: &str,
    read_only: bool,
) -> DevResult<Vec<AxBlockDevice>> {
    let geometry = BlockGeometry {
        block_size: parent.block_size(),
        blocks: parent.num_blocks(),
    };
    let partitions =
        axdriver_block::partition::gpt(geometry, |lba, out| parent.lock().read_block(lba, out))?;
    Ok(partitions
        .into_iter()
        .map(|part| {
            StaticBlockDevice::Partition(Box::new(PartitionBlock {
                parent: parent.clone(),
                start: part.start,
                blocks: part.blocks,
                sector: geometry.block_size,
                name: format!("{name}p{}", part.number),
                read_only,
            }))
        })
        .collect())
}

#[cfg(all(test, block_dev = "ramdisk"))]
mod tests {

    use super::*;
    #[test]
    fn view_bounds_offsets_and_ro_cannot_escape_parent() {
        let data: Vec<u8> = (0..8192).map(|i| (i % 251) as u8).collect();
        let parent = SharedBlockDevice::new(StaticBlockDevice::Existing(
            axdriver_block::ramdisk::RamDisk::from(data.as_slice()),
        ));
        let mut view = PartitionBlock {
            parent: parent.clone(),
            start: 4,
            blocks: 8,
            sector: 512,
            name: "nvme0n1p1".into(),
            read_only: true,
        };
        let mut out = [0; 512];
        view.read_block(0, &mut out).unwrap();
        assert_eq!(&out, &data[2048..2560]);
        assert!(view.read_block(8, &mut out).is_err());
        assert!(view.read_block(u64::MAX, &mut out).is_err());
        assert!(view.write_block(0, &[0xa5; 512]).is_err());
        view.read_only = false;
        view.write_block(7, &[0xa5; 512]).unwrap();
        parent.lock().read_block(11, &mut out).unwrap();
        assert_eq!(out, [0xa5; 512]);
        assert!(view.write_block(8, &[0; 512]).is_err());
    }
}
