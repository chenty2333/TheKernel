//! Linux `security.capability` value and wire-format parsing.
//!
//! The parser accepts only complete revision 1, 2, or 3 records and performs
//! no allocation. Filesystem lookup, xattr storage, and executable metadata
//! synchronization remain responsibilities of the embedding kernel.

use crate::{CAPABILITY_VALID_MASK, CAPABILITY_WORDS, CredError, Kuid};

const VFS_CAP_REVISION_MASK: u32 = 0xff00_0000;
const VFS_CAP_REVISION_1: u32 = 0x0100_0000;
const VFS_CAP_REVISION_2: u32 = 0x0200_0000;
const VFS_CAP_REVISION_3: u32 = 0x0300_0000;
const VFS_CAP_FLAGS_EFFECTIVE: u32 = 0x0000_0001;
const VFS_CAP_REVISION_1_SIZE: usize = 12;
const VFS_CAP_REVISION_2_SIZE: usize = 20;
const VFS_CAP_REVISION_3_SIZE: usize = 24;

/// Exact Linux xattr name bytes carrying executable file capabilities.
///
/// The terminating NUL used by the syscall ABI is not part of this value.
pub const SECURITY_CAPABILITY_XATTR_NAME: &[u8] = b"security.capability";

/// Strictly validated Linux executable file capabilities.
///
/// Revisions 1 and 2 are rooted at kernel UID 0. Revision 3 carries an
/// explicit kernel-global root ID. The parsed revision is deliberately not
/// retained because exec derivation depends only on these normalized values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileCapabilities {
    permitted: [u32; CAPABILITY_WORDS],
    inheritable: [u32; CAPABILITY_WORDS],
    effective: bool,
    rootid: Kuid,
}

impl FileCapabilities {
    /// Constructs file capabilities after validating every capability bit.
    ///
    /// # Errors
    ///
    /// Returns [`CredError::InvalidInput`] if either capability set contains a
    /// bit above Linux's supported `CAP_LAST_CAP`.
    pub fn try_new(
        permitted: [u32; CAPABILITY_WORDS],
        inheritable: [u32; CAPABILITY_WORDS],
        effective: bool,
        rootid: Kuid,
    ) -> Result<Self, CredError> {
        for word in 0..CAPABILITY_WORDS {
            if (permitted[word] | inheritable[word]) & !CAPABILITY_VALID_MASK[word] != 0 {
                return Err(CredError::InvalidInput);
            }
        }
        Ok(Self {
            permitted,
            inheritable,
            effective,
            rootid,
        })
    }

    /// Returns the file-permitted capability set.
    pub const fn permitted(self) -> [u32; CAPABILITY_WORDS] {
        self.permitted
    }

    /// Returns the file-inheritable capability set.
    pub const fn inheritable(self) -> [u32; CAPABILITY_WORDS] {
        self.inheritable
    }

    /// Returns whether the file requests its resulting permitted set to become
    /// effective.
    pub const fn effective(self) -> bool {
        self.effective
    }

    /// Returns the kernel-global user ID at which this record is rooted.
    pub const fn rootid(self) -> Kuid {
        self.rootid
    }
}

fn read_le_u32(value: &[u8], offset: usize) -> Result<u32, CredError> {
    let bytes = value
        .get(offset..offset + core::mem::size_of::<u32>())
        .ok_or(CredError::InvalidInput)?;
    Ok(u32::from_le_bytes(
        bytes.try_into().map_err(|_| CredError::InvalidInput)?,
    ))
}

