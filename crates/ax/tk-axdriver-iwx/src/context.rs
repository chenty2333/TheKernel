//! Firmware context-info wire layouts used by OpenBSD iwx self-load.
//!
//! Upstream: OpenBSD `sys/dev/pci/if_iwx.c` revision 1.230,
//! `iwx_ctxt_info_init()` and `iwx_ctxt_info_gen3_init()`; layout definitions
//! from `sys/dev/pci/if_iwxreg.h`. ISC.
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>
//!   Author: Stefan Sperling <stsp@openbsd.org>
//! Copyright (c) 2014 Fixup Software Ltd.
//! Copyright (c) 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>

use alloc::vec::Vec;

use crate::FirmwareDmaImages;

const MAX_DRAM_ENTRIES: usize = 64;
const RX_QUEUE_CB_SIZE: u8 = 9; // fls(512) - 1
const TX_QUEUE_CB_SIZE: u8 = 5; // fls(256) - 1 - 3
const RX_BUFFER_SIZE_4K: u32 = 4;
const TFD_FORMAT_LONG: u32 = 1 << 8;
const RX_CB_SIZE_POSITION: u32 = 4;
const RX_SIZE_POSITION: u32 = 9;
const GEN2_CONTEXT_SIZE: usize = 1792;
const GEN3_CONTEXT_SIZE: usize = 104;
const PRPH_SCRATCH_SIZE: usize = 1660;

/// Inputs common to the Gen2/AX210 context-info formats.
#[derive(Debug, Clone, Copy)]
pub struct ContextQueueAddresses {
    pub free_rbd: u64,
    pub used_rbd: u64,
    pub rx_status: u64,
    pub command_queue: u64,
}

/// Context construction failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextError {
    TooManyImages,
    InvalidAddressArray,
    NoMemory,
}

/// Build the packed Gen2 context-info structure in little-endian byte order.
// upstream: if_iwx.c iwx_ctxt_info_init()
pub fn build_gen2_context(
    mac_id: u16,
    queues: ContextQueueAddresses,
    firmware: &FirmwareDmaImages<impl crate::DmaRegion>,
) -> Result<Vec<u8>, ContextError> {
    if firmware.lmac_addresses.len() > MAX_DRAM_ENTRIES
        || firmware.umac_addresses.len() > MAX_DRAM_ENTRIES
        || firmware.paging_addresses.len() > MAX_DRAM_ENTRIES
    {
        return Err(ContextError::TooManyImages);
    }
    let mut bytes = zeroed_context(GEN2_CONTEXT_SIZE)?;
    put_u16(&mut bytes, 0, mac_id)?;
    put_u16(&mut bytes, 2, 0)?;
    put_u16(&mut bytes, 4, (GEN2_CONTEXT_SIZE / 4) as u16)?;
    let flags = TFD_FORMAT_LONG
        | (u32::from(RX_QUEUE_CB_SIZE) << RX_CB_SIZE_POSITION)
        | (RX_BUFFER_SIZE_4K << RX_SIZE_POSITION);
    put_u32(&mut bytes, 8, flags)?;
    put_u64(&mut bytes, 24, queues.free_rbd)?;
    put_u64(&mut bytes, 32, queues.used_rbd)?;
    put_u64(&mut bytes, 40, queues.rx_status)?;
    put_u64(&mut bytes, 48, queues.command_queue)?;
    bytes[56] = TX_QUEUE_CB_SIZE;
    write_image_addresses(&mut bytes, 192, &firmware.umac_addresses)?;
    write_image_addresses(&mut bytes, 704, &firmware.lmac_addresses)?;
    write_image_addresses(&mut bytes, 1216, &firmware.paging_addresses)?;
    Ok(bytes)
}

