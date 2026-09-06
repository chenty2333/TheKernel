//! Linux-visible pathname, discretionary-access, and setattr policy over a
//! generic VFS.
//!
//! This crate owns no filesystem tree and never selects a current task. A
//! kernel supplies stable location handles, one immutable credential snapshot,
//! and a generic walker. The types here describe the observable Linux policy
//! that the walker and mutation backend must enforce during that operation.

#![no_std]
#![warn(missing_docs)]

mod context;
mod dac;
mod fiemap;
mod linux_abi;
mod path;
mod setattr;
mod transaction;

pub use context::{PathContext, PathContextError};
pub use dac::{
    Access, CreateAttributes, DacCapability, DacCredentials, DacError, HardlinkCredentials,
    NodeKind, NodeMetadata, check_dac, check_directory_mutation, check_hardlink_source,
    check_sticky_mutation, initial_create_attributes,
};
pub use fiemap::{
    FIEMAP_MAX_EXTENTS, FIEMAP_STREAM_BATCH_EXTENTS, FIEMAP_SUPPORTED_FLAGS, Fiemap, FiemapExtent,
    FiemapExtentState, FiemapRequestError,
};
pub use linux_abi::{
    AT_EMPTY_PATH, AT_SYMLINK_NOFOLLOW, CacheStat, CachestatAdmissionError, CachestatPageRange,
    CachestatRange, FILE_AT_FLAGS, FILE_ATTR_SIZE_LATEST, FILE_ATTR_SIZE_VER0,
    FILE_ATTR_XFLAGS_MASK, Fadvise, FadvisePlan, FileAttr, FileRange, LinuxVfsError, MemfdSeals,
    QuotaUsage, StructCopyDirection, StructCopyPlan, VERSIONED_FILE_ABI_MAX_SIZE,
    XATTR_ARGS_SIZE_LATEST, XATTR_ARGS_SIZE_VER0, XATTR_CREATE, XATTR_REPLACE, XATTR_SET_FLAGS,
    XattrArgs, XattrValuePlan, cachestat_write_open, file_getattr_copy_plan,
    file_setattr_copy_plan, getxattrat_copy_plan, setxattrat_copy_plan,
    validate_cachestat_admission, validate_file_at_flags, validate_file_setattr_xflags,
    validate_getxattr_flags, validate_setxattr_flags,
};
pub use path::{
    LimitKind, Openat2Policy, PathLimitError, PathLimits, ResolveFlags, ResolveFlagsError,
    TopologyEvent, TraversalAction, WalkBudget, WalkError,
};
pub use setattr::{
    ChmodRequest, ChmodSetattrPlan, ChownRequest, ChownSetattrPlan, PreparedSetattr, SetattrError,
    plan_chmod, plan_chown,
};
pub use transaction::{MutationBackend, MutationTransaction};
