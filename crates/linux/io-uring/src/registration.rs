use crate::IoUringError;

const IORING_REGISTER_BUFFERS: u32 = 0;
const IORING_UNREGISTER_BUFFERS: u32 = 1;
const IORING_REGISTER_FILES: u32 = 2;
const IORING_UNREGISTER_FILES: u32 = 3;
const IORING_REGISTER_EVENTFD: u32 = 4;
const IORING_UNREGISTER_EVENTFD: u32 = 5;
const IORING_REGISTER_PROBE: u32 = 8;
const IORING_REGISTER_ENABLE_RINGS: u32 = 12;
const IORING_REGISTER_RING_FDS: u32 = 20;
const IORING_UNREGISTER_RING_FDS: u32 = 21;
const IORING_REGISTER_MEM_REGION: u32 = 34;
const IORING_REGISTER_USE_REGISTERED_RING: u32 = 1 << 31;

/// Linux v6.18 maximum classic registered-buffer slots.
pub const IORING_MAX_REGISTERED_BUFFERS: u32 = 1 << 14;
/// Linux v6.18 maximum operation records accepted by `REGISTER_PROBE`.
pub const IORING_MAX_PROBE_OPERATIONS: u32 = 256;
/// First registration opcode outside the Linux v6.18 UAPI enum.
pub const PINNED_IORING_REGISTER_LAST: u32 = 36;

/// Header-level registration operations implemented by the initial profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationOperation {
    /// Copy `count` iovec descriptors and build an unpublished fixed table.
    RegisterBuffers {
        /// Userspace address of the iovec array.
        argument: u64,
        /// Number of fixed-buffer slots.
        count: u32,
    },
    /// Retire the one published fixed-buffer table.
    UnregisterBuffers,
    /// Copy `count` signed descriptors and build an unpublished fixed table.
    RegisterFiles {
        /// Userspace address of the signed descriptor array.
        argument: u64,
        /// Number of fixed-file slots, including `-1` sparse entries.
        count: u32,
    },
    /// Retire the one published fixed-file table.
    UnregisterFiles,
    /// Deliver a signal to this eventfd after every successfully published
    /// completion.  The descriptor is copied by value, not as a pointer.
    RegisterEventFd {
        fd: i32,
    },
    /// Stop completion eventfd delivery.
    UnregisterEventFd,
    /// Fill a zeroed probe header and at most `operations` records.
    Probe {
        /// Userspace address of the probe object.
        argument: u64,
        /// Requested operation-record capacity.
        operations: u32,
    },
    /// Copy one `io_uring_rsrc_register` header and use its `data` array as
    /// a generation-bound, task-local registered-ring index table.
    RegisterRingFds {
        argument: u64,
        count: u32,
    },
    /// Retire every registered-ring index owned by this ring in the calling
    /// files table.  Existing enter users retain their typed ring owner.
    UnregisterRingFds {
        argument: u64,
        count: u32,
    },
    /// Copy one `io_uring_mem_region_reg` header and its region descriptor.
    /// The adapter validates/pins the user region before publishing it.
    RegisterMemRegion {
        argument: u64,
    },
    EnableRings,
}

/// Copied syscall registration header before any userspace array access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegistrationRequest {
    opcode: u32,
    argument: u64,
    count: u32,
}

impl RegistrationRequest {
    /// Preserves one copied raw registration header.
    pub const fn new(opcode: u32, argument: u64, count: u32) -> Self {
        Self {
            opcode,
            argument,
            count,
        }
    }

    /// Raw Linux registration opcode.
    pub const fn opcode(self) -> u32 {
        self.opcode
    }

    /// Raw userspace argument address.
    pub const fn argument(self) -> u64 {
        self.argument
    }

    /// Raw `nr_args` value.
    pub const fn count(self) -> u32 {
        self.count
    }

    /// Whether the raw opcode selects a previously registered ring index.
    pub const fn use_registered_ring(self) -> bool {
        self.opcode & IORING_REGISTER_USE_REGISTERED_RING != 0
    }