/// Parses and validates one complete Linux v1/v2/v3 file-capability record.
///
/// The parser rejects unknown revisions or flags, inexact record sizes,
/// unsupported capability bits, and revision 3's invalid all-ones root ID.
/// It performs no allocation.
///
/// # Errors
///
/// Returns [`CredError::InvalidInput`] for every malformed record.
pub fn parse_file_capabilities(value: &[u8]) -> Result<FileCapabilities, CredError> {
    let magic = read_le_u32(value, 0)?;
    let revision = magic & VFS_CAP_REVISION_MASK;
    let flags = magic & !VFS_CAP_REVISION_MASK;
    if flags & !VFS_CAP_FLAGS_EFFECTIVE != 0 {
        return Err(CredError::InvalidInput);
    }

    let expected_size = match revision {
        VFS_CAP_REVISION_1 => VFS_CAP_REVISION_1_SIZE,
        VFS_CAP_REVISION_2 => VFS_CAP_REVISION_2_SIZE,
        VFS_CAP_REVISION_3 => VFS_CAP_REVISION_3_SIZE,
        _ => return Err(CredError::InvalidInput),
    };
    if value.len() != expected_size {
        return Err(CredError::InvalidInput);
    }

    let mut permitted = [0; CAPABILITY_WORDS];
    let mut inheritable = [0; CAPABILITY_WORDS];
    permitted[0] = read_le_u32(value, 4)?;
    inheritable[0] = read_le_u32(value, 8)?;
    if revision != VFS_CAP_REVISION_1 {
        permitted[1] = read_le_u32(value, 12)?;
        inheritable[1] = read_le_u32(value, 16)?;
    }

    let rootid = if revision == VFS_CAP_REVISION_3 {
        Kuid::from_raw(read_le_u32(value, 20)?).ok_or(CredError::InvalidInput)?
    } else {
        Kuid::INITIAL_ROOT
    };

    FileCapabilities::try_new(
        permitted,
        inheritable,
        flags & VFS_CAP_FLAGS_EFFECTIVE != 0,
        rootid,
    )
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use linux_raw_sys::general::{CAP_CHOWN, CAP_DAC_OVERRIDE};

    use super::*;
    use crate::CapabilitySets;

    fn bit(capability: u32) -> [u32; CAPABILITY_WORDS] {
        let mut bits = [0; CAPABILITY_WORDS];
        let (word, mask) = CapabilitySets::cap_mask(capability).unwrap();
        bits[word] = mask;
        bits
    }

    fn append_word(bytes: &mut Vec<u8>, word: u32) {
        bytes.extend_from_slice(&word.to_le_bytes());
    }

    fn file_cap_bytes(
        revision: u32,
        effective: bool,
        permitted: [u32; CAPABILITY_WORDS],
        inheritable: [u32; CAPABILITY_WORDS],
        rootid: Option<u32>,
    ) -> Vec<u8> {
        let mut value = Vec::new();
        append_word(
            &mut value,
            revision | (u32::from(effective) * VFS_CAP_FLAGS_EFFECTIVE),
        );
        append_word(&mut value, permitted[0]);
        append_word(&mut value, inheritable[0]);
        if revision != VFS_CAP_REVISION_1 {
            append_word(&mut value, permitted[1]);
            append_word(&mut value, inheritable[1]);
        }
        if revision == VFS_CAP_REVISION_3 {
            append_word(&mut value, rootid.unwrap());
        }
        value
    }

    #[test]
    fn parses_file_capability_revisions_and_little_endian_words() {
        let first = bit(CAP_CHOWN);
        let second = bit(CAP_DAC_OVERRIDE);

        let v1 = file_cap_bytes(VFS_CAP_REVISION_1, true, first, second, None);
        let parsed = parse_file_capabilities(&v1).unwrap();
        assert_eq!(parsed.permitted(), first);
        assert_eq!(parsed.inheritable()[0], second[0]);
        assert_eq!(parsed.inheritable()[1], 0);
        assert!(parsed.effective());
        assert_eq!(parsed.rootid(), Kuid::INITIAL_ROOT);

        let v2 = file_cap_bytes(VFS_CAP_REVISION_2, false, second, first, None);
        let parsed = parse_file_capabilities(&v2).unwrap();
        assert_eq!(parsed.permitted(), second);
        assert_eq!(parsed.inheritable(), first);
        assert!(!parsed.effective());
        assert_eq!(parsed.rootid(), Kuid::INITIAL_ROOT);

        let v3 = file_cap_bytes(VFS_CAP_REVISION_3, true, first, second, Some(1000));
        let parsed = parse_file_capabilities(&v3).unwrap();
        assert_eq!(parsed.permitted(), first);
        assert_eq!(parsed.inheritable(), second);
        assert!(parsed.effective());
        assert_eq!(parsed.rootid(), Kuid::from_raw(1000).unwrap());
    }

    #[test]
    fn rejects_truncated_oversized_unknown_and_invalid_file_capabilities() {
        let caps = bit(CAP_CHOWN);
        let mut truncated = file_cap_bytes(VFS_CAP_REVISION_2, true, caps, caps, None);
        truncated.pop();
        assert_eq!(
            parse_file_capabilities(&truncated),
            Err(CredError::InvalidInput)
        );

        let mut oversized = file_cap_bytes(VFS_CAP_REVISION_1, true, caps, caps, None);
        oversized.push(0);
        assert_eq!(
            parse_file_capabilities(&oversized),
            Err(CredError::InvalidInput)
        );

        let unknown_revision = file_cap_bytes(0x0400_0000, false, caps, caps, None);
        assert_eq!(
            parse_file_capabilities(&unknown_revision),
            Err(CredError::InvalidInput)
        );

        let mut unknown_flag = file_cap_bytes(VFS_CAP_REVISION_2, false, caps, caps, None);
        unknown_flag[0] |= 0x02;
        assert_eq!(
            parse_file_capabilities(&unknown_flag),
            Err(CredError::InvalidInput)
        );

        let mut invalid_mask = file_cap_bytes(VFS_CAP_REVISION_2, false, caps, caps, None);
        invalid_mask[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            parse_file_capabilities(&invalid_mask),
            Err(CredError::InvalidInput)
        );

        let invalid_root = file_cap_bytes(VFS_CAP_REVISION_3, false, caps, caps, Some(u32::MAX));
        assert_eq!(
            parse_file_capabilities(&invalid_root),
            Err(CredError::InvalidInput)
        );

        assert_eq!(
            FileCapabilities::try_new(
                caps,
                [u32::MAX; CAPABILITY_WORDS],
                false,
                Kuid::INITIAL_ROOT
            ),
            Err(CredError::InvalidInput)
        );
    }
}
