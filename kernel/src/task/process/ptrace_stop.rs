//! Seized stop/listen/interrupt transitions; no user execution while listening.
use super::*;
use crate::task::AsThread;

fn exec_stop_kind(options: u32, seized: bool) -> Option<u8> {
    if options & (1 << 4) != 0 {
        Some(4)
    } else if !seized {
        Some(0)
    } else {
        None
    }
}

fn all_images_published(images: impl IntoIterator<Item = bool>) -> bool {
    let mut saw_thread = false;
    for ready in images {
        saw_thread = true;
        if !ready {
            return false;
        }
    }
    saw_thread
}

impl ProcessData {
    /// A requested stop is not yet a waitable stop. Every owner must have
    /// crossed its user boundary and published its value image first.
    pub(crate) fn ptrace_stop_ready(&self) -> bool {
        all_images_published(self.proc.thread_ids().map(|tid| {
            super::super::get_task(tid).ok().is_some_and(|task| {
                task.try_as_thread()
                    .is_some_and(|thread| thread.ptrace_registers.lock().is_some())
            })
        }))
    }

    pub(crate) fn claim_ptrace_stop_notification(&self) -> bool {
        let mut job = self.job_ctl.lock();
        if job
            .current_stop_report()
            .is_none_or(|stop| stop.ptrace_session.is_none())
            || job.stop_notified
        {
            return false;
        }
        job.stop_notified = true;
        true
    }

    /// Event-enabled exec is an event stop; legacy TRACEME/ATTACH emits a
    /// plain delivery trap. Seized tracees without TRACEEXEC get no trap.
    pub(crate) fn ptrace_exec_stop(&self, session: PtraceSession, old_pid: usize) -> Option<u8> {
        let mut control = self.ptrace_ctl.lock();
        if control.active_session() != Some(session) {
            return None;
        }
        let event = exec_stop_kind(control.options, control.seized)?;
        let mut job = self.job_ctl.lock();
        if job.state != StopState::Running {
            return None;
        }
        if event != 0 {
            control.event_message = old_pid;
        }
        job.state = StopState::Stopped;
        job.stop_kind = StopKind::Ptrace;
        job.stop_signal = Signo::SIGTRAP as u8;
        job.ptrace_event = event;
        job.ptrace_session = Some(session);
        job.stop_reported = false;
        job.stop_notified = false;
        job.continued = false;
        Some(event)
    }

    /// Publish syscall provenance and the wait status in one generation.
    pub(crate) fn ptrace_syscall_stop(&self, session: PtraceSession, op: u8) -> Option<bool> {
        let mut control = self.ptrace_ctl.lock();
        let mut job = self.job_ctl.lock();
        if control.active_session() != Some(session) || job.state != StopState::Running {
            return None;
        }
        let good = control.options & tk_linux_process::ptrace_options::TRACESYSGOOD != 0;
        control.event_message = op as usize;
        job.state = StopState::Stopped;
        job.stop_kind = StopKind::Ptrace;
        job.stop_signal = Signo::SIGTRAP as u8 | if good { 0x80 } else { 0 };
        job.ptrace_event = 0;
        job.ptrace_session = Some(session);
        job.stop_reported = false;
        job.stop_notified = false;
        job.continued = false;
        Some(good)
    }

    pub(crate) fn current_stop_report(&self) -> Option<StopReport> {
        self.job_ctl.lock().current_stop_report()
    }

    pub(crate) fn ptrace_listen(&self, session: PtraceSession) -> AxResult<bool> {
        let mut control = self.ptrace_ctl.lock();
        let mut job = self.job_ctl.lock();
        if control.active_session() != Some(session)
            || !control.seized
            || !job.is_ptrace_inactive_for(session)
            || job.ptrace_event != 128
        {
            return Err(LinuxError::EIO.into());
        }
        if control.interrupt_pending {
            control.interrupt_pending = false;
            job.stop_reported = false;
            job.stop_notified = false;
            return Ok(true);
        }
        control.listening = true;
        job.stop_reported = true;
        Ok(false)
    }

    pub(crate) fn ptrace_pending_interrupt_stop(&self, session: PtraceSession) -> bool {
        let mut control = self.ptrace_ctl.lock();
        let mut job = self.job_ctl.lock();
        if control.active_session() != Some(session)
            || !control.interrupt_pending
            || job.state != StopState::Running
        {
            return false;
        }
        control.interrupt_pending = false;
        job.state = StopState::Stopped;
        job.stop_kind = StopKind::Ptrace;
        job.stop_signal = Signo::SIGTRAP as u8;
        job.ptrace_event = 128;
        job.ptrace_session = Some(session);
        job.stop_reported = false;
        job.stop_notified = false;
        job.continued = false;
        true
    }

    pub(crate) fn ptrace_sigcont_wake(&self) -> bool {
        let mut control = self.ptrace_ctl.lock();
        let Some(session) = control.active_session() else {
            return false;
        };
        if !control.listening {
            return false;
        }
        let mut job = self.job_ctl.lock();
        control.listening = false;
        control.interrupt_pending = false;
        job.state = StopState::Stopped;
        job.stop_kind = StopKind::Ptrace;
        job.stop_signal = Signo::SIGTRAP as u8;
        job.ptrace_event = 128;
        job.ptrace_session = Some(session);
        job.stop_reported = false;
        job.stop_notified = false;
        job.continued = false;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::AsThread;
    #[test]
    fn exec_event_legacy_and_seized_admission_are_distinct() {
        assert_eq!(exec_stop_kind(0, false), Some(0));
        assert_eq!(exec_stop_kind(0, true), None);
        assert_eq!(exec_stop_kind(1 << 4, false), Some(4));
        assert_eq!(exec_stop_kind(1 << 4, true), Some(4));
    }

    #[test]
    fn a_requested_stop_is_not_reportable_until_all_owner_images_exist() {
        assert!(!all_images_published([]));
        assert!(!all_images_published([false]));
        assert!(!all_images_published([true, false]));
        assert!(all_images_published([true]));
        assert!(all_images_published([true, true]));
    }
}
