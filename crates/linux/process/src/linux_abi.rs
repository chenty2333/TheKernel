//! Pure clone, rusage and pidfd ABI policy.
#![allow(missing_docs)]
use crate::Pid;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessAbiError {
    InvalidFlags,
    InvalidExitSignal,
    InvalidStack,
    NonzeroTail,
    AddressOverflow,
    InvalidPidfdFlags,
    InvalidSize,
    TooLarge,
    InvalidSetTid,
    PermissionDenied,
    InvalidCgroup,
    InvalidRusageSelector,
}
pub mod clone_flags {
    pub const VM: u64 = 0x100;
    pub const FS: u64 = 0x200;
    pub const FILES: u64 = 0x400;
    pub const SIGHAND: u64 = 0x800;
    pub const PIDFD: u64 = 0x1000;
    pub const PTRACE: u64 = 0x2000;
    pub const VFORK: u64 = 0x4000;
    pub const PARENT: u64 = 0x8000;
    pub const THREAD: u64 = 0x10000;
    pub const PARENT_SETTID: u64 = 0x100000;
    pub const CHILD_CLEARTID: u64 = 0x200000;
    pub const CHILD_SETTID: u64 = 0x1000000;
    pub const NEWCGROUP: u64 = 0x2000000;
    pub const NEWNS: u64 = 0x0002_0000;
    pub const NEWUTS: u64 = 0x4000000;
    pub const NEWIPC: u64 = 0x8000000;
    pub const NEWUSER: u64 = 0x10000000;
    pub const NEWPID: u64 = 0x20000000;
    pub const NEWNET: u64 = 0x40000000;
    pub const IO: u64 = 0x80000000;
    pub const SYSVSEM: u64 = 0x40000;
    pub const SETTLS: u64 = 0x80000;
    pub const UNTRACED: u64 = 0x800000;
    pub const CLEAR_SIGHAND: u64 = 0x1_0000_0000;
    pub const INTO_CGROUP: u64 = 0x2_0000_0000;
    pub const DETACHED: u64 = 0x0040_0000;
    pub const KNOWN: u64 = VM
        | FS
        | FILES
        | SIGHAND
        | PIDFD
        | VFORK
        | THREAD
        | PARENT_SETTID
        | CHILD_CLEARTID
        | CHILD_SETTID
        | PTRACE
        | PARENT
        | NEWCGROUP
        | NEWNS
        | NEWUTS
        | NEWIPC
        | NEWUSER
        | NEWPID
        | NEWNET
        | IO
        | SYSVSEM
        | SETTLS
        | UNTRACED
        | CLEAR_SIGHAND
        | INTO_CGROUP
        | DETACHED;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClonePlan {
    pub flags: u64,
    pub exit_signal: u8,
    pub stack_top: u64,
    /// Explicit `clone3.stack_size`; zero for the legacy `clone(2)` ABI.
    pub stack_size: u64,
    pub tls: u64,
    pub parent_tid: u64,
    pub child_tid: u64,
    pub pidfd: u64,
}
impl ClonePlan {
    pub const fn from_clone(
        raw_flags: u64,
        stack_top: u64,
        tls: u64,
        parent_tid: u64,
        child_tid: u64,
    ) -> Result<Self, ProcessAbiError> {
        let flags = raw_flags & !255;
        // clone(2) overloads its parent_tid argument as the pidfd destination.
        // Unlike clone3, the two destinations cannot be supplied separately.
        if flags & (clone_flags::PIDFD | clone_flags::PARENT_SETTID)
            == (clone_flags::PIDFD | clone_flags::PARENT_SETTID)
        {
            return Err(ProcessAbiError::InvalidFlags);
        }
        Self::new(
            flags,
            raw_flags as u8,
            stack_top,
            0,
            tls,
            parent_tid,
            child_tid,
            0,
        )
    }
    pub const fn new(
        flags: u64,
        exit_signal: u8,
        stack_top: u64,
        stack_size: u64,
        tls: u64,
        parent_tid: u64,
        child_tid: u64,
        pidfd: u64,
    ) -> Result<Self, ProcessAbiError> {
        if flags & !clone_flags::KNOWN != 0 {
            return Err(ProcessAbiError::InvalidFlags);
        }
        if exit_signal > 64 || exit_signal != 0 && flags & clone_flags::THREAD != 0 {
            return Err(ProcessAbiError::InvalidExitSignal);
        }
        Ok(Self {
            flags,
            exit_signal,
            stack_top,
            stack_size,
            tls,
            parent_tid,
            child_tid,
            pidfd,
        })
    }
}
/// The x86-64 Linux `struct clone_args` wire layout passed to `clone3`.
///
/// Short, supported prefixes are zero-extended before decoding.  A caller
/// must separately copy any extension bytes and pass them to
/// [`Self::normalize`], which rejects non-zero extensions.
#[repr(C)]
#[derive(bytemuck::AnyBitPattern, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Clone3Args {
    pub flags: u64,
    pub pidfd: u64,
    pub child_tid: u64,
    pub parent_tid: u64,
    pub exit_signal: u64,
    pub stack: u64,
    pub stack_size: u64,
    pub tls: u64,
    pub set_tid: u64,
    pub set_tid_size: u64,
    pub cgroup: u64,
}

