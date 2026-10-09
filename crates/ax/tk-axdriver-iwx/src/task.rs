//! Deferred-work reference handling from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC).
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>; Copyright (c) 2014
//! Fixup Software Ltd.; Copyright (c) 2017, 2019, 2020 Stefan Sperling
//! <stsp@openbsd.org>.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskReferences {
    pub shutdown: bool,
    pub references: usize,
}

/// Hold a softc reference around task_add and undo it if already enqueued.
// upstream: if_iwx.c iwx_add_task()
pub fn add_task(state: &mut TaskReferences, mut task_add: impl FnMut() -> bool) -> bool {
    if state.shutdown {
        return false;
    }
    state.references += 1;
    if task_add() {
        true
    } else {
        state.references -= 1;
        false
    }
}

/// Release a queued reference only when task_del removed the pending task.
// upstream: if_iwx.c iwx_del_task()
pub fn delete_task(state: &mut TaskReferences, mut task_del: impl FnMut() -> bool) -> bool {
    if task_del() {
        state.references = state.references.saturating_sub(1);
        true
    } else {
        false
    }
}

/// Release the running task's reference and wake any shutdown waiter.
// upstream: if_iwx.c task refs release path
pub fn release_task_reference(state: &mut TaskReferences, mut wake_shutdown_waiter: impl FnMut()) {
    state.references = state.references.saturating_sub(1);
    if state.references == 0 {
        wake_shutdown_waiter();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_delete_and_release_paths_balance_only_real_queue_ownership() {
        let mut refs = TaskReferences::default();
        assert!(add_task(&mut refs, || true));
        assert_eq!(refs.references, 1);
        assert!(!add_task(&mut refs, || false));
        assert_eq!(refs.references, 1);
        assert!(!delete_task(&mut refs, || false));
        assert!(delete_task(&mut refs, || true));
        assert_eq!(refs.references, 0);
        refs.references = 1;
        let mut woke = false;
        release_task_reference(&mut refs, || woke = true);
        assert!(woke);
        refs.shutdown = true;
        assert!(!add_task(&mut refs, || panic!("shutdown must not queue")));
    }
}
