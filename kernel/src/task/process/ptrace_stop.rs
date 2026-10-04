//! Seized stop/listen/interrupt transitions; no user execution while listening.
use super::*;

impl ProcessData {
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
        job.continued = false;
        true
    }
}
