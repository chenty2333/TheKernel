mod epoll;
mod poll;
mod select;

use alloc::vec::Vec;
use core::{future::pending, task::Context};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::uspace::UserContext;
use axpoll::{IoEvents, Pollable};
use axtask::{current, future};
use starry_signal::SignalSet;

pub use self::{epoll::*, poll::*, select::*};
use crate::{
    file::{FileHandle, FileLike},
    task::{
        AsThread, ProcStateHint, check_signals, has_pending_syscall_signal, with_proc_state_hint,
    },
};

struct FdPollSet(pub Vec<(FileHandle<dyn FileLike>, IoEvents)>);
impl Pollable for FdPollSet {
    fn poll(&self) -> IoEvents {
        unreachable!()
    }

    fn register(&self, context: &mut Context<'_>, _events: IoEvents) {
        for (file, events) in &self.0 {
            file.register(context, *events);
        }
    }
}

fn wait_io_result(
    mut uctx: Option<&mut UserContext>,
    sigmask: Option<SignalSet>,
    mut wait_once: impl FnMut() -> Result<AxResult<isize>, future::Elapsed>,
) -> AxResult<isize> {
    let curr = current();
    let thr = curr.as_thread();
    let old_blocked = sigmask.map(|set| thr.signal.set_blocked(set));

    if let Some(uctx) = uctx.as_deref_mut() {
        // If a handler runs while the syscall is blocked, sigreturn must
        // observe -EINTR as the interrupted syscall result.
        uctx.set_retval(-LinuxError::EINTR.code() as usize);
    }

    if let Some(uctx) = uctx.as_deref_mut() {
        let handler_depth = thr.signal_handler_depth();
        if check_signals(thr, uctx, old_blocked) {
            if let Some(old_blocked) = old_blocked
                && thr.signal_handler_depth() == handler_depth
            {
                thr.signal.set_blocked(old_blocked);
            }
            return Err(AxError::Interrupted);
        }
    } else if has_pending_syscall_signal(thr) {
        if let Some(old_blocked) = old_blocked {
            thr.signal.set_blocked(old_blocked);
        }
        return Err(AxError::Interrupted);
    }

    with_proc_state_hint(ProcStateHint::Interruptible, || {
        loop {
            match wait_once() {
                Ok(Ok(res)) => {
                    if let Some(old_blocked) = old_blocked {
                        thr.signal.set_blocked(old_blocked);
                    }
                    return Ok(res);
                }
                Ok(Err(AxError::Interrupted)) => {
                    if let Some(uctx) = uctx.as_deref_mut() {
                        let handler_depth = thr.signal_handler_depth();
                        let handled = check_signals(thr, uctx, old_blocked);
                        if handled {
                            if let Some(old_blocked) = old_blocked
                                && thr.signal_handler_depth() == handler_depth
                            {
                                thr.signal.set_blocked(old_blocked);
                            }
                            return Err(AxError::Interrupted);
                        }
                    } else if has_pending_syscall_signal(thr) {
                        if let Some(old_blocked) = old_blocked {
                            thr.signal.set_blocked(old_blocked);
                        }
                        return Err(AxError::Interrupted);
                    }
                }
                Ok(Err(err)) => {
                    if let Some(old_blocked) = old_blocked {
                        thr.signal.set_blocked(old_blocked);
                    }
                    return Err(err);
                }
                Err(_) => {
                    if let Some(old_blocked) = old_blocked {
                        thr.signal.set_blocked(old_blocked);
                    }
                    return Ok(0);
                }
            }
        }
    })
}

fn wait_signal_only(
    uctx: Option<&mut UserContext>,
    timeout: Option<core::time::Duration>,
    sigmask: Option<SignalSet>,
) -> AxResult<isize> {
    let deadline = timeout.map(|dur| axhal::time::wall_time().saturating_add(dur));
    let mut wait_once = || {
        future::block_on(future::timeout(
            deadline.map(|end| end.saturating_sub(axhal::time::wall_time())),
            async {
                future::interruptible(pending::<()>())
                    .await
                    .map_err(AxError::from)?;
                Ok(0)
            },
        ))
    };

    wait_io_result(uctx, sigmask, &mut wait_once)
}
