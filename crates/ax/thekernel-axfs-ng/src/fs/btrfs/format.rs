use axerrno::{AxError, AxResult};

/// Bytes in every Btrfs superblock copy.
pub const BTRFS_SUPERBLOCK_SIZE: usize = 4096;
const CHECKSUM_BYTES: usize = 32;
const MAGIC_OFFSET: usize = 0x40;
const MAGIC: &[u8; 8] = b"_BHRfS_M";
const DEV_ITEM_OFFSET: usize = 0xc9;
const SYS_CHUNK_ARRAY_OFFSET: usize = 0x32b;
const SYS_CHUNK_ARRAY_BYTES: usize = 0x800;

/// The checksum algorithm selected by a Btrfs superblock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChecksumType {
    Crc32c,
}

/// A checksum stored in the checksum tree.  Btrfs uses a 32-byte slot even
/// for a four-byte CRC32C digest, so callers preserve the exact stored width.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Checksum {
    bytes: [u8; CHECKSUM_BYTES],
    len: u8,
    ty: ChecksumType,
}

impl Checksum {
    pub fn crc32c(value: u32) -> Self {
        let mut bytes = [0; CHECKSUM_BYTES];
        bytes[..4].copy_from_slice(&value.to_le_bytes());
        Self {
            bytes,
            len: 4,
            ty: ChecksumType::Crc32c,
        }
    }

    /// Decodes the checksum field stored ahead of a tree block.  CRC32C has a
    /// four-byte digest; callers pass the whole 32-byte header slot and this
    /// method deliberately ignores only its defined unused tail.
    pub fn from_disk(ty: ChecksumType, bytes: &[u8]) -> AxResult<Self> {
        match ty {
            ChecksumType::Crc32c => {
                let raw: [u8; 4] = bytes
                    .get(..4)
                    .and_then(|value| value.try_into().ok())
                    .ok_or(AxError::Io)?;
                Ok(Self::crc32c(u32::from_le_bytes(raw)))
            }
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    pub fn verify(&self, data: &[u8]) -> bool {
        match self.ty {
            ChecksumType::Crc32c => self.bytes() == crc32c(data).to_le_bytes(),
        }
    }
}

/// Parsed and validated fixed superblock state.  Tree roots remain raw
/// logical addresses until the chunk tree is loaded and checked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BtrfsSuperblock {
    raw: [u8; BTRFS_SUPERBLOCK_SIZE],
    pub fsid: [u8; 16],
    pub bytenr: u64,
    pub generation: u64,
    pub root: u64,
    pub chunk_root: u64,
    pub log_root: u64,
    pub log_root_transid: u64,
    pub total_bytes: u64,
    pub bytes_used: u64,
    pub root_dir_objectid: u64,
    pub num_devices: u64,
    pub sectorsize: u32,
    pub nodesize: u32,
    pub leafsize: u32,
    pub stripesize: u32,
    pub log_root_level: u8,
    pub csum_type: ChecksumType,
    /// Device ID of this superblock member.  Bootstrap chunk decoding uses
    /// it to reject a chunk which refers to a member the mount did not get.
    pub devid: u64,
    sys_chunk_array_size: u32,
}

impl BtrfsSuperblock {
    pub const fn system_chunk_array_capacity() -> usize {
        SYS_CHUNK_ARRAY_BYTES
    }
    /// Decodes a complete 4 KiB superblock after checking its checksum,
    /// magic, address, geometry, and selected checksum type.  Invalid media
    /// is `EIO`; an unsupported future checksum is `EOPNOTSUPP`.
    pub fn decode(bytes: &[u8], expected_bytenr: u64) -> AxResult<Self> {
        if bytes.len() != BTRFS_SUPERBLOCK_SIZE {
            return Err(AxError::Io);
        }
        if &bytes[MAGIC_OFFSET..MAGIC_OFFSET + MAGIC.len()] != MAGIC {
            return Err(AxError::Io);
        }
        let on_disk_checksum = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        if on_disk_checksum != crc32c(&bytes[CHECKSUM_BYTES..]) {
            return Err(AxError::Io);
        }
        let bytenr = le64(bytes, 0x30)?;
        if bytenr != expected_bytenr {
            return Err(AxError::Io);
        }
        let csum_type = match le16(bytes, 0xc4)? {
            0 => ChecksumType::Crc32c,
            _ => return Err(AxError::Unsupported),
        };
        let sectorsize = le32(bytes, 0x90)?;
        let nodesize = le32(bytes, 0x94)?;
        let leafsize = le32(bytes, 0x98)?;
        let stripesize = le32(bytes, 0x9c)?;
        validate_geometry(sectorsize, nodesize, leafsize, stripesize)?;
        let total_bytes = le64(bytes, 0x70)?;
        let bytes_used = le64(bytes, 0x78)?;
        if total_bytes == 0 || bytes_used > total_bytes {
            return Err(AxError::Io);
        }
        let log_root = le64(bytes, 0x60)?;
        let log_root_transid = le64(bytes, 0x68)?;
        let log_root_level = bytes[0xc8];
        if (log_root == 0 && (log_root_transid != 0 || log_root_level != 0))
            || (log_root != 0 && log_root_transid == 0)
        {
            return Err(AxError::Io);
        }
        let mut fsid = [0; 16];
        fsid.copy_from_slice(&bytes[0x20..0x30]);
        let sys_chunk_array_size = le32(bytes, 0xa0)?;
        if sys_chunk_array_size as usize > SYS_CHUNK_ARRAY_BYTES {
            return Err(AxError::Io);
        }
        Ok(Self {
            fsid,
            raw: bytes.try_into().map_err(|_| AxError::Io)?,
            bytenr,
            generation: le64(bytes, 0x48)?,
            root: le64(bytes, 0x50)?,
            chunk_root: le64(bytes, 0x58)?,
            log_root,
            log_root_transid,
            total_bytes,
            bytes_used,
            root_dir_objectid: le64(bytes, 0x80)?,
            num_devices: le64(bytes, 0x88)?,
            sectorsize,
            nodesize,
            leafsize,
            stripesize,
            log_root_level,
            csum_type,
            devid: le64(bytes, DEV_ITEM_OFFSET)?,
            sys_chunk_array_size,
        })
    }

