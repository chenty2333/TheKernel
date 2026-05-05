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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(in crate::task) enum ContinueResult {
    None,
    CanceledStopping,
    ResumedStopped,
}

#[derive(Debug, Clone, Copy)]
pub(in crate::task) struct JobControlState {
    state: StopState,
    stop_signal: u8,
    continued: bool,
    stop_reported: bool,
}

impl Default for JobControlState {
    fn default() -> Self {
        Self {
            state: StopState::Running,
            stop_signal: 0,
            continued: false,
            stop_reported: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::task) struct ExecControlState {
    owner: Option<Pid>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::task) struct VforkControlState {
    parent_tid: Option<Pid>,
}
