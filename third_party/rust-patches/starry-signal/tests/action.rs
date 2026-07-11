use starry_signal::{
    RawSignalAction, SignalAction, SignalActionFlags, SignalDisposition, SignalSet, Signo,
};

#[test]
fn flags_bits() {
    let mut flags = SignalActionFlags::default();
    flags.insert(SignalActionFlags::SIGINFO);
    assert!(flags.contains(SignalActionFlags::SIGINFO));

    flags.insert(SignalActionFlags::ONSTACK);
    assert!(flags.contains(SignalActionFlags::ONSTACK));

    flags.remove(SignalActionFlags::SIGINFO);
    assert!(!flags.contains(SignalActionFlags::SIGINFO));
    assert!(flags.contains(SignalActionFlags::ONSTACK));

    let bits = flags.bits();
    assert_ne!(bits, 0);
    assert!(!flags.is_empty());
}

#[test]
fn convert() {
    unsafe extern "C" fn test_handler(_: i32) {}
    let flag_disposition = vec![
        (SignalActionFlags::empty(), SignalDisposition::Default),
        (
            SignalActionFlags::RESTART | SignalActionFlags::ONSTACK,
            SignalDisposition::Ignore,
        ),
        (
            SignalActionFlags::SIGINFO | SignalActionFlags::NODEFER,
            SignalDisposition::Handler(test_handler as usize),
        ),
    ];

    for (flags, disposition) in flag_disposition {
        let action = SignalAction {
            flags,
            mask: {
                let mut m = SignalSet::default();
                m.add(Signo::SIGINT);
                m.add(Signo::SIGRT32);
                m
            },
            disposition: disposition.clone(),
            restorer: None,
        };
        let raw = RawSignalAction::from(action.clone());
        let action2 = SignalAction::from(raw);

        assert_eq!(action.flags.bits(), action2.flags.bits());
        assert_eq!(
            action.mask.has(Signo::SIGINT),
            action2.mask.has(Signo::SIGINT)
        );
        assert_eq!(
            action.mask.has(Signo::SIGRT32),
            action2.mask.has(Signo::SIGRT32)
        );
        match (&action.disposition, &action2.disposition) {
            (SignalDisposition::Default, SignalDisposition::Default) => {}
            (SignalDisposition::Ignore, SignalDisposition::Ignore) => {}
            (SignalDisposition::Handler(h1), SignalDisposition::Handler(h2)) => {
                assert_ne!(*h1, 0);
                assert_eq!(h1, h2);
            }
            _ => panic!(
                "Unexpected disposition combination: {:?} -> {:?}",
                action.disposition, action2.disposition
            ),
        }
    }
}

#[test]
fn raw_action_classifies_arbitrary_handler_bits_without_function_pointer_validity() {
    let raw = RawSignalAction {
        handler: usize::MAX,
        flags: SignalActionFlags::SIGINFO.bits(),
        #[cfg(sa_restorer)]
        restorer: 0,
        mask: SignalSet::default(),
    };

    let action = SignalAction::from(raw);
    assert!(matches!(
        action.disposition,
        SignalDisposition::Handler(address) if address == usize::MAX
    ));
}

#[cfg(sa_restorer)]
#[test]
fn explicit_null_restorer_is_not_replaced_by_the_default() {
    let raw = RawSignalAction {
        handler: 0x4000,
        flags: SignalActionFlags::RESTORER.bits(),
        restorer: 0,
        mask: SignalSet::default(),
    };

    let action = SignalAction::from(raw);
    assert_eq!(action.restorer, Some(0));
    assert_eq!(RawSignalAction::from(action).restorer, 0);
}
