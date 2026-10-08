//! ATA command/FIS translation for the AHCI block command path.
//!
//! The command encodings and register FIS layout follow FreeBSD
//! `ahci_setup_fis()` and ATA command definitions; CAM CCB translation is
//! replaced by a typed block request.

use super::regs::ATA_A_4BIT;

/// Host-to-device Register FIS type.
pub const FIS_TYPE_REG_H2D: u8 = 0x27;
/// Register FIS command bit.
pub const FIS_FLAG_COMMAND: u8 = 1 << 7;
/// ATA IDENTIFY DEVICE command.
pub const ATA_IDENTIFY_DEVICE: u8 = 0xec;
/// ATA READ DMA EXT command.
pub const ATA_READ_DMA_EXT: u8 = 0x25;
/// ATA WRITE DMA EXT command.
pub const ATA_WRITE_DMA_EXT: u8 = 0x35;
/// ATA READ FPDMA QUEUED command.
pub const ATA_READ_FPDMA_QUEUED: u8 = 0x60;
/// ATA WRITE FPDMA QUEUED command.
pub const ATA_WRITE_FPDMA_QUEUED: u8 = 0x61;
/// ATA FLUSH CACHE EXT command.
pub const ATA_FLUSH_CACHE_EXT: u8 = 0xea;
/// ATA DATA SET MANAGEMENT command.
pub const ATA_DATA_SET_MANAGEMENT: u8 = 0x06;
/// ATA DSM feature bit selecting TRIM.
pub const ATA_DSM_TRIM: u8 = 0x01;

/// A block-level ATA request that can be represented by a Register FIS.
///
/// Unlike FreeBSD CAM's generic ATA CCB, this request type intentionally
/// excludes ICC, AUX, and arbitrary control/reset commands: the block
/// interface does not issue those operations. Device-to-host reads and
/// host-to-device writes use the command-header direction bit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtaRequest {
    /// Identify the attached ATA device into a 512-byte data buffer.
    Identify { pmp_port: u8 },
    /// LBA48 READ/WRITE DMA EXT request.
    DmaExt {
        lba: u64,
        sectors: u16,
        write: bool,
        pmp_port: u8,
    },
    /// LBA48 NCQ read/write. `tag` occupies sector-count bits 7:3.
    Fpdma {
        lba: u64,
        sectors: u16,
        write: bool,
        tag: u8,
        pmp_port: u8,
    },
    /// Persist the device write cache.
    FlushCacheExt { pmp_port: u8 },
    /// Submit a DSM/TRIM parameter block of whole 512-byte sectors.
    DsmTrim {
        parameter_sectors: u16,
        pmp_port: u8,
    },
}

/// ATA command-transfer attributes needed to build the AHCI command header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandAttributes {
    /// ATA command opcode in the Register FIS.
    pub command: u8,
    /// AHCI command-header write bit.
    pub device_reads_buffer: bool,
    /// Number of 512-byte data sectors transferred.
    pub sectors: u16,
    /// Whether this command transfers data through PRDs.
    pub data_transfer: bool,
    /// Optional NCQ tag.
    pub tag: Option<u8>,
}

/// Invalid ATA command arguments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtaRequestError {
    /// LBA48 address range exceeded the ATA six-byte LBA field.
    LbaOutOfRange,
    /// Data commands require a non-zero sector count.
    EmptyTransfer,
    /// NCQ tags are five-bit values.
    InvalidTag,
    /// Port multiplier target is a four-bit ATA field.
    InvalidPmpPort,
}

