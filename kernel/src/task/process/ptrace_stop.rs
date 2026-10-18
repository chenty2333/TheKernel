//! Seized stop/listen/interrupt transitions; no user execution while listening.
use super::*;

fn exec_stop_kind(options: u32, seized: bool) -> Option<u8> {
    if options & (1 << 4) != 0 {
        Some(4)
    } else if !seized {
        Some(0)
    } else {
        None
    }
}

impl super::super::Thread {
    /// A requested stop is not yet a waitable stop. Every owner must have
    /// crossed its user boundary and published its value image first.
    pub(crate) fn ptrace_stop_ready(&self) -> bool {
        let job = self.ptrace_job_ctl.lock();
        job.current_stop_report()
            .is_some_and(|report| job.ptrace_stop_image_ready(report))
    }

    pub(crate) fn current_ptrace_stop_report(&self) -> Option<StopReport> {
        self.ptrace_job_ctl
            .lock()
            .current_stop_report()
            .filter(|report| report.traced())
    }

    /// Keeps the owner in its wait loop when a new ptrace stop raced the
    /// loop's preceding register-image snapshot.
    pub(crate) fn ptrace_stop_wait_can_block(&self) -> bool {
        self.ptrace_job_ctl.lock().ptrace_stop_wait_can_block()
    }

    /// A stop image remains useful across a resume-free notification only
    /// when the same owner explicitly re-publishes it for this incarnation.
    pub(crate) fn mark_ptrace_stop_image_ready(&self, report: StopReport) -> bool {
        self.ptrace_job_ctl
            .lock()
            .mark_ptrace_stop_image_ready(report)
    }

    pub(crate) fn ptrace_stop_image_ready_for(&self, report: StopReport) -> bool {
        self.ptrace_job_ctl
            .lock()
            .ptrace_stop_image_ready(report)
    }

    pub(crate) fn clear_ptrace_stop_image_ready(&self) {
        self.ptrace_job_ctl.lock().stop_image_generation = None;
    }

    pub(crate) fn claim_ptrace_stop_notification(&self) -> Option<StopReport> {
        let mut job = self.ptrace_job_ctl.lock();
        job.claim_ptrace_stop_notification()
    }

    /// Event-enabled exec is an event stop; legacy TRACEME/ATTACH emits a
    /// plain delivery trap. Seized tracees without TRACEEXEC get no trap.
    pub(crate) fn ptrace_exec_stop(&self, session: PtraceSession, old_pid: usize) -> Option<u8> {
        let mut control = self.ptrace_ctl.lock();
        if control.active_session() != Some(session) {
            return None;
        }
        let event = exec_stop_kind(control.options, control.seized)?;
        let mut job = self.ptrace_job_ctl.lock();
        if job.state != StopState::Running {
            return None;
        }
        if event != 0 {
            control.event_message = old_pid;
        }
        job.publish_ptrace_stop(session, Signo::SIGTRAP as u8, event);
        Some(event)
    }

    /// Publish syscall provenance and the wait status in one generation.
    pub(crate) fn ptrace_syscall_stop(&self, session: PtraceSession, op: u8) -> Option<bool> {
        let mut control = self.ptrace_ctl.lock();
        let mut job = self.ptrace_job_ctl.lock();
        if control.active_session() != Some(session) || job.state != StopState::Running {
            return None;
        }
        let good = control.options & tk_linux_process::ptrace_options::TRACESYSGOOD != 0;
        control.event_message = op as usize;
        job.publish_ptrace_stop(
            session,
            Signo::SIGTRAP as u8 | if good { 0x80 } else { 0 },
            0,
        );
        Some(good)
    }

    /// Each seized task observes the actual group-stop signal in EVENT_STOP.
    /// Shared job control stays group-owned; ptrace report/ack stays private.
    pub(crate) fn ptrace_group_stop(&self, signo: u8) -> bool {
        let control = self.ptrace_ctl.lock();
        if !control.seized { return false; }
        let Some(session) = control.active_session() else { return false; };
        let mut job = self.ptrace_job_ctl.lock();
        if job.state != StopState::Running { return false; }
        job.publish_ptrace_stop(session, signo, 128);
        true
    }

    pub(crate) fn current_stop_report(&self) -> Option<StopReport> {
        self.ptrace_job_ctl.lock().current_stop_report()
    }

    pub(crate) fn ptrace_listen(&self, session: PtraceSession) -> AxResult<bool> {
        let mut control = self.ptrace_ctl.lock();
        let mut job = self.ptrace_job_ctl.lock();
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
        let mut job = self.ptrace_job_ctl.lock();
        if control.active_session() != Some(session)
            || !control.interrupt_pending
            || job.state != StopState::Running
        {
            return false;
        }
        control.interrupt_pending = false;
        job.publish_ptrace_stop(session, Signo::SIGTRAP as u8, 128);
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
        let mut job = self.ptrace_job_ctl.lock();
        control.listening = false;
        control.interrupt_pending = false;
        job.publish_ptrace_stop(session, Signo::SIGTRAP as u8, 128);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exec_event_legacy_and_seized_admission_are_distinct() {
        assert_eq!(exec_stop_kind(0, false), Some(0));
        assert_eq!(exec_stop_kind(0, true), None);
        assert_eq!(exec_stop_kind(1 << 4, false), Some(4));
        assert_eq!(exec_stop_kind(1 << 4, true), Some(4));
    }

}