/// Bounded, copied `clone3.set_tid` vector. Values are innermost-to-outermost
/// namespace PIDs, and the embedding kernel supplies namespace authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetTidPlan {
    address: u64,
    count: u8,
}

impl SetTidPlan {
    pub const MAX_ENTRIES: usize = 32;

    pub const fn new(address: u64, count: u64) -> Result<Self, ProcessAbiError> {
        // Linux ignores `set_tid` when its count is zero, so an otherwise
        // unused non-null pointer must not require a valid user mapping.
        if count == 0 {
            return Ok(Self {
                address: 0,
                count: 0,
            });
        }
        if address == 0 || count == 0 || count > Self::MAX_ENTRIES as u64 {
            return Err(ProcessAbiError::InvalidSetTid);
        }
        Ok(Self {
            address,
            count: count as u8,
        })
    }

    pub const fn address(self) -> u64 {
        self.address
    }
    pub const fn count(self) -> usize {
        self.count as usize
    }

    /// Check the copied PID vector before namespace reservation/publication.
    ///
    /// Authorization is intentionally left to the embedding kernel: every
    /// requested element is authorized against a different PID namespace.
    pub fn validate_values(self, values: &[Pid]) -> Result<(), ProcessAbiError> {
        if values.len() != self.count() || values.contains(&0) {
            return Err(ProcessAbiError::InvalidSetTid);
        }
        Ok(())
    }
}

/// Decoded `clone3` policy, with usercopy-dependent requests kept typed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Clone3Plan {
    pub clone: ClonePlan,
    /// Base of the explicit user stack.  The embedding kernel validates this
    /// normalized range against its own virtual-address layout.
    pub stack_base: u64,
    pub set_tid: SetTidPlan,
    pub cgroup_fd: Option<i32>,
}
impl Clone3Args {
    pub const MIN_SIZE: usize = 64;
    pub const KNOWN_SIZE: usize = core::mem::size_of::<Self>();
    pub const MAX_SIZE: usize = 4096;

    /// Returns the number of wire bytes an embedding kernel must copy before
    /// decoding the known `clone_args` fields.
    pub const fn known_prefix_size(supplied_size: usize) -> Result<usize, ProcessAbiError> {
        if supplied_size < Self::MIN_SIZE {
            return Err(ProcessAbiError::InvalidSize);
        }
        if supplied_size > Self::MAX_SIZE {
            return Err(ProcessAbiError::TooLarge);
        }
        Ok(if supplied_size < Self::KNOWN_SIZE {
            supplied_size
        } else {
            Self::KNOWN_SIZE
        })
    }

    /// Decodes a checked, zero-extended known wire prefix.
    pub fn decode_prefix(supplied_size: usize, prefix: &[u8]) -> Result<Self, ProcessAbiError> {
        let known_size = Self::known_prefix_size(supplied_size)?;
        if prefix.len() != known_size {
            return Err(ProcessAbiError::InvalidSize);
        }
        let mut bytes = [0_u8; Self::KNOWN_SIZE];
        bytes[..known_size].copy_from_slice(prefix);
        bytemuck::try_pod_read_unaligned(&bytes).map_err(|_| ProcessAbiError::InvalidSize)
    }

    /// Checks extension bytes beyond [`Self::KNOWN_SIZE`].
    pub fn validate_tail(tail: &[u8]) -> Result<(), ProcessAbiError> {
        if tail.iter().any(|byte| *byte != 0) {
            Err(ProcessAbiError::NonzeroTail)
        } else {
            Ok(())
        }
    }