/// Build the 20-byte host-to-device Register FIS used by the AHCI command
/// table and return the matching data direction/transfer attributes.
// upstream: ahci.c ahci_setup_fis()
pub fn setup_register_fis(
    request: AtaRequest,
) -> Result<([u8; 20], CommandAttributes), AtaRequestError> {
    let mut fis = [0u8; 20];
    fis[0] = FIS_TYPE_REG_H2D;
    fis[1] = FIS_FLAG_COMMAND;
    let attributes = match request {
        AtaRequest::Identify { pmp_port } => {
            set_target(&mut fis, pmp_port)?;
            fis[15] = ATA_A_4BIT as u8;
            fis[2] = ATA_IDENTIFY_DEVICE;
            CommandAttributes {
                command: ATA_IDENTIFY_DEVICE,
                device_reads_buffer: false,
                sectors: 1,
                data_transfer: true,
                tag: None,
            }
        }
        AtaRequest::DmaExt {
            lba,
            sectors,
            write,
            pmp_port,
        } => {
            set_target(&mut fis, pmp_port)?;
            fis[15] = ATA_A_4BIT as u8;
            validate_lba_transfer(lba, sectors)?;
            let command = if write {
                ATA_WRITE_DMA_EXT
            } else {
                ATA_READ_DMA_EXT
            };
            fis[2] = command;
            encode_lba48(&mut fis, lba);
            fis[7] |= 1 << 6;
            fis[12] = sectors as u8;
            fis[13] = (sectors >> 8) as u8;
            CommandAttributes {
                command,
                device_reads_buffer: write,
                sectors,
                data_transfer: true,
                tag: None,
            }
        }
        AtaRequest::Fpdma {
            lba,
            sectors,
            write,
            tag,
            pmp_port,
        } => {
            set_target(&mut fis, pmp_port)?;
            fis[15] = ATA_A_4BIT as u8;
            validate_lba_transfer(lba, sectors)?;
            if tag >= 32 {
                return Err(AtaRequestError::InvalidTag);
            }
            let command = if write {
                ATA_WRITE_FPDMA_QUEUED
            } else {
                ATA_READ_FPDMA_QUEUED
            };
            fis[2] = command;
            encode_lba48(&mut fis, lba);
            fis[7] |= 1 << 6;
            // FPDMA encodes the transfer count in FEATURES and the command
            // tag in SECTOR_COUNT, unlike DMA EXT's count fields.
            fis[3] = sectors as u8;
            fis[11] = (sectors >> 8) as u8;
            fis[12] = (sectors as u8 & 0x07) | (tag << 3);
            CommandAttributes {
                command,
                device_reads_buffer: write,
                sectors,
                data_transfer: true,
                tag: Some(tag),
            }
        }
        AtaRequest::FlushCacheExt { pmp_port } => {
            set_target(&mut fis, pmp_port)?;
            fis[15] = ATA_A_4BIT as u8;
            fis[2] = ATA_FLUSH_CACHE_EXT;
            CommandAttributes {
                command: ATA_FLUSH_CACHE_EXT,
                device_reads_buffer: false,
                sectors: 0,
                data_transfer: false,
                tag: None,
            }
        }
        AtaRequest::DsmTrim {
            parameter_sectors,
            pmp_port,
        } => {
            set_target(&mut fis, pmp_port)?;
            fis[15] = ATA_A_4BIT as u8;
            if parameter_sectors == 0 {
                return Err(AtaRequestError::EmptyTransfer);
            }
            fis[2] = ATA_DATA_SET_MANAGEMENT;
            fis[3] = ATA_DSM_TRIM;
            fis[12] = parameter_sectors as u8;
            fis[13] = (parameter_sectors >> 8) as u8;
            CommandAttributes {
                command: ATA_DATA_SET_MANAGEMENT,
                device_reads_buffer: true,
                sectors: parameter_sectors,
                data_transfer: true,
                tag: None,
            }
        }
    };
    Ok((fis, attributes))
}

fn set_target(fis: &mut [u8; 20], pmp_port: u8) -> Result<(), AtaRequestError> {
    if pmp_port >= 16 {
        return Err(AtaRequestError::InvalidPmpPort);
    }
    // Upstream places the PMP port in bits 3:0 while retaining the command bit.
    fis[1] = FIS_FLAG_COMMAND | pmp_port;
    Ok(())
}

