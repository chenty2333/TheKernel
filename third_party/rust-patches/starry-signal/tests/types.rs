use starry_signal::{SignalInfo, SignalSet, Signo};

#[test]
fn signalset_add_remove_has_is_empty() {
    let mut set = SignalSet::default();
    assert!(set.is_empty());

    assert!(set.add(Signo::SIGINT));
    assert!(!set.is_empty());
    assert!(set.has(Signo::SIGINT));

    assert!(!set.add(Signo::SIGINT));

    assert!(set.remove(Signo::SIGINT));
    assert!(!set.has(Signo::SIGINT));
    assert!(set.is_empty());

    assert!(!set.remove(Signo::SIGINT));
}

#[test]
fn signalset_dequeue() {
    let mut set = SignalSet::default();
    assert!(set.add(Signo::SIGTERM));
    assert!(set.add(Signo::SIGINT));
    assert!(set.add(Signo::SIGHUP));

    let mut mask = SignalSet::default();
    mask.add(Signo::SIGHUP);
    mask.add(Signo::SIGINT);
    mask.add(Signo::SIGTERM);

    assert_eq!(set.dequeue(&mask).unwrap(), Signo::SIGHUP);
    assert_eq!(set.dequeue(&mask).unwrap(), Signo::SIGINT);
    assert_eq!(set.dequeue(&mask).unwrap(), Signo::SIGTERM);
    assert!(set.dequeue(&mask).is_none());

    assert!(set.add(Signo::SIGHUP));
    assert!(set.add(Signo::SIGINT));

    let mut mask2 = SignalSet::default();
    mask2.add(Signo::SIGINT);

    assert_eq!(set.dequeue(&mask2).unwrap(), Signo::SIGINT);
    assert!(set.has(Signo::SIGHUP));
}

#[test]
fn signalset_bounds() {
    let mut set = SignalSet::default();
    assert!(set.add(Signo::SIGHUP));
    assert!(set.add(Signo::SIGRT32));
    assert!(set.has(Signo::SIGHUP));
    assert!(set.has(Signo::SIGRT32));
    assert!(set.remove(Signo::SIGHUP));
    assert!(set.remove(Signo::SIGRT32));
}

#[test]
fn signalinfo_new_kernel() {
    let si = SignalInfo::new_kernel(Signo::SIGTERM);
    assert_eq!(si.signo(), Signo::SIGTERM);
    assert_eq!(si.code(), 128);
    assert_eq!(si.errno(), 0);
}

#[test]
fn signalinfo_new_user() {
    let si = SignalInfo::new_user(Signo::SIGINT, 9, 9);
    assert_eq!(si.signo(), Signo::SIGINT);
    assert_eq!(si.code(), 9);
    assert_eq!(
        unsafe {
            si.0.__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._sigchld
                ._pid
        },
        9
    );
    assert_eq!(si.errno(), 0);
}

#[test]
fn signalinfo_fault_preserves_code_and_address() {
    let si = SignalInfo::new_fault(Signo::SIGSEGV, 2, 0x1234_5000);
    assert_eq!(si.signo(), Signo::SIGSEGV);
    assert_eq!(si.code(), 2);
    assert_eq!(si.fault_address(), 0x1234_5000);
    assert_eq!(si.errno(), 0);
}

#[test]
fn signalinfo_sigsys_preserves_seccomp_trap_fields() {
    const AUDIT_ARCH_RISCV64: u32 = 0xc000_00f3;

    let si = SignalInfo::new_sigsys(0x5a5a, 0x1234_5678_9abc, -17, AUDIT_ARCH_RISCV64);

    assert_eq!(si.signo(), Signo::SIGSYS);
    assert_eq!(si.code(), 1); // SYS_SECCOMP
    assert_eq!(si.errno(), 0x5a5a);
    assert_eq!(si.sigsys_call_address(), 0x1234_5678_9abc);
    assert_eq!(si.sigsys_syscall(), -17);
    assert_eq!(si.sigsys_arch(), AUDIT_ARCH_RISCV64);

    let raw = unsafe { si.0.__bindgen_anon_1.__bindgen_anon_1._sifields._sigsys };
    assert_eq!(raw._call_addr as usize, 0x1234_5678_9abc);
    assert_eq!(raw._syscall, -17);
    assert_eq!(raw._arch, AUDIT_ARCH_RISCV64);
}

#[test]
fn signalinfo_sigsys_matches_linux_64_bit_abi_layout() {
    use core::mem::{align_of, size_of};

    let si = SignalInfo::new_sigsys(0x1122_3344, 0x1234_5678, 0x5566_7788, 0x99aa_bbcc);
    let base = core::ptr::addr_of!(si.0) as usize;
    let common = unsafe { &si.0.__bindgen_anon_1.__bindgen_anon_1 };
    let sigsys = unsafe { &common._sifields._sigsys };

    assert_eq!(size_of::<SignalInfo>(), 128);
    assert_eq!(align_of::<SignalInfo>(), align_of::<usize>());
    assert_eq!(core::ptr::addr_of!(common.si_signo) as usize - base, 0);
    assert_eq!(core::ptr::addr_of!(common.si_errno) as usize - base, 4);
    assert_eq!(core::ptr::addr_of!(common.si_code) as usize - base, 8);
    assert_eq!(core::ptr::addr_of!(sigsys._call_addr) as usize - base, 16);
    assert_eq!(core::ptr::addr_of!(sigsys._syscall) as usize - base, 24);
    assert_eq!(core::ptr::addr_of!(sigsys._arch) as usize - base, 28);
}