    /// Returns the exact valid prefix of the fixed bootstrap chunk array.
    /// Its records are parsed before the chunk tree is reachable, so callers
    /// must never scan its unused zero-filled tail.
    pub fn system_chunk_array(&self) -> &[u8] {
        let end = SYS_CHUNK_ARRAY_OFFSET + self.sys_chunk_array_size as usize;
        &self.raw[SYS_CHUNK_ARRAY_OFFSET..end]
    }

    /// Builds a new superblock image from this exact validated copy.  Unknown
    /// feature and backup-root fields are retained byte-for-byte; only the
    /// COW root publication fields and checksum are changed.
    // Writer-side superblock commit kept for the gated Btrfs COW writer.
    #[allow(dead_code)]
    pub fn prepare_commit(
        &self,
        generation: u64,
        root: u64,
        chunk_root: u64,
        log_root: u64,
        bytes_used: u64,
    ) -> AxResult<[u8; BTRFS_SUPERBLOCK_SIZE]> {
        if generation <= self.generation
            || root == 0
            || chunk_root == 0
            || bytes_used > self.total_bytes
        {
            return Err(AxError::InvalidInput);
        }
        let mut image = self.raw;
        image[0x48..0x50].copy_from_slice(&generation.to_le_bytes());
        image[0x50..0x58].copy_from_slice(&root.to_le_bytes());
        image[0x58..0x60].copy_from_slice(&chunk_root.to_le_bytes());
        image[0x60..0x68].copy_from_slice(&log_root.to_le_bytes());
        image[0x68..0x70].copy_from_slice(
            &(if log_root == 0 {
                0
            } else if log_root == self.log_root {
                self.log_root_transid
            } else {
                generation
            })
            .to_le_bytes(),
        );
        image[0xc8] = if log_root == 0 {
            0
        } else if log_root == self.log_root {
            self.log_root_level
        } else {
            0
        };
        image[0x78..0x80].copy_from_slice(&bytes_used.to_le_bytes());
        image[..CHECKSUM_BYTES].fill(0);
        let checksum = crc32c(&image[CHECKSUM_BYTES..]);
        image[..4].copy_from_slice(&checksum.to_le_bytes());
        Ok(image)
    }

    /// Prepares the final superblock publication for a device/chunk topology
    /// transaction.  The caller has already made the replacement chunk tree
    /// durable; this method is the one place that changes the bootstrap map,
    /// member count, roots and checksum together.
    // Writer-side superblock commit kept for the gated Btrfs COW writer.
    #[allow(dead_code)]
    pub fn prepare_topology_commit(
        &self,
        generation: u64,
        root: u64,
        chunk_root: u64,
        log_root: u64,
        bytes_used: u64,
        num_devices: u64,
        system_chunks: &[u8],
    ) -> AxResult<[u8; BTRFS_SUPERBLOCK_SIZE]> {
        if num_devices == 0 || system_chunks.len() > SYS_CHUNK_ARRAY_BYTES {
            return Err(AxError::InvalidInput);
        }
        self.prepare_topology_commit_with_total(
            generation,
            root,
            chunk_root,
            log_root,
            if log_root == self.log_root {
                self.log_root_level
            } else {
                0
            },
            bytes_used,
            self.total_bytes,
            num_devices,
            system_chunks,
        )
    }

