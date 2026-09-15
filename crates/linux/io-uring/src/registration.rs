use crate::IoUringError;

const IORING_REGISTER_BUFFERS: u32 = 0;
const IORING_UNREGISTER_BUFFERS: u32 = 1;
const IORING_REGISTER_FILES: u32 = 2;
const IORING_UNREGISTER_FILES: u32 = 3;
const IORING_REGISTER_EVENTFD: u32 = 4;
const IORING_UNREGISTER_EVENTFD: u32 = 5;
const IORING_REGISTER_FILES_UPDATE: u32 = 6;
const IORING_REGISTER_EVENTFD_ASYNC: u32 = 7;
const IORING_REGISTER_PROBE: u32 = 8;
const IORING_REGISTER_PERSONALITY: u32 = 9;
const IORING_UNREGISTER_PERSONALITY: u32 = 10;
const IORING_REGISTER_RESTRICTIONS: u32 = 11;
const IORING_REGISTER_ENABLE_RINGS: u32 = 12;
const IORING_REGISTER_FILES2: u32 = 13;
const IORING_REGISTER_FILES_UPDATE2: u32 = 14;
const IORING_REGISTER_BUFFERS2: u32 = 15;
const IORING_REGISTER_BUFFERS_UPDATE: u32 = 16;
const IORING_REGISTER_IOWQ_AFF: u32 = 17;
const IORING_UNREGISTER_IOWQ_AFF: u32 = 18;
const IORING_REGISTER_IOWQ_MAX_WORKERS: u32 = 19;
const IORING_REGISTER_RING_FDS: u32 = 20;
const IORING_UNREGISTER_RING_FDS: u32 = 21;
const IORING_REGISTER_PBUF_RING: u32 = 22;
const IORING_UNREGISTER_PBUF_RING: u32 = 23;
const IORING_REGISTER_SYNC_CANCEL: u32 = 24;
const IORING_REGISTER_FILE_ALLOC_RANGE: u32 = 25;
const IORING_REGISTER_PBUF_STATUS: u32 = 26;
const IORING_REGISTER_NAPI: u32 = 27;
const IORING_UNREGISTER_NAPI: u32 = 28;
const IORING_REGISTER_CLOCK: u32 = 29;
const IORING_REGISTER_CLONE_BUFFERS: u32 = 30;
const IORING_REGISTER_SEND_MSG_RING: u32 = 31;
const IORING_REGISTER_ZCRX_IFQ: u32 = 32;
const IORING_REGISTER_RESIZE_RINGS: u32 = 33;
const IORING_REGISTER_MEM_REGION: u32 = 34;
const IORING_REGISTER_QUERY: u32 = 35;
const IORING_REGISTER_ZCRX_CTRL: u32 = 36;
const IORING_REGISTER_BPF_FILTER: u32 = 37;
const IORING_REGISTER_USE_REGISTERED_RING: u32 = 1 << 31;

/// Linux v7.2.3 maximum classic registered-buffer slots.
pub const IORING_MAX_REGISTERED_BUFFERS: u32 = 1 << 14;
/// Linux v7.2.3 maximum operation records accepted by `REGISTER_PROBE`.
pub const IORING_MAX_PROBE_OPERATIONS: u32 = 256;
/// Linux v7.2.3 maximum restriction records accepted by
/// `REGISTER_RESTRICTIONS` (`io_uring/register.c:116-118`).
pub const IORING_MAX_RESTRICTIONS: u32 = 128;
/// First registration opcode outside the Linux v7.2.3 UAPI enum
/// (`include/uapi/linux/io_uring.h`: `IORING_REGISTER_LAST`).
pub const PINNED_IORING_REGISTER_LAST: u32 = 38;

/// Size of Linux's `struct io_uring_rsrc_update`.
const IO_URING_RSRC_UPDATE_BYTES: usize = 16;
/// Size of Linux's `struct io_uring_rsrc_register` and
/// `struct io_uring_rsrc_update2`.
const IO_URING_RSRC_REGISTER_BYTES: usize = 32;
/// Size of Linux's `struct zcrx_ctrl` and `struct io_uring_bpf`: a 8-byte
/// prefix plus a 64-byte payload (`include/uapi/linux/io_uring/zcrx.h:137`,
/// `include/uapi/linux/io_uring/bpf_filter.h:75`).
const IO_URING_CONTROL_RECORD_BYTES: usize = 72;
/// `IORING_OP_LAST` (`include/uapi/linux/io_uring.h`): the first opcode a BPF
/// filter record may not name (`io_uring/bpf_filter.c:326-327`).
const IO_URING_OP_LAST: u32 = 65;
/// `BPF_MAXINSNS` (`include/uapi/linux/bpf_common.h:54`).
const BPF_MAXINSNS: u32 = 4096;
/// `IO_URING_BPF_FILTER_DENY_REST | IO_URING_BPF_FILTER_SZ_STRICT`
/// (`io_uring/bpf_filter.c:310-311`).
const IO_URING_BPF_FILTER_FLAGS: u32 = 3;
/// `IO_URING_BPF_CMD_FILTER` (`include/uapi/linux/io_uring/bpf_filter.h:72`).
const IO_URING_BPF_CMD_FILTER: u16 = 1;
/// Size of Linux's `struct io_uring_task_restriction` without its inline
/// restriction array (`include/uapi/linux/io_uring.h:831-836`).
const IO_URING_TASK_RESTRICTION_BYTES: usize = 16;

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
    /// Copy one `__s32` descriptor from `argument` and deliver a signal to it
    /// after every successfully published completion.
    ///
    /// Linux reads the descriptor through `copy_from_user()`; the value is not
    /// passed by value (`io_uring/eventfd.c:120-127`).
    RegisterEventFd {
        /// Userspace address of the `__s32` descriptor.
        address: u64,
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
    /// Start admitting submissions on an `R_DISABLED` ring.
    EnableRings,
    /// An empty `REGISTER_QUERY` chain: `io_query()` walks a NULL head and
    /// returns zero without touching either argument
    /// (`io_uring/query.c:125-131`).
    QueryEmpty,
    /// A Linux-valid registration opcode whose body this profile cannot
    /// service.  The adapter applies `UnsupportedRegistration`'s pre-body
    /// header rules and then answers `-EOPNOTSUPP`.
    Unsupported(UnsupportedRegistration),
}

