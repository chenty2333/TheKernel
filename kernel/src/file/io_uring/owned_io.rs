//! Owned regular-file provider submission and terminal completion.

use super::*;

impl IoUring {
    pub(crate) fn publish_owned_file_io(
        &self,
        issued: IssuedRequest,
        prepared: axfs::PreparedOwnedFileIo,
        buffer: Option<IoUringBufferLease>,
    ) -> Result<
        (),
        (
            AxError,
            IssuedRequest,
            axfs::PreparedOwnedFileIo,
            Option<IoUringBufferLease>,
        ),
    > {
        if let Some(lease) = buffer.as_ref()
            && let Err(error) = lease.validate_world(self.world)
        {
            return Err((error, issued, prepared, buffer));
        }
        let id = issued.id();
        {
            let mut state = self.state.lock();
            // Close may have swept the owner table between issue and this
            // handoff. Do not publish new provider work behind that sweep.
            if self.final_close_requested.load(Ordering::Acquire) {
                return Err((AxError::BadState, issued, prepared, buffer));
            }
            let slot = id.slot() as usize;
            let Some(entry) = state.owned_file_io.get_mut(slot) else {
                return Err((AxError::BadState, issued, prepared, buffer));
            };
            if entry.is_some() {
                return Err((AxError::BadState, issued, prepared, buffer));
            }
            *entry = Some(OwnedFileIoOwner {
                id,
                state: OwnedFileIoControlState::Publishing {
                    generation: id.generation(),
                    bridge: OwnedFileIoBridge { issued },
                },
                buffer,
            });
        }
        match prepared.submit() {
            Ok(submitted) => {
                let mut state = self.state.lock();
                let entry = state
                    .owned_file_io
                    .get_mut(id.slot() as usize)
                    .and_then(Option::as_mut);
                if let Some(entry) = entry.filter(|entry| entry.id == id) {
                    let bridge = match core::mem::replace(
                        &mut entry.state,
                        OwnedFileIoControlState::Terminal,
                    ) {
                        OwnedFileIoControlState::Publishing { generation, bridge }
                            if generation == id.generation() =>
                        {
                            bridge
                        }
                        terminal => {
                            entry.state = terminal;
                            return Ok(());
                        }
                    };
                    entry.state = OwnedFileIoControlState::Submitted {
                        generation: id.generation(),
                        bridge,
                        control: submitted,
                    };
                }
                Ok(())
            }
            Err(error) => {
                let (request, completion) = (error.request, error.completion);
                drop((request, completion));
                self.complete_owned_file_io(id, -LinuxError::from(error.error).code());
                Ok(())
            }
        }
    }

    pub(crate) fn complete_owned_file_io(&self, id: RequestId, result: i32) {
        let terminal = {
            let mut state = self.state.lock();
            state
                .owned_file_io
                .get_mut(id.slot() as usize)
                .and_then(|entry| {
                    if entry.as_ref().is_none_or(|owner| owner.id != id) {
                        return None;
                    }
                    let cancellation_pending = entry.as_ref().is_some_and(|owner| {
                        owner.id == id
                            && matches!(&owner.state,
                            OwnedFileIoControlState::CancelPending { generation, .. }
                                if *generation == id.generation())
                    });
                    if cancellation_pending {
                        let owner = entry.as_mut().expect("owned I/O entry vanished");
                        let previous =
                            core::mem::replace(&mut owner.state, OwnedFileIoControlState::Terminal);
                        let OwnedFileIoControlState::CancelPending { generation, bridge } =
                            previous
                        else {
                            unreachable!("owned I/O cancel state changed under ring lock");
                        };
                        owner.state = OwnedFileIoControlState::CompletionPending {
                            generation,
                            bridge,
                            result,
                        };
                        return None;
                    }
                    if entry.as_ref().is_some_and(|owner| {
                        owner.id == id
                            && matches!(
                                &owner.state,
                                OwnedFileIoControlState::CompletionPending { .. }
                            )
                    }) {
                        return None;
                    }
                    entry.take().and_then(|entry| {
                        if entry.id != id {
                            return None;
                        }
                        match entry.state {
                            OwnedFileIoControlState::Publishing { generation, bridge }
                            | OwnedFileIoControlState::InFlight { generation, bridge }
                                if generation == id.generation() =>
                            {
                                Some(OwnedFileIoTerminal {
                                    issued: bridge.issued,
                                    result,
                                    buffer: entry.buffer,
                                })
                            }
                            OwnedFileIoControlState::Submitted {
                                generation,
                                bridge,
                                control: _,
                            } if generation == id.generation() => Some(OwnedFileIoTerminal {
                                issued: bridge.issued,
                                result,
                                buffer: entry.buffer,
                            }),
                            _ => None,
                        }
                    })
                })
        };
        let Some(terminal) = terminal else {
            return;
        };
        let _ = self.complete_owned_terminal(terminal);
    }

    pub(super) fn complete_owned_terminal(&self, terminal: OwnedFileIoTerminal) -> AxResult<()> {
        const IORING_CQE_F_BUFFER: u32 = 1;
        let OwnedFileIoTerminal {
            issued,
            result,
            mut buffer,
        } = terminal;
        let flags = if result >= 0 {
            buffer
                .as_ref()
                .and_then(IoUringBufferLease::provided_id)
                .map(|id| IORING_CQE_F_BUFFER | (u32::from(id) << 16))
                .unwrap_or(0)
        } else {
            0
        };
        let completed = self.complete_issued_with_claim_hook(
            issued,
            TerminalCause::Completed,
            result,
            flags,
            || {
                if flags & IORING_CQE_F_BUFFER != 0 {
                    if let Some(buffer) = buffer.as_mut() {
                        buffer.consume_provided();
                    }
                }
                // Completion permits userspace to re-register an unregistered
                // fixed-buffer table immediately. Retire its final lease
                // before publishing the CQ tail, not after waking its reader.
                drop(buffer.take());
            },
        );
        drop(buffer);
        completed
    }

    /// Completes an admission-time NOWAIT provider result while preserving a
    /// supplied/fixed buffer lease through the same terminal-CQE hand-off as
    /// queued provider work.
    pub(crate) fn complete_owned_immediate(
        &self,
        issued: IssuedRequest,
        result: i32,
        buffer: Option<IoUringBufferLease>,
    ) -> AxResult<()> {
        self.complete_owned_terminal(OwnedFileIoTerminal {
            issued,
            result,
            buffer,
        })
    }
}
