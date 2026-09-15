use alloc::sync::Arc;

use axerrno::AxResult;
use axtask::current;
use linux_raw_sys::general::{CAP_SYS_PTRACE, O_CLOEXEC, O_NONBLOCK, O_RDONLY};
use tk_linux_mm::{
    UFFD_O_CLOEXEC, UFFD_O_NONBLOCK, UffdAdmissionFacts, UffdCreateFlags, admit_raw_creation,
};

use crate::{
    file::{FileDescription, reserve_fd, userfaultfd::UserfaultFile},
    mm::{AddrSpace, uffd_policy_error, unprivileged_userfaultfd},
    task::AsThread,
};

const _: () = {
    assert!(UFFD_O_NONBLOCK == O_NONBLOCK);
    assert!(UFFD_O_CLOEXEC == O_CLOEXEC);
};

const fn userfaultfd_status_flags(flags: UffdCreateFlags) -> u32 {
    O_RDONLY | if flags.nonblocking() { O_NONBLOCK } else { 0 }
}

/// Facts for the Linux `userfaultfd_syscall_allowed()` gate.
///
/// `capable(CAP_SYS_PTRACE)` is evaluated in the initial user namespace, which
/// is what [`AsThread::has_effective_capability`] implements.
///
/// The final field is this kernel's honest answer about kernel-mode fault
/// delivery: `UFFD_USER_MODE_ONLY` is the *only* thing a context without it
/// withholds, and this kernel never delivers a fault taken in kernel mode to a
/// userfaultfd handler. Such faults are raised by kernel-originated user
/// copies, which must stay non-sleeping (see `mm/fault.rs`), so admitting the
/// privileged half of the gate would hand back a context that silently fails
/// to intercept exactly the faults the caller asked for. Linux's own answer
/// for a caller it refuses is `-EPERM`, so the refusal keeps the ABI shape and
/// only narrows which privileged callers succeed.
fn uffd_admission_facts() -> UffdAdmissionFacts {
    let thread = current();
    let thread = thread.as_thread();
    UffdAdmissionFacts {
        capable_sys_ptrace: thread.has_effective_capability(CAP_SYS_PTRACE),
        sysctl_unprivileged_userfaultfd: unprivileged_userfaultfd(),
        kernel_mode_fault_delivery: false,
    }
}

fn checked_userfaultfd_flags_with(
    facts: UffdAdmissionFacts,
    flags: i32,
) -> AxResult<UffdCreateFlags> {
    let flags = flags as u32;
    // The permission gate runs first, on the raw word, before the flag
    // namespace is validated: an unprivileged caller whose creation is denied
    // sees EPERM even when an unknown bit is also present, while
    // USER_MODE_ONLY combined with an unknown bit passes the gate and then
    // fails with EINVAL.
    admit_raw_creation(flags, facts).map_err(uffd_policy_error)?;
    UffdCreateFlags::from_bits(flags).map_err(uffd_policy_error)
}

fn checked_userfaultfd_flags(flags: i32) -> AxResult<UffdCreateFlags> {
    checked_userfaultfd_flags_with(uffd_admission_facts(), flags)
}

fn prepare_userfaultfd_description(
    aspace: Arc<axsync::Mutex<AddrSpace>>,
    flags: UffdCreateFlags,
) -> AxResult<Arc<FileDescription>> {
    let file = UserfaultFile::try_new(aspace, flags)?;
    FileDescription::new_with_flags(file, userfaultfd_status_flags(flags))
}

pub fn sys_userfaultfd(raw_flags: i32) -> AxResult<isize> {
    let flags = checked_userfaultfd_flags(raw_flags)?;

    // Reserve the process-visible name before attaching a handler to the
    // address space. EMFILE therefore has no userfaultfd/MM side effect. Every
    // later fallible owner is unpublished and rolls back through Drop.
    let reservation = reserve_fd(flags.close_on_exec())?;
    let aspace = current().as_thread().proc_data.aspace();
    let description = prepare_userfaultfd_description(aspace, flags)?;
    let publication = reservation.prepare_publication(description)?;

    // Descriptor accounting and exact-table admission are complete. This is
    // the only visibility transition and is infallible.
    Ok(publication.commit() as isize)
}

#[cfg(test)]
mod tests {
    use axerrno::AxError;
    use linux_raw_sys::general::{O_ACCMODE, UFFD_USER_MODE_ONLY};

    use super::*;

    fn facts(ptrace: bool, sysctl: bool, delivery: bool) -> UffdAdmissionFacts {
        UffdAdmissionFacts {
            capable_sys_ptrace: ptrace,
            sysctl_unprivileged_userfaultfd: sysctl,
            kernel_mode_fault_delivery: delivery,
        }
    }

    #[test]
    fn status_flags_match_linux_read_only_ofd() {
        let blocking = UffdCreateFlags::from_bits(UFFD_USER_MODE_ONLY).unwrap();
        assert_eq!(userfaultfd_status_flags(blocking) & O_ACCMODE, O_RDONLY);
        assert_eq!(userfaultfd_status_flags(blocking) & O_NONBLOCK, 0);

        let nonblocking =
            UffdCreateFlags::from_bits(UFFD_USER_MODE_ONLY | UFFD_O_NONBLOCK).unwrap();
        assert_eq!(userfaultfd_status_flags(nonblocking), O_RDONLY | O_NONBLOCK);
    }

    #[test]
    fn profile_gate_distinguishes_permission_from_unknown_bits() {
        // Unprivileged caller, default `vm.unprivileged_userfaultfd=0`: the
        // permission gate refuses before the unknown bit is ever examined.
        let unprivileged = facts(false, false, true);
        assert_eq!(
            checked_userfaultfd_flags_with(unprivileged, 0),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            checked_userfaultfd_flags_with(unprivileged, 2),
            Err(AxError::OperationNotPermitted)
        );
        // UFFD_USER_MODE_ONLY passes the gate, so the unknown bit is EINVAL.
        assert_eq!(
            checked_userfaultfd_flags_with(unprivileged, (UFFD_USER_MODE_ONLY | 2) as i32),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn privileged_half_is_refused_without_kernel_mode_fault_delivery() {
        // CAP_SYS_PTRACE and the sysctl both pass Linux's gate, but this
        // kernel cannot deliver the kernel-mode faults they would buy, so the
        // result stays EPERM rather than an under-delivering context.
        assert_eq!(
            checked_userfaultfd_flags_with(facts(true, true, false), 0),
            Err(AxError::OperationNotPermitted)
        );
        // A hypothetical kernel that could deliver them admits the caller.
        assert_eq!(
            checked_userfaultfd_flags_with(facts(false, true, true), 0)
                .unwrap()
                .user_mode_only(),
            false
        );
    }

    #[test]
    fn user_mode_only_creation_succeeds_for_every_caller() {
        for caller in [facts(false, false, false), facts(true, true, true)] {
            let flags = checked_userfaultfd_flags_with(
                caller,
                (UFFD_USER_MODE_ONLY | UFFD_O_NONBLOCK | UFFD_O_CLOEXEC) as i32,
            )
            .unwrap();
            assert!(flags.user_mode_only());
            assert!(flags.nonblocking());
            assert!(flags.close_on_exec());
        }
    }
}