/// A Linux-valid registration opcode whose body this profile does not
/// implement, carrying the exact pre-body shape Linux applies first.
///
/// `io_uring_register()` validates the `arg`/`nr_args` header inside its
/// dispatcher before the opcode body runs (`io_uring/register.c:774-976`), and
/// several bodies copy a fixed-size request record before validating it.  A
/// nil pointer and a malformed record are therefore `-EFAULT`/`-EINVAL` on
/// Linux, not `-EOPNOTSUPP`, so the adapter must reproduce that answer before
/// falling back to this profile's unsupported-operation error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsupportedRegistration {
    opcode: u32,
    argument: u64,
    count: u32,
    header: RegistrationHeader,
}

impl UnsupportedRegistration {
    /// Raw registration opcode without the registered-ring selector.
    pub const fn opcode(self) -> u32 {
        self.opcode
    }

    /// Raw userspace record address the caller supplied.
    pub const fn argument(self) -> u64 {
        self.argument
    }

    /// The unvalidated `nr_args` value the caller supplied.
    pub const fn count(self) -> u32 {
        self.count
    }

    /// The fixed-size request record Linux copies before its body runs.
    pub const fn header(self) -> RegistrationHeader {
        self.header
    }

    /// Applies Linux's own field validation to `header`'s copied record.
    ///
    /// The adapter copies exactly [`RegistrationHeader::bytes`] bytes from the
    /// caller's argument (mapping a fault to `-EFAULT`) before calling this.
    pub fn validate_header(self, bytes: &[u8]) -> Result<(), IoUringError> {
        self.header.validate(self.count, bytes)
    }

    /// The answer Linux's body gives for a record that passed
    /// [`validate_header`](Self::validate_header).
    pub const fn outcome(self) -> UnsupportedOutcome {
        match self.header {
            // `io_zcrx_ctrl()` looks the validated record's `zcrx_id` up in
            // the ring's zcrx context array and reports -ENXIO when the id is
            // absent (`io_uring/zcrx.c:1437-1439`).  Only
            // `IORING_REGISTER_ZCRX_IFQ` can populate that array
            // (`io_uring/zcrx.c:884-895`), and this profile refuses it, so
            // every validated control request names an unknown id.
            RegistrationHeader::ZcrxControl => UnsupportedOutcome::ZcrxIdNotFound,
            // Every other record reaches a body that needs a primitive this
            // profile does not implement.
            _ => UnsupportedOutcome::Unsupported,
        }
    }
}

/// What Linux's registration body answers for a record this profile cannot
/// service but can fully validate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsupportedOutcome {
    /// The feature does not exist here.
    Unsupported,
    /// The record is valid and names a zcrx id this ring cannot have.
    ZcrxIdNotFound,
}

/// Fixed-size request record Linux copies before a registration body runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationHeader {
    /// No usercopy precedes the body: `-EOPNOTSUPP` is the first answer this
    /// profile can give.
    None,
    /// `__s32` eventfd descriptor (`io_uring/eventfd.c:120-127`).
    EventFdDescriptor,
    /// `struct io_uring_rsrc_update`, copied by
    /// `io_register_files_update()` before its `resv` check
    /// (`io_uring/rsrc.c:439-453`).
    RsrcUpdate,
    /// `struct io_uring_rsrc_register`, copied by `io_register_rsrc()` after
    /// its `nr_args` size check (`io_uring/rsrc.c:395-418`).
    RsrcRegister,
    /// `struct io_uring_rsrc_update2`, copied by `io_register_rsrc_update()`
    /// after its `nr_args` size check (`io_uring/rsrc.c:420-437`).
    RsrcUpdate2,
    /// `struct zcrx_ctrl`, copied by `io_zcrx_ctrl()`
    /// (`io_uring/zcrx.c:1430-1435`).
    ZcrxControl,
    /// `struct io_uring_bpf`, copied by `io_bpf_filter_import()`
    /// (`io_uring/bpf_filter.c:319-335`).
    BpfFilter,
    /// `struct io_uring_task_restriction`, copied by
    /// `io_register_restrictions_task()` (`io_uring/register.c:218-230`).
    TaskRestriction,
}

impl RegistrationHeader {
    /// Exact bytes Linux transfers before the body's own validation.
    pub const fn bytes(self) -> usize {
        match self {
            Self::None => 0,
            Self::EventFdDescriptor => 4,
            Self::RsrcUpdate => IO_URING_RSRC_UPDATE_BYTES,
            Self::RsrcRegister | Self::RsrcUpdate2 => IO_URING_RSRC_REGISTER_BYTES,
            Self::ZcrxControl | Self::BpfFilter => IO_URING_CONTROL_RECORD_BYTES,
            Self::TaskRestriction => IO_URING_TASK_RESTRICTION_BYTES,
        }
    }

