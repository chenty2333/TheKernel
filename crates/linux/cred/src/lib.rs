//! Allocation-aware credential values for Linux ABI kernels.
//!
//! This crate contains no syscall glue and owns no process, signal, VFS,
//! file-descriptor, socket-transport, memory-management, or usercopy state.
//! Kernel adapters map [`CredError`] into their local errno type and explicitly
//! own namespace synchronization, lifetime admission, and subsystem extensions.

#![no_std]
#![feature(allocator_api)]
#![warn(missing_docs)]

extern crate alloc;

mod content_write;
mod credential;
mod error;
mod exec;
mod file_capability;
mod idmap;
mod key_security;
mod mmap_security;
mod namespace;
mod security;
mod socket_security;
mod transition;

pub use content_write::{
    ContentWriteMode, ContentWriteSetIdAuthority, ContentWriteSetIdCleanup, ContentWriteSetIdPlan,
    plan_content_write_setid_cleanup,
};
pub(crate) use credential::CredentialTransitionMode;
pub use credential::{
    CAPABILITY_VALID_MASK, CAPABILITY_WORDS, CapabilitySets, Credential, CredentialIds,
    CredentialTransitionEffects, FsCredentialSnapshot, GroupInfo, PreparedCredential,
    SECBIT_EXEC_DENY_INTERACTIVE, SECBIT_EXEC_DENY_INTERACTIVE_LOCKED, SECBIT_EXEC_RESTRICT_FILE,
    SECBIT_EXEC_RESTRICT_FILE_LOCKED, SECBIT_KEEP_CAPS, SECBIT_KEEP_CAPS_LOCKED,
    SECBIT_NO_CAP_AMBIENT_RAISE, SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED, SECBIT_NO_SETUID_FIXUP,
    SECBIT_NO_SETUID_FIXUP_LOCKED, SECBIT_NOROOT, SECBIT_NOROOT_LOCKED, SECURE_ALL_BITS,
    SECURE_ALL_LOCKS, SECURE_ALL_UNPRIVILEGED, UserNamespaceView, credential_cap_is_subset,
    ns_capable,
};
pub use error::CredError;
pub use exec::{
    ExecAuxIdentity, ExecCredentialEffects, ExecCredentialInput, ExecCredentialProposal,
    ExecDumpability, ExecFileOwner, ExecImageReadability, ExecMountPrivilege,
    ExecPtraceRevalidation, ExecTraceState, ExecUserNamespaceView, commoncap_exec_transition,
    derive_exec_credential,
};
pub use file_capability::{
    FileCapabilities, SECURITY_CAPABILITY_XATTR_NAME, parse_file_capabilities,
};
pub use idmap::{
    ID_MAP_MAX_EXTENTS, IdMap, IdMapInputExtent, Kgid, Kuid, UserGid, UserUid,
    validate_id_map_input,
};
pub use key_security::{KeyPermission, KeyPermissionMask};
pub use mmap_security::{
    FileMprotectContext, MemoryProtection, MmapAddressContext, MmapFileContext, MmapFileFlags,
    MmapFileOperation, MmapFileSecurityRef, MmapFileTarget,
};
pub use namespace::{
    USER_NAMESPACE_MAX_CREATION_PARENT_LEVEL, USER_NAMESPACE_OVERFLOW_ID, UserNamespaceDomain,
    UserNamespaceMapState,
};
pub use security::{
    AuthorizationError, CapabilityNumber, CapabilitySecurityContext, CapabilitySecurityOperation,
    CredentialPublicationContext, CredentialPublicationOperation, FileOpenAccess, FileOpenContext,
    FileOpenOperation, InodeChmodIntent, InodeChownIntent, InodeCreateContext, InodeCreateMode,
    InodeFileAttrIntent, InodeLinkContext, InodeMkdirContext, InodeMknodContext, InodeMknodKind,
    InodeMknodOperation, InodePermissionAccess, InodePermissionContext, InodePostSetattrContext,
    InodeRenameContext, InodeRmdirContext, InodeSetattrContext, InodeSetattrIntent,
    InodeSetattrMode, InodeSetattrPrivilegeCleanup, InodeSetattrProposal, InodeSymlinkContext,
    InodeTimestampIntent, InodeTimestampValue, InodeUnlinkContext, InodeXattrContext,
    InodeXattrOperation, PreparedCredentialCapabilityContext,
    PreparedCredentialCapabilityOperation, PtraceAccessContext, PtraceAccessKind,
    PtraceCredentialKind, PtraceTracemeContext, SchedulerSecurityContext,
    SchedulerSecurityOperation, SignalCoreAuthorization, SignalCoreAuthorizationReason,
    SignalDeliveryScope, SignalNumber, SignalSecurityContext, SignalSecurityOperation,
    SignalSecuritySource, TaskGetSchedulerContext, XATTR_NAME_MAX, XattrSetFlags, XattrValueClass,
    authorize_capability_core, authorize_prepared_credential_capability_core,
    authorize_signal_core, commoncap_ptrace_access, commoncap_ptrace_traceme, commoncap_scheduler,
};
pub use socket_security::{
    SocketAcceptContext, SocketBindContext, SocketConnectContext, SocketCreateContext,
    SocketCreateSpec, SocketGetOptionContext, SocketGetPeerNameContext, SocketGetSockNameContext,
    SocketListenBacklog, SocketListenContext, SocketOption, SocketPairContext,
    SocketPostCreateContext, SocketReceiveMessageContext, SocketSendMessageContext,
    SocketSetOptionContext, SocketShutdownContext, UnixMaySendContext, UnixStreamConnectContext,
};
pub use transition::{
    CapsetAuthority, CapsetPlan, CapsetRequest, GroupIdAuthority, GroupIdTransitionInput,
    GroupIdTransitionPlan, UserIdAuthority, UserIdTransitionInput, UserIdTransitionPlan,
    plan_capset, plan_group_id_transition, plan_user_id_transition,
};
