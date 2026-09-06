use core::mem::{align_of, size_of};

use linux_raw_sys::general::{SI_MESGQ, SI_QUEUE, SI_SIGIO, SI_TIMER, siginfo_t};
use thekernel_linux_signal::{
    SignalInfo, SignalPollPayload, SignalRtPayload, SignalSet, SignalTimerPayload, Signo,
};

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
    let si = SignalInfo::new_user(Signo::SIGINT, 9, 9, 1000);
    assert_eq!(si.signo(), Signo::SIGINT);
    assert_eq!(si.code(), 9);
    assert_eq!(si.pid(), 9);
    assert_eq!(si.uid(), 1000);
    assert_eq!(
        // SAFETY: new_user initializes the complete siginfo_t record and the
        // test reads the integer PID member written by that constructor.
        unsafe {
            si.as_raw()
                .__bindgen_anon_1
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
fn signalinfo_fault_roundtrip() {
    let si = SignalInfo::new_fault(Signo::SIGSEGV, 2, 0x1234_5000);
    assert_eq!(si.signo(), Signo::SIGSEGV);
    assert_eq!(si.code(), 2);
    assert_eq!(si.fault_address(), 0x1234_5000);
    assert_eq!(si.errno(), 0);
}

#[test]
fn signalinfo_sigsys_roundtrip() {
    const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

    let si = SignalInfo::new_sigsys(0x5a5a, 0x1234_5678_9abc, -17, AUDIT_ARCH_X86_64);
    assert_eq!(si.signo(), Signo::SIGSYS);
    assert_eq!(si.code(), 1); // SYS_SECCOMP
    assert_eq!(si.errno(), 0x5a5a);
    assert_eq!(si.sigsys_call_address(), 0x1234_5678_9abc);
    assert_eq!(si.sigsys_syscall(), -17);
    assert_eq!(si.sigsys_arch(), AUDIT_ARCH_X86_64);
}

#[test]
fn signalinfo_timer_payload_roundtrip() {
    let payload = SignalTimerPayload::new(-13, 17, 0xfeed_beef_dead_beef, -9);
    let mut si = SignalInfo::new_timer(Signo::SIGRT1, payload);

    assert_eq!(si.signo(), Signo::SIGRT1);
    assert_eq!(si.code(), SI_TIMER);
    assert_eq!(si.timer_payload(), payload);

    let replacement = SignalTimerPayload::new(23, -4, 0x1234_5678_9abc_def0, 31);
    si.set_timer_payload(replacement);
    assert_eq!(si.code(), SI_TIMER);
    assert_eq!(si.timer_payload(), replacement);

    // Both sigval interpretations observe the same low-level bits.
    let raw = si.as_raw();
    // SAFETY: `new_timer` and `set_timer_payload` initialize the complete
    // timer payload before this test reads its union members.
    unsafe {
        let timer = raw.__bindgen_anon_1.__bindgen_anon_1._sifields._timer;
        assert_eq!(timer._sigval.sival_ptr as usize, replacement.value);
        assert_eq!(timer._sigval.sival_int, replacement.value as i32);
    }
}

#[test]
fn signalinfo_rt_payload_roundtrip() {
    let payload = SignalRtPayload::new(-19, 4242, 0xfeed_beef_dead_beef);
    let mut si = SignalInfo::new_rt(Signo::SIGRT2, SI_MESGQ, payload);

    assert_eq!(si.signo(), Signo::SIGRT2);
    assert_eq!(si.code(), SI_MESGQ);
    assert_eq!(si.rt_payload(), payload);

    let replacement = SignalRtPayload::new(7, 99, 0x1234_5678_9abc_def0);
    si.set_code(SI_QUEUE);
    si.set_rt_payload(replacement);
    assert_eq!(si.code(), SI_QUEUE);
    assert_eq!(si.rt_payload(), replacement);
}

#[test]
fn signalinfo_poll_payload_roundtrip() {
    let payload = SignalPollPayload::new(-0x1234, -9);
    let mut si = SignalInfo::new_poll(Signo::SIGIO, payload);

    assert_eq!(si.signo(), Signo::SIGIO);
    assert_eq!(si.code(), SI_SIGIO);
    assert_eq!(si.poll_payload(), payload);

    let replacement = SignalPollPayload::new(0xfeed_beef, 17);
    si.set_poll_payload(replacement);
    assert_eq!(si.code(), SI_SIGIO);
    assert_eq!(si.poll_payload(), replacement);
}

#[test]
fn signalinfo_matches_linux_x86_64_layout() {
    let si = SignalInfo::new_sigsys(1, 2, 3, 4);
    let raw = si.as_raw();
    let base = raw as *const siginfo_t as usize;
    // SAFETY: the constructor selected and initialized the common siginfo
    // header and the `_sigsys` payload inspected by this layout test.
    let common = unsafe { &raw.__bindgen_anon_1.__bindgen_anon_1 };
    // SAFETY: `new_sigsys` initialized the `_sigsys` union arm.
    let sigsys = unsafe { &common._sifields._sigsys };

    assert_eq!(size_of::<SignalInfo>(), 128);
    assert_eq!(align_of::<SignalInfo>(), align_of::<siginfo_t>());
    assert_eq!(core::ptr::addr_of!(common.si_signo) as usize - base, 0);
    assert_eq!(core::ptr::addr_of!(common.si_errno) as usize - base, 4);
    assert_eq!(core::ptr::addr_of!(common.si_code) as usize - base, 8);
    assert_eq!(core::ptr::addr_of!(sigsys._call_addr) as usize - base, 16);
    assert_eq!(core::ptr::addr_of!(sigsys._syscall) as usize - base, 24);
    assert_eq!(core::ptr::addr_of!(sigsys._arch) as usize - base, 28);

    let timer = SignalInfo::new_timer(Signo::SIGRT1, SignalTimerPayload::new(1, 2, 3, 4));
    let timer_raw = timer.as_raw();
    let timer_base = timer_raw as *const siginfo_t as usize;
    // SAFETY: `new_timer` initialized the `_timer` union arm.
    let timer_fields = unsafe { &timer_raw.__bindgen_anon_1.__bindgen_anon_1._sifields._timer };
    assert_eq!(
        core::ptr::addr_of!(timer_fields._tid) as usize - timer_base,
        16
    );
    assert_eq!(
        core::ptr::addr_of!(timer_fields._overrun) as usize - timer_base,
        20
    );
    assert_eq!(
        core::ptr::addr_of!(timer_fields._sigval) as usize - timer_base,
        24
    );
    assert_eq!(
        core::ptr::addr_of!(timer_fields._sys_private) as usize - timer_base,
        32
    );
}