    fn validate(self, count: u32, bytes: &[u8]) -> Result<(), IoUringError> {
        match self {
            Self::None => Ok(()),
            // The eventfd descriptor is an `__s32`; every bit pattern is a
            // valid descriptor number and `eventfd_ctx_fdget()` owns the
            // `-EBADF`/`-EINVAL` verdict.
            Self::EventFdDescriptor => Ok(()),
            Self::RsrcUpdate => {
                //     if (up.resv || up.resv2)
                //             return -EINVAL;
                //
                // (`io_uring/rsrc.c:445-447`).  Only `offset`, `resv` and
                // `data` are copied from `struct io_uring_rsrc_update`; the
                // trailing fields of the wider update2 record are zeroed.
                if read_u32(bytes, 4) != 0 {
                    return Err(IoUringError::InvalidRegistration);
                }
                //     if (check_add_overflow(up->offset, nr_args, &tmp))
                //             return -EOVERFLOW;
                //
                // (`io_uring/rsrc.c:426-428`).
                if read_u32(bytes, 0).checked_add(count).is_none() {
                    return Err(IoUringError::RegistrationRangeOverflow);
                }
                Ok(())
            }
            Self::RsrcRegister => {
                //     if (!rr.nr || rr.resv2)
                //             return -EINVAL;
                //     if (rr.flags & ~IORING_RSRC_REGISTER_SPARSE)
                //             return -EINVAL;
                //
                // (`io_uring/rsrc.c:405-408`), `IORING_RSRC_REGISTER_SPARSE`
                // being bit zero.
                if read_u32(bytes, 0) == 0 || read_u64(bytes, 8) != 0 || read_u32(bytes, 4) & !1 != 0
                {
                    return Err(IoUringError::InvalidRegistration);
                }
                Ok(())
            }
            Self::ZcrxControl => {
                //     if (nr_args)
                //             return -EINVAL;
                //     if (copy_from_user(&ctrl, arg, sizeof(ctrl)))
                //             return -EFAULT;
                //     if (!mem_is_zero(&ctrl.__resv, sizeof(ctrl.__resv)))
                //             return -EFAULT;
                //
                // (`io_uring/zcrx.c:1430-1435`): a non-zero reserved word is
                // reported as a fault, not as a malformed record.
                if read_u64(bytes, 8) != 0 || read_u64(bytes, 16) != 0 {
                    return Err(IoUringError::RegistrationFault);
                }
                Ok(())
            }
            Self::BpfFilter => {
                // `io_bpf_filter_import()` (`io_uring/bpf_filter.c:319-335`).
                // Every field below answers -EINVAL, so the order in which
                // they are judged cannot change the errno.
                if read_u16(bytes, 0) != IO_URING_BPF_CMD_FILTER
                    || read_u16(bytes, 2) != 0
                    || read_u32(bytes, 4) != 0
                    || read_u32(bytes, 8) >= IO_URING_OP_LAST
                    || read_u32(bytes, 12) & !IO_URING_BPF_FILTER_FLAGS != 0
                    || bytes[21] != 0
                    || bytes[22] != 0
                    || bytes[23] != 0
                    || read_u64(bytes, 32) != 0
                    || read_u64(bytes, 40) != 0
                    || read_u64(bytes, 48) != 0
                    || read_u64(bytes, 56) != 0
                    || read_u64(bytes, 64) != 0
                {
                    return Err(IoUringError::InvalidRegistration);
                }
                let length = read_u32(bytes, 16);
                if length == 0 || length > BPF_MAXINSNS {
                    return Err(IoUringError::InvalidRegistration);
                }
                Ok(())
            }
            Self::TaskRestriction => {
                //     if (tres.flags)
                //             return -EINVAL;
                //     if (!mem_is_zero(tres.resv, sizeof(tres.resv)))
                //             return -EINVAL;
                //     ret = io_parse_restrictions(ures->restrictions, tres.nr_res, res);
                //
                // (`io_uring/register.c:224-235`) with the nested
                // `nr_args > IORING_MAX_RESTRICTIONS` check of
                // `io_parse_restrictions()` (`io_uring/register.c:116-118`).
                // The restriction records follow the copied header inline, so
                // their own validation is the unimplemented body's job.
                if read_u16(bytes, 0) != 0
                    || read_u32(bytes, 4) != 0
                    || read_u32(bytes, 8) != 0
                    || read_u32(bytes, 12) != 0
                    || read_u16(bytes, 2) as u32 > IORING_MAX_RESTRICTIONS
                {
                    return Err(IoUringError::InvalidRegistration);
                }
                Ok(())
            }
            Self::RsrcUpdate2 => {
                //     if (!up.nr || up.resv || up.resv2)
                //             return -EINVAL;
                //
                // (`io_uring/rsrc.c:462-464`).
                let nr = read_u32(bytes, 24);
                if nr == 0 || read_u32(bytes, 4) != 0 || read_u32(bytes, 28) != 0 {
                    return Err(IoUringError::InvalidRegistration);
                }
                //     if (check_add_overflow(up->offset, nr_args, &tmp))
                //             return -EOVERFLOW;
                //
                // (`io_uring/rsrc.c:426-428`) with the record's own `nr`.
                if read_u32(bytes, 0).checked_add(nr).is_none() {
                    return Err(IoUringError::RegistrationRangeOverflow);
                }
                Ok(())
            }
        }
    }
}