    /// Performs only syscall-envelope classification.  Linux rejects an
    /// opcode outside the v6.18 registration enum before it resolves `fd`,
    /// while every known opcode reaches ring lookup before its operation body
    /// validates `arg`/`nr_args`.
    pub const fn validate_envelope(self) -> Result<(), IoUringError> {
        let opcode = self.opcode & !IORING_REGISTER_USE_REGISTERED_RING;
        if opcode >= PINNED_IORING_REGISTER_LAST {
            Err(IoUringError::UnknownRegistration)
        } else {
            Ok(())
        }
    }

    /// Validates an already-classified operation body and classifies supported
    /// versus unsupported work.
    pub const fn decode(self) -> Result<RegistrationOperation, IoUringError> {
        let opcode = self.opcode & !IORING_REGISTER_USE_REGISTERED_RING;
        match opcode {
            IORING_REGISTER_BUFFERS => {
                // A NULL argument is still passed to the syscall adapter so
                // it can reproduce Linux's EFAULT precedence over the count
                // cap without allocating or touching an unbounded range.
                if self.argument != 0
                    && (self.count == 0 || self.count > IORING_MAX_REGISTERED_BUFFERS)
                {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::RegisterBuffers {
                        argument: self.argument,
                        count: self.count,
                    })
                }
            }
            IORING_UNREGISTER_BUFFERS => {
                if self.argument != 0 || self.count != 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::UnregisterBuffers)
                }
            }
            IORING_REGISTER_FILES => {
                // Two header fields are checked by two different layers, and
                // they do not share an errno:
                //
                //     case IORING_REGISTER_FILES:
                //             ret = -EFAULT;
                //             if (!arg)
                //                     break;
                //             ret = io_sqe_files_register(ctx, arg, nr_args, NULL);
                //
                // (`io_uring/register.c:786-790`) rejects a NULL descriptor
                // array with -EFAULT before the table routine runs, while
                // `io_sqe_files_register()` itself owns -EINVAL for a zero
                // `nr_args`, -EBUSY for an already-registered table and
                // -EMFILE for both `nr_args` ceilings — the last of which is
                // the caller's `RLIMIT_NOFILE`
                // (`io_uring/rsrc.c:624-631`).  Neither value is judged here:
                // both travel to the syscall adapter, which owns the caller's
                // address space and resource limits.
                Ok(RegistrationOperation::RegisterFiles {
                    argument: self.argument,
                    count: self.count,
                })
            }
            IORING_UNREGISTER_FILES => {
                if self.argument != 0 || self.count != 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::UnregisterFiles)
                }
            }
            IORING_REGISTER_EVENTFD => {
                if self.argument > i32::MAX as u64 || self.count != 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::RegisterEventFd {
                        fd: self.argument as i32,
                    })
                }
            }
            IORING_UNREGISTER_EVENTFD => {
                if self.argument != 0 || self.count != 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::UnregisterEventFd)
                }
            }
            IORING_REGISTER_PROBE => {
                if self.argument == 0 || self.count > IORING_MAX_PROBE_OPERATIONS {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::Probe {
                        argument: self.argument,
                        operations: self.count,
                    })
                }
            }
            IORING_REGISTER_ENABLE_RINGS => {
                if self.argument != 0 || self.count != 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::EnableRings)
                }
            }
            IORING_REGISTER_RING_FDS => {
                if self.argument == 0 || self.count == 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::RegisterRingFds {
                        argument: self.argument,
                        count: self.count,
                    })
                }
            }
            IORING_UNREGISTER_RING_FDS => {
                if self.argument == 0 || self.count == 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::UnregisterRingFds {
                        argument: self.argument,
                        count: self.count,
                    })
                }
            }
            IORING_REGISTER_MEM_REGION => {
                if self.argument == 0 || self.count != 1 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::RegisterMemRegion {
                        argument: self.argument,
                    })
                }
            }
            opcode if opcode < PINNED_IORING_REGISTER_LAST => {
                Err(IoUringError::UnsupportedRegistration)
            }
            _ => Err(IoUringError::UnknownRegistration),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_file_headers_are_strictly_validated() {
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_FILES, 0x1000, 4).decode(),
            Ok(RegistrationOperation::RegisterFiles {
                argument: 0x1000,
                count: 4
            })
        );
        // A NULL array is the dispatcher's -EFAULT and a zero count is
        // `io_sqe_files_register()`'s -EINVAL, so neither is collapsed into a
        // malformed-header error here.
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_FILES, 0, 4).decode(),
            Ok(RegistrationOperation::RegisterFiles {
                argument: 0,
                count: 4
            })
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_FILES, 0x1000, 0).decode(),
            Ok(RegistrationOperation::RegisterFiles {
                argument: 0x1000,
                count: 0
            })
        );
        assert_eq!(
            RegistrationRequest::new(IORING_UNREGISTER_FILES, 0, 0).decode(),
            Ok(RegistrationOperation::UnregisterFiles)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_UNREGISTER_FILES, 1, 0).decode(),
            Err(IoUringError::InvalidRegistration)
        );
    }

    #[test]
    fn file_registration_limits_are_decoded_for_the_adapter() {
        // `io_sqe_files_register()` answers -EMFILE for both `nr_args`
        // ceilings (`io_uring/rsrc.c:628-631`), so decoding must not collapse
        // an oversized count into the generic -EINVAL of a malformed header.
        // `RegisteredFileTable::new` still refuses to build such a table.
        for count in [1 << 20, (1 << 20) + 1, u32::MAX] {
            assert_eq!(
                RegistrationRequest::new(IORING_REGISTER_FILES, 0x1000, count).decode(),
                Ok(RegistrationOperation::RegisterFiles {
                    argument: 0x1000,
                    count,
                })
            );
        }
    }

    #[test]
    fn buffer_headers_distinguish_malformed_from_supported() {
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_BUFFERS, 0, 1).decode(),
            Ok(RegistrationOperation::RegisterBuffers {
                argument: 0,
                count: 1,
            })
        );
        assert_eq!(
            RegistrationRequest::new(
                IORING_REGISTER_BUFFERS,
                0x1000,
                IORING_MAX_REGISTERED_BUFFERS + 1
            )
            .decode(),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_BUFFERS, 0, 0).decode(),
            Ok(RegistrationOperation::RegisterBuffers {
                argument: 0,
                count: 0,
            })
        );
        assert_eq!(
            RegistrationRequest::new(
                IORING_REGISTER_BUFFERS,
                0,
                IORING_MAX_REGISTERED_BUFFERS + 1
            )
            .decode(),
            Ok(RegistrationOperation::RegisterBuffers {
                argument: 0,
                count: IORING_MAX_REGISTERED_BUFFERS + 1,
            })
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_BUFFERS, 0x1000, 1).decode(),
            Ok(RegistrationOperation::RegisterBuffers {
                argument: 0x1000,
                count: 1,
            })
        );
        assert_eq!(
            RegistrationRequest::new(IORING_UNREGISTER_BUFFERS, 1, 0).decode(),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_UNREGISTER_BUFFERS, 0, 0).decode(),
            Ok(RegistrationOperation::UnregisterBuffers)
        );
        let request = RegistrationRequest::new(
            IORING_REGISTER_USE_REGISTERED_RING | IORING_REGISTER_BUFFERS,
            0x1000,
            1,
        );
        assert!(request.use_registered_ring());
        assert_eq!(
            request.decode(),
            Ok(RegistrationOperation::RegisterBuffers {
                argument: 0x1000,
                count: 1
            })
        );
    }

    #[test]
    fn probe_and_pinned_opcode_range_need_no_consumer_magic() {
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_PROBE, 0x2000, 256).decode(),
            Ok(RegistrationOperation::Probe {
                argument: 0x2000,
                operations: 256
            })
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_PROBE, 0x2000, 257).decode(),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(PINNED_IORING_REGISTER_LAST - 1, 0, 0).decode(),
            Err(IoUringError::UnsupportedRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(PINNED_IORING_REGISTER_LAST, 0, 0).decode(),
            Err(IoUringError::UnknownRegistration)
        );
    }
}