    pub fn normalize(
        self,
        supplied_size: usize,
        tail: &[u8],
    ) -> Result<Clone3Plan, ProcessAbiError> {
        Self::known_prefix_size(supplied_size)?;
        let expected_tail_size = supplied_size.saturating_sub(Self::KNOWN_SIZE);
        if tail.len() != expected_tail_size {
            return Err(ProcessAbiError::InvalidSize);
        }
        Self::validate_tail(tail)?;
        if (self.stack == 0) != (self.stack_size == 0) {
            return Err(ProcessAbiError::InvalidStack);
        }
        if self.exit_signal > 255 {
            return Err(ProcessAbiError::InvalidExitSignal);
        }
        if self.exit_signal != 0 && self.flags & (clone_flags::THREAD | clone_flags::PARENT) != 0 {
            return Err(ProcessAbiError::InvalidExitSignal);
        }
        let top = self
            .stack
            .checked_add(self.stack_size)
            .ok_or(ProcessAbiError::AddressOverflow)?;
        let clone = ClonePlan::new(
            self.flags,
            self.exit_signal as u8,
            top,
            self.stack_size,
            self.tls,
            self.parent_tid,
            self.child_tid,
            self.pidfd,
        )?;
        if self.flags & (clone_flags::PIDFD | clone_flags::PARENT_SETTID)
            == (clone_flags::PIDFD | clone_flags::PARENT_SETTID)
            && self.pidfd == self.parent_tid
        {
            return Err(ProcessAbiError::InvalidFlags);
        }
        let set_tid = SetTidPlan::new(self.set_tid, self.set_tid_size)?;
        let cgroup_fd = if self.flags & clone_flags::INTO_CGROUP != 0 {
            if supplied_size < Self::KNOWN_SIZE || self.cgroup > i32::MAX as u64 {
                return Err(ProcessAbiError::InvalidCgroup);
            }
            Some(self.cgroup as i32)
        } else {
            None
        };
        Ok(Clone3Plan {
            clone,
            stack_base: self.stack,
            set_tid,
            cgroup_fd,
        })
    }
}