/// Which Linux entry point a copied registration header is dispatched from.
///
/// The ring-less entry is not a subset of the ring one: `IORING_REGISTER_
/// RESTRICTIONS` reads a `struct io_uring_task_restriction` there
/// (`io_uring/register.c:201-240`) and a `struct io_uring_restriction` array
/// through the ring (`io_uring/register.c:167-196`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationDispatch {
    /// Dispatched through `__io_uring_register()` with a resolved ring.
    Ring,
    /// Dispatched through `io_uring_register_blind()` with `fd == -1`
    /// (`io_uring/register.c:1029-1030`).
    Blind,
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

    /// Whether this opcode is dispatched without a ring.
    ///
    /// ```text
    /// if (fd == -1)
    ///         return io_uring_register_blind(opcode, arg, nr_args);
    /// ```
    ///
    /// (`io_uring/register.c:1029-1030`), where the blind entry handles
    /// `IORING_REGISTER_SEND_MSG_RING`, `IORING_REGISTER_QUERY`,
    /// `IORING_REGISTER_RESTRICTIONS` and `IORING_REGISTER_BPF_FILTER` and
    /// answers every other opcode with `-EINVAL`
    /// (`io_uring/register.c:998-1013`).
    pub const fn blind(self) -> bool {
        matches!(
            self.opcode & !IORING_REGISTER_USE_REGISTERED_RING,
            IORING_REGISTER_RESTRICTIONS
                | IORING_REGISTER_SEND_MSG_RING
                | IORING_REGISTER_QUERY
                | IORING_REGISTER_BPF_FILTER
        )
    }

    /// Performs only syscall-envelope classification.  Linux rejects an
    /// opcode outside the v7.2.3 registration enum before it resolves `fd`,
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
    ///
    /// Every arm reproduces the `arg`/`nr_args` verdict of Linux's dispatcher
    /// (`io_uring/register.c:774-976`) or of the opcode body's own first
    /// check, so an opcode this profile cannot service still answers a
    /// malformed header with Linux's `-EINVAL` instead of `-EOPNOTSUPP`.
    pub const fn decode(
        self,
        dispatch: RegistrationDispatch,
    ) -> Result<RegistrationOperation, IoUringError> {
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
                //     ret = -EINVAL;
                //     if (arg || nr_args)
                //             break;
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
                //     case IORING_REGISTER_EVENTFD:
                //             ret = -EINVAL;
                //             if (nr_args != 1)
                //                     break;
                //             ret = io_eventfd_register(ctx, arg, 0);
                //
                // (`io_uring/register.c:801-805`).  The descriptor travels as
                // a pointer to `__s32`, not by value.
                if self.count != 1 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::RegisterEventFd {
                        address: self.argument,
                    })
                }
            }
            IORING_REGISTER_EVENTFD_ASYNC => {
                // Same header shape as `IORING_REGISTER_EVENTFD`
                // (`io_uring/register.c:807-811`); only the delivery context
                // of the signal differs, and that body is not implemented.
                if self.count != 1 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::EventFdDescriptor))
                }
            }
            IORING_UNREGISTER_EVENTFD => {
                if self.argument != 0 || self.count != 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::UnregisterEventFd)
                }
            }
            IORING_REGISTER_FILES_UPDATE => {
                //     if (!nr_args)
                //             return -EINVAL;
                //     ...copy_from_user(&up, arg, sizeof(struct io_uring_rsrc_update))...
                //
                // (`io_uring/rsrc.c:439-450`): the count is judged before the
                // record is read, so a zero count outranks a nil pointer.
                if self.count == 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::RsrcUpdate))
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
            IORING_REGISTER_PERSONALITY => {
                //     ret = -EINVAL;
                //     if (arg || nr_args)
                //             break;
                //     ret = io_register_personality(ctx);
                //
                // (`io_uring/register.c:825-829`).
                if self.argument != 0 || self.count != 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_UNREGISTER_PERSONALITY => {
                //     ret = -EINVAL;
                //     if (arg)
                //             break;
                //     ret = io_unregister_personality(ctx, nr_args);
                //
                // (`io_uring/register.c:831-835`): `nr_args` selects the
                // personality id and is not otherwise restricted.
                if self.argument != 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_REGISTER_RESTRICTIONS => match dispatch {
                RegistrationDispatch::Ring => {
                    // `io_parse_restrictions()` owns the header:
                    //
                    //     if (!arg || nr_args > IORING_MAX_RESTRICTIONS)
                    //             return -EINVAL;
                    //
                    // (`io_uring/register.c:116-118`).  The state-dependent
                    // `-EBADFD`/`-EBUSY` prefix of `io_register_restrictions()`
                    // (`io_uring/register.c:167-183`) is not modelled here.
                    if self.argument == 0 || self.count > IORING_MAX_RESTRICTIONS {
                        Err(IoUringError::InvalidRegistration)
                    } else {
                        Ok(self.unsupported(opcode, RegistrationHeader::None))
                    }
                }
                RegistrationDispatch::Blind => {
                    //     if (nr_args != 1)
                    //             return -EINVAL;
                    //     if (copy_from_user(&tres, arg, sizeof(tres)))
                    //             return -EFAULT;
                    //     if (tres.flags)
                    //             return -EINVAL;
                    //     if (!mem_is_zero(tres.resv, sizeof(tres.resv)))
                    //             return -EINVAL;
                    //     ...
                    //     ret = io_parse_restrictions(ures->restrictions, tres.nr_res, res);
                    //
                    // (`io_uring/register.c:201-235`), where the trailing
                    // `io_parse_restrictions()` re-checks only
                    // `nr_args > IORING_MAX_RESTRICTIONS`
                    // (`io_uring/register.c:116-118`).
                    if self.count != 1 {
                        Err(IoUringError::InvalidRegistration)
                    } else {
                        Ok(self.unsupported(opcode, RegistrationHeader::TaskRestriction))
                    }
                }
            },
            IORING_REGISTER_ENABLE_RINGS => {
                if self.argument != 0 || self.count != 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::EnableRings)
                }
            }
            IORING_REGISTER_FILES2 | IORING_REGISTER_BUFFERS2 => {
                // `io_register_rsrc()` passes `nr_args` as the size of
                // `struct io_uring_rsrc_register`:
                //
                //     if (size != sizeof(rr))
                //             return -EINVAL;
                //
                // (`io_uring/rsrc.c:398-400`), before the copy and the
                // record's own `nr`/`resv2`/`flags` checks.
                if self.count != IO_URING_RSRC_REGISTER_BYTES as u32 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::RsrcRegister))
                }
            }
            IORING_REGISTER_FILES_UPDATE2 | IORING_REGISTER_BUFFERS_UPDATE => {
                //     if (size != sizeof(up))
                //             return -EINVAL;
                //
                // (`io_uring/rsrc.c:455-457`), before the copy and the
                // record's own `nr`/`resv`/`resv2` checks.
                if self.count != IO_URING_RSRC_REGISTER_BYTES as u32 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::RsrcUpdate2))
                }
            }
            IORING_REGISTER_IOWQ_AFF => {
                //     ret = -EINVAL;
                //     if (!arg || !nr_args)
                //             break;
                //
                // (`io_uring/register.c:860-864`).
                if self.argument == 0 || self.count == 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_UNREGISTER_IOWQ_AFF => {
                //     ret = -EINVAL;
                //     if (arg || nr_args)
                //             break;
                //
                // (`io_uring/register.c:866-870`).
                if self.argument != 0 || self.count != 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_REGISTER_IOWQ_MAX_WORKERS => {
                //     ret = -EINVAL;
                //     if (!arg || nr_args != 2)
                //             break;
                //
                // (`io_uring/register.c:872-876`).
                if self.argument == 0 || self.count != 2 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_REGISTER_RING_FDS => {
                //     if (!nr_args || nr_args > IO_RINGFD_REG_MAX)
                //             return -EINVAL;
                //
                // (`io_uring/tctx.c:325-326`).  The record array is read one
                // `struct io_uring_rsrc_update` at a time, so a NULL array is
                // `-EFAULT` rather than `-EINVAL`; the syscall adapter owns
                // that copy and this profile's ring-fd table.
                if self.count == 0 || self.count as usize > IORING_MAX_RING_FDS {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::RegisterRingFds {
                        argument: self.argument,
                        count: self.count,
                    })
                }
            }
            IORING_UNREGISTER_RING_FDS => {
                //     if (!nr_args || nr_args > IO_RINGFD_REG_MAX)
                //             return -EINVAL;
                //
                // (`io_uring/tctx.c:384-385`).  A task without an io_uring
                // context unregisters nothing and never reads the array
                // (`io_uring/tctx.c:386-387`), so the adapter owns both the
                // zero-count no-op and the `-EFAULT` of a nil array.
                if self.count == 0 || self.count as usize > IORING_MAX_RING_FDS {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(RegistrationOperation::UnregisterRingFds {
                        argument: self.argument,
                        count: self.count,
                    })
                }
            }
            IORING_REGISTER_PBUF_RING | IORING_REGISTER_PBUF_STATUS => {
                //     ret = -EINVAL;
                //     if (!arg || nr_args != 1)
                //             break;
                //
                // (`io_uring/register.c:884-888`, `:908-912`).
                if self.argument == 0 || self.count != 1 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_UNREGISTER_PBUF_RING => {
                //     ret = -EINVAL;
                //     if (!arg || nr_args != 1)
                //             break;
                //
                // (`io_uring/register.c:890-894`).
                if self.argument == 0 || self.count != 1 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_REGISTER_SYNC_CANCEL => {
                //     ret = -EINVAL;
                //     if (!arg || nr_args != 1)
                //             break;
                //
                // (`io_uring/register.c:896-900`).
                if self.argument == 0 || self.count != 1 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_REGISTER_FILE_ALLOC_RANGE => {
                //     ret = -EINVAL;
                //     if (!arg || nr_args)
                //             break;
                //
                // (`io_uring/register.c:902-906`).
                if self.argument == 0 || self.count != 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_REGISTER_NAPI => {
                //     ret = -EINVAL;
                //     if (!arg || nr_args != 1)
                //             break;
                //
                // (`io_uring/register.c:914-918`).
                if self.argument == 0 || self.count != 1 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_UNREGISTER_NAPI => {
                //     ret = -EINVAL;
                //     if (nr_args != 1)
                //             break;
                //
                // (`io_uring/register.c:920-924`): `arg` is the NAPI id and
                // is deliberately unrestricted.
                if self.count != 1 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_REGISTER_CLOCK => {
                //     ret = -EINVAL;
                //     if (!arg || nr_args)
                //             break;
                //
                // (`io_uring/register.c:926-930`).
                if self.argument == 0 || self.count != 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_REGISTER_CLONE_BUFFERS | IORING_REGISTER_ZCRX_IFQ => {
                //     ret = -EINVAL;
                //     if (!arg || nr_args != 1)
                //             break;
                //
                // (`io_uring/register.c:932-936`, `:938-942`).
                if self.argument == 0 || self.count != 1 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_REGISTER_SEND_MSG_RING => {
                //     if (!arg || nr_args != 1)
                //             return -EINVAL;
                //
                // (`io_uring/register.c:979-980`): the blind `MSG_RING` entry
                // rejects a nil record itself, so `-EINVAL` outranks the
                // `-EFAULT` a copy would raise.
                if self.argument == 0 || self.count != 1 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_REGISTER_RESIZE_RINGS => {
                //     ret = -EINVAL;
                //     if (!arg || nr_args != 1)
                //             break;
                //
                // (`io_uring/register.c:944-948`).
                if self.argument == 0 || self.count != 1 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
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
            IORING_REGISTER_QUERY => {
                //     int io_query(void __user *arg, unsigned nr_args)
                //     {
                //             ...
                //             if (nr_args)
                //                     return -EINVAL;
                //             while (uhdr) { ... }
                //             return 0;
                //     }
                //
                // (`io_uring/query.c:125-131`): a zero count with a NULL chain
                // head is a successful no-op, while a non-empty chain needs the
                // query registry this profile does not implement.
                if self.count != 0 {
                    Err(IoUringError::InvalidRegistration)
                } else if self.argument == 0 {
                    Ok(RegistrationOperation::QueryEmpty)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::None))
                }
            }
            IORING_REGISTER_ZCRX_CTRL => {
                //     if (nr_args)
                //             return -EINVAL;
                //     if (copy_from_user(&ctrl, arg, sizeof(ctrl)))
                //             return -EFAULT;
                //
                // (`io_uring/zcrx.c:1430-1432`): the dispatcher adds no header
                // of its own (`io_uring/register.c:959-961`), and the record
                // is read and field-checked before the zcrx lookup.
                if self.count != 0 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::ZcrxControl))
                }
            }
            IORING_REGISTER_BPF_FILTER => {
                //     ret = -EINVAL;
                //     if (nr_args != 1)
                //             break;
                //     ret = io_register_bpf_filter(&ctx->restrictions, arg);
                //
                // (`io_uring/register.c:962-970`): a zero count is enough to
                // reject the filter record, and the record itself is then read
                // and field-checked by `io_bpf_filter_import()`.
                if self.count != 1 {
                    Err(IoUringError::InvalidRegistration)
                } else {
                    Ok(self.unsupported(opcode, RegistrationHeader::BpfFilter))
                }
            }
            _ => Err(IoUringError::UnknownRegistration),
        }
    }

    /// Records one known-but-unimplemented opcode and its pre-body header.
    const fn unsupported(self, opcode: u32, header: RegistrationHeader) -> RegistrationOperation {
        RegistrationOperation::Unsupported(UnsupportedRegistration {
            opcode,
            argument: self.argument,
            count: self.count,
            header,
        })
    }
}

/// Linux v7.2.3's `IO_RINGFD_REG_MAX` (`io_uring/tctx.h`).
pub const IORING_MAX_RING_FDS: usize = 16;

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_file_headers_are_strictly_validated() {
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_FILES, 0x1000, 4).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::RegisterFiles {
                argument: 0x1000,
                count: 4
            })
        );
        // A NULL array is the dispatcher's -EFAULT and a zero count is
        // `io_sqe_files_register()`'s -EINVAL, so neither is collapsed into a
        // malformed-header error here.
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_FILES, 0, 4).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::RegisterFiles {
                argument: 0,
                count: 4
            })
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_FILES, 0x1000, 0).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::RegisterFiles {
                argument: 0x1000,
                count: 0
            })
        );
        assert_eq!(
            RegistrationRequest::new(IORING_UNREGISTER_FILES, 0, 0).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::UnregisterFiles)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_UNREGISTER_FILES, 1, 0).decode(RegistrationDispatch::Ring),
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
                RegistrationRequest::new(IORING_REGISTER_FILES, 0x1000, count).decode(RegistrationDispatch::Ring),
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
            RegistrationRequest::new(IORING_REGISTER_BUFFERS, 0, 1).decode(RegistrationDispatch::Ring),
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
            .decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_BUFFERS, 0, 0).decode(RegistrationDispatch::Ring),
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
            .decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::RegisterBuffers {
                argument: 0,
                count: IORING_MAX_REGISTERED_BUFFERS + 1,
            })
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_BUFFERS, 0x1000, 1).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::RegisterBuffers {
                argument: 0x1000,
                count: 1,
            })
        );
        assert_eq!(
            RegistrationRequest::new(IORING_UNREGISTER_BUFFERS, 1, 0).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_UNREGISTER_BUFFERS, 0, 0).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::UnregisterBuffers)
        );
        let request = RegistrationRequest::new(
            IORING_REGISTER_USE_REGISTERED_RING | IORING_REGISTER_BUFFERS,
            0x1000,
            1,
        );
        assert!(request.use_registered_ring());
        assert_eq!(
            request.decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::RegisterBuffers {
                argument: 0x1000,
                count: 1
            })
        );
    }

    #[test]
    fn probe_and_pinned_opcode_range_need_no_consumer_magic() {
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_PROBE, 0x2000, 256).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::Probe {
                argument: 0x2000,
                operations: 256
            })
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_PROBE, 0x2000, 257).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        // Both v7.2.3 auxiliary opcodes are inside the enum and reach their
        // own body validation.
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_ZCRX_CTRL, 0x1000, 0).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::Unsupported(UnsupportedRegistration {
                opcode: IORING_REGISTER_ZCRX_CTRL,
                argument: 0x1000,
                count: 0,
                header: RegistrationHeader::ZcrxControl,
            }))
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_ZCRX_CTRL, 0x1000, 1).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_BPF_FILTER, 0, 0).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(PINNED_IORING_REGISTER_LAST, 0, 0).decode(RegistrationDispatch::Ring),
            Err(IoUringError::UnknownRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(PINNED_IORING_REGISTER_LAST - 1, 0, 1).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::Unsupported(UnsupportedRegistration {
                opcode: PINNED_IORING_REGISTER_LAST - 1,
                argument: 0,
                count: 1,
                header: RegistrationHeader::BpfFilter,
            }))
        );
    }

    #[test]
    fn eventfd_registration_takes_a_pointer_and_one_count() {
        // `io_uring/register.c:801-805`: `nr_args` must be one and the
        // descriptor is read from the caller's `__s32`.
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_EVENTFD, 0x4000, 1).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::RegisterEventFd { address: 0x4000 })
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_EVENTFD, 0x4000, 0).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_EVENTFD, 5, 1).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::RegisterEventFd { address: 5 })
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_EVENTFD_ASYNC, 0x4000, 1).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::Unsupported(UnsupportedRegistration {
                opcode: IORING_REGISTER_EVENTFD_ASYNC,
                argument: 0x4000,
                count: 1,
                header: RegistrationHeader::EventFdDescriptor,
            }))
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_EVENTFD_ASYNC, 0x4000, 2).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
    }

    #[test]
    fn unsupported_opcodes_still_answer_malformed_headers_with_einval() {
        // `io_uring/register.c:825-835`: personality registration and
        // unregistration reject stray `arg`/`nr_args` before any body work.
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_PERSONALITY, 1, 0).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_UNREGISTER_PERSONALITY, 1, 0).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        // `io_uring/register.c:884-912`: the pbuf, sync-cancel and napi
        // entries require one argument and, where applicable, one count.
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_PBUF_RING, 0x1000, 0).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_PBUF_RING, 0x1000, 1).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::Unsupported(UnsupportedRegistration {
                opcode: IORING_REGISTER_PBUF_RING,
                argument: 0x1000,
                count: 1,
                header: RegistrationHeader::None,
            }))
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_FILE_ALLOC_RANGE, 0x1000, 1).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_UNREGISTER_NAPI, 0, 0).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        // `io_uring/rsrc.c:398-400` and `:455-457` pass `nr_args` as the
        // record size, so only the exact struct size reaches the copy.
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_FILES2, 0x1000, 16).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_FILES2, 0x1000, 32).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::Unsupported(UnsupportedRegistration {
                opcode: IORING_REGISTER_FILES2,
                argument: 0x1000,
                count: 32,
                header: RegistrationHeader::RsrcRegister,
            }))
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_BUFFERS_UPDATE, 0x1000, 32).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::Unsupported(UnsupportedRegistration {
                opcode: IORING_REGISTER_BUFFERS_UPDATE,
                argument: 0x1000,
                count: 32,
                header: RegistrationHeader::RsrcUpdate2,
            }))
        );
        // `io_uring/rsrc.c:439-443`: a zero count outranks the nil record.
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_FILES_UPDATE, 0, 0).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_FILES_UPDATE, 0, 1).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::Unsupported(UnsupportedRegistration {
                opcode: IORING_REGISTER_FILES_UPDATE,
                argument: 0,
                count: 1,
                header: RegistrationHeader::RsrcUpdate,
            }))
        );
    }

    #[test]
    fn unsupported_header_records_reuse_linux_field_rules() {
        let update = UnsupportedRegistration {
            opcode: IORING_REGISTER_FILES_UPDATE,
            argument: 0x1000,
            count: 1,
            header: RegistrationHeader::RsrcUpdate,
        };
        let mut record = [0_u8; IO_URING_RSRC_UPDATE_BYTES];
        assert_eq!(update.validate_header(&record), Ok(()));
        // `if (up.resv || up.resv2) return -EINVAL;`
        record[4] = 1;
        assert_eq!(
            update.validate_header(&record),
            Err(IoUringError::InvalidRegistration)
        );
        record[4] = 0;
        // `if (check_add_overflow(up->offset, nr_args, &tmp)) return -EOVERFLOW;`
        record[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            update.validate_header(&record),
            Err(IoUringError::RegistrationRangeOverflow)
        );

        let register = UnsupportedRegistration {
            opcode: IORING_REGISTER_FILES2,
            argument: 0x1000,
            count: 32,
            header: RegistrationHeader::RsrcRegister,
        };
        let mut record = [0_u8; IO_URING_RSRC_REGISTER_BYTES];
        // `if (!rr.nr || rr.resv2) return -EINVAL;`
        assert_eq!(
            register.validate_header(&record),
            Err(IoUringError::InvalidRegistration)
        );
        record[0..4].copy_from_slice(&4_u32.to_le_bytes());
        assert_eq!(register.validate_header(&record), Ok(()));
        // `if (rr.flags & ~IORING_RSRC_REGISTER_SPARSE) return -EINVAL;`
        record[4..8].copy_from_slice(&2_u32.to_le_bytes());
        assert_eq!(
            register.validate_header(&record),
            Err(IoUringError::InvalidRegistration)
        );
        record[4..8].copy_from_slice(&1_u32.to_le_bytes());
        assert_eq!(register.validate_header(&record), Ok(()));

        let update2 = UnsupportedRegistration {
            opcode: IORING_REGISTER_BUFFERS_UPDATE,
            argument: 0x1000,
            count: 32,
            header: RegistrationHeader::RsrcUpdate2,
        };
        let mut record = [0_u8; IO_URING_RSRC_REGISTER_BYTES];
        assert_eq!(
            update2.validate_header(&record),
            Err(IoUringError::InvalidRegistration)
        );
        record[24..28].copy_from_slice(&2_u32.to_le_bytes());
        assert_eq!(update2.validate_header(&record), Ok(()));
        record[28..32].copy_from_slice(&1_u32.to_le_bytes());
        assert_eq!(
            update2.validate_header(&record),
            Err(IoUringError::InvalidRegistration)
        );
    }

    #[test]
    fn auxiliary_control_records_reuse_linux_field_rules() {
        // `io_uring/zcrx.c:1430-1435`: the reserved words answer -EFAULT, and
        // every validated record resolves to the absent zcrx id.
        let zcrx = UnsupportedRegistration {
            opcode: IORING_REGISTER_ZCRX_CTRL,
            argument: 0x1000,
            count: 0,
            header: RegistrationHeader::ZcrxControl,
        };
        let mut record = [0_u8; IO_URING_CONTROL_RECORD_BYTES];
        assert_eq!(zcrx.validate_header(&record), Ok(()));
        assert_eq!(zcrx.outcome(), UnsupportedOutcome::ZcrxIdNotFound);
        record[8] = 1;
        assert_eq!(
            zcrx.validate_header(&record),
            Err(IoUringError::RegistrationFault)
        );
        record[8] = 0;
        record[23] = 1;
        assert_eq!(
            zcrx.validate_header(&record),
            Err(IoUringError::RegistrationFault)
        );
        // The union payload after the reserved words is not a reserved field:
        // the zcrx lookup precedes any use of it.
        record[23] = 0;
        record[24] = 1;
        assert_eq!(zcrx.validate_header(&record), Ok(()));

        // `io_uring/bpf_filter.c:319-335`.
        let filter = UnsupportedRegistration {
            opcode: IORING_REGISTER_BPF_FILTER,
            argument: 0x1000,
            count: 1,
            header: RegistrationHeader::BpfFilter,
        };
        let mut record = [0_u8; IO_URING_CONTROL_RECORD_BYTES];
        assert_eq!(
            filter.validate_header(&record),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(filter.outcome(), UnsupportedOutcome::Unsupported);
        record[0..2].copy_from_slice(&1_u16.to_le_bytes());
        // `if (!reg->filter.filter_len || reg->filter.filter_len > BPF_MAXINSNS)`
        assert_eq!(
            filter.validate_header(&record),
            Err(IoUringError::InvalidRegistration)
        );
        record[16..20].copy_from_slice(&1_u32.to_le_bytes());
        assert_eq!(filter.validate_header(&record), Ok(()));
        // `if (reg->filter.opcode >= IORING_OP_LAST)`
        record[8..12].copy_from_slice(&IO_URING_OP_LAST.to_le_bytes());
        assert_eq!(
            filter.validate_header(&record),
            Err(IoUringError::InvalidRegistration)
        );
        record[8..12].copy_from_slice(&1_u32.to_le_bytes());
        // `if (reg->filter.flags & ~IO_URING_BPF_FILTER_FLAGS)`
        record[12..16].copy_from_slice(&4_u32.to_le_bytes());
        assert_eq!(
            filter.validate_header(&record),
            Err(IoUringError::InvalidRegistration)
        );
        record[12..16].copy_from_slice(&3_u32.to_le_bytes());
        assert_eq!(filter.validate_header(&record), Ok(()));
        // `if (!mem_is_zero(reg->filter.resv2, sizeof(reg->filter.resv2)))`
        record[64] = 1;
        assert_eq!(
            filter.validate_header(&record),
            Err(IoUringError::InvalidRegistration)
        );
    }

    #[test]
    fn blind_registration_entries_are_a_fixed_four_opcode_set() {
        for opcode in [
            IORING_REGISTER_RESTRICTIONS,
            IORING_REGISTER_SEND_MSG_RING,
            IORING_REGISTER_QUERY,
            IORING_REGISTER_BPF_FILTER,
        ] {
            assert!(RegistrationRequest::new(opcode, 0, 0).blind());
        }
        for opcode in [
            IORING_REGISTER_BUFFERS,
            IORING_REGISTER_FILES,
            IORING_REGISTER_PROBE,
            IORING_REGISTER_MEM_REGION,
            IORING_REGISTER_ZCRX_CTRL,
        ] {
            assert!(!RegistrationRequest::new(opcode, 0, 0).blind());
        }
        // `io_uring/query.c:125-131`: an empty chain is a successful no-op.
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_QUERY, 0, 0).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::QueryEmpty)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_QUERY, 0x1000, 0).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::Unsupported(UnsupportedRegistration {
                opcode: IORING_REGISTER_QUERY,
                argument: 0x1000,
                count: 0,
                header: RegistrationHeader::None,
            }))
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_QUERY, 0x1000, 1).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        // `io_uring/register.c:979-980`: the blind MSG_RING record rejects a
        // nil argument with -EINVAL rather than faulting.
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_SEND_MSG_RING, 0, 1).decode(RegistrationDispatch::Ring),
            Err(IoUringError::InvalidRegistration)
        );
        assert_eq!(
            RegistrationRequest::new(IORING_REGISTER_SEND_MSG_RING, 0x1000, 1).decode(RegistrationDispatch::Ring),
            Ok(RegistrationOperation::Unsupported(UnsupportedRegistration {
                opcode: IORING_REGISTER_SEND_MSG_RING,
                argument: 0x1000,
                count: 1,
                header: RegistrationHeader::None,
            }))
        );
    }
}