    pub fn prepare_topology_commit_with_total(
        &self,
        generation: u64,
        root: u64,
        chunk_root: u64,
        log_root: u64,
        log_root_level: u8,
        bytes_used: u64,
        total_bytes: u64,
        num_devices: u64,
        system_chunks: &[u8],
    ) -> AxResult<[u8; BTRFS_SUPERBLOCK_SIZE]> {
        if generation <= self.generation
            || root == 0
            || chunk_root == 0
            || num_devices == 0
            || system_chunks.len() > SYS_CHUNK_ARRAY_BYTES
            || total_bytes == 0
            || bytes_used > total_bytes
        {
            return Err(AxError::InvalidInput);
        }
        // `total_bytes` is changed by this transaction, so validating through
        // `prepare_commit` would incorrectly compare the final usage to the
        // retiring topology's capacity.  Assemble the root and usage fields
        // here against the final capacity, then seal the complete image once.
        let mut image = self.raw;
        image[0x48..0x50].copy_from_slice(&generation.to_le_bytes());
        image[0x50..0x58].copy_from_slice(&root.to_le_bytes());
        image[0x58..0x60].copy_from_slice(&chunk_root.to_le_bytes());
        image[0x60..0x68].copy_from_slice(&log_root.to_le_bytes());
        image[0x68..0x70].copy_from_slice(
            &(if log_root == 0 {
                0
            } else if log_root == self.log_root {
                self.log_root_transid
            } else {
                generation
            })
            .to_le_bytes(),
        );
        image[0xc8] = if log_root == 0 { 0 } else { log_root_level };
        image[0x78..0x80].copy_from_slice(&bytes_used.to_le_bytes());
        image[0x70..0x78].copy_from_slice(&total_bytes.to_le_bytes());
        image[0x88..0x90].copy_from_slice(&num_devices.to_le_bytes());
        image[0xa0..0xa4].copy_from_slice(&(system_chunks.len() as u32).to_le_bytes());
        image[SYS_CHUNK_ARRAY_OFFSET..SYS_CHUNK_ARRAY_OFFSET + SYS_CHUNK_ARRAY_BYTES].fill(0);
        image[SYS_CHUNK_ARRAY_OFFSET..SYS_CHUNK_ARRAY_OFFSET + system_chunks.len()]
            .copy_from_slice(system_chunks);
        image[..CHECKSUM_BYTES].fill(0);
        let checksum = crc32c(&image[CHECKSUM_BYTES..]);
        image[..4].copy_from_slice(&checksum.to_le_bytes());
        Ok(image)
    }

    /// Produces the topology generation for one concrete member.  New
    /// devices begin from the last known-good member image, but their native
    /// `dev_item` is replaced in full before the checksum is sealed; existing
    /// devices follow the same path so no publication can accidentally copy
    /// the selected member's devid/UUID onto another member.
    pub fn prepare_topology_member_commit(
        &self,
        generation: u64,
        root: u64,
        chunk_root: u64,
        log_root: u64,
        log_root_level: u8,
        bytes_used: u64,
        total_bytes: u64,
        num_devices: u64,
        system_chunks: &[u8],
        device: super::BtrfsDeviceItem,
        bytenr: u64,
    ) -> AxResult<[u8; BTRFS_SUPERBLOCK_SIZE]> {
        if device.devid == 0 || device.fsid != self.fsid {
            return Err(AxError::InvalidInput);
        }
        let mut image = self.prepare_topology_commit_with_total(
            generation,
            root,
            chunk_root,
            log_root,
            log_root_level,
            bytes_used,
            total_bytes,
            num_devices,
            system_chunks,
        )?;
        image[0x30..0x38].copy_from_slice(&bytenr.to_le_bytes());
        let encoded = device.encode()?;
        image[DEV_ITEM_OFFSET..DEV_ITEM_OFFSET + encoded.len()].copy_from_slice(&encoded);
        image[..CHECKSUM_BYTES].fill(0);
        let checksum = crc32c(&image[CHECKSUM_BYTES..]);
        image[..4].copy_from_slice(&checksum.to_le_bytes());
        Ok(image)
    }
}

fn validate_geometry(
    sectorsize: u32,
    nodesize: u32,
    leafsize: u32,
    stripesize: u32,
) -> AxResult<()> {
    let valid_power_of_two = |value: u32| value >= 4096 && value.is_power_of_two();
    if !valid_power_of_two(sectorsize)
        || !valid_power_of_two(nodesize)
        || leafsize != nodesize
        || nodesize < sectorsize
        || stripesize < sectorsize
        || !stripesize.is_power_of_two()
    {
        return Err(AxError::Io);
    }
    Ok(())
}

fn le16(bytes: &[u8], offset: usize) -> AxResult<u16> {
    bytes
        .get(offset..offset + 2)
        .and_then(|v| v.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or(AxError::Io)
}
fn le32(bytes: &[u8], offset: usize) -> AxResult<u32> {
    bytes
        .get(offset..offset + 4)
        .and_then(|v| v.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(AxError::Io)
}
fn le64(bytes: &[u8], offset: usize) -> AxResult<u64> {
    bytes
        .get(offset..offset + 8)
        .and_then(|v| v.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(AxError::Io)
}

/// CRC-32C (Castagnoli) with the Btrfs wire initial/final complement.
/// The small table-free routine is used only for metadata and checksum-tree
/// verification; it avoids a mutable global table during early mount.
pub fn crc32c(bytes: &[u8]) -> u32 {
    !crc32c_seed(!0u32, bytes)
}

/// Raw seeded CRC-32C update, matching the kernel `crc32c(seed, bytes, len)`
/// convention.  Btrfs uses this form for the extended inode-ref key hash,
/// whose seed is the parent objectid truncated to the CRC accumulator width.
pub fn crc32c_seed(mut crc: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    crc
}