/// Build the Gen3 peripheral scratch object and its firmware-image map.
// upstream: if_iwx.c iwx_ctxt_info_gen3_init() scratch initialization
pub fn build_gen3_prph_scratch(
    mac_id: u16,
    free_rbd: u64,
    firmware: &FirmwareDmaImages<impl crate::DmaRegion>,
    imr_enabled: bool,
) -> Result<Vec<u8>, ContextError> {
    validate_image_counts(firmware)?;
    let mut bytes = zeroed_context(PRPH_SCRATCH_SIZE)?;
    put_u16(&mut bytes, 0, mac_id)?;
    put_u16(&mut bytes, 2, 0)?;
    put_u16(&mut bytes, 4, (PRPH_SCRATCH_SIZE / 4) as u16)?;
    let mut flags = (1 << 16) | (1 << 17) | 0x000c_0000; // 4K RB, MTR and 256B TFD.
    if imr_enabled {
        flags |= 1 << 1;
    }
    put_u32(&mut bytes, 8, flags)?;
    put_u64(&mut bytes, 48, free_rbd)?;
    write_image_addresses(&mut bytes, 124, &firmware.umac_addresses)?;
    write_image_addresses(&mut bytes, 636, &firmware.lmac_addresses)?;
    write_image_addresses(&mut bytes, 1148, &firmware.paging_addresses)?;
    Ok(bytes)
}

/// Build the Gen3 IPC context-info object.
// upstream: if_iwx.c iwx_ctxt_info_gen3_init()
pub fn build_gen3_context(
    addresses: ContextQueueAddresses,
    prph_info: u64,
    prph_scratch: u64,
    prph_scratch_size: usize,
) -> Result<Vec<u8>, ContextError> {
    let mut bytes = zeroed_context(GEN3_CONTEXT_SIZE)?;
    put_u16(&mut bytes, 0, 0)?;
    put_u16(&mut bytes, 2, (GEN3_CONTEXT_SIZE / 4) as u16)?;
    put_u64(&mut bytes, 8, prph_info)?;
    put_u64(&mut bytes, 16, addresses.rx_status)?;
    put_u64(&mut bytes, 24, prph_info + 4096 / 2)?;
    put_u64(&mut bytes, 32, prph_info + 3 * 4096 / 4)?;
    put_u64(&mut bytes, 52, addresses.command_queue)?;
    put_u64(&mut bytes, 60, addresses.used_rbd)?;
    put_u16(&mut bytes, 68, TX_QUEUE_CB_SIZE as u16)?;
    put_u16(&mut bytes, 70, RX_QUEUE_CB_SIZE as u16)?;
    put_u64(&mut bytes, 88, prph_scratch)?;
    put_u32(
        &mut bytes,
        96,
        u32::try_from(prph_scratch_size / 4).map_err(|_| ContextError::InvalidAddressArray)?,
    )?;
    Ok(bytes)
}

fn validate_image_counts<R: crate::DmaRegion>(
    firmware: &FirmwareDmaImages<R>,
) -> Result<(), ContextError> {
    if firmware.lmac_addresses.len() > MAX_DRAM_ENTRIES
        || firmware.umac_addresses.len() > MAX_DRAM_ENTRIES
        || firmware.paging_addresses.len() > MAX_DRAM_ENTRIES
    {
        Err(ContextError::TooManyImages)
    } else {
        Ok(())
    }
}

fn write_image_addresses(
    bytes: &mut [u8],
    offset: usize,
    addresses: &[u64],
) -> Result<(), ContextError> {
    if addresses.len() > MAX_DRAM_ENTRIES {
        return Err(ContextError::TooManyImages);
    }
    for (index, address) in addresses.iter().enumerate() {
        put_u64(bytes, offset + index * 8, *address)?;
    }
    Ok(())
}

