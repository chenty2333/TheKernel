use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    hint::spin_loop,
    mem::ManuallyDrop,
    num::NonZeroUsize,
    ops::Range,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    task::Context,
};

use axalloc::{UsageKind, global_allocator};
use axdriver::{
    AsyncBlockWaitPolicy, prelude::BlockResetOutcome, virtio_async_block_enabled,
    virtio_async_block_wait_policy,
};
#[cfg(not(feature = "ext4"))]
pub use axfs_ng_vfs::PhysicalIoNotSubmittedReason;
use axfs_ng_vfs::{
    AsyncVectoredWriteOutcome, DirNodeOps, FileAttr, FileAttrMutationGuard, FileExtentMap,
    FileIoCancelOutcome, FileIoCompletion, FileIoOpcode, FileIoPolicy, FileIoPrepareError,
    FileIoPublishError, FileIoPublishPayload, FileIoRequest, FileIoRequestAccess, FileIoSyncMode,
    FileIoWritePlacement, FileNode, FileNodeOps, FileRangeRequest, FilesystemOps, FsName, FsPath,
    ImmediateFileIoResult, Location, Mountpoint, NodeFlags, NodePermission, NodeType,
    NowaitAdmission, OwnedFileIoCompletion,
    PhysicalIoNotSubmittedReason as PhysicalIoAttemptNotSubmittedReason, PreparedFileIo,
    PreparedFileIoSubmission, RangeMutation, SubmittedFileIo, SubmittedFileIoControl, VfsError,
    VfsResult, WeakDirEntry, WritebackAnchor,
};
#[cfg(feature = "times")]
use axfs_ng_vfs::{MetadataUpdate, Timestamp};
pub use axfs_ng_vfs::{PhysicalIoAttempt, PhysicalIoSegment};
use axhal::mem::{PhysAddr, VirtAddr, total_ram_size};
#[cfg(target_os = "none")]
use axhal::mem::{phys_to_virt, virt_to_phys};
use axio::{SeekFrom, prelude::*};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
#[cfg(target_os = "none")]
use axsync::Mutex;
use axsync::Mutex as SleepingMutex;
use axtask::WaitQueue;
use intrusive_collections::{LinkedList, LinkedListAtomicLink, intrusive_adapter};
use lru::LruCache;
#[cfg(feature = "ext4")]
use lwext4_rust::PhysicalIoEffect as Ext4PhysicalIoEffect;
#[cfg(feature = "ext4")]
pub use lwext4_rust::{
    PhysicalIoCompletion, PhysicalIoCompletionOutcome, PhysicalIoEffectState,
    PhysicalIoNotSubmittedReason, PhysicalIoOperation, PhysicalIoPendingReason, PhysicalIoPlan,
    PhysicalIoPublication, PhysicalIoPublishOutcome, PhysicalIoSettlement,
};
#[cfg(not(target_os = "none"))]
use spin::Mutex;
use spin::{Once, RwLock};

use super::{FsContext, PathwalkPolicy};

bitflags::bitflags! {
    /// Flags describing the access mode of an opened file.
    #[derive(Debug, Clone, Copy)]
    pub struct FileFlags: u8 {
        /// Read access.
        const READ = 1;
        /// Write access.
        const WRITE = 2;
        /// Execute access.
        const EXECUTE = 4;
        /// Append mode — writes always go to the end of the file.
        const APPEND = 8;
        /// Path-only handle, no actual I/O is permitted.
        const PATH = 16;
        /// Suppress access-time updates on successful reads.
        const NOATIME = 32;
        /// Direct-I/O mode requested by the opener.
        const DIRECT = 64;
    }
}

mod backend;
mod cached_file;
mod cached_file_cache;
mod cached_file_fill;
mod cached_file_io;
mod dirty_writeback;
mod handle;
mod handle_current;
mod handle_positioned;
mod handle_setup;
mod invalidation;
mod open;
mod owned_io;
mod page_cache;
mod physical_effect;
mod pinned;
mod reclaim;
mod registry;
mod shared;

pub(crate) use self::dirty_writeback::*;
pub use self::{
    cached_file::*, handle::*, invalidation::*, open::*, owned_io::*, page_cache::*,
    physical_effect::*, pinned::*, reclaim::*, registry::*, shared::*,
};

#[cfg(test)]
mod tests;
