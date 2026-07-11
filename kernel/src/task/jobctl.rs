use starry_process::Pid;

#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(in crate::task) enum StopState {
    Running  = 0,
    Stopping = 1,
    Stopped  = 2,
}

impl From<u8> for StopState {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Running,
            1 => Self::Stopping,
            2 => Self::Stopped,
            _ => unreachable!(),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(in crate::task) enum StopKind {
    JobControl = 0,
    Ptrace     = 1,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct StopReport {
    pub(crate) signal: u8,
    pub(crate) traced: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ContinueResult {
    None,
    CanceledStopping,
    ResumedStopped,
}

#[derive(Debug, Clone, Copy)]
pub(in crate::task) struct JobControlState {
    pub(in crate::task) state: StopState,
    pub(in crate::task) stop_signal: u8,
    pub(in crate::task) stop_kind: StopKind,
    pub(in crate::task) continued: bool,
    pub(in crate::task) stop_reported: bool,
}

impl Default for JobControlState {
    fn default() -> Self {
        Self {
            state: StopState::Running,
            stop_signal: 0,
            stop_kind: StopKind::JobControl,
            continued: false,
            stop_reported: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::task) struct PtraceControlState {
    pub(in crate::task) tracer: Option<Pid>,
    pub(in crate::task) options: u32,
    pub(in crate::task) event_message: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::task) struct ExecControlState {
    pub(in crate::task) owner: Option<Pid>,
    pub(in crate::task) pending_thread_additions: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::task) struct VforkControlState {
    pub(in crate::task) parent_tid: Option<Pid>,
}