fn zeroed_context(size: usize) -> Result<Vec<u8>, ContextError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| ContextError::NoMemory)?;
    bytes.resize(size, 0);
    Ok(bytes)
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) -> Result<(), ContextError> {
    bytes
        .get_mut(offset..offset + 2)
        .ok_or(ContextError::InvalidAddressArray)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), ContextError> {
    bytes
        .get_mut(offset..offset + 4)
        .ok_or(ContextError::InvalidAddressArray)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) -> Result<(), ContextError> {
    bytes
        .get_mut(offset..offset + 8)
        .ok_or(ContextError::InvalidAddressArray)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::{DmaError, DmaRegion, FirmwareDmaImages};

    struct Region {
        address: u64,
        bytes: Vec<u8>,
    }
    impl DmaRegion for Region {
        fn device_address(&self) -> u64 {
            self.address
        }
        fn capacity(&self) -> usize {
            self.bytes.len()
        }
        fn write(&mut self, data: &[u8]) -> Result<(), DmaError> {
            self.bytes.copy_from_slice(data);
            Ok(())
        }
    }
    #[test]
    fn gen2_context_matches_packed_field_offsets_and_flags() {
        let firmware = FirmwareDmaImages::<Region> {
            lmac: vec![],
            umac: vec![],
            paging: vec![],
            lmac_addresses: vec![0x1000],
            umac_addresses: vec![0x2000],
            paging_addresses: vec![0x3000],
        };
        let ctx = build_gen2_context(
            0x5678,
            ContextQueueAddresses {
                free_rbd: 1,
                used_rbd: 2,
                rx_status: 3,
                command_queue: 4,
            },
            &firmware,
        )
        .unwrap();
        assert_eq!(ctx.len(), GEN2_CONTEXT_SIZE);
        assert_eq!(u16::from_le_bytes(ctx[0..2].try_into().unwrap()), 0x5678);
        assert_eq!(u16::from_le_bytes(ctx[4..6].try_into().unwrap()), 448);
        assert_eq!(u32::from_le_bytes(ctx[8..12].try_into().unwrap()), 0x990);
        assert_eq!(&ctx[24..32], &1u64.to_le_bytes());
        assert_eq!(ctx[56], TX_QUEUE_CB_SIZE);
        assert_eq!(&ctx[704..712], &0x1000u64.to_le_bytes());
        assert_eq!(&ctx[192..200], &0x2000u64.to_le_bytes());
        assert_eq!(&ctx[1216..1224], &0x3000u64.to_le_bytes());
    }

    #[test]
    fn gen3_scratch_and_context_sizes_and_ring_offsets_match_wire_format() {
        let firmware = FirmwareDmaImages::<Region> {
            lmac: vec![],
            umac: vec![],
            paging: vec![],
            lmac_addresses: vec![0x1000],
            umac_addresses: vec![0x2000],
            paging_addresses: vec![0x3000],
        };
        let scratch = build_gen3_prph_scratch(0x1234, 0x4567, &firmware, true).unwrap();
        assert_eq!(scratch.len(), PRPH_SCRATCH_SIZE);
        assert_eq!(u16::from_le_bytes(scratch[4..6].try_into().unwrap()), 415);
        assert_eq!(
            u32::from_le_bytes(scratch[8..12].try_into().unwrap()),
            0xf0002
        );
        assert_eq!(&scratch[48..56], &0x4567u64.to_le_bytes());
        assert_eq!(&scratch[636..644], &0x1000u64.to_le_bytes());
        assert_eq!(&scratch[124..132], &0x2000u64.to_le_bytes());
        assert_eq!(&scratch[1148..1156], &0x3000u64.to_le_bytes());

        let context = build_gen3_context(
            ContextQueueAddresses {
                free_rbd: 1,
                used_rbd: 2,
                rx_status: 3,
                command_queue: 4,
            },
            0x8000,
            0x9000,
            PRPH_SCRATCH_SIZE,
        )
        .unwrap();
        assert_eq!(context.len(), GEN3_CONTEXT_SIZE);
        assert_eq!(u16::from_le_bytes(context[2..4].try_into().unwrap()), 26);
        assert_eq!(&context[16..24], &3u64.to_le_bytes());
        assert_eq!(&context[52..60], &4u64.to_le_bytes());
        assert_eq!(&context[60..68], &2u64.to_le_bytes());
        assert_eq!(&context[88..96], &0x9000u64.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(context[96..100].try_into().unwrap()),
            415
        );
    }
}
