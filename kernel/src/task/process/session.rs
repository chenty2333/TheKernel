//! `task::process` subsections; see the parent `mod.rs` for the module map.

use super::*;

struct SessionSidBinding {
    session: Arc<Session>,
    pid_ns: Arc<PidNamespace>,
}

struct SessionSidBindings {
    entries: Vec<SessionSidBinding>,
    pending: usize,
}

static SESSION_SID_BINDINGS: Once<Mutex<SessionSidBindings>> = Once::new();

fn session_sid_bindings() -> &'static Mutex<SessionSidBindings> {
    SESSION_SID_BINDINGS.call_once(|| {
        Mutex::new(SessionSidBindings {
            entries: Vec::new(),
            pending: 0,
        })
    })
}

pub(crate) struct PreparedSessionSidBinding {
    armed: bool,
}

pub(crate) fn prepare_session_sid_binding() -> AxResult<PreparedSessionSidBinding> {
    let mut bindings = session_sid_bindings().lock();
    let needed = bindings.pending.checked_add(1).ok_or(AxError::NoMemory)?;
    bindings
        .entries
        .try_reserve(needed)
        .map_err(|_| AxError::NoMemory)?;
    bindings.pending = needed;
    Ok(PreparedSessionSidBinding { armed: true })
}

impl PreparedSessionSidBinding {
pub(crate) fn commit(mut self, session: Arc<Session>, pid_ns: Arc<PidNamespace>) {
        let mut bindings = session_sid_bindings().lock();
        debug_assert!(bindings.pending != 0);
        bindings.pending -= 1;
        bindings.entries.push(SessionSidBinding { session, pid_ns });
        self.armed = false;
    }
}

impl Drop for PreparedSessionSidBinding {
    fn drop(&mut self) {
        if self.armed {
            let mut bindings = session_sid_bindings().lock();
            debug_assert!(bindings.pending != 0);
            bindings.pending -= 1;
        }
    }
}

pub(crate) fn release_dead_session_sid_binding(
    session: &Arc<Session>,
    fallback: &Arc<PidNamespace>,
) {
    let pid_ns = {
        let mut bindings = session_sid_bindings().lock();
        bindings
            .entries
            .iter()
            .position(|entry| Arc::ptr_eq(&entry.session, session))
            .map(|index| bindings.entries.swap_remove(index).pid_ns)
            .unwrap_or_else(|| fallback.clone())
    };
    pid_ns.release_reaped_process(session.sid());
}
