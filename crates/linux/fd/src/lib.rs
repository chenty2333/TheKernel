//! Bounded Linux file-descriptor, open-file-description, and readiness state.
//!
//! The crate owns no current task and performs no syscall usercopy. A kernel
//! supplies stable object handles and external synchronization. Descriptor
//! tables require exclusive access for mutation, making the lock and sleep
//! policy explicit in the consumer instead of hiding it in a global singleton.

#![no_std]
#![warn(missing_docs)]

#[cfg(feature = "alloc")]
extern crate alloc;
#[cfg(test)]
extern crate std;

mod eventfd;
mod ofd;
mod pidfd;
mod setfl;
mod table;
mod types;

#[cfg(feature = "alloc")]
mod epoll;
#[cfg(feature = "alloc")]
mod graph;
#[cfg(feature = "alloc")]
mod subscription;

pub use eventfd::{
    EVENTFD_COUNTER_MAX, EventFdError, EventFdPlan, EventFdSnapshot, LeaseError, LeaseId,
    LeasePlan, LeaseSnapshot, LeaseType,
};
pub use ofd::{ExternalOffset, OfdOffsetError, OpenFileDescriptionState};
pub use pidfd::{
    PIDFD_SELF_THREAD, PIDFD_SELF_THREAD_GROUP, PIDFD_SEND_SIGNAL_FLAGS, PIDFD_SIGNAL_PROCESS_GROUP,
    PIDFD_SIGNAL_THREAD, PIDFD_SIGNAL_THREAD_GROUP, PidfdSignalError, PidfdSignalPlan, SignalScope,
    SignalTarget, pidfd_signal_plan,
};
pub use setfl::{
    O_APPEND, O_DIRECT, O_NDELAY, O_NOATIME, O_NONBLOCK, SETFL_MASK, SetFlError, SetFlPlan,
    plan_setfl,
};
pub use table::{
    DescriptorEntry, DescriptorToken, FdTable, FdTableError, PublishError, ReservationToken,
};
pub use types::{
    DescriptorFlags, EpollGraphId, EpollId, FdNumber, FdTableId, InterestMask, InterestMode, OfdId,
    ReadyMask,
};

#[cfg(feature = "alloc")]
pub use epoll::{
    DeliveryCommitError, DeliveryOutcome, DeliveryPreparation, DeliveryToken, EpollCore,
    EpollError, EpollInterest, EpollKey, EpollPublishError, EpollToken, NotifyOutcome, ReadyEvent,
    RescanProgress, RescanToken,
};
#[cfg(feature = "alloc")]
pub use graph::{EpollGraph, EpollGraphLimits, GraphEdgeToken, GraphError, GraphNodeToken};
#[cfg(feature = "alloc")]
pub use subscription::{
    AggregateError, ArmError, CancelState, CommitSubscriptionError, PrepareSubscriptionError,
    PreparedSubscription, RetainedRegistration, Subscription, WatchAccount, WatchChargeError,
};
#[cfg(feature = "alloc")]
pub use table::{
    CancelPreparedError, CloseBatch, CommittedCloseOnExec, PreparePublicationError,
    PreparedCloseOnExec, PreparedPublication,
};
