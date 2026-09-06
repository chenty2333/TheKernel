//! Pure x86_64 Linux ABI policy plans.
#![no_std]
#![allow(missing_docs)]

pub const PKEY_COUNT: u8 = 16;
pub const DEFAULT_PKEY: u8 = 0;
pub const PKEY_DISABLE_ACCESS: u32 = 1;
pub const PKEY_DISABLE_WRITE: u32 = 2;
pub const PKEY_RIGHTS_MASK: u32 = PKEY_DISABLE_ACCESS | PKEY_DISABLE_WRITE;
/// Bit used by the x86 shadow-stack signal ABI to distinguish data words
/// from return addresses.
pub const CET_SHSTK_DATA_BIT: u64 = 1 << 63;
/// The x86_64 shadow-stack word size.
pub const CET_SHSTK_WORD_SIZE: u64 = 8;
/// A signal transition pushes a restorer and its one restore token.
pub const CET_SIGNAL_FRAME_SIZE: u64 = CET_SHSTK_WORD_SIZE * 2;
/// ELF note type used by `PTRACE_{GET,SET}REGSET` for the x86 shadow-stack
/// state. This is the Linux x86_64 UAPI value.
pub const NT_X86_SHSTK: usize = 0x204;
/// `arch_prctl(ARCH_SHSTK_UNLOCK)` operation.
pub const ARCH_SHSTK_UNLOCK: i32 = 0x5004;
/// `siginfo_t.si_code` for an x86 control-protection exception.
pub const SEGV_CPERR: i32 = 10;

/// Linux's x86 shadow-stack ptrace regset payload. `NT_X86_SHSTK` has exactly
/// one eight-byte element: the task's IA32_PL3_SSP value.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct X86ShstkRegset {
    pub ssp: u64,
}

const _: () = assert!(core::mem::size_of::<X86ShstkRegset>() == 8);
const _: () = assert!(core::mem::align_of::<X86ShstkRegset>() == 8);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchPolicyError {
    InvalidPkey,
    InvalidPkeyRights,
    DefaultPkey,
    InvalidCetFlags,
    InvalidCetSignalToken,
    IoPortOverflow,
    InvalidIopl,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PkeyPlan {
    key: u8,
    rights: u32,
}
impl PkeyPlan {
    pub const fn new(key: u8, rights: u32) -> Result<Self, ArchPolicyError> {
        if key >= PKEY_COUNT {
            return Err(ArchPolicyError::InvalidPkey);
        }
        if key == DEFAULT_PKEY {
            return Err(ArchPolicyError::DefaultPkey);
        }
        if rights & !PKEY_RIGHTS_MASK != 0 {
            return Err(ArchPolicyError::InvalidPkeyRights);
        }
        Ok(Self { key, rights })
    }
    pub const fn key(self) -> u8 {
        self.key
    }
    pub const fn rights(self) -> u32 {
        self.rights
    }
    pub const fn apply_to_pkru(self, pkru: u32) -> u32 {
        let shift = self.key as u32 * 2;
        (pkru & !(PKEY_RIGHTS_MASK << shift)) | (self.rights << shift)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CetRestore {
    pub shadow_stack_pointer: u64,
    pub features: u64,
}
impl CetRestore {
    pub const fn new(shadow_stack_pointer: u64, features: u64) -> Result<Self, ArchPolicyError> {
        if features & !3 != 0 {
            Err(ArchPolicyError::InvalidCetFlags)
        } else {
            Ok(Self {
                shadow_stack_pointer,
                features,
            })
        }
    }
}

/// Encodes the restore token placed below a signal handler's shadow-stack
/// restorer. The token names the pre-signal SSP and is tagged as data so it
/// cannot be consumed as a return address.
pub const fn cet_signal_restore_token(old_ssp: u64) -> Result<u64, ArchPolicyError> {
    if old_ssp & CET_SHSTK_DATA_BIT != 0 || old_ssp % CET_SHSTK_WORD_SIZE != 0 {
        return Err(ArchPolicyError::InvalidCetSignalToken);
    }
    Ok(old_ssp | CET_SHSTK_DATA_BIT)
}

/// Decodes a signal restore token. Address-space and canonical-address
/// policy remain kernel responsibilities, but malformed token bits and
/// alignment are rejected here before any state is committed.
pub const fn cet_signal_restore_ssp(token: u64) -> Result<u64, ArchPolicyError> {
    if token & CET_SHSTK_DATA_BIT == 0 {
        return Err(ArchPolicyError::InvalidCetSignalToken);
    }
    let old_ssp = token & !CET_SHSTK_DATA_BIT;
    if old_ssp % CET_SHSTK_WORD_SIZE != 0 {
        return Err(ArchPolicyError::InvalidCetSignalToken);
    }
    Ok(old_ssp)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoPortPlan {
    pub first: usize,
    pub count: usize,
    pub enable: bool,
}
impl IoPortPlan {
    pub const fn new(first: usize, count: usize, enable: bool) -> Result<Self, ArchPolicyError> {
        let end = match first.checked_add(count) {
            Some(end) => end,
            None => return Err(ArchPolicyError::IoPortOverflow),
        };
        if count == 0 || end > 65_536 {
            Err(ArchPolicyError::IoPortOverflow)
        } else {
            Ok(Self {
                first,
                count,
                enable,
            })
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoplPlan {
    level: u8,
}
impl IoplPlan {
    pub const fn new(level: u8) -> Result<Self, ArchPolicyError> {
        if level > 3 {
            Err(ArchPolicyError::InvalidIopl)
        } else {
            Ok(Self { level })
        }
    }
    pub const fn level(self) -> u8 {
        self.level
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pkey_and_port_boundaries() {
        assert_eq!(PkeyPlan::new(0, 0), Err(ArchPolicyError::DefaultPkey));
        assert_eq!(PkeyPlan::new(16, 0), Err(ArchPolicyError::InvalidPkey));
        assert_eq!(
            IoPortPlan::new(65535, 2, true),
            Err(ArchPolicyError::IoPortOverflow)
        );
        assert_eq!(IoPortPlan::new(0, 65_536, true).unwrap().count, 65_536);
    }

    #[test]
    fn cet_signal_restore_token_is_tagged_and_aligned() {
        let ssp = 0x0000_7fff_ffff_f000;
        let token = cet_signal_restore_token(ssp).unwrap();
        assert_eq!(token, ssp | CET_SHSTK_DATA_BIT);
        assert_eq!(cet_signal_restore_ssp(token), Ok(ssp));
        assert_eq!(
            cet_signal_restore_token(ssp + 1),
            Err(ArchPolicyError::InvalidCetSignalToken)
        );
        assert_eq!(
            cet_signal_restore_ssp(ssp),
            Err(ArchPolicyError::InvalidCetSignalToken)
        );
    }

    #[test]
    fn cet_observability_uapi_matches_linux_x86_64() {
        assert_eq!(NT_X86_SHSTK, 0x204);
        assert_eq!(ARCH_SHSTK_UNLOCK, 0x5004);
        assert_eq!(SEGV_CPERR, 10);
        assert_eq!(core::mem::size_of::<X86ShstkRegset>(), 8);
    }
}
