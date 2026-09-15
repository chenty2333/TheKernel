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

/// Linux `XFEATURE_MAX` from arch/x86/include/asm/fpu/types.h: the number of
/// xfeature components, and therefore the exclusive bound on the component
/// index accepted by `ARCH_REQ_XCOMP_PERM`.
pub const XFEATURE_MAX: u64 = 20;
/// `XFEATURE_XTILE_DATA`, the only component `xstate_prctl_req[]` maps to a
/// requestable facility mask; aliased as `ARCH_XCOMP_TILEDATA`.
pub const XFEATURE_XTILE_DATA: u64 = 18;
/// `XFEATURE_MASK_XTILE_DATA`: the AMX tile-data facility.
pub const XFEATURE_MASK_XTILE_DATA: u64 = 1 << XFEATURE_XTILE_DATA;
/// `XFEATURE_MASK_FP | XFEATURE_MASK_SSE`: the legacy user state that a kernel
/// without XSAVE still reports through `ARCH_GET_XCOMP_SUPP`.
pub const XFEATURE_MASK_FPSSE: u64 = 0b11;

/// Why `ARCH_REQ_XCOMP_PERM` / `ARCH_REQ_XCOMP_GUEST_PERM` refuses a request.
///
/// `fpu_xstate_prctl()` in arch/x86/kernel/fpu/xstate.c validates the
/// component index before it consults `xstate_prctl_req[]`, so the two
/// refusals are distinguishable and both are part of the UAPI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XcompRequestRejection {
    /// `idx >= XFEATURE_MAX`: `-EINVAL`.
    UnknownComponent,
    /// A known component with no requestable facility, or a facility outside
    /// this kernel's `fpu_user_cfg.max_features`: `-EOPNOTSUPP`.
    UnsupportedFacility,
}

/// Applies `xstate_request_perm()`'s index admission from
/// arch/x86/kernel/fpu/xstate.c.
///
/// `max_features` is this kernel's user-visible xfeature mask (Linux's
/// `fpu_user_cfg.max_features`). It returns `Ok(facility)` only when the whole
/// requested facility is already enabled, which is the one case an embedding
/// kernel may answer without a permission-state transaction.
pub const fn xcomp_request_admission(
    index: u64,
    max_features: u64,
) -> Result<u64, XcompRequestRejection> {
    if index >= XFEATURE_MAX {
        return Err(XcompRequestRejection::UnknownComponent);
    }
    // `xstate_prctl_req[]` only carries XFEATURE_XTILE_DATA; every other index
    // has a zero entry, which Linux maps to -EOPNOTSUPP.
    if index != XFEATURE_XTILE_DATA {
        return Err(XcompRequestRejection::UnsupportedFacility);
    }
    if max_features & XFEATURE_MASK_XTILE_DATA != XFEATURE_MASK_XTILE_DATA {
        return Err(XcompRequestRejection::UnsupportedFacility);
    }
    Ok(XFEATURE_MASK_XTILE_DATA)
}

/// Reports whether `ARCH_SET_FS` / `ARCH_SET_GS` accept `base`.
///
/// `do_arch_prctl_64()` in arch/x86/kernel/process_64.c rejects
/// `arg2 >= TASK_SIZE_MAX` with `-EPERM` before it touches either segment
/// base, and it applies the identical check to both commands.
pub const fn arch_segment_base_permitted(base: u64, task_size_max: u64) -> bool {
    base < task_size_max
}

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
/// How a validated `iopl(2)` request relates to the caller's current level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoplTransition {
    /// The request is a no-op; Linux returns success before any check.
    Unchanged,
    /// The request grants I/O access, which is the only privileged direction.
    Raise,
    /// The request drops I/O access, which an unprivileged task may always do.
    Lower,
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
    /// `SYSCALL_DEFINE1(iopl, ...)` in arch/x86/kernel/ioport.c returns
    /// `-EINVAL` above level 3, then returns 0 immediately when the level is
    /// unchanged, and only reaches the `capable(CAP_SYS_RAWIO)` /
    /// `security_locked_down(LOCKDOWN_IOPORT)` pair when `level > old`.
    pub const fn transition(self, current_level: u8) -> IoplTransition {
        if self.level == current_level {
            IoplTransition::Unchanged
        } else if self.level > current_level {
            IoplTransition::Raise
        } else {
            IoplTransition::Lower
        }
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
    fn iopl_only_charges_privilege_when_raising_the_level() {
        let plan = |level| IoplPlan::new(level).unwrap();
        assert_eq!(
            plan(0).transition(0),
            IoplTransition::Unchanged,
            "Linux returns before any check when the level is unchanged"
        );
        assert_eq!(plan(3).transition(3), IoplTransition::Unchanged);
        assert_eq!(plan(0).transition(3), IoplTransition::Lower);
        assert_eq!(plan(2).transition(3), IoplTransition::Lower);
        assert_eq!(plan(1).transition(0), IoplTransition::Raise);
        assert_eq!(plan(3).transition(2), IoplTransition::Raise);
        assert_eq!(IoplPlan::new(4), Err(ArchPolicyError::InvalidIopl));
    }

    #[test]
    fn arch_set_segment_base_rejects_addresses_at_or_above_task_size_max() {
        // Linux's x86_64 TASK_SIZE_MAX is (1 << 47) - PAGE_SIZE.
        const TASK_SIZE_MAX: u64 = (1 << 47) - 4096;
        assert!(arch_segment_base_permitted(0, TASK_SIZE_MAX));
        assert!(arch_segment_base_permitted(
            TASK_SIZE_MAX - 1,
            TASK_SIZE_MAX
        ));
        assert!(!arch_segment_base_permitted(TASK_SIZE_MAX, TASK_SIZE_MAX));
        assert!(!arch_segment_base_permitted(u64::MAX, TASK_SIZE_MAX));
    }

    #[test]
    fn xcomp_request_distinguishes_einval_from_eopnotsupp() {
        let no_amx = XFEATURE_MASK_FPSSE | (1 << 9);
        assert_eq!(
            xcomp_request_admission(XFEATURE_MAX, no_amx),
            Err(XcompRequestRejection::UnknownComponent)
        );
        assert_eq!(
            xcomp_request_admission(u64::MAX, no_amx),
            Err(XcompRequestRejection::UnknownComponent)
        );
        // TILECFG has no requestable facility mask, so Linux answers
        // -EOPNOTSUPP rather than -EINVAL.
        assert_eq!(
            xcomp_request_admission(XFEATURE_XTILE_DATA - 1, no_amx),
            Err(XcompRequestRejection::UnsupportedFacility)
        );
        assert_eq!(
            xcomp_request_admission(0, no_amx),
            Err(XcompRequestRejection::UnsupportedFacility)
        );
        assert_eq!(
            xcomp_request_admission(XFEATURE_XTILE_DATA, no_amx),
            Err(XcompRequestRejection::UnsupportedFacility)
        );
        assert_eq!(
            xcomp_request_admission(XFEATURE_XTILE_DATA, no_amx | XFEATURE_MASK_XTILE_DATA),
            Ok(XFEATURE_MASK_XTILE_DATA)
        );
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