/// Valid x86-64 Linux `getrusage` selectors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RusageSelector {
    SelfUsage,
    Children,
    Thread,
}
impl RusageSelector {
    pub const fn decode(value: i32) -> Result<Self, ProcessAbiError> {
        match value {
            0 => Ok(Self::SelfUsage),
            -1 => Ok(Self::Children),
            1 => Ok(Self::Thread),
            _ => Err(ProcessAbiError::InvalidRusageSelector),
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TimeVal {
    pub seconds: i64,
    pub microseconds: i64,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UsageSnapshot {
    pub user_ns: u64,
    pub system_ns: u64,
    pub max_rss_bytes: u64,
    pub minor_faults: u64,
    pub major_faults: u64,
    pub inblock: u64,
    pub oublock: u64,
    pub voluntary_switches: u64,
    pub involuntary_switches: u64,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rusage {
    pub utime: TimeVal,
    pub stime: TimeVal,
    pub maxrss_kib: i64,
    pub ixrss: i64,
    pub idrss: i64,
    pub isrss: i64,
    pub minflt: i64,
    pub majflt: i64,
    pub nswap: i64,
    pub inblock: i64,
    pub oublock: i64,
    pub msgsnd: i64,
    pub msgrcv: i64,
    pub nsignals: i64,
    pub nvcsw: i64,
    pub nivcsw: i64,
}
impl UsageSnapshot {
    pub fn project(self) -> Rusage {
        Rusage {
            utime: TimeVal {
                seconds: (self.user_ns / 1_000_000_000) as i64,
                microseconds: ((self.user_ns % 1_000_000_000) / 1000) as i64,
            },
            stime: TimeVal {
                seconds: (self.system_ns / 1_000_000_000) as i64,
                microseconds: ((self.system_ns % 1_000_000_000) / 1000) as i64,
            },
            maxrss_kib: (self.max_rss_bytes / 1024).min(i64::MAX as u64) as i64,
            ixrss: 0,
            idrss: 0,
            isrss: 0,
            minflt: self.minor_faults.min(i64::MAX as u64) as i64,
            majflt: self.major_faults.min(i64::MAX as u64) as i64,
            nswap: 0,
            inblock: self.inblock.min(i64::MAX as u64) as i64,
            oublock: self.oublock.min(i64::MAX as u64) as i64,
            msgsnd: 0,
            msgrcv: 0,
            nsignals: 0,
            nvcsw: self.voluntary_switches.min(i64::MAX as u64) as i64,
            nivcsw: self.involuntary_switches.min(i64::MAX as u64) as i64,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PidfdPlan {
    pub target: Pid,
    pub thread: bool,
    pub nonblocking: bool,
}
impl PidfdPlan {
    pub const NONBLOCK: u32 = 2048;
    pub const THREAD: u32 = 128;
    pub const FLAGS: u32 = Self::NONBLOCK | Self::THREAD;

    pub const fn open(target: Pid, flags: u32) -> Result<Self, ProcessAbiError> {
        if flags & !Self::FLAGS != 0 {
            Err(ProcessAbiError::InvalidPidfdFlags)
        } else {
            Ok(Self {
                target,
                thread: flags & Self::THREAD != 0,
                nonblocking: flags & Self::NONBLOCK != 0,
            })
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edges() {
        assert_eq!(
            Clone3Args {
                stack: 1,
                ..Default::default()
            }
            .normalize(64, &[]),
            Err(ProcessAbiError::InvalidStack)
        );
        assert_eq!(
            Clone3Args::default().normalize(Clone3Args::KNOWN_SIZE + 1, &[1]),
            Err(ProcessAbiError::NonzeroTail)
        );
        assert_eq!(
            UsageSnapshot {
                max_rss_bytes: 2048,
                ..Default::default()
            }
            .project()
            .maxrss_kib,
            2
        );
    }

    #[test]
    fn clone3_size_is_checked_before_tail_and_tail_before_fields() {
        let invalid_stack = Clone3Args {
            stack: 1,
            ..Default::default()
        };
        assert_eq!(
            invalid_stack.normalize(Clone3Args::MIN_SIZE - 1, &[1]),
            Err(ProcessAbiError::InvalidSize)
        );
        assert_eq!(
            invalid_stack.normalize(Clone3Args::KNOWN_SIZE + 1, &[1]),
            Err(ProcessAbiError::NonzeroTail)
        );
        assert_eq!(
            Clone3Args::default().normalize(Clone3Args::MAX_SIZE + 1, &[]),
            Err(ProcessAbiError::TooLarge)
        );
        assert_eq!(
            Clone3Args::default().normalize(Clone3Args::KNOWN_SIZE + 1, &[]),
            Err(ProcessAbiError::InvalidSize)
        );
    }

    #[test]
    fn clone3_rejects_exit_signal_for_thread_or_parent() {
        for flag in [clone_flags::THREAD, clone_flags::PARENT] {
            assert_eq!(
                Clone3Args {
                    flags: flag,
                    exit_signal: 1,
                    ..Default::default()
                }
                .normalize(Clone3Args::KNOWN_SIZE, &[]),
                Err(ProcessAbiError::InvalidExitSignal)
            );
        }
    }

    #[test]
    fn clone_normalizes_legacy_flags_and_rejects_its_output_slot_collision() {
        let plan =
            ClonePlan::from_clone(clone_flags::VM | 17, 0x4000, 0x5000, 0x6000, 0x7000).unwrap();
        assert_eq!(plan.flags, clone_flags::VM);
        assert_eq!(plan.exit_signal, 17);
        assert_eq!(plan.stack_top, 0x4000);
        assert_eq!(plan.tls, 0x5000);
        assert_eq!(plan.parent_tid, 0x6000);
        assert_eq!(plan.child_tid, 0x7000);
        assert_eq!(
            ClonePlan::from_clone(clone_flags::PIDFD | clone_flags::PARENT_SETTID, 0, 0, 0, 0,),
            Err(ProcessAbiError::InvalidFlags)
        );
    }

    #[test]
    fn clone3_decodes_short_prefix_by_zero_extending_the_wire_layout() {
        let mut prefix = [0_u8; Clone3Args::MIN_SIZE];
        prefix[..8].copy_from_slice(&clone_flags::VM.to_ne_bytes());
        let decoded = Clone3Args::decode_prefix(Clone3Args::MIN_SIZE, &prefix).unwrap();
        assert_eq!(decoded.flags, clone_flags::VM);
        assert_eq!(decoded.set_tid, 0);
        assert_eq!(decoded.cgroup, 0);
        assert_eq!(
            Clone3Args::decode_prefix(Clone3Args::MIN_SIZE, &prefix[..63]),
            Err(ProcessAbiError::InvalidSize)
        );
    }

    #[test]
    fn clone3_set_tid_requires_a_complete_nonzero_vector() {
        let args = Clone3Args {
            set_tid: 0x1000,
            set_tid_size: 2,
            ..Default::default()
        };
        let plan = args.normalize(Clone3Args::KNOWN_SIZE, &[]).unwrap();
        assert_eq!(
            plan.set_tid.validate_values(&[7]),
            Err(ProcessAbiError::InvalidSetTid)
        );
        assert_eq!(
            plan.set_tid.validate_values(&[7, 0]),
            Err(ProcessAbiError::InvalidSetTid)
        );
        assert_eq!(plan.set_tid.validate_values(&[7, 8]), Ok(()));
    }

    #[test]
    fn clone3_ignores_set_tid_pointer_for_a_zero_length_request() {
        let plan = Clone3Args {
            set_tid: 0xdead_beef,
            set_tid_size: 0,
            ..Default::default()
        }
        .normalize(Clone3Args::KNOWN_SIZE, &[])
        .unwrap();
        assert_eq!(plan.set_tid.count(), 0);
        assert_eq!(plan.set_tid.address(), 0);
        assert_eq!(plan.set_tid.validate_values(&[]), Ok(()));
    }
}