fn validate_lba_transfer(lba: u64, sectors: u16) -> Result<(), AtaRequestError> {
    if sectors == 0 {
        return Err(AtaRequestError::EmptyTransfer);
    }
    const LBA48_LIMIT: u64 = 1 << 48;
    let end = lba
        .checked_add(u64::from(sectors))
        .ok_or(AtaRequestError::LbaOutOfRange)?;
    if lba >= LBA48_LIMIT || end > LBA48_LIMIT {
        return Err(AtaRequestError::LbaOutOfRange);
    }
    Ok(())
}

// upstream: ahci.c ahci_setup_fis() LBA48 field encoding
fn encode_lba48(fis: &mut [u8; 20], lba: u64) {
    fis[4] = lba as u8;
    fis[5] = (lba >> 8) as u8;
    fis[6] = (lba >> 16) as u8;
    fis[8] = (lba >> 24) as u8;
    fis[9] = (lba >> 32) as u8;
    fis[10] = (lba >> 40) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dma_ext_fis_encodes_lba_and_sector_count_in_ata_order() {
        let (fis, attrs) = setup_register_fis(AtaRequest::DmaExt {
            lba: 0x12_3456_789abc,
            sectors: 0x1234,
            write: false,
            pmp_port: 2,
        })
        .unwrap();
        assert_eq!(
            &fis[..14],
            &[
                0x27, 0x82, 0x25, 0, 0xbc, 0x9a, 0x78, 0x40, 0x56, 0x34, 0x12, 0, 0x34, 0x12
            ]
        );
        assert_eq!(attrs.command, ATA_READ_DMA_EXT);
        assert!(!attrs.device_reads_buffer);
        assert_eq!(attrs.sectors, 0x1234);
    }

    #[test]
    fn ncq_fis_keeps_count_and_tag_in_their_distinct_fields() {
        let (fis, attrs) = setup_register_fis(AtaRequest::Fpdma {
            lba: 0x1020_3040_5060,
            sectors: 0x0234,
            write: true,
            tag: 7,
            pmp_port: 0,
        })
        .unwrap();
        assert_eq!(fis[2], ATA_WRITE_FPDMA_QUEUED);
        assert_eq!(fis[3], 0x34);
        assert_eq!(fis[11], 0x02);
        assert_eq!(fis[12], (7 << 3) | 4);
        assert_eq!(fis[15], ATA_A_4BIT as u8);
        assert!(attrs.device_reads_buffer);
        assert_eq!(attrs.tag, Some(7));
        assert_eq!(fis[15], ATA_A_4BIT as u8);
    }

    #[test]
    fn flush_identify_and_trim_use_the_upstream_opcodes() {
        assert_eq!(
            setup_register_fis(AtaRequest::Identify { pmp_port: 0 })
                .unwrap()
                .0[2],
            ATA_IDENTIFY_DEVICE
        );
        assert_eq!(
            setup_register_fis(AtaRequest::FlushCacheExt { pmp_port: 0 })
                .unwrap()
                .0[2],
            ATA_FLUSH_CACHE_EXT
        );
        let (fis, attrs) = setup_register_fis(AtaRequest::DsmTrim {
            parameter_sectors: 1,
            pmp_port: 0,
        })
        .unwrap();
        assert_eq!(&fis[2..4], &[ATA_DATA_SET_MANAGEMENT, ATA_DSM_TRIM]);
        assert!(attrs.data_transfer);
    }

    #[test]
    fn rejects_empty_transfers_invalid_tags_and_lba48_overflow() {
        assert_eq!(
            setup_register_fis(AtaRequest::DmaExt {
                lba: 0,
                sectors: 0,
                write: false,
                pmp_port: 0,
            }),
            Err(AtaRequestError::EmptyTransfer)
        );
        assert_eq!(
            setup_register_fis(AtaRequest::DmaExt {
                lba: (1 << 48) - 1,
                sectors: 2,
                write: false,
                pmp_port: 0,
            }),
            Err(AtaRequestError::LbaOutOfRange)
        );
        assert_eq!(
            setup_register_fis(AtaRequest::Fpdma {
                lba: 0,
                sectors: 1,
                write: false,
                tag: 32,
                pmp_port: 0,
            }),
            Err(AtaRequestError::InvalidTag)
        );
    }
}
