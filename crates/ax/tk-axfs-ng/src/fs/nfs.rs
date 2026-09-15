//! Clean-room NFSv4.1 client protocol core.
//!
//! The VFS registry must only publish an instance after `negotiate` succeeds:
//! it performs actual ONC RPC, EXCHANGE_ID, CREATE_SESSION and
//! RECLAIM_COMPLETE.  This module has no v2/v3 or server implementation.

use alloc::{
    borrow::ToOwned,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    task::Context,
};

use axfs_ng_vfs::path::{FsName, FsNameBuf};
use axfs_ng_vfs::{
    CreateDisposition, CreateOutcome, DirEntry, DirEntrySink, DirNode, DirNodeOps, FileLock,
    FileNode, FileNodeOps, Filesystem, FilesystemOps, LockOps, Metadata, MetadataUpdate,
    NamedCreateOptions, NodeOps, NodePermission, NodeType, NodeUserData, NowaitAdmission,
    ObjectKey, QuotaOps, QuotaUsage, Reference, RenameRequest, StatFs, Timestamp, UnlinkRequest,
    VfsError, VfsResult, WeakDirEntry, XattrProvider, XattrSetMode,
};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use axsync::Mutex;
use axtask::WaitQueue;

const RPC_CALL: u32 = 0;
const RPC_REPLY: u32 = 1;
const RPC_VERSION: u32 = 2;
const RPC_ACCEPTED: u32 = 0;
const RPC_SUCCESS: u32 = 0;
const AUTH_NONE: u32 = 0;
const AUTH_SYS: u32 = 1;
const RPCSEC_GSS: u32 = 6;
const NFS_PROGRAM: u32 = 100_003;
const NFS_VERSION: u32 = 4;
const COMPOUND: u32 = 1;
const NFS_OK: u32 = 0;
/// The client advertises at most this many COMPOUND operations in its
/// CREATE_SESSION channel attributes.  A reply cannot legitimately contain
/// more result records than the request/protocol grant permits.
const MAX_COMPOUND_OPERATIONS: usize = 64;
const DELAY: u32 = 10008;
const BADSESSION: u32 = 10052;
const BADSLOT: u32 = 10050;
const CONN_NOT_BOUND_TO_SESSION: u32 = 10055;
const SEQ_MISORDERED: u32 = 10063;
const RETRY_UNCACHED_REP: u32 = 10068;
const SEQ_FALSE_RETRY: u32 = 10076;
const DEADSESSION: u32 = 10078;
// NFSv4 status codes reserved for the in-progress state-recovery path.
#[allow(dead_code)]
const STALE_CLIENTID: u32 = 10022;
#[allow(dead_code)]
const STALE_STATEID: u32 = 10023;
#[allow(dead_code)]
const OLD_STATEID: u32 = 10024;
const BAD_STATEID: u32 = 10025;
#[allow(dead_code)]
const EXPIRED: u32 = 10011;
const GRACE: u32 = 10013;
const BADXDR: u32 = 10036;
const OP_PUTFH: u32 = 22;
const OP_PUTROOTFH: u32 = 24;
const OP_LOOKUP: u32 = 15;
const OP_OPEN: u32 = 18;
const OP_OPENATTR: u32 = 19;
const OP_LOCK: u32 = 12;
const OP_LOCKT: u32 = 13;
const OP_LOCKU: u32 = 14;
const OP_GETFH: u32 = 10;
const OP_LINK: u32 = 11;
const OP_GETATTR: u32 = 9;
const OP_READ: u32 = 25;
const OP_READLINK: u32 = 27;
const OP_WRITE: u32 = 38;
const OP_VERIFY: u32 = 37;
const OP_COMMIT: u32 = 5;
const OP_CLOSE: u32 = 4;
const OP_CREATE: u32 = 6;
const OP_DELEGRETURN: u32 = 8;
const OP_RENEW: u32 = 30;
const OP_REMOVE: u32 = 28;
const OP_RENAME: u32 = 29;
const OP_READDIR: u32 = 26;
const OP_RESTOREFH: u32 = 31;
const OP_SAVEFH: u32 = 32;
const OP_SETATTR: u32 = 34;
const OP_BIND_CONN_TO_SESSION: u32 = 41;
const OP_EXCHANGE_ID: u32 = 42;
const OP_CREATE_SESSION: u32 = 43;
const OP_RECLAIM_COMPLETE: u32 = 58;
const OP_SEQUENCE: u32 = 53;
const CB_RECALL: u32 = 4;
const CB_SEQUENCE: u32 = 11;
const SEQ4_STATUS_CB_PATH_DOWN: u32 = 1;
const SEQ4_STATUS_CB_GSS_CONTEXTS_EXPIRING: u32 = 1 << 1;
const SEQ4_STATUS_CB_GSS_CONTEXTS_EXPIRED: u32 = 1 << 2;
const SEQ4_STATUS_EXPIRED_ALL_STATE_REVOKED: u32 = 1 << 3;
const SEQ4_STATUS_EXPIRED_SOME_STATE_REVOKED: u32 = 1 << 4;
const SEQ4_STATUS_ADMIN_STATE_REVOKED: u32 = 1 << 5;
const SEQ4_STATUS_RECALLABLE_STATE_REVOKED: u32 = 1 << 6;
const SEQ4_STATUS_LEASE_MOVED: u32 = 1 << 7;
const SEQ4_STATUS_RESTART_RECLAIM_NEEDED: u32 = 1 << 8;

#[inline]
fn retry_sleep(duration: core::time::Duration) {
    // Host unit tests do not initialize the kernel time source, so the
    // axtask busy-wait clock cannot advance there.  Preserve the real delay
    // and retry behavior with the host scheduler while keeping the kernel
    // path on axtask.
    #[cfg(test)]
    std::thread::sleep(duration);
    #[cfg(not(test))]
    let _ = axtask::sleep(duration);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NfsError {
    Transport,
    Malformed,
    Rpc(u32),
    Status(u32),
    TopLevelDelay,
    Denied {
        offset: u64,
        length: u64,
        write: bool,
        owner: Vec<u8>,
    },
    WouldBlock,
    SessionLost,
    Security,
    Length,
}
pub type NfsResult<T> = Result<T, NfsError>;

/// Implemented by the shared cancellable operation/network engine. The call
/// transports one complete RPC record; cancellation is by XID, never by
/// dropping a request which may have reached the server.
pub trait RpcTransport: Send + Sync {
    fn call(&self, record: &[u8]) -> NfsResult<Vec<u8>>;
    fn cancel(&self, xid: u32);
    /// Rebuilds the underlying connection after a transport/session loss.
    /// The returned transport must have no in-flight calls from the retired
    /// connection; NFS session and callback negotiation are redone by the
    /// mount before it admits ordinary I/O on this replacement.
    fn reconnect(&self, callback: Weak<NfsMount>) -> NfsResult<Arc<dyn RpcTransport>>;
    /// Mount teardown is transport-visible so an in-flight RPC cannot keep a
    /// dead superblock waiting for a retained socket reader.
    fn shutdown(&self) {}
}

/// RPCSEC_GSS boundary.  A GSS mount cannot silently fall back to AUTH_NONE.
pub trait RpcsecGss: Send + Sync {
    /// Establish/refresh the RPCSEC_GSS context before the first protected
    /// COMPOUND.  Implementations perform the RFC 2203 INIT/CONTINUE_INIT
    /// exchange through their transport binding and reject downgraded service
    /// flavours rather than falling back to AUTH_NONE.
    fn establish(&self) -> NfsResult<()> {
        Ok(())
    }
    /// Monotonic per-context sequence used for replay protection.  A real GSS
    /// implementation binds it into the MIC/wrap token it returns below.
    fn sequence(&self) -> NfsResult<u32> {
        Ok(0)
    }
    /// Encodes RFC 2203 `rpc_gss_cred_t` for one DATA call. `sequence` is the
    /// same value bound into the request MIC and wrapped body.
    fn credential(&self, xid: u32, sequence: u32) -> NfsResult<Vec<u8>>;
    /// RFC 2203 DATA call verifier: GSS_GetMIC over the XDR RPC call from
    /// XID through (and including) the credential, before the verifier.
    fn verifier(&self, call_through_credential: &[u8]) -> NfsResult<Vec<u8>>;
    /// RFC 2203 DATA reply verifier: the peer MIC of that same request
    /// sequence number.  The generic RPC framing must never accidentally
    /// authenticate a variable reply prefix instead.
    fn verify_reply(&self, sequence: u32, verifier: &[u8]) -> NfsResult<()>;
    fn wrap(&self, sequence: u32, bytes: &[u8]) -> NfsResult<Vec<u8>>;
    fn unwrap(&self, sequence: u32, bytes: &[u8]) -> NfsResult<Vec<u8>>;
    /// The requested pseudoflavour is part of the authenticated contract.
    /// It lets the generic RPC layer reject a context constructed for krb5
    /// when the mount explicitly requested krb5i or krb5p.
    fn service(&self) -> RpcGssService;
    /// Server-to-client callback direction.  Kept separate from forechannel
    /// methods because acceptor keys and replay windows are reversed.
    fn verify_callback(
        &self,
        _sequence: u32,
        _call_through_credential: &[u8],
        _verifier: &[u8],
    ) -> NfsResult<()> {
        Err(NfsError::Security)
    }
    fn unwrap_callback(&self, _sequence: u32, _bytes: &[u8]) -> NfsResult<Vec<u8>> {
        Err(NfsError::Security)
    }
    fn wrap_callback_reply(&self, _sequence: u32, _bytes: &[u8]) -> NfsResult<Vec<u8>> {
        Err(NfsError::Security)
    }
    fn callback_verifier(
        &self,
        _sequence: u32,
        _reply_through_credential: &[u8],
    ) -> NfsResult<Vec<u8>> {
        Err(NfsError::Security)
    }
    fn context_handle(&self) -> &[u8] {
        &[]
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RpcGssService {
    None,
    Integrity,
    Privacy,
}

/// Imported `gss_krb5` v2 context as supplied by rpc.gssd in the Linux
/// auth_gss downcall.  This parser is intentionally separated from the
/// cryptosystem: malformed imported material must be rejected before any key
/// derivation, and the session key remains owned by the resulting mechanism.
#[derive(Clone, Eq, PartialEq)]
pub struct Krb5ImportedContext {
    pub flags: u32,
    pub endtime: u32,
    pub sequence: u64,
    pub enctype: u32,
    session_key: Vec<u8>,
}

/// Bounded RFC 2203 replay window.  It is deliberately independent from an
/// RPC transport slot: retransmitting one XID reuses its protected record,
/// while a newly accepted peer token is checked against this context-global
/// window before its NFS payload reaches the decoder.
pub struct GssSequenceWindow {
    next_send: AtomicU32,
    receive: Mutex<GssReceiveWindow>,
    width: u32,
}
struct GssReceiveWindow {
    highest: u32,
    seen: u64,
    initialized: bool,
}
impl GssSequenceWindow {
    pub fn new(initial: u32, width: u32) -> NfsResult<Self> {
        if width == 0 || width > 64 {
            return Err(NfsError::Security);
        }
        Ok(Self {
            next_send: AtomicU32::new(initial.max(1)),
            receive: Mutex::new(GssReceiveWindow {
                highest: 0,
                seen: 0,
                initialized: false,
            }),
            width,
        })
    }
    pub fn allocate(&self) -> NfsResult<u32> {
        self.next_send
            .try_update(Ordering::AcqRel, Ordering::Acquire, |sequence| {
                (sequence < 0x8000_0000).then_some(sequence + 1)
            })
            .map_err(|_| NfsError::Security)
    }
    /// Atomically accepts exactly one sequence number. A number older than
    /// the negotiated window or already represented in the bitmap is replay.
    pub fn accept(&self, sequence: u32) -> NfsResult<()> {
        if sequence == 0 {
            return Err(NfsError::Security);
        }
        let mut receive = self.receive.lock();
        if !receive.initialized {
            receive.highest = sequence;
            receive.seen = 1;
            receive.initialized = true;
            return Ok(());
        }
        if sequence > receive.highest {
            let delta = sequence - receive.highest;
            receive.seen = if delta >= 64 {
                1
            } else {
                (receive.seen << delta) | 1
            };
            receive.highest = sequence;
            return Ok(());
        }
        let delta = receive.highest - sequence;
        if delta >= self.width || delta >= 64 || receive.seen & (1u64 << delta) != 0 {
            return Err(NfsError::Security);
        }
        receive.seen |= 1u64 << delta;
        Ok(())
    }
}
impl Krb5ImportedContext {
    pub fn parse(bytes: &[u8]) -> NfsResult<Self> {
        const HEADER: usize = 4 + 4 + 8 + 4;
        if bytes.len() < HEADER {
            return Err(NfsError::Security);
        }
        let word = |at: usize| -> NfsResult<u32> {
            Ok(u32::from_ne_bytes(
                bytes
                    .get(at..at + 4)
                    .ok_or(NfsError::Security)?
                    .try_into()
                    .map_err(|_| NfsError::Security)?,
            ))
        };
        let flags = word(0)?;
        let endtime = word(4)?;
        // gssd imports a client-initiator context only. A zero expiry or a
        // context without the initiator bit is never safe to publish.
        if endtime == 0 || flags & 1 == 0 {
            return Err(NfsError::Security);
        }
        let sequence = u64::from_ne_bytes(
            bytes
                .get(8..16)
                .ok_or(NfsError::Security)?
                .try_into()
                .map_err(|_| NfsError::Security)?,
        );
        // This is the mechanism's 64-bit RFC 4121 sequence.  The separate
        // rpc_gss_cred_t sequence always starts at one in the RPC layer.
        if sequence == 0 {
            return Err(NfsError::Security);
        }
        let enctype = word(16)?;
        // RFC 3961 key lengths for enctypes supported by modern Linux
        // rpcsec_gss_krb5: AES128/256 SHA1 (17/18) and SHA2 (19/20).
        let key_len = match enctype {
            17 | 19 => 16,
            18 | 20 => 32,
            _ => return Err(NfsError::Security),
        };
        if bytes.len() != HEADER + key_len {
            return Err(NfsError::Security);
        }
        let mut session_key = Vec::new();
        session_key
            .try_reserve_exact(key_len)
            .map_err(|_| NfsError::Transport)?;
        session_key.extend_from_slice(&bytes[HEADER..]);
        Ok(Self {
            flags,
            endtime,
            sequence,
            enctype,
            session_key,
        })
    }
    /// The session key is exposed only to the selected kernel mechanism while
    /// it is being constructed; callers cannot recover it from an RPC auth
    /// object or VFS/keyring handle.
    pub fn into_session_key(mut self) -> Vec<u8> {
        core::mem::take(&mut self.session_key)
    }
    pub fn expiry(&self) -> u32 {
        self.endtime
    }
    pub fn mechanism_flags(&self) -> u32 {
        self.flags
    }
    /// The Kerberos mechanism's RFC 4121 token sequence is independent of
    /// rpc_gss_cred_t's 31-bit RPC sequence space.
    pub fn initial_mechanism_sequence(&self) -> u64 {
        self.sequence
    }
    pub fn enctype(&self) -> u32 {
        self.enctype
    }
}
impl Drop for Krb5ImportedContext {
    fn drop(&mut self) {
        self.session_key.fill(0);
    }
}
/// RFC 5531 AUTH_SYS credential.  Keep it owned by the mount rather than
/// consulting a current task at RPC time: an NFS superblock may outlive the
/// task which mounted it, and a worker must never acquire credentials from an
/// arbitrary caller of a later VFS operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RpcSysAuth {
    pub machine_name: Vec<u8>,
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
}
impl RpcSysAuth {
    pub fn new(machine_name: Vec<u8>, uid: u32, gid: u32, groups: Vec<u32>) -> NfsResult<Self> {
        Self::validate(&machine_name, &groups)?;
        Ok(Self {
            machine_name,
            uid,
            gid,
            groups,
        })
    }
    fn validate(machine_name: &[u8], groups: &[u32]) -> NfsResult<()> {
        if machine_name.len() > 255 || groups.len() > 16 {
            return Err(NfsError::Length);
        }
        Ok(())
    }
    fn encode_credential(&self, xid: u32) -> NfsResult<Vec<u8>> {
        let mut xdr = Xdr::default();
        // Linux' AUTH_SYS stamp is not an identity.  A mount-owned changing
        // stamp gives a server no stable handle to confuse with uid/gid.
        xdr.u32(xid);
        xdr.opaque(&self.machine_name);
        xdr.u32(self.uid);
        xdr.u32(self.gid);
        xdr.u32(self.groups.len().try_into().map_err(|_| NfsError::Length)?);
        for group in &self.groups {
            xdr.u32(*group);
        }
        Ok(xdr.0)
    }
}

#[derive(Clone)]
pub enum RpcAuth {
    None,
    Sys(RpcSysAuth),
    Gss(Arc<dyn RpcsecGss>),
}

/// `sec=` is a security contract, not a best-effort preference.  In
/// particular krb5i/krb5p must not be silently demoted to krb5 or AUTH_SYS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NfsSecurityFlavor {
    Sys,
    Krb5,
    Krb5i,
    Krb5p,
}
impl NfsSecurityFlavor {
    pub fn parse(value: &[u8]) -> NfsResult<Self> {
        match value {
            b"sys" => Ok(Self::Sys),
            b"krb5" => Ok(Self::Krb5),
            b"krb5i" => Ok(Self::Krb5i),
            b"krb5p" => Ok(Self::Krb5p),
            _ => Err(NfsError::Security),
        }
    }
    pub const fn requires_gss(self) -> bool {
        !matches!(self, Self::Sys)
    }
}

/// The mount-owned idmapping boundary.  NFSv4 owner attributes are opaque
/// UTF-8-ish principals, never host uid_t values; translating them in the
/// provider keeps an idmapped mount from leaking raw server identities into
/// VFS DAC decisions.
pub trait NfsIdMapper: Send + Sync {
    fn owner_to_uid(&self, owner: &[u8]) -> NfsResult<u32>;
    fn group_to_gid(&self, group: &[u8]) -> NfsResult<u32>;
    fn uid_to_owner(&self, uid: u32) -> NfsResult<Vec<u8>>;
    fn gid_to_group(&self, gid: u32) -> NfsResult<Vec<u8>>;
}
struct NumericIdMapper;
fn decimal_id(bytes: &[u8]) -> NfsResult<u32> {
    if bytes.is_empty() {
        return Err(NfsError::Security);
    }
    let mut value = 0u32;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return Err(NfsError::Security);
        }
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add((byte - b'0') as u32))
            .ok_or(NfsError::Security)?
    }
    Ok(value)
}
impl NfsIdMapper for NumericIdMapper {
    fn owner_to_uid(&self, owner: &[u8]) -> NfsResult<u32> {
        decimal_id(owner)
    }
    fn group_to_gid(&self, group: &[u8]) -> NfsResult<u32> {
        decimal_id(group)
    }
    fn uid_to_owner(&self, uid: u32) -> NfsResult<Vec<u8>> {
        Ok(alloc::format!("{uid}").into_bytes())
    }
    fn gid_to_group(&self, gid: u32) -> NfsResult<Vec<u8>> {
        Ok(alloc::format!("{gid}").into_bytes())
    }
}

/// NFS filehandles are capped at 128 bytes by this client. Keeping the wire
/// value inline makes cloning a delegation/reply snapshot allocation-free,
/// which is essential after a server has already granted state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileHandle {
    bytes: [u8; 128],
    len: u8,
}
impl FileHandle {
    pub fn new(bytes: Vec<u8>) -> NfsResult<Self> {
        if bytes.is_empty() || bytes.len() > 128 {
            return Err(NfsError::Length);
        }
        let mut out = Self {
            bytes: [0; 128],
            len: bytes.len() as u8,
        };
        out.bytes[..bytes.len()].copy_from_slice(&bytes);
        Ok(out)
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StableHow {
    Unstable = 0,
    DataSync = 1,
    FileSync = 2,
}
#[derive(Clone, Debug)]
pub struct ReadResult {
    pub eof: bool,
    pub data: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct WriteResult {
    pub count: u32,
    pub committed: StableHow,
    pub verifier: [u8; 8],
}
#[derive(Clone, Debug)]
pub struct ReadDirEntry {
    pub name: FsNameBuf,
    pub cookie: u64,
    pub kind: u32,
    pub fileid: u64,
}
#[derive(Clone, Debug)]
pub struct ReadDirResult {
    pub verifier: [u8; 8],
    pub eof: bool,
    pub entries: Vec<ReadDirEntry>,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct NfsQuota {
    pub hard_available: u64,
    pub soft_available: u64,
    pub used: u64,
}
/// Server-supplied metadata used for remote dentry/page-cache revalidation.
/// `change` is the NFS change attribute, never a locally invented epoch.
/// Validated NFSv4 `acl4` wire value.  It is intentionally distinct from a
/// Linux POSIX ACL xattr: providers must make an explicit, lossless mapping.
#[derive(Clone, Debug, Default)]
pub struct Nfs4Acl(Vec<u8>);
impl Nfs4Acl {
    pub fn as_wire(&self) -> &[u8] {
        &self.0
    }
}
#[derive(Clone, Debug, Default)]
pub struct NfsAclCapabilities {
    pub supported_attrs: [u32; 2],
    pub acl_support: u32,
    pub root_acl: Nfs4Acl,
}
impl NfsAclCapabilities {
    fn permits_allow_deny(&self) -> bool {
        self.supported_attrs[0] & (1 << 12) != 0
            && self.supported_attrs[0] & (1 << 13) != 0
            && self.acl_support & 3 == 3
    }
}
#[derive(Clone, Debug, Default)]
pub struct NfsAttr {
    pub kind: u32,
    pub size: u64,
    pub change: u64,
    pub fileid: u64,
    pub mode: u32,
    pub nlink: u32,
    pub fsid_major: u64,
    pub fsid_minor: u64,
    pub lease_time: u32,
    pub owner: Vec<u8>,
    pub owner_group: Vec<u8>,
    pub max_name: u32,
    pub quota_avail_hard: u64,
    pub quota_avail_soft: u64,
    pub quota_used: u64,
    pub space_avail: u64,
    pub space_free: u64,
    pub space_total: u64,
    pub acl: Option<Nfs4Acl>,
    pub acl_support: u32,
}
#[derive(Clone, Copy, Debug)]
pub struct RenameChange {
    target_atomic: bool,
    target_before: u64,
    target_after: u64,
}
#[derive(Clone, Debug)]
pub struct OpenState {
    pub stateid: [u8; 16],
    pub handle: FileHandle,
    pub owner: u64,
    pub sequence: u32,
}
#[derive(Clone, Debug)]
pub struct LockState {
    pub stateid: [u8; 16],
    pub owner: u64,
    pub sequence: u32,
}
#[derive(Clone, Debug)]
pub struct NfsMountOptions {
    pub owner: Vec<u8>,
    pub slots: u32,
    pub security: NfsSecurityFlavor,
    pub auth_sys: RpcSysAuth,
}
impl Default for NfsMountOptions {
    fn default() -> Self {
        Self {
            owner: b"thekernel-nfs41".to_vec(),
            slots: 16,
            security: NfsSecurityFlavor::Sys,
            auth_sys: RpcSysAuth {
                machine_name: b"thekernel".to_vec(),
                uid: 0,
                gid: 0,
                groups: Vec::new(),
            },
        }
    }
}

/// Per-session-slot replay state.  A slot is never reused until the caller
/// has observed the reply.  In particular, a transport failure leaves the
/// sequence number intact: retrying a non-idempotent compound must use the
/// same SEQUENCE tuple, not manufacture a second OPEN/WRITE.
/// A forechannel slot has exactly one owner.  In particular, `Sent` owns the
/// immutable protected record until the server has definitely accepted or
/// rejected its SEQUENCE tuple.  Do not fold this into a Boolean: a TCP
/// timeout is ambiguous and the record (including XID and GSS MIC) is the
/// only legal retransmission image.
enum SlotLifecycle {
    Free,
    Sent(Replay),
    Terminal(Replay),
}
struct Slot {
    id: u32,
    sequence: u32,
    unusable: bool,
    lifecycle: SlotLifecycle,
}
/// A completed slot keeps the exact wire request that consumed its sequence;
/// this binds diagnostics/replay handling to the XID and protected byte image
/// rather than to a reconstructed operation list.
struct Replay {
    xid: u32,
    sequence: u32,
    request: Vec<u8>,
    gss_sequence: Option<u32>,
    reply: Vec<u8>,
    terminal: Option<NfsError>,
}
struct Session {
    clientid: u64,
    // Session setup state kept for the in-progress EXCHANGE_ID renewal path.
    #[allow(dead_code)]
    exchange_sequence: u32,
    id: [u8; 16],
    max_operations: usize,
    slots: Vec<Slot>,
    highest_slot: u32,
    target_highest_slot: u32,
    reclaiming: bool,
    replay_barrier: bool,
}
#[derive(Clone, Copy)]
struct ExchangeIdentity {
    clientid: u64,
    sequenceid: u32,
}
#[derive(Clone, Copy)]
struct CreateSessionGrant {
    id: [u8; 16],
    fore_slots: u32,
    fore_max_operations: usize,
}
#[derive(Clone, Copy)]
struct ChannelGrant {
    max_requests: u32,
    max_operations: usize,
}
/// Immutable wire image retained for a slot retransmit.  In particular, GSS
/// credential/body sequence values are allocated once, never regenerated.
struct EncodedRpc {
    record: Vec<u8>,
    gss_sequence: Option<u32>,
}
/// Inputs that identify durable server state.  These are deliberately kept as
/// protocol values, rather than reconstructed from a VFS path during
/// recovery: a rename after OPEN must not turn a reclaim into an OPEN of a
/// different object.
#[derive(Clone)]
enum StateReplay {
    Open {
        parent: FileHandle,
        name: FsNameBuf,
        share_access: u32,
        share_deny: u32,
        create: bool,
        mode: u32,
        create_verifier: [u8; 8],
        delegation_preference: bool,
    },
    Lock {
        open_owner: u64,
        open_stateid: [u8; 16],
        open_seqid: u32,
        offset: u64,
        length: u64,
        write: bool,
        lock_seqid: u32,
    },
}
struct StateRecord {
    owner: u64,
    handle: FileHandle,
    stateid: [u8; 16],
    next_seqid: u32,
    lock: bool,
    reclaim: bool,
    replay: StateReplay,
}

/// One non-overlapping dirty interval.  `data_start` lets a trim or a newer
/// overlapping WRITE retain either side of an old interval without allocating
/// or copying after the server has accepted a WRITE.
#[derive(Clone, Debug)]
struct UnstableWrite {
    handle: Arc<FileHandle>,
    stateid: [u8; 16],
    offset: u64,
    count: u32,
    verifier: [u8; 8],
    data: Arc<Vec<u8>>,
    data_start: usize,
    generation: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DelegationState {
    Active,
    RecallPending,
    RecallInFlight,
}
#[derive(Clone, Debug)]
struct Delegation {
    handle: FileHandle,
    stateid: [u8; 16],
    generation: u64,
    session_generation: u64,
    teardown_epoch: u64,
    state: DelegationState,
}
#[derive(Clone, Debug)]
struct RecallWork {
    handle: FileHandle,
    stateid: [u8; 16],
    generation: u64,
    session_generation: u64,
    teardown_epoch: u64,
}
struct CallbackReplay {
    sequence: u32,
    request: Vec<u8>,
    reply: Vec<u8>,
}
struct CallbackSlot {
    id: u32,
    next_sequence: u32,
    inflight: Option<(u32, Vec<u8>)>,
    replay: Option<CallbackReplay>,
}
struct CallbackSession {
    id: [u8; 16],
    slots: Vec<CallbackSlot>,
}
enum CallbackAdmission {
    Replay(Vec<u8>),
    Execute,
    Wait(u64),
}
struct CompoundAdmission<'a> {
    mount: &'a NfsMount,
    generation: u64,
}
impl Drop for CompoundAdmission<'_> {
    fn drop(&mut self) {
        self.mount.active_compounds.fetch_sub(1, Ordering::AcqRel);
    }
}
/// Once a replacement connection is admitted, every fallible replay step
/// must leave the old immutable Sent images fenced.  This guard makes that
/// invariant independent of `?`/early-return paths in the replay loop.
struct SurvivingReplayGuard<'a> {
    mount: &'a NfsMount,
    complete: bool,
}
impl Drop for SurvivingReplayGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.mount.lease_faulted.store(true, Ordering::Release);
            if let Some(session) = self.mount.session.lock().as_mut() {
                session.replay_barrier = true;
            }
        }
    }
}
/// One forechannel repair may own a replay generation.  Every ambiguous
/// caller keeps its own `Sent` slot, while non-owners sleep until the owner
/// has published Terminal records or a teardown wakes them.
struct RecoveryElection {
    generation: AtomicU64,
    owner: AtomicBool,
    waiters: WaitQueue,
}

/// Transport-backed NFSv4.1 mount state.  The remote-coherency epoch is
/// changed on recall/session loss; VFS dentries/page cache must key remote
/// validation against it rather than serving stale local state.
pub struct NfsMount {
    transport: Mutex<Arc<dyn RpcTransport>>,
    auth: RpcAuth,
    xid: AtomicU32,
    self_ref: Mutex<Weak<NfsMount>>,
    recovery: RecoveryElection,
    reconnect_ops: Mutex<()>,
    session: Mutex<Option<Session>>,
    next_owner: AtomicU64,
    session_generation: AtomicU64,
    reclaim_gate: AtomicBool,
    active_compounds: AtomicU32,
    /// State-owner seqids are an ordered stream.  Keep OPEN/CLOSE/LOCK and
    /// LOCKU transitions serialized while their resulting stateid/seqid is
    /// committed, rather than allowing two OFDs to race a shared owner.
    state_ops: Mutex<()>,
    namespace_ops: Mutex<()>,
    state_records: Mutex<Vec<StateRecord>>,
    coherency_epoch: AtomicU64,
    /// Serializes the per-file dirty interval ledger with writes, truncates,
    /// COMMIT retirement, and verifier recovery.  In particular, a replay
    /// cannot race a later successful WRITE or SETATTR(size).
    unstable_ops: Mutex<()>,
    unstable: Mutex<Vec<UnstableWrite>>,
    unstable_generation: AtomicU64,
    delegations: Mutex<Vec<Delegation>>,
    delegation_generation: AtomicU64,
    recall_waiters: WaitQueue,
    recall_worker_started: AtomicBool,
    recall_worker_stop: AtomicBool,
    lease_seconds: AtomicU32,
    lease_worker_started: AtomicBool,
    lease_worker_stop: AtomicBool,
    lease_faulted: AtomicBool,
    callback_alive: AtomicBool,
    /// Changed only by callback-loss/teardown while `reconnect_ops` is held.
    /// Recovery snapshots and rechecks this under the same lock so a teardown
    /// cannot turn a just-published replacement into a false success.
    teardown_epoch: AtomicU64,
    callback_session: Mutex<Option<CallbackSession>>,
    callback_epoch: AtomicU64,
    callback_waiters: WaitQueue,
    supported_attrs: Mutex<Option<[u32; 2]>>,
    acl_capabilities: Mutex<Option<NfsAclCapabilities>>,
    options: Mutex<Option<NfsMountOptions>>,
    id_mapper: Arc<dyn NfsIdMapper>,
}
impl NfsMount {
    pub fn new(transport: Arc<dyn RpcTransport>, auth: RpcAuth) -> Self {
        Self::new_with_id_mapper(transport, auth, Arc::new(NumericIdMapper))
    }
    pub fn new_with_id_mapper(
        transport: Arc<dyn RpcTransport>,
        auth: RpcAuth,
        id_mapper: Arc<dyn NfsIdMapper>,
    ) -> Self {
        Self {
            transport: Mutex::new(transport),
            auth,
            xid: AtomicU32::new(1),
            self_ref: Mutex::new(Weak::new()),
            recovery: RecoveryElection {
                generation: AtomicU64::new(1),
                owner: AtomicBool::new(false),
                waiters: WaitQueue::new(),
            },
            reconnect_ops: Mutex::new(()),
            session: Mutex::new(None),
            next_owner: AtomicU64::new(1),
            session_generation: AtomicU64::new(1),
            reclaim_gate: AtomicBool::new(false),
            active_compounds: AtomicU32::new(0),
            state_ops: Mutex::new(()),
            namespace_ops: Mutex::new(()),
            state_records: Mutex::new(Vec::new()),
            coherency_epoch: AtomicU64::new(1),
            unstable_ops: Mutex::new(()),
            unstable: Mutex::new(Vec::new()),
            unstable_generation: AtomicU64::new(1),
            delegations: Mutex::new(Vec::new()),
            delegation_generation: AtomicU64::new(1),
            recall_waiters: WaitQueue::new(),
            recall_worker_started: AtomicBool::new(false),
            recall_worker_stop: AtomicBool::new(false),
            lease_seconds: AtomicU32::new(0),
            lease_worker_started: AtomicBool::new(false),
            lease_worker_stop: AtomicBool::new(false),
            lease_faulted: AtomicBool::new(false),
            callback_alive: AtomicBool::new(false),
            teardown_epoch: AtomicU64::new(1),
            callback_session: Mutex::new(None),
            callback_epoch: AtomicU64::new(1),
            callback_waiters: WaitQueue::new(),
            supported_attrs: Mutex::new(None),
            acl_capabilities: Mutex::new(None),
            options: Mutex::new(None),
            id_mapper,
        }
    }
    fn install_self_ref(&self, this: &Arc<Self>) {
        *self.self_ref.lock() = Arc::downgrade(this);
    }
    fn transport(&self) -> Arc<dyn RpcTransport> {
        self.transport.lock().clone()
    }
    pub fn owner_to_uid(&self, owner: &[u8]) -> NfsResult<u32> {
        self.id_mapper.owner_to_uid(owner)
    }
    pub fn group_to_gid(&self, group: &[u8]) -> NfsResult<u32> {
        self.id_mapper.group_to_gid(group)
    }
    pub fn uid_to_owner(&self, uid: u32) -> NfsResult<Vec<u8>> {
        self.id_mapper.uid_to_owner(uid)
    }
    pub fn gid_to_group(&self, gid: u32) -> NfsResult<Vec<u8>> {
        self.id_mapper.gid_to_group(gid)
    }
    pub fn coherency_epoch(&self) -> u64 {
        self.coherency_epoch.load(Ordering::Acquire)
    }
    fn invalidate(&self) {
        self.coherency_epoch.fetch_add(1, Ordering::AcqRel);
    }
    fn admit_compound(&self, reclaim: bool) -> NfsResult<CompoundAdmission<'_>> {
        if self.reclaim_gate.load(Ordering::Acquire) && !reclaim {
            return Err(NfsError::SessionLost);
        }
        if !reclaim
            && self
                .session
                .lock()
                .as_ref()
                .is_some_and(|session| session.reclaiming)
        {
            return Err(NfsError::SessionLost);
        }
        let generation = self.session_generation.load(Ordering::Acquire);
        self.active_compounds.fetch_add(1, Ordering::AcqRel);
        if self.session_generation.load(Ordering::Acquire) != generation
            || (self.reclaim_gate.load(Ordering::Acquire) && !reclaim)
        {
            self.active_compounds.fetch_sub(1, Ordering::AcqRel);
            return Err(NfsError::SessionLost);
        }
        Ok(CompoundAdmission {
            mount: self,
            generation,
        })
    }
    fn lease_interval(&self) -> core::time::Duration {
        // Renew at half the server-advertised lease, leaving scheduling room
        // while ensuring an idle OPEN/LOCK/delegation does not expire.
        core::time::Duration::from_secs(u64::from(
            (self.lease_seconds.load(Ordering::Acquire) / 2).max(1),
        ))
    }
    fn start_lease_worker(self: &Arc<Self>, lease_seconds: u32) -> NfsResult<()> {
        if lease_seconds == 0 {
            return Err(NfsError::Malformed);
        }
        self.lease_seconds.store(lease_seconds, Ordering::Release);
        self.lease_worker_stop.store(false, Ordering::Release);
        if self.lease_worker_started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let weak = Arc::downgrade(self);
        if axtask::try_spawn_with_name(move || Self::lease_worker(weak), "nfs41_lease_renew".into())
            .is_err()
        {
            self.lease_worker_started.store(false, Ordering::Release);
            return Err(NfsError::Transport);
        }
        Ok(())
    }
    fn stop_lease_worker(&self) {
        self.lease_worker_stop.store(true, Ordering::Release);
        self.recovery.waiters.notify_all(false);
    }
    /// A completed recovery is visible only after it has reopened admission,
    /// cleared the fault and removed the replay barrier.  Terminal records
    /// may still exist for their original waiters and do not make a healthy
    /// session recoverable again.
    fn recovery_needed(&self) -> bool {
        self.lease_faulted.load(Ordering::Acquire)
            || self.reclaim_gate.load(Ordering::Acquire)
            || !self
                .session
                .lock()
                .as_ref()
                .is_some_and(|session| !session.replay_barrier && !session.reclaiming)
    }
    /// Join the same recovery election as ambiguous forechannel callers.
    /// In particular, a lease worker must never take `reconnect_ops` merely
    /// because it observed an old `lease_faulted=true` before another owner
    /// repaired the session.
    fn recover_elected(self: &Arc<Self>) -> NfsResult<()> {
        let observed = self.recovery.generation.load(Ordering::Acquire);
        if self
            .recovery
            .owner
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let result = self.recover();
            self.recovery.generation.fetch_add(1, Ordering::AcqRel);
            self.recovery.owner.store(false, Ordering::Release);
            self.recovery.waiters.notify_all(false);
            return result;
        }
        self.recovery
            .waiters
            .wait_until(|| {
                self.lease_worker_stop.load(Ordering::Acquire)
                    || self.recovery.generation.load(Ordering::Acquire) != observed
                    || !self.recovery.owner.load(Ordering::Acquire)
            })
            .map_err(|_| NfsError::SessionLost)?;
        if self.lease_worker_stop.load(Ordering::Acquire) {
            return Err(NfsError::SessionLost);
        }
        if self.recovery_needed() {
            Err(NfsError::SessionLost)
        } else {
            Ok(())
        }
    }
    fn start_recall_worker(self: &Arc<Self>) -> NfsResult<()> {
        if self.recall_worker_started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let weak = Arc::downgrade(self);
        if axtask::try_spawn_with_name(
            move || Self::recall_worker(weak),
            "nfs41_delegreturn".into(),
        )
        .is_err()
        {
            self.recall_worker_started.store(false, Ordering::Release);
            return Err(NfsError::Transport);
        }
        Ok(())
    }
    fn stop_recall_worker(&self) {
        self.recall_worker_stop.store(true, Ordering::Release);
        self.recall_waiters.notify_all(false);
    }
    fn recall_worker(weak: Weak<Self>) {
        loop {
            let Some(mount) = weak.upgrade() else {
                return;
            };
            if mount.recall_worker_stop.load(Ordering::Acquire) {
                mount.recall_worker_started.store(false, Ordering::Release);
                return;
            }
            let work = mount.claim_pending_recall();
            let Some(work) = work else {
                let _ = mount.recall_waiters.wait_until(|| {
                    mount.recall_worker_stop.load(Ordering::Acquire)
                        || mount.pending_recall_exists()
                });
                continue;
            };
            let result = mount.complete_recall_work(&work);
            if result.is_err() && !mount.recall_worker_stop.load(Ordering::Acquire) {
                // Only a still-identical failed recall becomes pending again.
                // Transient network/session errors retain it for retry; an
                // obsolete generation must never revive after recovery.
                let mut delegations = mount.delegations.lock();
                if let Some(entry) = delegations.iter_mut().find(|entry| {
                    entry.handle == work.handle
                        && entry.stateid == work.stateid
                        && entry.generation == work.generation
                        && entry.session_generation == work.session_generation
                        && entry.teardown_epoch == work.teardown_epoch
                        && entry.state == DelegationState::RecallInFlight
                }) {
                    entry.state = DelegationState::RecallPending;
                    mount.recall_waiters.notify_all(false);
                }
            }
            drop(mount);
            // A failed return must not spin a dead server or starve ordinary
            // work; it remains pending and receives another bounded retry.
            if result.is_err() {
                let _ = axtask::sleep(core::time::Duration::from_secs(1));
            }
        }
    }
    fn lease_worker(weak: Weak<Self>) {
        loop {
            let Some(mount) = weak.upgrade() else {
                return;
            };
            if mount.lease_worker_stop.load(Ordering::Acquire) {
                return;
            }
            let interval = mount.lease_interval();
            if mount.lease_faulted.load(Ordering::Acquire) {
                // A faulted lease has already closed ordinary admission.  The
                // worker joins the same reconnect/reclaim election as the
                // forechannel caller that observed the fault.  It must not
                // later close the healthy replacement session on a stale
                // pre-lock fault sample.
                let _ = mount.recover_elected();
                drop(mount);
                let _ = axtask::sleep(interval);
                continue;
            }
            // The sleeper must not retain a superblock solely because it is
            // waiting for the next RENEW deadline.
            drop(mount);
            let _ = axtask::sleep(interval);
            let Some(mount) = weak.upgrade() else {
                return;
            };
            if mount.lease_worker_stop.load(Ordering::Acquire) {
                return;
            }
            match mount.renew() {
                Ok(()) | Err(NfsError::WouldBlock) | Err(NfsError::Status(GRACE)) => {}
                Err(_) => {
                    mount.invalidate_session();
                    mount.lease_faulted.store(true, Ordering::Release);
                    let _ = mount.recover_elected();
                }
            }
        }
    }
    fn reserve_state_record(&self) -> NfsResult<()> {
        self.state_records
            .lock()
            .try_reserve(1)
            .map_err(|_| NfsError::Transport)
    }
    /// Reserve delegation ownership before OPEN is visible remotely. The
    /// fixed-size FileHandle then moves into this slot without allocation.
    fn reserve_delegation_record(&self) -> NfsResult<()> {
        self.delegations
            .lock()
            .try_reserve(1)
            .map_err(|_| NfsError::Transport)
    }
    fn register_reserved_state(
        &self,
        owner: u64,
        handle: FileHandle,
        stateid: [u8; 16],
        lock: bool,
        replay: StateReplay,
    ) {
        self.state_records.lock().push(StateRecord {
            owner,
            handle,
            stateid,
            next_seqid: 1,
            lock,
            reclaim: false,
            replay,
        });
    }
    // State management for the in-progress open/lock recovery path.
    #[allow(dead_code)]
    fn register_state(
        &self,
        owner: u64,
        handle: FileHandle,
        stateid: [u8; 16],
        lock: bool,
        replay: StateReplay,
    ) -> NfsResult<()> {
        self.reserve_state_record()?;
        self.register_reserved_state(owner, handle, stateid, lock, replay);
        Ok(())
    }
    fn state_seqid(&self, owner: u64, stateid: [u8; 16], lock: bool) -> NfsResult<u32> {
        self.state_records
            .lock()
            .iter()
            .find(|record| {
                record.owner == owner && record.stateid == stateid && record.lock == lock
            })
            .map(|record| record.next_seqid)
            .ok_or(NfsError::SessionLost)
    }
    fn authoritative_state(
        &self,
        owner: u64,
        stateid: [u8; 16],
        lock: bool,
    ) -> NfsResult<([u8; 16], u32)> {
        let records = self.state_records.lock();
        records
            .iter()
            .find(|record| {
                record.owner == owner && record.stateid == stateid && record.lock == lock
            })
            .or_else(|| {
                records
                    .iter()
                    .find(|record| record.owner == owner && record.lock == lock && !record.reclaim)
            })
            .map(|record| (record.stateid, record.next_seqid))
            .ok_or(NfsError::SessionLost)
    }
    pub fn current_open_stateid(&self, state: &OpenState) -> NfsResult<[u8; 16]> {
        self.authoritative_state(state.owner, state.stateid, false)
            .map(|state| state.0)
    }
    fn advance_state(&self, owner: u64, old: [u8; 16], new: [u8; 16], lock: bool, remove: bool) {
        let mut records = self.state_records.lock();
        if let Some(index) = records.iter().position(|record| {
            record.owner == owner && record.stateid == old && record.lock == lock
        }) {
            if remove {
                records.remove(index);
            } else {
                let record = &mut records[index];
                record.stateid = new;
                record.next_seqid = record.next_seqid.saturating_add(1)
            }
        }
    }
    /// CLOSE variant for callers that already serialize state-owner seqids.
    /// It deliberately does not acquire `state_ops`, so create rollback cannot
    /// deadlock while it still owns that mutex.
    fn close_open_locked(&self, state: &OpenState) -> NfsResult<()> {
        let (stateid, seqid) = self.authoritative_state(state.owner, state.stateid, false)?;
        self.compound(&[
            Operation::PutFh(&state.handle),
            Operation::Close {
                sequence: seqid,
                stateid,
            },
        ])?;
        self.advance_state(state.owner, stateid, [0; 16], false, true);
        Ok(())
    }
    /// An OPEN may succeed before local state-record allocation does.  The
    /// stateid is still valid for the first CLOSE seqid, so retire it directly.
    // State management for the in-progress open/lock recovery path.
    #[allow(dead_code)]
    fn close_untracked_open_locked(&self, handle: &FileHandle, stateid: [u8; 16]) -> NfsResult<()> {
        self.compound(&[
            Operation::PutFh(handle),
            Operation::Close {
                sequence: 1,
                stateid,
            },
        ])
        .map(|_| ())
    }
    fn mark_reclaim_needed(&self) {
        for record in self.state_records.lock().iter_mut() {
            record.reclaim = true;
        }
    }
    fn invalidate_session(&self) {
        self.reclaim_gate.store(true, Ordering::Release);
        self.session_generation.fetch_add(1, Ordering::AcqRel);
        self.mark_reclaim_needed();
        *self.session.lock() = None;
        self.callback_alive.store(false, Ordering::Release);
        self.callback_session.lock().take();
        self.delegations.lock().clear();
        self.callback_epoch.fetch_add(1, Ordering::AcqRel);
        self.callback_waiters.notify_all(false);
        self.recall_waiters.notify_all(false);
        self.invalidate();
    }
    fn update_slot_window(&self, sessionid: [u8; 16], highest: u32, target: u32) {
        let mut guard = self.session.lock();
        let Some(session) = guard.as_mut() else {
            return;
        };
        if session.id != sessionid {
            return;
        }
        let limit = (session.slots.len() as u32).saturating_sub(1);
        session.highest_slot = highest.min(limit);
        session.target_highest_slot = target.min(limit);
    }
    fn apply_sequence_status(&self, flags: u32) -> NfsResult<()> {
        if flags == 0 {
            return Ok(());
        }
        if flags
            & (SEQ4_STATUS_CB_PATH_DOWN
                | SEQ4_STATUS_CB_GSS_CONTEXTS_EXPIRING
                | SEQ4_STATUS_CB_GSS_CONTEXTS_EXPIRED
                | SEQ4_STATUS_RECALLABLE_STATE_REVOKED)
            != 0
        {
            // No callback path means no delegation can authorize local cache
            // use. Drop that authority before any caller sees the reply.
            self.delegations.lock().clear();
            self.invalidate();
        }
        if flags & SEQ4_STATUS_LEASE_MOVED != 0 {
            self.delegations.lock().clear();
            // Treat relocation as a session-generation transition so replies
            // already admitted on the old server cannot publish afterwards.
            self.invalidate_session();
            self.lease_faulted.store(true, Ordering::Release);
            return Err(NfsError::SessionLost);
        }
        if flags & SEQ4_STATUS_EXPIRED_ALL_STATE_REVOKED != 0 {
            // The server revoked every stateid: retain neither an OPEN/LOCK
            // token nor a delegation that could be replayed as live state.
            self.state_records.lock().clear();
            self.delegations.lock().clear();
            self.invalidate_session();
            self.lease_faulted.store(true, Ordering::Release);
            return Err(NfsError::SessionLost);
        }
        if flags
            & (SEQ4_STATUS_RESTART_RECLAIM_NEEDED
                | SEQ4_STATUS_EXPIRED_SOME_STATE_REVOKED
                | SEQ4_STATUS_ADMIN_STATE_REVOKED)
            != 0
        {
            self.delegations.lock().clear();
            self.invalidate_session();
            self.lease_faulted.store(true, Ordering::Release);
            return Err(NfsError::SessionLost);
        }
        Ok(())
    }
    pub fn negotiate(&self, options: &NfsMountOptions) -> NfsResult<()> {
        if options.owner.is_empty() || options.slots == 0 {
            return Err(NfsError::Length);
        }
        RpcSysAuth::validate(&options.auth_sys.machine_name, &options.auth_sys.groups)?;
        match (&self.auth, options.security) {
            (RpcAuth::Sys(_), NfsSecurityFlavor::Sys)
            | (
                RpcAuth::Gss(_),
                NfsSecurityFlavor::Krb5 | NfsSecurityFlavor::Krb5i | NfsSecurityFlavor::Krb5p,
            ) => {}
            // `None` is retained for wire-level and model callers only; it
            // is not a valid mounted NFS security flavour.
            _ => return Err(NfsError::Security),
        }
        if let RpcAuth::Gss(gss) = &self.auth {
            let expected = match options.security {
                NfsSecurityFlavor::Krb5 => RpcGssService::None,
                NfsSecurityFlavor::Krb5i => RpcGssService::Integrity,
                NfsSecurityFlavor::Krb5p => RpcGssService::Privacy,
                NfsSecurityFlavor::Sys => return Err(NfsError::Security),
            };
            if gss.service() != expected {
                return Err(NfsError::Security);
            }
            gss.establish()?;
        }
        // CREATE_SESSION advertises this exact AUTH_SYS callback identity.
        // Publish it before the backchannel is bound because a server may
        // issue CB_SEQUENCE immediately after BIND_CONN_TO_SESSION.
        *self.options.lock() = Some(options.clone());
        let exchange =
            parse_exchange(&self.compound_raw(0, &[Operation::ExchangeId(&options.owner)])?)?;
        let grant = parse_create_session(&self.compound_raw(
            0,
            &[Operation::CreateSession {
                clientid: exchange.clientid,
                sequenceid: exchange.sequenceid,
                slots: options.slots,
                callback_auth: &options.auth_sys,
            }],
        )?)?;
        let slots = options.slots.min(grant.fore_slots);
        if slots == 0 {
            return Err(NfsError::Malformed);
        }
        let id = grant.id;
        *self.session.lock() = Some(Session {
            clientid: exchange.clientid,
            exchange_sequence: exchange.sequenceid,
            id,
            max_operations: grant.fore_max_operations,
            slots: (0..slots)
                .map(|id| Slot {
                    id,
                    sequence: 1,
                    unusable: false,
                    lifecycle: SlotLifecycle::Free,
                })
                .collect(),
            highest_slot: slots - 1,
            target_highest_slot: slots - 1,
            reclaiming: true,
            replay_barrier: false,
        });
        // A caller that successfully re-established the session may resume
        // normal compounds; the worker itself never clears this health bit.
        self.lease_faulted.store(false, Ordering::Release);
        self.install_callback_session(id, slots)?;
        // The reader is already live when the connection binding is
        // acknowledged, so publish the matching callback slot/session first.
        // A server callback immediately following BIND_CONN_TO_SESSION can
        // then be authenticated instead of tearing down the forechannel as
        // an apparent callback-session loss.
        self.bind_connection_to_session(id)?;
        // A recovered client must replay durable OPEN/LOCK state during the
        // server grace period before publishing RECLAIM_COMPLETE.  A fresh
        // mount has no retained state and can complete immediately.
        self.reclaim_state()?;
        Ok(())
    }
    /// Re-establishes the v4.1 client/session after BADSESSION,
    /// DEADSESSION, or a changed write verifier.  The validated mount options
    /// are saved before CREATE_SESSION so its advertised callback credential
    /// is available to an immediate server callback; an unpublished failed
    /// initial mount has no caller which can enter this recovery path.
    pub fn recover(self: &Arc<Self>) -> NfsResult<()> {
        let observed_generation = self.recovery.generation.load(Ordering::Acquire);
        let _reconnect = self.reconnect_ops.lock();
        if self.lease_worker_stop.load(Ordering::Acquire) {
            return Err(NfsError::SessionLost);
        }
        // A caller can wait at `reconnect_ops` while the elected owner
        // completes recovery.  Re-evaluate after acquiring it: generation
        // advancement plus a non-faulted, unbarriered session means there is
        // no recovery work left.  A real BADSESSION path remains recoverable
        // because it leaves the fault/gate set or removes the session.
        let advanced = self.recovery.generation.load(Ordering::Acquire) != observed_generation;
        match (advanced, self.recovery_needed()) {
            (true, false) => return Ok(()),
            // A direct maintenance caller can enter `recover` without first
            // joining the election; it is still never permitted to tear down
            // an already healthy, unbarriered session.
            (false, false) => return Ok(()),
            (_, true) => {}
        }
        let teardown_epoch = self.teardown_epoch.load(Ordering::Acquire);
        // A broken transport is not itself evidence that the server threw
        // away this session.  If one slot still owns an ambiguous request,
        // preserve the session, bind the replacement connection, and replay
        // its immutable record before considering destructive state reclaim.
        if self
            .session
            .lock()
            .as_ref()
            .is_some_and(|session| session.replay_barrier)
        {
            let result = self.reconnect_and_replay_session(teardown_epoch);
            if result.is_ok() {
                self.lease_faulted.store(false, Ordering::Release);
            }
            match result {
                Err(NfsError::Status(BADSESSION | DEADSESSION | CONN_NOT_BOUND_TO_SESSION)) => {
                    self.invalidate_session()
                }
                other => return other,
            }
        }
        // Another waiter may have won the recovery election while this caller
        // waited on `reconnect_ops`.  Terminal records are retained for their
        // original owners, so do not destroy a healthy session underneath
        // them merely because the replay barrier has already been cleared.
        if self.session.lock().as_ref().is_some_and(|session| {
            session
                .slots
                .iter()
                .any(|slot| matches!(&slot.lifecycle, SlotLifecycle::Terminal(_)))
        }) {
            return Ok(());
        }
        let options = self.options.lock().clone().ok_or(NfsError::SessionLost)?;
        // Close admission before touching the old channel.  An admitted RPC
        // can be asleep in `RpcTransport::call` waiting for a reply that will
        // never arrive after a server/session failure; waiting for its
        // `CompoundAdmission` to drain before shutdown would deadlock the
        // recovery owner against that waiter.  The old transport is still
        // published until the drain completes, so no caller can observe the
        // replacement before every old-channel result has retired.
        self.reclaim_gate.store(true, Ordering::Release);
        let old = self.transport();
        old.shutdown();
        while self.active_compounds.load(Ordering::Acquire) != 0 {
            axtask::yield_now();
        }
        self.invalidate_session();
        // Full state reconstruction mutates OPEN/LOCK records.  A stateful
        // caller may be the one that synchronously discovered the fatal
        // reply and still own this mutex while waiting for `recover()`.  Do
        // not turn that into a circular wait after the channel has already
        // been fenced: retain the fault/gate and let the lease worker (or a
        // later recovery owner) rebuild once the caller has unwound.
        let Some(_state_ops) = self.state_ops.try_lock() else {
            self.lease_faulted.store(true, Ordering::Release);
            return Err(NfsError::SessionLost);
        };
        // The replacement remains private until negotiation has rebuilt the
        // forechannel and callback binding.
        let replacement = match old.reconnect(Arc::downgrade(self)) {
            Ok(transport) => transport,
            Err(error) => {
                self.lease_faulted.store(true, Ordering::Release);
                return Err(error);
            }
        };
        if self.lease_worker_stop.load(Ordering::Acquire)
            || self.teardown_epoch.load(Ordering::Acquire) != teardown_epoch
        {
            replacement.shutdown();
            return Err(NfsError::SessionLost);
        }
        *self.transport.lock() = replacement.clone();
        let result = self.negotiate(&options);
        if result.is_err() {
            self.lease_faulted.store(true, Ordering::Release);
            return result;
        }
        if self.lease_worker_stop.load(Ordering::Acquire)
            || self.teardown_epoch.load(Ordering::Acquire) != teardown_epoch
        {
            replacement.shutdown();
            self.lease_faulted.store(true, Ordering::Release);
            return Err(NfsError::SessionLost);
        }
        Ok(())
    }
    /// Reconnect a surviving session without reconstructing a COMPOUND.  The
    /// only bytes admitted after BIND_CONN_TO_SESSION are the record captured
    /// in `SlotLifecycle::Sent`; this is what prevents a timeout around OPEN
    /// or WRITE from becoming a duplicate state-changing operation.
    fn reconnect_and_replay_session(self: &Arc<Self>, teardown_epoch: u64) -> NfsResult<()> {
        let old = self.transport();
        let replacement = old.reconnect(Arc::downgrade(self))?;
        if self.lease_worker_stop.load(Ordering::Acquire)
            || self.teardown_epoch.load(Ordering::Acquire) != teardown_epoch
        {
            replacement.shutdown();
            return Err(NfsError::SessionLost);
        }
        let sessionid = self
            .session
            .lock()
            .as_ref()
            .ok_or(NfsError::SessionLost)?
            .id;
        old.shutdown();
        *self.transport.lock() = replacement;
        // `compound_mode` is used only for the binding handshake here.  It
        // has one ordinary free slot and does not expose the barrier to an
        // external caller; all externally admitted compounds still fail
        // until every captured record below has a definitive reply.
        // `reclaim=true` is reserved to the recovery owner below.  Keep the
        // replay barrier set while it binds, so an ordinary caller cannot
        // race this free slot between BIND and the exact retransmission.
        self.lease_faulted.store(false, Ordering::Release);
        let mut replay_guard = SurvivingReplayGuard {
            mount: self,
            complete: false,
        };
        let bind = self.bind_connection_to_session(sessionid);
        if bind.is_err() {
            self.lease_faulted.store(true, Ordering::Release);
            if let Some(session) = self.session.lock().as_mut() {
                session.replay_barrier = true;
            }
            return bind;
        }
        let replays: Vec<(u32, u32, Replay)> = self
            .session
            .lock()
            .as_ref()
            .ok_or(NfsError::SessionLost)?
            .slots
            .iter()
            .filter_map(|slot| match &slot.lifecycle {
                SlotLifecycle::Sent(replay) => Some((
                    slot.id,
                    slot.sequence,
                    Replay {
                        xid: replay.xid,
                        sequence: replay.sequence,
                        request: replay.request.clone(),
                        gss_sequence: replay.gss_sequence,
                        reply: replay.reply.clone(),
                        terminal: replay.terminal.clone(),
                    },
                )),
                SlotLifecycle::Free | SlotLifecycle::Terminal(_) => None,
            })
            .collect();
        for (slot, sequence, replay) in replays {
            if replay.sequence != sequence {
                self.invalidate_session();
                return Err(NfsError::SessionLost);
            }
            let request = EncodedRpc {
                record: replay.request.clone(),
                gss_sequence: replay.gss_sequence,
            };
            let reply = self.rpc_record_exact(replay.xid, &request)?;
            // These statuses revoke the session itself.  They must escape to
            // recover() before any old-slot Terminal is published; otherwise
            // a caller could consume an outcome tied to a session that full
            // EXCHANGE_ID/CREATE_SESSION recovery has just replaced.
            if matches!(
                &reply.terminal_error,
                Some(NfsError::Status(
                    BADSESSION | DEADSESSION | CONN_NOT_BOUND_TO_SESSION
                ))
            ) {
                return Err(reply.terminal_error.clone().ok_or(NfsError::SessionLost)?);
            }
            if reply.items.len() == 1
                && reply.items[0].0 == OP_SEQUENCE
                && reply.terminal_error.is_some()
            {
                let encoded = reply.bytes();
                let terminal = reply.terminal_error.clone();
                let mut guard = self.session.lock();
                let session = guard.as_mut().ok_or(NfsError::SessionLost)?;
                let entry = session
                    .slots
                    .iter_mut()
                    .find(|entry| entry.id == slot)
                    .ok_or(NfsError::SessionLost)?;
                match &entry.lifecycle {
                    SlotLifecycle::Sent(current)
                        if current.xid == replay.xid
                            && current.sequence == sequence
                            && current.request == replay.request =>
                    {
                        // Keep replay completion transitions identical to
                        // the direct SEQUENCE-error state machine.  In
                        // particular a BADSLOT must survive Terminal
                        // consumption: `take_replayed_reply` frees custody,
                        // but cannot make the server-rejected slot usable.
                        match &terminal {
                            Some(NfsError::Status(BADSLOT)) => entry.unusable = true,
                            Some(NfsError::Status(RETRY_UNCACHED_REP)) => {
                                entry.sequence = entry.sequence.wrapping_add(1)
                            }
                            // SEQ_FALSE_RETRY and SEQ_MISORDERED did not
                            // acknowledge a new sequence.  Retain this
                            // sequence number exactly; the caller receives
                            // the terminal status rather than a new logical
                            // request being synthesized here.
                            Some(NfsError::Status(SEQ_FALSE_RETRY | SEQ_MISORDERED)) | _ => {}
                        }
                        entry.lifecycle = SlotLifecycle::Terminal(Replay {
                            xid: replay.xid,
                            sequence,
                            request: replay.request,
                            gss_sequence: replay.gss_sequence,
                            reply: encoded,
                            terminal,
                        });
                    }
                    _ => return Err(NfsError::SessionLost),
                }
                continue;
            }
            if reply.top_status != NFS_OK && reply.operation_count == 0 {
                let encoded = reply.bytes();
                let terminal = reply.terminal_error.clone();
                let mut guard = self.session.lock();
                let session = guard.as_mut().ok_or(NfsError::SessionLost)?;
                let entry = session
                    .slots
                    .iter_mut()
                    .find(|entry| entry.id == slot)
                    .ok_or(NfsError::SessionLost)?;
                match &entry.lifecycle {
                    SlotLifecycle::Sent(current)
                        if current.xid == replay.xid
                            && current.sequence == sequence
                            && current.request == replay.request =>
                    {
                        entry.lifecycle = SlotLifecycle::Terminal(Replay {
                            xid: replay.xid,
                            sequence,
                            request: replay.request,
                            gss_sequence: replay.gss_sequence,
                            reply: encoded,
                            terminal,
                        })
                    }
                    _ => return Err(NfsError::SessionLost),
                }
                continue;
            }
            if reply.sequence_sessionid != Some(sessionid)
                || reply.sequence_slot != Some(slot)
                || reply.sequence_id != Some(sequence)
            {
                self.invalidate_session();
                return Err(NfsError::SessionLost);
            }
            self.update_slot_window(
                sessionid,
                reply.sequence_highest.ok_or(NfsError::Malformed)?,
                reply.sequence_target_highest.ok_or(NfsError::Malformed)?,
            );
            self.apply_sequence_status(reply.sequence_flags)?;
            let encoded = reply.bytes();
            let mut guard = self.session.lock();
            let session = guard.as_mut().ok_or(NfsError::SessionLost)?;
            let entry = session
                .slots
                .iter_mut()
                .find(|entry| entry.id == slot)
                .ok_or(NfsError::SessionLost)?;
            match &entry.lifecycle {
                SlotLifecycle::Sent(current)
                    if current.xid == replay.xid
                        && current.sequence == sequence
                        && current.request == replay.request =>
                {
                    entry.sequence = entry.sequence.wrapping_add(1);
                    entry.lifecycle = SlotLifecycle::Terminal(Replay {
                        xid: replay.xid,
                        sequence,
                        request: replay.request,
                        gss_sequence: replay.gss_sequence,
                        reply: encoded,
                        terminal: reply.terminal_error.clone(),
                    });
                }
                _ => return Err(NfsError::SessionLost),
            }
            drop(guard);
        }
        let mut guard = self.session.lock();
        let session = guard.as_mut().ok_or(NfsError::SessionLost)?;
        session.replay_barrier = false;
        self.lease_faulted.store(false, Ordering::Release);
        replay_guard.complete = true;
        Ok(())
    }
    /// BIND_CONN_TO_SESSION is an out-of-band, one-operation COMPOUND.  It
    /// must not prepend SEQUENCE or claim a normal forechannel slot: recovery
    /// is required to work even when every normal slot is still `Sent`.
    fn bind_connection_to_session(&self, sessionid: [u8; 16]) -> NfsResult<()> {
        let xid = self.xid.fetch_add(1, Ordering::Relaxed);
        let request = self.compound_record(
            xid,
            1,
            &[Operation::BindConnToSession {
                sessionid,
                direction: 3,
            }],
        )?;
        let reply = self.rpc_record(xid, &request)?;
        if let Some(error) = reply.terminal_error.clone() {
            return Err(error);
        }
        if reply.top_status != NFS_OK
            || reply.items.len() != 1
            || reply.items[0].0 != OP_BIND_CONN_TO_SESSION
        {
            return Err(NfsError::Malformed);
        }
        parse_bind_connection(&reply, sessionid, 3, false)?;
        Ok(())
    }
    /// Hands the exact replay completion back to the original caller.  A
    /// completion is consumed once, after which the slot becomes available;
    /// this prevents a late waiter, cancellation, or close path from
    /// accidentally publishing the same OPEN/WRITE result twice.
    fn take_replayed_reply(
        &self,
        sessionid: [u8; 16],
        slot: u32,
        sequence: u32,
        xid: u32,
    ) -> NfsResult<Reply> {
        let replay = {
            let mut guard = self.session.lock();
            let session = guard.as_mut().ok_or(NfsError::SessionLost)?;
            if session.id != sessionid {
                return Err(NfsError::SessionLost);
            }
            let entry = session
                .slots
                .iter_mut()
                .find(|entry| entry.id == slot)
                .ok_or(NfsError::SessionLost)?;
            let SlotLifecycle::Terminal(current) = &entry.lifecycle else {
                return Err(NfsError::SessionLost);
            };
            if current.xid != xid || current.sequence != sequence {
                return Err(NfsError::SessionLost);
            }
            let replay = Replay {
                xid: current.xid,
                sequence: current.sequence,
                request: current.request.clone(),
                gss_sequence: current.gss_sequence,
                reply: current.reply.clone(),
                terminal: current.terminal.clone(),
            };
            entry.lifecycle = SlotLifecycle::Free;
            replay
        };
        if let Some(error) = replay.terminal {
            return Err(error);
        }
        Reply::parse(&replay.reply)
    }
    /// Replay every state-owner record made durable before the previous
    /// session died.  OPEN records are replayed before their dependent LOCK
    /// records, and each accepted replacement stateid becomes authoritative
    /// before the next dependent request is encoded.
    fn reclaim_state(&self) -> NfsResult<()> {
        let clientid = self.clientid()?;
        let opens: Vec<(u64, [u8; 16], u32, StateReplay)> = self
            .state_records
            .lock()
            .iter()
            .filter(|record| record.reclaim && !record.lock)
            .map(|record| {
                (
                    record.owner,
                    record.stateid,
                    record.next_seqid,
                    record.replay.clone(),
                )
            })
            .collect();
        for (owner, old_stateid, seqid, replay) in opens {
            let StateReplay::Open {
                parent,
                name,
                share_access,
                share_deny,
                create,
                mode,
                create_verifier,
                delegation_preference,
            } = replay
            else {
                continue;
            };
            // CLAIM_PREVIOUS is the only recovery claim: creation details are
            // retained for audit/replay identity, but never re-issued as a
            // CREATE during grace.  The server already owns the object.
            let _ = (create, mode, create_verifier, delegation_preference);
            self.reserve_delegation_record()?;
            let reply = self.retry_grace(|| {
                self.compound_mode(
                    &[
                        Operation::PutFh(&parent),
                        Operation::Open {
                            seqid,
                            reclaim: true,
                            clientid,
                            owner,
                            share_access,
                            share_deny,
                            name: name.clone(),
                        },
                    ],
                    true,
                )
            })?;
            let (replacement, delegation) = match parse_open_result(&reply) {
                Ok(result) => result,
                Err(error) => {
                    self.invalidate_session();
                    return Err(error);
                }
            };
            if let Some(delegation) = delegation {
                let handle = self
                    .state_records
                    .lock()
                    .iter()
                    .find(|record| {
                        record.owner == owner && record.stateid == old_stateid && !record.lock
                    })
                    .map(|record| record.handle.clone())
                    .ok_or(NfsError::SessionLost)?;
                if let Err(error) = self.install_delegation(handle.clone(), delegation) {
                    return Err(self.abandon_unrecorded_delegation(&handle, delegation, error));
                }
            }
            self.replace_reclaimed_state(owner, old_stateid, replacement, false)?;
        }
        let locks: Vec<(u64, [u8; 16], FileHandle, u32, StateReplay)> = self
            .state_records
            .lock()
            .iter()
            .filter(|record| record.reclaim && record.lock)
            .map(|record| {
                (
                    record.owner,
                    record.stateid,
                    record.handle.clone(),
                    record.next_seqid,
                    record.replay.clone(),
                )
            })
            .collect();
        for (owner, old_stateid, handle, next_lock_seqid, replay) in locks {
            let StateReplay::Lock {
                open_owner,
                open_stateid,
                open_seqid: recorded_open_seqid,
                offset,
                length,
                write,
                lock_seqid: recorded_lock_seqid,
            } = replay
            else {
                continue;
            };
            let recovered_open = self.reclaimed_open_stateid(open_owner, open_stateid)?;
            let open_seqid = self
                .state_seqid(open_owner, recovered_open, false)?
                .max(recorded_open_seqid);
            let reply = self.retry_grace(|| {
                self.compound_mode(
                    &[
                        Operation::PutFh(&handle),
                        Operation::Lock {
                            open_seqid,
                            lock_seqid: next_lock_seqid,
                            reclaim: true,
                            stateid: recovered_open,
                            clientid,
                            owner,
                            offset,
                            length,
                            write,
                        },
                    ],
                    true,
                )
            })?;
            let replacement = parse_lock(&reply)?;
            // Every recovered new lock-owner consumes the recovered OPEN
            // owner's sequence just like the non-recovery LOCK path.
            self.advance_state(open_owner, recovered_open, recovered_open, false, false);
            self.replace_reclaimed_state(owner, old_stateid, replacement, true)?;
            let mut records = self.state_records.lock();
            if let Some(record) = records.iter_mut().find(|record| {
                record.owner == owner && record.stateid == replacement && record.lock
            }) {
                record.next_seqid = next_lock_seqid
                    .saturating_add(1)
                    .max(recorded_lock_seqid.saturating_add(1));
                if let StateReplay::Lock { open_stateid, .. } = &mut record.replay {
                    *open_stateid = recovered_open;
                }
            }
        }
        if self
            .state_records
            .lock()
            .iter()
            .any(|record| record.reclaim)
        {
            return Err(NfsError::SessionLost);
        }
        self.retry_grace(|| self.compound_mode(&[Operation::ReclaimComplete], true))?;
        self.session
            .lock()
            .as_mut()
            .ok_or(NfsError::SessionLost)?
            .reclaiming = false;
        self.reclaim_gate.store(false, Ordering::Release);
        Ok(())
    }
    fn retry_grace<T>(&self, mut operation: impl FnMut() -> NfsResult<T>) -> NfsResult<T> {
        // A compound is retained in its slot's replay image, so retrying a
        // GRACE response does not manufacture a second owner operation.
        let mut grace = 0;
        loop {
            match operation() {
                Err(NfsError::Status(GRACE)) if grace < 8 => {
                    grace += 1;
                    retry_sleep(core::time::Duration::from_millis(10));
                }
                result => return result,
            }
        }
    }
    fn replace_reclaimed_state(
        &self,
        owner: u64,
        old: [u8; 16],
        replacement: [u8; 16],
        lock: bool,
    ) -> NfsResult<()> {
        let mut records = self.state_records.lock();
        let record = records
            .iter_mut()
            .find(|record| record.owner == owner && record.stateid == old && record.lock == lock)
            .ok_or(NfsError::SessionLost)?;
        record.stateid = replacement;
        record.next_seqid = record.next_seqid.saturating_add(1);
        record.reclaim = false;
        Ok(())
    }
    fn reclaimed_open_stateid(&self, owner: u64, previous: [u8; 16]) -> NfsResult<[u8; 16]> {
        let records = self.state_records.lock();
        records
            .iter()
            .find(|record| record.owner == owner && !record.lock && !record.reclaim)
            .map(|record| record.stateid)
            .or_else(|| {
                records
                    .iter()
                    .find(|record| {
                        record.owner == owner && !record.lock && record.stateid == previous
                    })
                    .map(|record| record.stateid)
            })
            .ok_or(NfsError::SessionLost)
    }
    pub fn root_filehandle(&self) -> NfsResult<FileHandle> {
        parse_getfh(&self.compound(&[Operation::PutRootFh, Operation::GetFh])?)
    }
    /// RFC 5661 named-attribute directory.  `create` is only used by setxattr
    /// after the caller has committed to changing the namespace.
    pub fn openattr(&self, file: &FileHandle, create: bool) -> NfsResult<FileHandle> {
        parse_getfh(&self.compound(&[
            Operation::PutFh(file),
            Operation::OpenAttr(create),
            Operation::GetFh,
        ])?)
    }
    pub fn statfs(&self) -> NfsResult<NfsAttr> {
        self.root_attrs()
    }
    pub fn quota(&self, file: &FileHandle) -> NfsResult<NfsQuota> {
        let attr = parse_attr(&self.compound(&[Operation::PutFh(file), Operation::GetAttr])?)?;
        Ok(NfsQuota {
            hard_available: attr.quota_avail_hard,
            soft_available: attr.quota_avail_soft,
            used: attr.quota_used,
        })
    }
    pub fn clientid(&self) -> NfsResult<u64> {
        Ok(self
            .session
            .lock()
            .as_ref()
            .ok_or(NfsError::SessionLost)?
            .clientid)
    }
    /// The standard CREATE arms below encode mode/owner/group/ACL directly as
    /// fattr4; callers use this capability rather than a private xattr.
    fn supported_attrs(&self) -> NfsResult<[u32; 2]> {
        if let Some(attrs) = *self.supported_attrs.lock() {
            return Ok(attrs);
        }
        let attrs = parse_supported_attrs(
            &self.compound(&[Operation::PutRootFh, Operation::GetSupportedAttrs])?,
        )?;
        let mut cached = self.supported_attrs.lock();
        if cached.is_none() {
            *cached = Some(attrs);
        }
        Ok(cached.unwrap_or(attrs))
    }
    fn require_attrs(&self, first: u32, second: u32) -> NfsResult<()> {
        let supported = self.supported_attrs()?;
        if supported[0] & first != first || supported[1] & second != second {
            return Err(NfsError::Status(10004));
        }
        Ok(())
    }
    /// Capability discovery is tied to the server's real GETATTR reply.  Do
    /// not treat the mount type as evidence that ordered ALLOW/DENY ACLs are
    /// accepted.
    pub fn acl_capabilities(&self) -> NfsResult<NfsAclCapabilities> {
        if let Some(caps) = self.acl_capabilities.lock().clone() {
            return Ok(caps);
        }
        let supported = self.supported_attrs()?;
        if supported[0] & (1 << 12) == 0 || supported[0] & (1 << 13) == 0 {
            return Err(NfsError::Status(10004));
        }
        let caps = parse_acl_capabilities(
            &self.compound(&[Operation::PutRootFh, Operation::GetAclCapabilities])?,
            supported,
        )?;
        if !caps.permits_allow_deny() {
            return Err(NfsError::Status(10004));
        }
        let mut cached = self.acl_capabilities.lock();
        if cached.is_none() {
            *cached = Some(caps.clone())
        };
        Ok(cached.clone().unwrap_or(caps))
    }
    /// This is discovered from the server's actual FATTR4_SUPPORTED_ATTRS
    /// bitmap, never a static assumption about an NFS provider.
    pub fn supports_create_attrs(&self) -> bool {
        self.require_attrs(
            1 << 12,
            (1 << (33 - 32)) | (1 << (36 - 32)) | (1 << (37 - 32)),
        )
        .is_ok()
    }
    pub fn lookup(&self, parent: Option<&FileHandle>, name: &FsName) -> NfsResult<FileHandle> {
        if name.as_bytes().is_empty() || name.as_bytes().contains(&0) {
            return Err(NfsError::Length);
        }
        let first = parent
            .map(|fh| Operation::PutFh(fh))
            .unwrap_or(Operation::PutRootFh);
        parse_getfh(&self.compound(&[
            first,
            Operation::Lookup(name.to_owned()),
            Operation::GetFh,
        ])?)
    }
    pub fn lookup_attrs(
        &self,
        parent: Option<&FileHandle>,
        name: &FsName,
    ) -> NfsResult<(FileHandle, NfsAttr)> {
        if name.as_bytes().is_empty() || name.as_bytes().contains(&0) {
            return Err(NfsError::Length);
        }
        let first = parent.map(Operation::PutFh).unwrap_or(Operation::PutRootFh);
        let reply = self.compound(&[
            first,
            Operation::Lookup(name.to_owned()),
            Operation::GetFh,
            Operation::GetAttr,
        ])?;
        Ok((parse_getfh(&reply)?, parse_attr(&reply)?))
    }
    pub fn root_attrs(&self) -> NfsResult<NfsAttr> {
        parse_attr(&self.compound(&[Operation::PutRootFh, Operation::GetAttr])?)
    }
    pub fn attrs(&self, handle: &FileHandle) -> NfsResult<NfsAttr> {
        parse_attr(&self.compound(&[Operation::PutFh(handle), Operation::GetAttr])?)
    }
    /// Opens a name through an already resolved parent.  Owner values are
    /// per-open descriptions, so close/reopen and fork state cannot alias a
    /// local pathname cache accidentally.
    pub fn open(
        &self,
        parent: &FileHandle,
        name: &FsName,
        share_access: u32,
        share_deny: u32,
    ) -> NfsResult<OpenState> {
        if name.as_bytes().is_empty() || name.as_bytes().contains(&0) {
            return Err(NfsError::Length);
        }
        let _state_ops = self.state_ops.lock();
        // Reserve before OPEN is visible remotely; otherwise an allocator
        // failure can strand an open-owner with no durable reclaim record.
        self.reserve_state_record()?;
        self.reserve_delegation_record()?;
        let owner = self.allocate_owner();
        let clientid = self.clientid()?;
        let replay = StateReplay::Open {
            parent: parent.clone(),
            name: name.to_owned(),
            share_access,
            share_deny,
            create: false,
            mode: 0,
            create_verifier: [0; 8],
            delegation_preference: true,
        };
        let reply = self.compound(&[
            Operation::PutFh(parent),
            Operation::Open {
                seqid: 0,
                reclaim: false,
                clientid,
                owner,
                share_access,
                share_deny,
                name: name.to_owned(),
            },
            Operation::GetFh,
        ])?;
        let parsed = (|| -> NfsResult<([u8; 16], Option<[u8; 16]>, FileHandle)> {
            let (stateid, delegation) = parse_open_result(&reply)?;
            let handle = parse_getfh(&reply)?;
            Ok((stateid, delegation, handle))
        })();
        let (stateid, delegation, handle) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                self.invalidate_session();
                return Err(error);
            }
        };
        self.register_reserved_state(owner, handle.clone(), stateid, false, replay);
        if let Some(delegation) = delegation {
            if let Err(error) = self.install_delegation(handle.clone(), delegation) {
                let error = self.abandon_unrecorded_delegation(&handle, delegation, error);
                let _ = self.close_open_locked(&OpenState {
                    stateid,
                    handle: handle.clone(),
                    owner,
                    sequence: 1,
                });
                return Err(error);
            }
        }
        Ok(OpenState {
            stateid,
            handle,
            owner,
            sequence: 1,
        })
    }
    pub fn create_file(
        &self,
        parent: &FileHandle,
        name: &FsName,
        mode: u32,
        share_access: u32,
    ) -> NfsResult<OpenState> {
        self.create_file_with_attrs(parent, name, mode, share_access, None, None)
    }
    /// OPEN(CREATE) fattr4 is the only atomic regular-file create path.  UID
    /// and GID are translated to NFSv4 owner principals before this compound;
    /// no follow-up SETATTR is used for a newly visible name.
    pub fn create_file_with_attrs(
        &self,
        parent: &FileHandle,
        name: &FsName,
        mode: u32,
        share_access: u32,
        attr_owner: Option<&[u8]>,
        attr_group: Option<&[u8]>,
    ) -> NfsResult<OpenState> {
        self.create_file_with_acl_attrs(
            parent,
            name,
            mode,
            share_access,
            attr_owner,
            attr_group,
            None,
        )
    }
    /// `attr_acl` is an already XDR-encoded `acl4` value (ACE count and ACEs),
    /// supplied by the VFS ACL translator; it is never a private xattr.
    pub fn create_file_with_acl_attrs(
        &self,
        parent: &FileHandle,
        name: &FsName,
        mode: u32,
        share_access: u32,
        attr_owner: Option<&[u8]>,
        attr_group: Option<&[u8]>,
        attr_acl: Option<&[u8]>,
    ) -> NfsResult<OpenState> {
        if attr_acl.is_some() {
            let _ = self.acl_capabilities()?;
        }
        self.require_attrs(
            if attr_acl.is_some() { 1 << 12 } else { 0 },
            (1 << (33 - 32))
                | if attr_owner.is_some() {
                    1 << (36 - 32)
                } else {
                    0
                }
                | if attr_group.is_some() {
                    1 << (37 - 32)
                } else {
                    0
                },
        )?;
        self.create_file_with_attrs_how(
            parent,
            name,
            mode,
            share_access,
            attr_owner,
            attr_group,
            attr_acl,
            CreateHow::Guarded,
        )
        .map(|result| result.0)
    }
    /// The OPEN(CREATE UNCHECKED) reply contains both the opened object and
    /// the directory change_info4, so OpenOrCreate never needs a racy
    /// lookup-before-create probe.
    pub fn open_or_create_file_with_attrs(
        &self,
        parent: &FileHandle,
        name: &FsName,
        mode: u32,
        share_access: u32,
        attr_owner: Option<&[u8]>,
        attr_group: Option<&[u8]>,
        attr_acl: Option<&[u8]>,
    ) -> NfsResult<(OpenState, bool, NfsAttr)> {
        if attr_acl.is_some() {
            let _ = self.acl_capabilities()?;
        }
        self.require_attrs(
            if attr_acl.is_some() { 1 << 12 } else { 0 },
            (1 << (33 - 32))
                | if attr_owner.is_some() {
                    1 << (36 - 32)
                } else {
                    0
                }
                | if attr_group.is_some() {
                    1 << (37 - 32)
                } else {
                    0
                },
        )?;
        // UNCHECKED cannot distinguish a pre-existing name from a name this
        // RPC installed.  GUARDED makes EXIST the linearization point; only
        // that definitive reply may fall through to a normal OPEN.
        for _ in 0..8 {
            match self.create_file_with_attrs_how(
                parent,
                name,
                mode,
                share_access,
                attr_owner,
                attr_group,
                attr_acl,
                CreateHow::Guarded,
            ) {
                Ok(created) => return Ok(created),
                Err(NfsError::Status(17)) => match self.open(parent, name, share_access, 0) {
                    Ok(open) => match self.attrs(&open.handle) {
                        Ok(attr) => return Ok((open, false, attr)),
                        Err(error) => {
                            let _ = self.close(&open);
                            return Err(error);
                        }
                    },
                    Err(NfsError::Status(2)) => continue,
                    Err(error) => return Err(error),
                },
                Err(error) => return Err(error),
            }
        }
        Err(NfsError::WouldBlock)
    }
    fn create_file_with_attrs_how(
        &self,
        parent: &FileHandle,
        name: &FsName,
        mode: u32,
        share_access: u32,
        attr_owner: Option<&[u8]>,
        attr_group: Option<&[u8]>,
        attr_acl: Option<&[u8]>,
        create_how: CreateHow,
    ) -> NfsResult<(OpenState, bool, NfsAttr)> {
        if name.is_empty() || name.as_bytes().contains(&0) {
            return Err(NfsError::Length);
        }
        let _state_ops = self.state_ops.lock();
        // Match normal OPEN and LOCK: state-record capacity is reserved
        // before CREATE can publish an open-owner on the server.
        self.reserve_state_record()?;
        self.reserve_delegation_record()?;
        let owner = self.allocate_owner();
        let clientid = self.clientid()?;
        let verifier = owner.to_be_bytes();
        let replay = StateReplay::Open {
            parent: parent.clone(),
            name: name.to_owned(),
            share_access,
            share_deny: 0,
            create: true,
            mode,
            create_verifier: verifier,
            delegation_preference: true,
        };
        let reply = self.compound(&[
            Operation::PutFh(parent),
            Operation::OpenCreate {
                seqid: 0,
                clientid,
                owner,
                share_access,
                name: name.to_owned(),
                mode,
                attr_owner,
                attr_group,
                attr_acl,
                create_how,
            },
            Operation::GetFh,
        ])?;
        let parsed = (|| -> NfsResult<([u8; 16], Option<[u8; 16]>, bool, [u32; 2], FileHandle)> {
            let (stateid, delegation) = parse_open_result(&reply)?;
            let created = matches!(create_how, CreateHow::Guarded) || parse_open_created(&reply)?;
            let applied = parse_open_attrset(&reply)?;
            let handle = parse_getfh(&reply)?;
            Ok((stateid, delegation, created, applied, handle))
        })();
        let (stateid, delegation, created, applied, handle) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                self.invalidate_session();
                return Err(error);
            }
        };
        let required_first = if attr_acl.is_some() { 1 << 12 } else { 0 };
        let required_second = (1 << (33 - 32))
            | if attr_owner.is_some() {
                1 << (36 - 32)
            } else {
                0
            }
            | if attr_group.is_some() {
                1 << (37 - 32)
            } else {
                0
            };
        let open = OpenState {
            stateid,
            handle: handle.clone(),
            owner,
            sequence: 1,
        };
        self.register_reserved_state(owner, handle, stateid, false, replay);
        if applied[0] & required_first != required_first
            || applied[1] & required_second != required_second
        {
            if let Some(delegation) = delegation {
                self.return_granted_or_recover(&open.handle, delegation);
            }
            let cleanup = self.close_open_locked(&open);
            if cleanup.is_ok() && created {
                self.rollback_created(parent, name, &open.handle);
            }
            return Err(cleanup.err().unwrap_or(NfsError::Status(10004)));
        }
        let verified = self
            .compound(&[Operation::PutFh(&open.handle), Operation::GetAttr])
            .and_then(|reply| {
                let attr = parse_attr(&reply)?;
                if attr.mode != mode
                    || attr_owner.is_some_and(|owner| attr.owner.as_slice() != owner)
                    || attr_group.is_some_and(|group| attr.owner_group.as_slice() != group)
                {
                    return Err(NfsError::Status(10004));
                }
                Ok(attr)
            });
        match verified {
            Ok(attr) => {
                if let Some(delegation) = delegation {
                    if let Err(error) = self.install_delegation(open.handle.clone(), delegation) {
                        let error =
                            self.abandon_unrecorded_delegation(&open.handle, delegation, error);
                        let cleanup = self.close_open_locked(&open);
                        if cleanup.is_ok() && created {
                            self.rollback_created(parent, name, &open.handle);
                        }
                        return Err(cleanup.err().unwrap_or(error));
                    }
                }
                Ok((open, created, attr))
            }
            Err(error) => {
                if let Some(delegation) = delegation {
                    self.return_granted_or_recover(&open.handle, delegation);
                }
                let cleanup = self.close_open_locked(&open);
                if cleanup.is_ok() && created {
                    self.rollback_created(parent, name, &open.handle);
                }
                Err(cleanup.err().unwrap_or(error))
            }
        }
    }
    /// Cleanup is identity-checked in one COMPOUND, so a racing replacement
    /// under the same name is never removed.
    fn rollback_created(&self, parent: &FileHandle, name: &FsName, handle: &FileHandle) {
        if let Ok(attr) = self.attrs(handle) {
            let _ = self.remove_verified(parent, name, &attr);
        }
    }
    /// Acquires an NFS byte-range lock.  The remote stateid returned here is
    /// retained independently from the open stateid and must be released by a
    /// matching CLOSE/LOCKU path during VFS teardown.
    pub fn lock(
        &self,
        open: &OpenState,
        offset: u64,
        length: u64,
        write: bool,
    ) -> NfsResult<LockState> {
        let _state_ops = self.state_ops.lock();
        // Reserve bookkeeping before the server can create a lock-owner.  If
        // allocation cannot succeed, issuing LOCK would leave a remote lock
        // that the caller has no state object to release or reclaim.
        self.reserve_state_record()?;
        let owner = self.allocate_owner();
        let clientid = self.clientid()?;
        let (open_stateid, open_seqid) =
            self.authoritative_state(open.owner, open.stateid, false)?;
        let replay = StateReplay::Lock {
            open_owner: open.owner,
            open_stateid,
            open_seqid,
            offset,
            length,
            write,
            lock_seqid: 0,
        };
        let lock_seqid = 0;
        let reply = self.compound(&[
            Operation::PutFh(&open.handle),
            Operation::Lock {
                open_seqid,
                lock_seqid,
                reclaim: false,
                stateid: open_stateid,
                clientid,
                owner,
                offset,
                length,
                write,
            },
        ])?;
        let stateid = parse_lock(&reply)?;
        // LOCK with a new lock-owner consumes the parent open-owner seqid.
        // Advance it even if subsequent local bookkeeping fails: the server
        // has already accepted this state transition, and a cleanup LOCKU or
        // later CLOSE must never reuse the old open sequence number.
        self.advance_state(open.owner, open_stateid, open_stateid, false, false);
        self.register_reserved_state(owner, open.handle.clone(), stateid, true, replay);
        Ok(LockState {
            stateid,
            owner,
            sequence: 1,
        })
    }
    pub fn test_lock(
        &self,
        handle: &FileHandle,
        offset: u64,
        length: u64,
        write: bool,
    ) -> NfsResult<()> {
        let clientid = self.clientid()?;
        self.compound(&[
            Operation::PutFh(handle),
            Operation::LockT {
                clientid,
                offset,
                length,
                write,
            },
        ])
        .map(|_| ())
    }
    pub fn unlock(
        &self,
        handle: &FileHandle,
        lock: &LockState,
        offset: u64,
        length: u64,
        write: bool,
    ) -> NfsResult<[u8; 16]> {
        let _state_ops = self.state_ops.lock();
        let (stateid, seqid) = self.authoritative_state(lock.owner, lock.stateid, true)?;
        let next = parse_locku(&self.compound(&[
            Operation::PutFh(handle),
            Operation::LockU {
                seqid,
                stateid,
                offset,
                length,
                write,
            },
        ])?)?;
        // Each VFS lock gets a distinct lock-owner.  Once its matching LOCKU
        // succeeds no remote lock state remains for that owner, so retaining
        // the record would incorrectly reclaim an already released lock
        // after session recovery.
        self.advance_state(lock.owner, stateid, next, true, true);
        Ok(next)
    }
    pub fn create_dir(&self, parent: &FileHandle, name: &FsName) -> NfsResult<FileHandle> {
        self.create_dir_with_attrs(parent, name, 0o777, None, None, None)
    }
    pub fn create_dir_with_attrs(
        &self,
        parent: &FileHandle,
        name: &FsName,
        mode: u32,
        owner: Option<&[u8]>,
        group: Option<&[u8]>,
        acl: Option<&[u8]>,
    ) -> NfsResult<FileHandle> {
        if name.is_empty() || name.as_bytes().contains(&0) {
            return Err(NfsError::Length);
        }
        if acl.is_some() {
            let _ = self.acl_capabilities()?;
        }
        let required_first = if acl.is_some() { 1 << 12 } else { 0 };
        let required_second = (1 << (33 - 32))
            | if owner.is_some() { 1 << (36 - 32) } else { 0 }
            | if group.is_some() { 1 << (37 - 32) } else { 0 };
        self.require_attrs(required_first, required_second)?;
        let reply = self.compound(&[
            Operation::PutFh(parent),
            Operation::CreateDir {
                name: name.to_owned(),
                mode,
                owner,
                group,
                acl,
            },
            Operation::GetFh,
        ])?;
        let applied = parse_create_attrset(&reply)?;
        let handle = parse_getfh(&reply)?;
        if applied[0] & required_first != required_first
            || applied[1] & required_second != required_second
        {
            self.rollback_created(parent, name, &handle);
            return Err(NfsError::Status(10004));
        }
        let attr = self.attrs(&handle)?;
        if attr.mode != mode
            || owner.is_some_and(|value| attr.owner.as_slice() != value)
            || group.is_some_and(|value| attr.owner_group.as_slice() != value)
        {
            self.rollback_created(parent, name, &handle);
            return Err(NfsError::Status(10004));
        }
        Ok(handle)
    }
    pub fn create_symlink(
        &self,
        parent: &FileHandle,
        name: &FsName,
        target: &axfs_ng_vfs::FsPath,
    ) -> NfsResult<FileHandle> {
        self.create_symlink_with_attrs(parent, name, target, 0o777, None, None, None)
    }
    pub fn create_symlink_with_attrs(
        &self,
        parent: &FileHandle,
        name: &FsName,
        target: &axfs_ng_vfs::FsPath,
        mode: u32,
        owner: Option<&[u8]>,
        group: Option<&[u8]>,
        acl: Option<&[u8]>,
    ) -> NfsResult<FileHandle> {
        if name.is_empty() || name.as_bytes().contains(&0) || target.as_bytes().contains(&0) {
            return Err(NfsError::Length);
        }
        if acl.is_some() {
            let _ = self.acl_capabilities()?;
        }
        let required_first = if acl.is_some() { 1 << 12 } else { 0 };
        let required_second = (1 << (33 - 32))
            | if owner.is_some() { 1 << (36 - 32) } else { 0 }
            | if group.is_some() { 1 << (37 - 32) } else { 0 };
        self.require_attrs(required_first, required_second)?;
        let reply = self.compound(&[
            Operation::PutFh(parent),
            Operation::CreateSymlink {
                name: name.to_owned(),
                target: target.as_bytes(),
                mode,
                owner,
                group,
                acl,
            },
            Operation::GetFh,
        ])?;
        let applied = parse_create_attrset(&reply)?;
        let handle = parse_getfh(&reply)?;
        if applied[0] & required_first != required_first
            || applied[1] & required_second != required_second
        {
            self.rollback_created(parent, name, &handle);
            return Err(NfsError::Status(10004));
        }
        let attr = self.attrs(&handle)?;
        if attr.mode != mode
            || owner.is_some_and(|value| attr.owner.as_slice() != value)
            || group.is_some_and(|value| attr.owner_group.as_slice() != value)
        {
            self.rollback_created(parent, name, &handle);
            return Err(NfsError::Status(10004));
        }
        Ok(handle)
    }
    pub fn remove(&self, parent: &FileHandle, name: &FsName) -> NfsResult<()> {
        if name.is_empty() || name.as_bytes().contains(&0) {
            return Err(NfsError::Length);
        }
        self.compound(&[Operation::PutFh(parent), Operation::Remove(name.to_owned())])
            .map(|_| self.invalidate())
    }
    pub fn remove_verified(
        &self,
        parent: &FileHandle,
        name: &FsName,
        expected: &NfsAttr,
    ) -> NfsResult<()> {
        if name.is_empty() || name.as_bytes().contains(&0) {
            return Err(NfsError::Length);
        }
        self.compound(&[
            Operation::PutFh(parent),
            Operation::SaveFh,
            Operation::Lookup(name.to_owned()),
            Operation::Verify {
                fsid_major: expected.fsid_major,
                fsid_minor: expected.fsid_minor,
                fileid: expected.fileid,
            },
            Operation::RestoreFh,
            Operation::Remove(name.to_owned()),
        ])
        .map(|_| self.invalidate())
    }
    pub fn rename(
        &self,
        old_parent: &FileHandle,
        old_name: &FsName,
        new_parent: &FileHandle,
        new_name: &FsName,
    ) -> NfsResult<RenameChange> {
        if old_name.is_empty()
            || new_name.is_empty()
            || old_name.as_bytes().contains(&0)
            || new_name.as_bytes().contains(&0)
        {
            return Err(NfsError::Length);
        }
        let reply = self.compound(&[
            Operation::PutFh(old_parent),
            Operation::SaveFh,
            Operation::PutFh(new_parent),
            Operation::Rename {
                old: old_name.to_owned(),
                new: new_name.to_owned(),
            },
        ])?;
        let change = parse_rename_change(&reply)?;
        self.invalidate();
        Ok(change)
    }
    pub fn rename_verified(
        &self,
        old_parent: &FileHandle,
        old_name: &FsName,
        source: &NfsAttr,
        new_parent: &FileHandle,
        new_name: &FsName,
        destination: Option<&NfsAttr>,
        target_parent_change: Option<u64>,
    ) -> NfsResult<RenameChange> {
        if old_name.is_empty()
            || new_name.is_empty()
            || old_name.as_bytes().contains(&0)
            || new_name.as_bytes().contains(&0)
        {
            return Err(NfsError::Length);
        }
        let mut ops = Vec::new();
        ops.extend_from_slice(&[
            Operation::PutFh(old_parent),
            Operation::Lookup(old_name.to_owned()),
            Operation::Verify {
                fsid_major: source.fsid_major,
                fsid_minor: source.fsid_minor,
                fileid: source.fileid,
            },
        ]);
        if let Some(destination) = destination {
            ops.extend_from_slice(&[
                Operation::PutFh(new_parent),
                Operation::Lookup(new_name.to_owned()),
                Operation::Verify {
                    fsid_major: destination.fsid_major,
                    fsid_minor: destination.fsid_minor,
                    fileid: destination.fileid,
                },
            ]);
        } else {
            ops.push(Operation::PutFh(new_parent));
            ops.push(Operation::VerifyChange(
                target_parent_change.ok_or(NfsError::Malformed)?,
            ));
        }
        ops.extend_from_slice(&[
            Operation::PutFh(old_parent),
            Operation::SaveFh,
            Operation::PutFh(new_parent),
            Operation::Rename {
                old: old_name.to_owned(),
                new: new_name.to_owned(),
            },
        ]);
        let reply = self.compound(&ops)?;
        let change = parse_rename_change(&reply)?;
        self.invalidate();
        Ok(change)
    }
    /// Atomically publishes `file` under `parent/name`.  NFSv4 LINK uses the
    /// saved filehandle for the source object and the current filehandle for
    /// the containing directory.  Restore the source before GETATTR so the
    /// returned FATTR4 (including nlink/change) is for the linked object.
    pub fn link(
        &self,
        parent: &FileHandle,
        name: &FsName,
        file: &FileHandle,
    ) -> NfsResult<NfsAttr> {
        if name.is_empty() || name.as_bytes().contains(&0) {
            return Err(NfsError::Length);
        }
        let reply = self.compound(&[
            Operation::PutFh(file),
            Operation::SaveFh,
            Operation::PutFh(parent),
            Operation::Link(name.to_owned()),
            Operation::RestoreFh,
            Operation::GetAttr,
        ])?;
        let attr = parse_attr(&reply)?;
        self.invalidate();
        Ok(attr)
    }
    pub fn read_dir(
        &self,
        handle: &FileHandle,
        cookie: u64,
        verifier: [u8; 8],
        max_bytes: u32,
    ) -> NfsResult<ReadDirResult> {
        if max_bytes < 1024 {
            return Err(NfsError::Length);
        }
        parse_readdir(&self.compound(&[
            Operation::PutFh(handle),
            Operation::ReadDir {
                cookie,
                verifier,
                max_bytes,
            },
        ])?)
    }
    pub fn setattr(
        &self,
        handle: &FileHandle,
        stateid: [u8; 16],
        mode: Option<u32>,
        size: Option<u64>,
    ) -> NfsResult<()> {
        if mode.is_none() && size.is_none() {
            return Err(NfsError::Length);
        }
        // A successful truncate supersedes all dirty bytes at and beyond EOF.
        // Keep the ledger gate over the RPC and the retirement so recovery can
        // never restore data after a later visible SETATTR(size).
        let _ledger = self.unstable_ops.lock();
        self.compound(&[
            Operation::PutFh(handle),
            Operation::SetAttr {
                stateid,
                mode,
                size,
                owner: None,
                group: None,
                acl: None,
            },
        ])?;
        if let Some(size) = size {
            self.trim_unstable_locked(handle, size)?;
        }
        self.invalidate();
        Ok(())
    }
    pub fn setattr_owner(
        &self,
        handle: &FileHandle,
        stateid: [u8; 16],
        uid: u32,
        gid: u32,
    ) -> NfsResult<()> {
        let owner = self.uid_to_owner(uid)?;
        let group = self.gid_to_group(gid)?;
        self.compound(&[
            Operation::PutFh(handle),
            Operation::SetAttr {
                stateid,
                mode: None,
                size: None,
                owner: Some(&owner),
                group: Some(&group),
                acl: None,
            },
        ])
        .map(|_| self.invalidate())
    }
    pub fn read(
        &self,
        handle: &FileHandle,
        stateid: [u8; 16],
        offset: u64,
        count: u32,
    ) -> NfsResult<ReadResult> {
        parse_read(&self.compound(&[
            Operation::PutFh(handle),
            Operation::Read {
                stateid,
                offset,
                count,
            },
        ])?)
    }
    pub fn readlink(&self, handle: &FileHandle) -> NfsResult<alloc::vec::Vec<u8>> {
        parse_readlink(&self.compound(&[Operation::PutFh(handle), Operation::ReadLink])?)
    }
    fn stage_unstable_data(data: &[u8]) -> NfsResult<Arc<Vec<u8>>> {
        let mut staged = Vec::new();
        staged
            .try_reserve_exact(data.len())
            .map_err(|_| NfsError::Transport)?;
        staged.extend_from_slice(data);
        Arc::try_new(staged).map_err(|_| NfsError::Transport)
    }
    /// Holds enough fallible storage before an RPC whose success changes the
    /// dirty ledger.  The caller supplies the exact maximum net growth, so
    /// every post-RPC `push` is allocation-free.
    fn prepare_unstable_rewrite(&self, growth: usize) -> NfsResult<Vec<UnstableWrite>> {
        let current = self.unstable.lock().len();
        let capacity = current.checked_add(growth).ok_or(NfsError::Length)?;
        let mut rewritten = Vec::new();
        rewritten
            .try_reserve_exact(capacity)
            .map_err(|_| NfsError::Transport)?;
        Ok(rewritten)
    }
    fn interval_end(offset: u64, count: u32) -> NfsResult<u64> {
        offset.checked_add(u64::from(count)).ok_or(NfsError::Length)
    }
    fn replace_unstable_coverage_locked(
        &self,
        rewritten: &mut Vec<UnstableWrite>,
        handle: &FileHandle,
        start: u64,
        end: Option<u64>,
        replacement: Option<UnstableWrite>,
        required_verifier: Option<[u8; 8]>,
    ) -> NfsResult<()> {
        let mut current = core::mem::take(&mut *self.unstable.lock());
        for mut old in current.drain(..) {
            let old_end = Self::interval_end(old.offset, old.count)?;
            let overlaps = old.handle.as_ref() == handle
                && required_verifier.map_or(true, |verifier| old.verifier == verifier)
                && old.offset < end.unwrap_or(u64::MAX)
                && start < old_end;
            if !overlaps {
                rewritten.push(old);
                continue;
            }
            let keep_prefix = old.offset < start;
            let keep_suffix = end.is_some_and(|stop| old_end > stop);
            if keep_prefix {
                let prefix_count =
                    u32::try_from(start - old.offset).map_err(|_| NfsError::Length)?;
                let suffix = if let Some(stop) = end {
                    let suffix_count =
                        u32::try_from(old_end - stop).map_err(|_| NfsError::Length)?;
                    Some(UnstableWrite {
                        handle: old.handle.clone(),
                        stateid: old.stateid,
                        offset: stop,
                        count: suffix_count,
                        verifier: old.verifier,
                        data: old.data.clone(),
                        data_start: old
                            .data_start
                            .checked_add(
                                usize::try_from(stop - old.offset).map_err(|_| NfsError::Length)?,
                            )
                            .ok_or(NfsError::Length)?,
                        generation: old.generation,
                    })
                } else {
                    None
                };
                old.count = prefix_count;
                rewritten.push(old);
                if let Some(suffix) = suffix {
                    rewritten.push(suffix);
                }
            } else if keep_suffix {
                let stop = end.ok_or(NfsError::Length)?;
                old.data_start = old
                    .data_start
                    .checked_add(usize::try_from(stop - old.offset).map_err(|_| NfsError::Length)?)
                    .ok_or(NfsError::Length)?;
                old.offset = stop;
                old.count = u32::try_from(old_end - stop).map_err(|_| NfsError::Length)?;
                rewritten.push(old);
            }
        }
        if let Some(replacement) = replacement {
            rewritten.push(replacement);
        }
        *self.unstable.lock() = core::mem::take(rewritten);
        Ok(())
    }
    fn trim_unstable_locked(&self, handle: &FileHandle, size: u64) -> NfsResult<()> {
        let mut rewritten = self.prepare_unstable_rewrite(1)?;
        self.replace_unstable_coverage_locked(&mut rewritten, handle, size, None, None, None)
    }
    fn write_locked(
        &self,
        handle: &FileHandle,
        stateid: [u8; 16],
        offset: u64,
        stability: StableHow,
        data: &[u8],
    ) -> NfsResult<WriteResult> {
        // XDR opaque uses u32 length.  Validate both its representation and
        // the requested end before the RPC, so any accepted short count has a
        // representable ledger range rather than becoming a post-write error.
        let requested = u32::try_from(data.len()).map_err(|_| NfsError::Length)?;
        let _requested_end = Self::interval_end(offset, requested)?;
        // Stage bytes and reserve the maximum ledger rewrite before the RPC.
        // After the server reports count no allocation is needed to publish the
        // exact (possibly short) coverage.
        // A server response is authoritative: retain a staged image even for
        // a requested stable WRITE in case it reports UNSTABLE completion.
        let staged = Self::stage_unstable_data(data)?;
        // One old segment can split into prefix+suffix, and an UNSTABLE reply
        // adds the new segment: maximum net growth is exactly two.
        let mut rewritten = self.prepare_unstable_rewrite(2)?;
        let stable_handle = Arc::try_new(handle.clone()).map_err(|_| NfsError::Transport)?;
        let result = parse_write(&self.compound(&[
            Operation::PutFh(handle),
            Operation::Write {
                stateid,
                offset,
                stability,
                data,
            },
        ])?)?;
        let count = usize::try_from(result.count).map_err(|_| NfsError::Length)?;
        if count > data.len() {
            return Err(NfsError::Malformed);
        }
        if result.count != 0 {
            let end = Self::interval_end(offset, result.count)?;
            let replacement = if result.committed == StableHow::Unstable {
                Some(UnstableWrite {
                    handle: stable_handle,
                    stateid,
                    offset,
                    count: result.count,
                    verifier: result.verifier,
                    data: staged,
                    data_start: 0,
                    generation: self.unstable_generation.fetch_add(1, Ordering::AcqRel),
                })
            } else {
                None
            };
            // DATA_SYNC and FILE_SYNC also supersede older unstable bytes.
            self.replace_unstable_coverage_locked(
                &mut rewritten,
                handle,
                offset,
                Some(end),
                replacement,
                None,
            )?;
        }
        Ok(result)
    }
    pub fn write(
        &self,
        handle: &FileHandle,
        stateid: [u8; 16],
        offset: u64,
        stability: StableHow,
        data: &[u8],
    ) -> NfsResult<WriteResult> {
        // A recalled delegation loses local write-cache authority before this
        // WRITE can leave the client.  Flush and return it synchronously so a
        // callback race cannot publish post-recall data under that delegation.
        self.wait_recalled_delegation(handle)?;
        let _ledger = self.unstable_ops.lock();
        self.write_locked(handle, stateid, offset, stability, data)
    }
    fn commit_locked(&self, handle: &FileHandle, offset: u64, count: u32) -> NfsResult<[u8; 8]> {
        // NFS COMMIT count==0 is the open-ended interval [offset, EOF), so it
        // intentionally performs no offset addition.  A finite range must be
        // representable before its RPC can succeed, otherwise retirement could
        // fail after the server has already made bytes stable.
        let end = if count == 0 {
            None
        } else {
            Some(Self::interval_end(offset, count)?)
        };
        let mut rewritten = self.prepare_unstable_rewrite(1)?;
        let verifier = parse_commit(&self.compound(&[
            Operation::PutFh(handle),
            Operation::Commit { offset, count },
        ])?)?;
        // COMMIT count==0 means through EOF.  Retire only the committed
        // intersection and only entries bearing the verifier returned here.
        self.replace_unstable_coverage_locked(
            &mut rewritten,
            handle,
            offset,
            end,
            None,
            Some(verifier),
        )?;
        Ok(verifier)
    }
    pub fn commit(&self, handle: &FileHandle, offset: u64, count: u32) -> NfsResult<[u8; 8]> {
        self.wait_recalled_delegation(handle)?;
        let _ledger = self.unstable_ops.lock();
        self.commit_locked(handle, offset, count)
    }
    /// Flushes every locally tracked unstable write for one file.  A changed
    /// verifier proves the server did not make the old unstable range stable;
    /// replay the retained bytes before retrying COMMIT instead of dropping
    /// the dirty range or falsely reporting a completed fsync.
    pub fn commit_pending(&self, handle: &FileHandle) -> NfsResult<()> {
        self.wait_recalled_delegation(handle)?;
        self.commit_pending_recall_worker(handle)
    }
    /// The recall worker has already changed this exact delegation to
    /// RecallInFlight, so its COMMIT must not re-admit normal delegation use.
    fn commit_pending_recall_worker(&self, handle: &FileHandle) -> NfsResult<()> {
        let _ledger = self.unstable_ops.lock();
        loop {
            let next = self
                .unstable
                .lock()
                .iter()
                .find(|entry| entry.handle.as_ref() == handle)
                .cloned();
            let Some(entry) = next else {
                return Ok(());
            };
            let verifier = self.commit_locked(handle, entry.offset, entry.count)?;
            if verifier != entry.verifier {
                self.invalidate();
                // A verifier change can invalidate every unstable range for
                // this file.  Snapshot only ledger entries that are current
                // now; the gate prevents a replay from overwriting a later
                // successful WRITE or truncate.
                let entries = {
                    let ledger = self.unstable.lock();
                    let mut entries = Vec::new();
                    entries
                        .try_reserve_exact(ledger.len())
                        .map_err(|_| NfsError::Transport)?;
                    for dirty in ledger
                        .iter()
                        .filter(|dirty| dirty.handle.as_ref() == handle)
                    {
                        entries.push(dirty.clone());
                    }
                    entries
                };
                for dirty in entries {
                    let current = self.unstable.lock().iter().any(|live| {
                        live.handle == dirty.handle
                            && live.offset == dirty.offset
                            && live.count == dirty.count
                            && live.verifier == dirty.verifier
                            && live.generation == dirty.generation
                            && Arc::ptr_eq(&live.data, &dirty.data)
                            && live.data_start == dirty.data_start
                    });
                    if !current {
                        continue;
                    }
                    let stateid = self
                        .state_records
                        .lock()
                        .iter()
                        .find(|record| {
                            !record.lock && record.handle == *dirty.handle && !record.reclaim
                        })
                        .map(|record| record.stateid)
                        .ok_or(NfsError::SessionLost)?;
                    let finish = dirty
                        .data_start
                        .checked_add(dirty.count as usize)
                        .ok_or(NfsError::Length)?;
                    let bytes = dirty
                        .data
                        .get(dirty.data_start..finish)
                        .ok_or(NfsError::Malformed)?;
                    let rewritten = self.write_locked(
                        dirty.handle.as_ref(),
                        stateid,
                        dirty.offset,
                        StableHow::Unstable,
                        bytes,
                    )?;
                    if rewritten.count != dirty.count {
                        return Err(NfsError::Transport);
                    }
                }
            }
        }
    }
    /// Do not release an OPEN state while this file still has locally tracked
    /// unstable bytes.  A failed flush leaves both the state and its ledger
    /// record intact for recovery/retry instead of reporting a false close.
    pub fn close(&self, state: &OpenState) -> NfsResult<()> {
        self.commit_pending(&state.handle)?;
        let _state_ops = self.state_ops.lock();
        self.close_open_locked(state)
    }
    pub fn get_named_attr(&self, file: &FileHandle, name: &FsName) -> NfsResult<Vec<u8>> {
        let dir = self.openattr(file, false)?;
        let (fh, attr) = self.lookup_attrs(Some(&dir), name)?;
        if attr.size > u32::MAX as u64 {
            return Err(NfsError::Length);
        };
        self.read(&fh, [0; 16], 0, attr.size as u32)
            .map(|reply| reply.data)
    }
    pub fn list_named_attrs(&self, file: &FileHandle) -> NfsResult<Vec<FsNameBuf>> {
        let dir = self.openattr(file, false)?;
        let mut cookie = 0;
        let mut verifier = [0; 8];
        let mut names = Vec::new();
        loop {
            let page = self.read_dir(&dir, cookie, verifier, 64 * 1024)?;
            for entry in page.entries {
                cookie = entry.cookie;
                names.push(entry.name)
            }
            verifier = page.verifier;
            if page.eof {
                return Ok(names);
            }
        }
    }
    pub fn set_named_attr(
        &self,
        file: &FileHandle,
        name: &FsName,
        value: &[u8],
        mode: XattrSetMode,
    ) -> NfsResult<()> {
        let dir = self.openattr(file, true)?;
        let existing = self.lookup(Some(&dir), name);
        let state = match (existing, mode) {
            (Ok(_), XattrSetMode::Create) => return Err(NfsError::Status(17)),
            (Err(_), XattrSetMode::Replace) => return Err(NfsError::Status(61)),
            (Ok(_), _) => self.open(&dir, name, 3, 0)?,
            (Err(_), _) => self.create_file(&dir, name, 0o600, 3)?,
        };
        self.setattr(&state.handle, state.stateid, None, Some(value.len() as u64))?;
        let written = self.write(&state.handle, state.stateid, 0, StableHow::FileSync, value)?;
        if written.count as usize != value.len() {
            return Err(NfsError::Transport);
        };
        self.commit(&state.handle, 0, 0)?;
        self.close(&state)
    }
    pub fn remove_named_attr(&self, file: &FileHandle, name: &FsName) -> NfsResult<()> {
        let dir = self.openattr(file, false)?;
        self.remove(&dir, name)
    }
    /// NFSv4.1 renews its lease by sending SEQUENCE on the established
    /// session.  RENEW is a v4.0 operation and must not appear in a minor-1
    /// COMPOUND.
    pub fn renew(&self) -> NfsResult<()> {
        self.compound(&[]).map(|_| ())
    }
    pub fn delegreturn(&self, handle: &FileHandle, stateid: [u8; 16]) -> NfsResult<()> {
        self.compound(&[Operation::PutFh(handle), Operation::DelegReturn { stateid }])
            .map(|_| self.invalidate())
    }
    fn return_granted_or_recover(&self, handle: &FileHandle, stateid: [u8; 16]) {
        if self.delegreturn(handle, stateid).is_err() {
            self.invalidate_session();
        }
    }
    /// Records a server delegation.  Callback dispatch uses this table to
    /// reject recalls for a different file/stateid instead of invalidating an
    /// unrelated mount cache.
    pub fn install_delegation(&self, handle: FileHandle, stateid: [u8; 16]) -> NfsResult<()> {
        let mut delegations = self.delegations.lock();
        let generation = self.delegation_generation.fetch_add(1, Ordering::AcqRel);
        let entry = Delegation {
            handle,
            stateid,
            generation,
            session_generation: self.session_generation.load(Ordering::Acquire),
            teardown_epoch: self.teardown_epoch.load(Ordering::Acquire),
            state: DelegationState::Active,
        };
        if let Some(index) = delegations
            .iter()
            .position(|known| known.handle == entry.handle && known.stateid == entry.stateid)
        {
            delegations[index] = entry;
        } else {
            delegations.push(entry);
        }
        Ok(())
    }
    fn abandon_unrecorded_delegation(
        &self,
        handle: &FileHandle,
        stateid: [u8; 16],
        error: NfsError,
    ) -> NfsError {
        // The server already granted this state.  Never silently ignore a
        // local allocation failure: synchronously return it while the caller
        // still owns the decoded filehandle.  A failed return invalidates the
        // session, forcing protocol recovery rather than advertising a local
        // BAD_STATEID against a live server delegation.
        self.return_granted_or_recover(handle, stateid);
        error
    }
    /// Callback listener lifecycle is explicit.  The network binding must
    /// authenticate the callback principal before it calls this method.
    pub fn set_callback_listener_alive(&self, alive: bool) {
        if alive {
            self.callback_alive.store(true, Ordering::Release);
            return;
        }
        let _reconnect = self.reconnect_ops.lock();
        self.callback_alive.store(false, Ordering::Release);
        self.teardown_epoch.fetch_add(1, Ordering::AcqRel);
        self.stop_lease_worker();
        self.stop_recall_worker();
        self.callback_session.lock().take();
        self.delegations.lock().clear();
        self.callback_epoch.fetch_add(1, Ordering::AcqRel);
        self.callback_waiters.notify_all(false);
        self.transport().shutdown();
        self.invalidate();
    }
    /// Installs the callback session negotiated by the authenticated callback
    /// channel.  A new session deliberately drops all old CB_SEQUENCE replay
    /// state; accepting an old callback after client/session recreation would
    /// let a stale server revoke unrelated cache authority.
    pub fn install_callback_session(&self, id: [u8; 16], slots: u32) -> NfsResult<()> {
        if slots == 0 || slots > 64 {
            return Err(NfsError::Length);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(slots as usize)
            .map_err(|_| NfsError::Transport)?;
        for id in 0..slots {
            entries.push(CallbackSlot {
                id,
                next_sequence: 1,
                inflight: None,
                replay: None,
            });
        }
        *self.callback_session.lock() = Some(CallbackSession { id, slots: entries });
        self.callback_epoch.fetch_add(1, Ordering::AcqRel);
        self.callback_waiters.notify_all(false);
        self.set_callback_listener_alive(true);
        Ok(())
    }
    fn callback_replay_or_admit(
        &self,
        id: [u8; 16],
        slot: u32,
        sequence: u32,
        request: &[u8],
    ) -> NfsResult<CallbackAdmission> {
        if !self.callback_alive.load(Ordering::Acquire) {
            return Err(NfsError::SessionLost);
        }
        let mut session = self.callback_session.lock();
        let session = session.as_mut().ok_or(NfsError::SessionLost)?;
        if session.id != id {
            return Err(NfsError::SessionLost);
        }
        let entry = session
            .slots
            .iter_mut()
            .find(|entry| entry.id == slot)
            .ok_or(NfsError::Malformed)?;
        if entry.next_sequence == sequence {
            if let Some((inflight, body)) = &entry.inflight {
                return if *inflight == sequence && body.as_slice() == request {
                    Ok(CallbackAdmission::Wait(
                        self.callback_epoch.load(Ordering::Acquire),
                    ))
                } else {
                    Err(NfsError::Status(10026))
                };
            }
            entry.inflight = Some((sequence, request.to_vec()));
            return Ok(CallbackAdmission::Execute);
        }
        if let Some(replay) = &entry.replay {
            if replay.sequence == sequence && replay.request == request {
                return Ok(CallbackAdmission::Replay(replay.reply.clone()));
            }
        }
        Err(NfsError::Status(10026))
    }
    fn commit_callback_sequence(
        &self,
        id: [u8; 16],
        slot: u32,
        sequence: u32,
        request: &[u8],
        reply: Vec<u8>,
    ) -> NfsResult<()> {
        let mut session = self.callback_session.lock();
        let session = session.as_mut().ok_or(NfsError::SessionLost)?;
        if session.id != id {
            return Err(NfsError::SessionLost);
        }
        let entry = session
            .slots
            .iter_mut()
            .find(|entry| entry.id == slot)
            .ok_or(NfsError::Malformed)?;
        if entry.next_sequence != sequence
            || !entry
                .inflight
                .as_ref()
                .is_some_and(|(inflight, body)| *inflight == sequence && body.as_slice() == request)
        {
            return Err(NfsError::Status(10026));
        }
        entry.inflight = None;
        entry.replay = Some(CallbackReplay {
            sequence,
            request: request.to_vec(),
            reply,
        });
        entry.next_sequence = entry.next_sequence.wrapping_add(1);
        self.callback_epoch.fetch_add(1, Ordering::AcqRel);
        self.callback_waiters.notify_all(false);
        Ok(())
    }
    /// Dispatches a verified CB_COMPOUND body.  The callback listener must
    /// first authenticate and unwrap RPCSEC_GSS (when configured); this layer
    /// accepts only the resulting clear XDR, checks CB_SEQUENCE before every
    /// recall, and never treats an unauthenticated filehandle as a cache hint.
    pub fn handle_callback_compound(&self, body: &[u8]) -> NfsResult<Vec<u8>> {
        let mut x = XdrIn::new(body);
        let tag = x.opaque()?.to_vec();
        if x.u32()? != 1 {
            return Err(NfsError::Malformed);
        }
        let count = x.u32()?;
        if count == 0 {
            return Err(NfsError::Malformed);
        }
        if x.u32()? != CB_SEQUENCE {
            return Err(NfsError::Malformed);
        }
        let sessionid: [u8; 16] = x.take(16)?.try_into().map_err(|_| NfsError::Malformed)?;
        let sequence = x.u32()?;
        let slot = x.u32()?;
        let highest = x.u32()?;
        let _cache_this = x.u32()?;
        if slot > highest {
            return Err(NfsError::Malformed);
        }
        loop {
            match self.callback_replay_or_admit(sessionid, slot, sequence, body)? {
                CallbackAdmission::Replay(reply) => return Ok(reply),
                CallbackAdmission::Execute => break,
                CallbackAdmission::Wait(epoch) => self
                    .callback_waiters
                    .wait_until(|| self.callback_epoch.load(Ordering::Acquire) != epoch)
                    .map_err(|_| NfsError::SessionLost)?,
            }
        }
        let recalls = (|| -> NfsResult<Vec<([u8; 16], FileHandle)>> {
            let mut recalls = Vec::new();
            for _ in 1..count {
                if x.u32()? != CB_RECALL {
                    return Err(NfsError::Status(10044));
                }
                let stateid = x.take(16)?.try_into().map_err(|_| NfsError::Malformed)?;
                let _truncate = x.u32()?;
                let handle = FileHandle::new(x.opaque()?.to_vec())?;
                recalls.try_reserve(1).map_err(|_| NfsError::Transport)?;
                recalls.push((stateid, handle));
            }
            if x.at != body.len() {
                return Err(NfsError::Malformed);
            }
            Ok(recalls)
        })();
        let recalls = match recalls {
            Ok(recalls) => recalls,
            Err(NfsError::Status(status)) => {
                let reply =
                    self.callback_result(&tag, sessionid, sequence, slot, highest, count, status);
                self.commit_callback_sequence(sessionid, slot, sequence, body, reply.clone())?;
                return Ok(reply);
            }
            Err(_) => {
                let reply =
                    self.callback_result(&tag, sessionid, sequence, slot, highest, count, BADXDR);
                self.commit_callback_sequence(sessionid, slot, sequence, body, reply.clone())?;
                return Ok(reply);
            }
        };
        for (index, (stateid, handle)) in recalls.into_iter().enumerate() {
            if !self.recall_delegation(&handle, stateid) {
                let reply = self.callback_result(
                    &tag,
                    sessionid,
                    sequence,
                    slot,
                    highest,
                    (index as u32) + 2,
                    BAD_STATEID,
                );
                self.commit_callback_sequence(sessionid, slot, sequence, body, reply.clone())?;
                return Ok(reply);
            }
        }
        let reply = self.callback_result(&tag, sessionid, sequence, slot, highest, count, NFS_OK);
        self.commit_callback_sequence(sessionid, slot, sequence, body, reply.clone())?;
        Ok(reply)
    }
    /// Authenticates one RPCSEC_GSS DATA/DESTROY callback before callback
    /// state is touched.  The transport supplies the exact XDR prefix used by
    /// the RFC 2203 verifier, never a reconstructed header.
    pub fn authenticated_callback(
        &self,
        flavor: u32,
        credential: &[u8],
        verifier: &[u8],
        call_prefix: &[u8],
        body: &[u8],
    ) -> NfsResult<(u32, Vec<u8>)> {
        if matches!(&self.auth, RpcAuth::None)
            && flavor == AUTH_NONE
            && credential.is_empty()
            && verifier.is_empty()
        {
            return Ok((0, self.handle_callback_compound(body)?));
        }
        if flavor == AUTH_SYS && verifier.is_empty() {
            let expected = self
                .options
                .lock()
                .as_ref()
                .map(|options| options.auth_sys.clone())
                .ok_or(NfsError::SessionLost)?;
            let mut x = XdrIn::new(credential);
            // CREATE_SESSION advertises a zero AUTH_SYS stamp.  AUTH_SYS is
            // weak by design, but the callback credential must still be the
            // exact parameter set we negotiated rather than merely a
            // well-formed credential from an arbitrary server principal.
            if x.u32()? != 0 {
                return Err(NfsError::Security);
            }
            let machine = x.opaque()?;
            let uid = x.u32()?;
            let gid = x.u32()?;
            let groups = x.u32()?;
            if machine != expected.machine_name
                || uid != expected.uid
                || gid != expected.gid
                || groups as usize != expected.groups.len()
                || groups > 16
            {
                return Err(NfsError::Security);
            }
            for expected_group in &expected.groups {
                if x.u32()? != *expected_group {
                    return Err(NfsError::Security);
                }
            }
            if x.at != credential.len() {
                return Err(NfsError::Security);
            }
            return Ok((0, self.handle_callback_compound(body)?));
        }
        if flavor != RPCSEC_GSS {
            return Err(NfsError::Security);
        }
        let RpcAuth::Gss(g) = &self.auth else {
            return Err(NfsError::Security);
        };
        let mut x = XdrIn::new(credential);
        if x.u32()? != 1 {
            return Err(NfsError::Security);
        }
        let procedure = x.u32()?;
        let sequence = x.u32()?;
        let service = match x.u32()? {
            1 => RpcGssService::None,
            2 => RpcGssService::Integrity,
            3 => RpcGssService::Privacy,
            _ => return Err(NfsError::Security),
        };
        let context = x.opaque()?;
        if x.at != credential.len() || context != g.context_handle() || service != g.service() {
            return Err(NfsError::Security);
        }
        g.verify_callback(sequence, call_prefix, verifier)?;
        let clear = match procedure {
            0 => g.unwrap_callback(sequence, body)?,
            // There is no context-destruction handoff. Do not acknowledge a
            // DESTROY while continuing to accept this context for DATA.
            3 => return Err(NfsError::Security),
            _ => return Err(NfsError::Security),
        };
        let response = if procedure == 0 {
            self.handle_callback_compound(&clear)?
        } else {
            Vec::new()
        };
        Ok((sequence, response))
    }
    /// The transport supplies the exact accepted-reply prefix, including the
    /// server-chosen XID and GSS verifier length, before emitting the MIC.
    pub fn callback_reply_verifier(
        &self,
        flavor: u32,
        sequence: u32,
        reply_prefix: &[u8],
    ) -> NfsResult<Vec<u8>> {
        if flavor != RPCSEC_GSS {
            return Ok(Vec::new());
        }
        let RpcAuth::Gss(g) = &self.auth else {
            return Err(NfsError::Security);
        };
        g.callback_verifier(sequence, reply_prefix)
    }
    pub fn wrap_callback_reply(
        &self,
        flavor: u32,
        sequence: u32,
        body: &[u8],
    ) -> NfsResult<Vec<u8>> {
        if flavor != RPCSEC_GSS {
            return Ok(body.to_vec());
        }
        let RpcAuth::Gss(g) = &self.auth else {
            return Err(NfsError::Security);
        };
        g.wrap_callback_reply(sequence, body)
    }
    fn callback_result(
        &self,
        tag: &[u8],
        sessionid: [u8; 16],
        sequence: u32,
        slot: u32,
        highest: u32,
        operations: u32,
        status: u32,
    ) -> Vec<u8> {
        let mut reply = Xdr::default();
        reply.u32(status);
        reply.opaque(tag);
        reply.u32(operations);
        reply.u32(CB_SEQUENCE);
        reply.u32(NFS_OK);
        reply.fixed(&sessionid);
        reply.u32(sequence);
        reply.u32(slot);
        reply.u32(highest);
        reply.u32(highest);
        reply.u32(0);
        for index in 1..operations {
            reply.u32(CB_RECALL);
            reply.u32(if status != NFS_OK && index + 1 == operations {
                status
            } else {
                NFS_OK
            });
        }
        reply.0
    }
    /// Claims a pre-existing delegation in place. Arc cloning only bumps a
    /// refcount; no FileHandle bytes or queue nodes are allocated after ACK.
    fn claim_pending_recall(&self) -> Option<RecallWork> {
        let session_generation = self.session_generation.load(Ordering::Acquire);
        let teardown_epoch = self.teardown_epoch.load(Ordering::Acquire);
        let mut delegations = self.delegations.lock();
        let entry = delegations.iter_mut().find(|entry| {
            entry.state == DelegationState::RecallPending
                && entry.session_generation == session_generation
                && entry.teardown_epoch == teardown_epoch
        })?;
        entry.state = DelegationState::RecallInFlight;
        Some(RecallWork {
            handle: entry.handle.clone(),
            stateid: entry.stateid,
            generation: entry.generation,
            session_generation: entry.session_generation,
            teardown_epoch: entry.teardown_epoch,
        })
    }
    fn pending_recall_exists(&self) -> bool {
        let session_generation = self.session_generation.load(Ordering::Acquire);
        let teardown_epoch = self.teardown_epoch.load(Ordering::Acquire);
        self.delegations.lock().iter().any(|entry| {
            entry.state == DelegationState::RecallPending
                && entry.session_generation == session_generation
                && entry.teardown_epoch == teardown_epoch
        })
    }
    fn wait_recalled_delegation(&self, handle: &FileHandle) -> NfsResult<()> {
        // A callback has invalidated local delegation authority.  Do not let
        // an ordinary WRITE/CLOSE race the queued COMMIT/DELEGRETURN; callers
        // receive a retryable outcome while the mount worker owns completion.
        let session_generation = self.session_generation.load(Ordering::Acquire);
        let teardown_epoch = self.teardown_epoch.load(Ordering::Acquire);
        if self.delegations.lock().iter().any(|entry| {
            entry.handle == *handle
                && entry.session_generation == session_generation
                && entry.teardown_epoch == teardown_epoch
                && entry.state != DelegationState::Active
        }) {
            return Err(NfsError::WouldBlock);
        }
        Ok(())
    }
    /// Called by authenticated CB_RECALL handling.  Returning false tells the
    /// callback binding to reject the request as NFS4ERR_BAD_STATEID.
    pub fn recall_delegation(&self, handle: &FileHandle, stateid: [u8; 16]) -> bool {
        if !self.callback_alive.load(Ordering::Acquire) {
            return false;
        }
        let session_generation = self.session_generation.load(Ordering::Acquire);
        let teardown_epoch = self.teardown_epoch.load(Ordering::Acquire);
        let mut delegations = self.delegations.lock();
        let Some(entry) = delegations.iter_mut().find(|entry| {
            entry.handle == *handle
                && entry.stateid == stateid
                && entry.session_generation == session_generation
                && entry.teardown_epoch == teardown_epoch
        }) else {
            return false;
        };
        if entry.state == DelegationState::Active {
            entry.state = DelegationState::RecallPending;
        }
        self.invalidate();
        true
    }
    /// Called by the callback transport only after the fully framed reply has
    /// been written.  Before this point a forechannel COMMIT could deadlock
    /// behind the transport's one reader waiting on the callback write.
    pub fn callback_reply_sent(&self) {
        if !self.callback_alive.load(Ordering::Acquire)
            || self.recall_worker_stop.load(Ordering::Acquire)
        {
            return;
        }
        // The pending bit is stored in the already allocated Delegation. No
        // Vec/FileHandle allocation is permitted on this post-ACK path.
        self.recall_waiters.notify_all(false);
    }
    /// Completes an authenticated CB_RECALL: flush unstable data before the
    /// DELEGRETURN RPC, then retire precisely that delegation.  A failed
    /// flush/return leaves the recalled record installed so recovery cannot
    /// accidentally grant cache authority again.
    pub fn complete_recall(&self, handle: &FileHandle, stateid: [u8; 16]) -> NfsResult<()> {
        let work = {
            let mut delegations = self.delegations.lock();
            let entry = delegations
                .iter_mut()
                .find(|entry| {
                    entry.handle == *handle
                        && entry.stateid == stateid
                        && entry.state == DelegationState::RecallPending
                })
                .ok_or(NfsError::SessionLost)?;
            entry.state = DelegationState::RecallInFlight;
            RecallWork {
                handle: entry.handle.clone(),
                stateid: entry.stateid,
                generation: entry.generation,
                session_generation: entry.session_generation,
                teardown_epoch: entry.teardown_epoch,
            }
        };
        self.complete_recall_work(&work)
    }
    fn complete_recall_work(&self, work: &RecallWork) -> NfsResult<()> {
        {
            let mut delegations = self.delegations.lock();
            let entry = delegations
                .iter_mut()
                .find(|entry| {
                    entry.handle == work.handle
                        && entry.stateid == work.stateid
                        && entry.generation == work.generation
                        && entry.session_generation == work.session_generation
                        && entry.teardown_epoch == work.teardown_epoch
                })
                .ok_or(NfsError::SessionLost)?;
            if entry.state == DelegationState::RecallPending {
                entry.state = DelegationState::RecallInFlight;
            }
            if entry.state != DelegationState::RecallInFlight {
                return Err(NfsError::WouldBlock);
            }
        }
        if self.session_generation.load(Ordering::Acquire) != work.session_generation
            || self.teardown_epoch.load(Ordering::Acquire) != work.teardown_epoch
        {
            return Err(NfsError::SessionLost);
        }
        self.commit_pending_recall_worker(&work.handle)?;
        self.delegreturn(&work.handle, work.stateid)?;
        let mut delegations = self.delegations.lock();
        let index = delegations
            .iter()
            .position(|entry| {
                entry.handle == work.handle
                    && entry.stateid == work.stateid
                    && entry.generation == work.generation
                    && entry.session_generation == work.session_generation
                    && entry.teardown_epoch == work.teardown_epoch
                    && entry.state == DelegationState::RecallInFlight
            })
            .ok_or(NfsError::SessionLost)?;
        if self.session_generation.load(Ordering::Acquire) != work.session_generation
            || self.teardown_epoch.load(Ordering::Acquire) != work.teardown_epoch
        {
            return Err(NfsError::SessionLost);
        }
        delegations.remove(index);
        self.invalidate();
        Ok(())
    }
    pub fn allocate_owner(&self) -> u64 {
        self.next_owner.fetch_add(1, Ordering::Relaxed)
    }
    /// Probes session-slot availability without reserving a slot or starting
    /// an RPC. NOWAIT may issue remote I/O only when this returns true.
    pub fn nowait_rpc_admit(&self) -> bool {
        self.session.lock().as_ref().is_some_and(|session| {
            !session.replay_barrier
                && session
                    .slots
                    .iter()
                    .any(|slot| !slot.unusable && matches!(&slot.lifecycle, SlotLifecycle::Free))
        })
    }
    fn compound(&self, operations: &[Operation<'_>]) -> NfsResult<Reply> {
        self.compound_mode(operations, false)
    }
    /// Status redrive is iterative and bounded.  Recursive retries can pin a
    /// task stack indefinitely when a faulty server repeats BADSLOT/DELAY;
    /// each attempt also rechecks teardown before allocating another slot.
    fn compound_mode(&self, operations: &[Operation<'_>], reclaim: bool) -> NfsResult<Reply> {
        let mut retry = 0u8;
        'redrive: loop {
            if self.lease_faulted.load(Ordering::Acquire) {
                return Err(NfsError::SessionLost);
            }
            let admission = self.admit_compound(reclaim)?;
            let operation_count = operations.len().checked_add(1).ok_or(NfsError::Length)?;
            let (slot, sequence, sessionid) = {
                let mut guard = self.session.lock();
                let session = guard.as_mut().ok_or(NfsError::SessionLost)?;
                if session.replay_barrier && !reclaim {
                    return Err(NfsError::SessionLost);
                }
                if operation_count > session.max_operations {
                    return Err(NfsError::Length);
                }
                let highest = session.highest_slot.min(session.target_highest_slot);
                let entry = session
                    .slots
                    .iter_mut()
                    .find(|v| {
                        !v.unusable
                            && matches!(&v.lifecycle, SlotLifecycle::Free)
                            && v.id <= highest
                    })
                    .ok_or(NfsError::WouldBlock)?;
                (entry.id, entry.sequence, session.id)
            };
            let mut all = Vec::with_capacity(operations.len() + 1);
            all.push(Operation::Sequence {
                sessionid,
                slot,
                sequence,
            });
            all.extend_from_slice(operations);
            // TCP failure after a request write is ambiguous.  A session slot is
            // therefore retained until it receives one definitive reply or the
            // lease is reset.  Every retry below sends the exact original bytes:
            // same XID, same SEQUENCE tuple and (for GSS) same protected record.
            // A new encoding could turn an ambiguous WRITE/OPEN into a second
            // operation even though its NFS slot sequence did not advance.
            let xid = self.xid.fetch_add(1, Ordering::Relaxed);
            let request = match self.compound_record(xid, 1, &all) {
                Ok(request) => request,
                Err(error) => {
                    if let Some(session) = self.session.lock().as_mut() {
                        if let Some(entry) = session.slots.iter_mut().find(|entry| entry.id == slot)
                        {
                            entry.lifecycle = SlotLifecycle::Free;
                        }
                    }
                    return Err(error);
                }
            };
            // Persist the immutable image before the first write.  Transport
            // ambiguity leaves this slot Sent and fences the whole forechannel;
            // recovery first rebinds the same session then sends these bytes,
            // never a reconstructed operation under a new lease.
            if let Some(session) = self.session.lock().as_mut() {
                if session.id == sessionid {
                    if let Some(entry) = session.slots.iter_mut().find(|entry| entry.id == slot) {
                        entry.lifecycle = SlotLifecycle::Sent(Replay {
                            xid,
                            sequence,
                            request: request.record.clone(),
                            gss_sequence: request.gss_sequence,
                            reply: Vec::new(),
                            terminal: None,
                        });
                    }
                }
            }
            let result = self.rpc_record_exact(xid, &request).and_then(|reply| {
                // A successful COMPOUND is not enough to release a v4.1 slot.
                // Bind its SEQUENCE reply to the exact request tuple; otherwise a
                // delayed reply from a previous session can be mistaken for the
                // request currently owning this slot.
                // SEQUENCE itself may fail without a resok payload.  Route its
                // status before demanding the resok tuple: BADSLOT,
                // RETRY_UNCACHED_REP, SEQ_FALSE_RETRY and SEQ_MISORDERED each
                // have dedicated slot ownership transitions below.
                if reply.items.len() == 1 && reply.items[0].0 == OP_SEQUENCE {
                    if let Some(error) = reply.terminal_error.clone() {
                        return Err(error);
                    }
                }
                // A zero-op top-level error (notably DELAY) has no SEQUENCE
                // acknowledgement.  Prefix-bearing non-NFS_OK replies continue
                // below so their explicit SEQUENCE result can retire the slot.
                if reply.top_status == DELAY && reply.operation_count == 0 {
                    return Err(NfsError::TopLevelDelay);
                }
                if reply.top_status != NFS_OK && reply.operation_count == 0 {
                    return Err(NfsError::Status(reply.top_status));
                }
                if reply.items.is_empty()
                    || reply.items.len() > all.len()
                    || reply.terminal_error.is_none() && reply.items.len() != all.len()
                    || reply
                        .items
                        .iter()
                        .zip(all.iter())
                        .any(|(item, operation)| item.0 != operation.opcode())
                    || reply.sequence_sessionid != Some(sessionid)
                    || reply.sequence_id != Some(sequence)
                    || reply.sequence_slot != Some(slot)
                    || !reply
                        .sequence_highest
                        .is_some_and(|highest| highest >= slot)
                {
                    self.invalidate_session();
                    return Err(NfsError::SessionLost);
                }
                self.update_slot_window(
                    sessionid,
                    reply.sequence_highest.unwrap_or(0),
                    reply.sequence_target_highest.unwrap_or(0),
                );
                self.apply_sequence_status(reply.sequence_flags)?;
                if reply.top_status != NFS_OK {
                    return Err(NfsError::Status(reply.top_status));
                }
                if let Some(error) = reply.terminal_error.clone() {
                    return Err(error);
                }
                Ok(reply)
            });
            if matches!(
                result,
                Err(NfsError::Transport
                    | NfsError::Malformed
                    | NfsError::Security
                    | NfsError::Rpc(_))
            ) {
                // Cancellation is deliberately not issued here.  It only tears
                // down the local waiter and cannot prove that TCP did not deliver
                // the record.  Preserve this exact XID/GSS-protected byte image
                // and fence every other slot until reconnect has rebound this
                // session and replayed it.
                if let Some(session) = self.session.lock().as_mut() {
                    if session.id == sessionid {
                        session.replay_barrier = true;
                    }
                }
                self.lease_faulted.store(true, Ordering::Release);
                // The originating caller remains the owner of this logical
                // request.  Drop its admission before synchronous recovery (the
                // recovery drain waits for all active compounds), then return
                // the exact replay terminal reply instead of asking VFS to
                // manufacture a second OPEN/WRITE intent.
                drop(admission);
                let mount = self
                    .self_ref
                    .lock()
                    .upgrade()
                    .ok_or(NfsError::SessionLost)?;
                let replayed = match self.take_replayed_reply(sessionid, slot, sequence, xid) {
                    Ok(reply) => reply,
                    Err(_) => {
                        if mount
                            .recovery
                            .owner
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            let recovered = mount.recover();
                            mount.recovery.generation.fetch_add(1, Ordering::AcqRel);
                            mount.recovery.owner.store(false, Ordering::Release);
                            mount.recovery.waiters.notify_all(false);
                            recovered?;
                        } else {
                            let _ = mount.recovery.waiters.wait_until(|| {
                                !mount.recovery.owner.load(Ordering::Acquire)
                                    || mount.lease_worker_stop.load(Ordering::Acquire)
                            });
                            if mount.lease_worker_stop.load(Ordering::Acquire) {
                                return Err(NfsError::SessionLost);
                            }
                        }
                        self.take_replayed_reply(sessionid, slot, sequence, xid)?
                    }
                };
                return replayed.terminal_error.clone().map_or(Ok(replayed), Err);
            }
            // These SEQUENCE outcomes have deliberately different ownership
            // rules.  RETRY_UNCACHED_REP and SEQ_FALSE_RETRY acknowledge the old
            // sequence but establish no result for the following logical ops, so
            // allocate the next sequence and encode the logical compound again.
            // BADSLOT rejects this slot only; another negotiated slot remains a
            // valid route.  SEQ_MISORDERED is ambiguous until a lone SEQUENCE
            // probe can disambiguate it, therefore it remains Sent behind the
            // replay barrier rather than being reused or discarded.
            match &result {
                Err(NfsError::Status(RETRY_UNCACHED_REP)) => {
                    {
                        let mut guard = self.session.lock();
                        let session = guard.as_mut().ok_or(NfsError::SessionLost)?;
                        if session.id != sessionid {
                            return Err(NfsError::SessionLost);
                        }
                        let entry = session
                            .slots
                            .iter_mut()
                            .find(|entry| entry.id == slot)
                            .ok_or(NfsError::SessionLost)?;
                        entry.sequence = entry.sequence.wrapping_add(1);
                        entry.lifecycle = SlotLifecycle::Free;
                    }
                    if retry >= 8 || self.lease_worker_stop.load(Ordering::Acquire) {
                        return Err(NfsError::Status(RETRY_UNCACHED_REP));
                    }
                    retry_sleep(core::time::Duration::from_millis(1u64 << retry));
                    retry += 1;
                    continue 'redrive;
                }
                Err(NfsError::Status(SEQ_FALSE_RETRY)) => {
                    // This is a failed SEQUENCE, not permission to allocate the
                    // next sequence and execute OPEN/WRITE again. Preserve its
                    // exact record behind the replay barrier; recovery either
                    // publishes that request's terminal outcome or rebuilds the
                    // session, never manufactures a new logical tail.
                    if let Some(session) = self.session.lock().as_mut() {
                        if session.id == sessionid {
                            session.replay_barrier = true;
                        }
                    }
                    self.lease_faulted.store(true, Ordering::Release);
                    drop(admission);
                    let mount = self
                        .self_ref
                        .lock()
                        .upgrade()
                        .ok_or(NfsError::SessionLost)?;
                    if mount
                        .recovery
                        .owner
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        let recovered = mount.recover();
                        mount.recovery.generation.fetch_add(1, Ordering::AcqRel);
                        mount.recovery.owner.store(false, Ordering::Release);
                        mount.recovery.waiters.notify_all(false);
                        recovered?;
                    } else {
                        let _ = mount.recovery.waiters.wait_until(|| {
                            !mount.recovery.owner.load(Ordering::Acquire)
                                || mount.lease_worker_stop.load(Ordering::Acquire)
                        });
                        if mount.lease_worker_stop.load(Ordering::Acquire) {
                            return Err(NfsError::SessionLost);
                        }
                    }
                    let reply = self.take_replayed_reply(sessionid, slot, sequence, xid)?;
                    return reply.terminal_error.clone().map_or(Ok(reply), Err);
                }
                Err(NfsError::Status(BADSLOT)) => {
                    {
                        let mut guard = self.session.lock();
                        let session = guard.as_mut().ok_or(NfsError::SessionLost)?;
                        if session.id != sessionid {
                            return Err(NfsError::SessionLost);
                        }
                        let entry = session
                            .slots
                            .iter_mut()
                            .find(|entry| entry.id == slot)
                            .ok_or(NfsError::SessionLost)?;
                        entry.unusable = true;
                        entry.lifecycle = SlotLifecycle::Free;
                    }
                    if retry >= 8 || self.lease_worker_stop.load(Ordering::Acquire) {
                        return Err(NfsError::Status(BADSLOT));
                    }
                    retry_sleep(core::time::Duration::from_millis(1u64 << retry));
                    retry += 1;
                    continue 'redrive;
                }
                Err(NfsError::TopLevelDelay) => {
                    // A top-level DELAY has no SEQUENCE result to acknowledge.
                    // Never advance or reconstruct the logical operation.  Once
                    // the exact-record retry budget is exhausted, fence the slot
                    // and run the same recovery election as a transport-ambiguous
                    // write; it publishes Terminal (or tears the session down),
                    // so no Sent slot is leaked indefinitely.
                    if let Some(session) = self.session.lock().as_mut() {
                        if session.id == sessionid {
                            session.replay_barrier = true;
                        }
                    }
                    self.lease_faulted.store(true, Ordering::Release);
                    drop(admission);
                    let mount = self
                        .self_ref
                        .lock()
                        .upgrade()
                        .ok_or(NfsError::SessionLost)?;
                    if mount
                        .recovery
                        .owner
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        let recovered = mount.recover();
                        mount.recovery.generation.fetch_add(1, Ordering::AcqRel);
                        mount.recovery.owner.store(false, Ordering::Release);
                        mount.recovery.waiters.notify_all(false);
                        recovered?;
                    } else {
                        let _ = mount.recovery.waiters.wait_until(|| {
                            !mount.recovery.owner.load(Ordering::Acquire)
                                || mount.lease_worker_stop.load(Ordering::Acquire)
                        });
                        if mount.lease_worker_stop.load(Ordering::Acquire) {
                            return Err(NfsError::SessionLost);
                        }
                    }
                    let reply = self.take_replayed_reply(sessionid, slot, sequence, xid)?;
                    return reply.terminal_error.clone().map_or(Ok(reply), Err);
                }
                Err(NfsError::Status(SEQ_MISORDERED)) => {
                    // RFC 5661 requires a lone SEQUENCE probe before treating a
                    // misordered reply as session death.  It is a distinct
                    // logical request, but retains the same session/slot/seq
                    // tuple; a successful probe proves the original tail never
                    // ran, so only then may we advance and redrive it.
                    let probe_xid = self.xid.fetch_add(1, Ordering::Relaxed);
                    let probe_ops = [Operation::Sequence {
                        sessionid,
                        slot,
                        sequence,
                    }];
                    let probe = self
                        .compound_record(probe_xid, 1, &probe_ops)
                        .and_then(|request| self.rpc_record(probe_xid, &request));
                    if let Ok(reply) = probe {
                        if reply.sequence_sessionid == Some(sessionid)
                            && reply.sequence_slot == Some(slot)
                            && reply.sequence_id == Some(sequence)
                            && reply.terminal_error.is_none()
                        {
                            {
                                let mut guard = self.session.lock();
                                let session = guard.as_mut().ok_or(NfsError::SessionLost)?;
                                let entry = session
                                    .slots
                                    .iter_mut()
                                    .find(|entry| entry.id == slot)
                                    .ok_or(NfsError::SessionLost)?;
                                entry.sequence = entry.sequence.wrapping_add(1);
                                entry.lifecycle = SlotLifecycle::Free;
                            }
                            if retry >= 8 || self.lease_worker_stop.load(Ordering::Acquire) {
                                return Err(NfsError::Status(SEQ_MISORDERED));
                            }
                            retry_sleep(core::time::Duration::from_millis(1u64 << retry));
                            retry += 1;
                            continue 'redrive;
                        }
                    }
                    if let Some(session) = self.session.lock().as_mut() {
                        if session.id == sessionid {
                            session.replay_barrier = true;
                        }
                    }
                    self.lease_faulted.store(true, Ordering::Release);
                    return Err(NfsError::SessionLost);
                }
                _ => {}
            }
            // Session-fatal SEQUENCE replies revoke this entire session.  Do not
            // publish an old-session terminal result, and (critically) do not
            // re-enter `invalidate_session` while its `session` mutex is held.
            // Full recovery will discard this Sent image after the originating
            // caller's admission is released.
            let session_fatal = matches!(
                &result,
                Err(NfsError::Status(
                    BADSESSION | DEADSESSION | CONN_NOT_BOUND_TO_SESSION
                ))
            );
            {
                let mut guard = self.session.lock();
                if !session_fatal {
                    if let Some(session) = guard.as_mut() {
                        if session.id == sessionid {
                            if let Some(entry) = session.slots.iter_mut().find(|v| v.id == slot) {
                                if matches!(
                                    result,
                                    Ok(_) | Err(NfsError::Status(_)) | Err(NfsError::Denied { .. })
                                ) {
                                    entry.sequence = entry.sequence.wrapping_add(1);
                                    entry.lifecycle = SlotLifecycle::Free;
                                }
                            }
                        }
                    }
                }
            }
            if admission.generation != self.session_generation.load(Ordering::Acquire) {
                return Err(NfsError::SessionLost);
            }
            if session_fatal {
                // `invalidate_session` takes `session`; release both the session
                // guard above and this caller's active-compound token first.
                // That also lets the recovery owner drain the old transport.
                self.reclaim_gate.store(true, Ordering::Release);
                drop(admission);
                self.lease_faulted.store(true, Ordering::Release);
                self.invalidate_session();
                return Err(NfsError::SessionLost);
            }
            return result;
        }
    }
    fn compound_record(
        &self,
        xid: u32,
        minor: u32,
        ops: &[Operation<'_>],
    ) -> NfsResult<EncodedRpc> {
        let mut body = Xdr::default();
        body.opaque(b"");
        body.u32(minor);
        body.u32(ops.len() as u32);
        for op in ops {
            op.encode(&mut body);
        }
        self.rpc_record_for_body(xid, &body.0)
    }
    /// Bootstrap RPCs (EXCHANGE_ID/CREATE_SESSION) precede session-slot
    /// ownership, but still retain one immutable record for their transport
    /// invocation and share the same authenticated RPC encoder.
    fn compound_raw(&self, minor: u32, ops: &[Operation<'_>]) -> NfsResult<Reply> {
        let xid = self.xid.fetch_add(1, Ordering::Relaxed);
        let request = self.compound_record(xid, minor, ops)?;
        let result = self.rpc_record(xid, &request);
        if matches!(
            result,
            Err(NfsError::Transport | NfsError::Malformed | NfsError::Security | NfsError::Rpc(_))
        ) {
            self.transport().cancel(xid);
            self.invalidate_session();
            return Err(NfsError::SessionLost);
        }
        result
    }
    /// Reissues only an unacknowledged, top-level DELAY using the captured
    /// record.  XID, SEQUENCE tuple, RPCSEC_GSS credential/MIC and wrapped
    /// body are immutable across every retry; a new logical COMPOUND would
    /// be unsafe for OPEN/WRITE.
    fn rpc_record_exact(&self, xid: u32, request: &EncodedRpc) -> NfsResult<Reply> {
        for retry in 0..=8u8 {
            let reply = self.rpc_record(xid, request)?;
            // A result array means SEQUENCE (and possibly a tail operation)
            // already executed. Only the zero-op top-level DELAY is safe to
            // resend under the identical record/XID contract.
            if reply.top_status != DELAY
                || reply.operation_count != 0
                || retry == 8
                || self.lease_worker_stop.load(Ordering::Acquire)
            {
                return Ok(reply);
            }
            retry_sleep(core::time::Duration::from_millis(1u64 << retry));
        }
        Err(NfsError::SessionLost)
    }
    fn rpc_record(&self, xid: u32, request: &EncodedRpc) -> NfsResult<Reply> {
        self.rpc_record_raw(xid, request)
    }
    /// Keeps the complete decoded COMPOUND envelope, including a top-level
    /// status and zero operation count.  Callers decide whether that status
    /// acknowledged a slot; no `?` may discard it before slot lifecycle code
    /// records the terminal outcome.
    fn rpc_record_raw(&self, xid: u32, request: &EncodedRpc) -> NfsResult<Reply> {
        let reply = decode_record(&self.transport().call(&request.record)?)?;
        self.decode_rpc_reply(xid, &reply, request.gss_sequence)
            .and_then(|body| Reply::parse(&body))
    }
    fn rpc_record_for_body(&self, xid: u32, body: &[u8]) -> NfsResult<EncodedRpc> {
        let sequence = match &self.auth {
            RpcAuth::Gss(g) => Some(g.sequence()?),
            _ => None,
        };
        let body = body.to_vec();
        let mut call = Xdr::default();
        call.u32(xid);
        call.u32(RPC_CALL);
        call.u32(RPC_VERSION);
        call.u32(NFS_PROGRAM);
        call.u32(NFS_VERSION);
        call.u32(COMPOUND);
        match &self.auth {
            RpcAuth::None => {
                call.u32(AUTH_NONE);
                call.u32(0);
                call.u32(AUTH_NONE);
                call.u32(0)
            }
            RpcAuth::Sys(sys) => {
                let c = sys.encode_credential(xid)?;
                call.u32(AUTH_SYS);
                call.opaque(&c);
                call.u32(AUTH_NONE);
                call.u32(0)
            }
            RpcAuth::Gss(g) => {
                let c = g.credential(xid, sequence.ok_or(NfsError::Security)?)?;
                call.u32(RPCSEC_GSS);
                call.opaque(&c);
                let v = g.verifier(&call.0)?;
                call.u32(RPCSEC_GSS);
                call.opaque(&v)
            }
        }
        let body = match (&self.auth, sequence) {
            (RpcAuth::Gss(g), Some(sequence)) => g.wrap(sequence, &body)?,
            _ => body,
        };
        call.0.extend_from_slice(&body);
        if call.0.len() > 0x7fff_ffff {
            return Err(NfsError::Length);
        }
        let mut record = Xdr::default();
        record.u32(0x8000_0000 | call.0.len() as u32);
        record.0.extend_from_slice(&call.0);
        Ok(EncodedRpc {
            record: record.0,
            gss_sequence: sequence,
        })
    }
    fn decode_rpc_reply(
        &self,
        xid: u32,
        reply: &[u8],
        sequence: Option<u32>,
    ) -> NfsResult<Vec<u8>> {
        let mut r = XdrIn::new(reply);
        if r.u32()? != xid || r.u32()? != RPC_REPLY || r.u32()? != RPC_ACCEPTED {
            return Err(NfsError::Malformed);
        };
        let verifier_flavor = r.u32()?;
        let verifier = r.opaque()?.to_vec();
        let accept = r.u32()?;
        if accept != RPC_SUCCESS {
            return Err(NfsError::Rpc(accept));
        };
        if let (RpcAuth::Gss(g), Some(sequence)) = (&self.auth, sequence) {
            if verifier_flavor != RPCSEC_GSS {
                return Err(NfsError::Security);
            }
            g.verify_reply(sequence, &verifier)?;
            let payload = reply.get(r.at..).ok_or(NfsError::Malformed)?;
            return g.unwrap(sequence, payload);
        }
        let payload = reply.get(r.at..).ok_or(NfsError::Malformed)?;
        Ok(payload.to_vec())
    }
}

// Create modes reserved for the in-progress create-with-attrs path.
#[allow(dead_code)]
#[derive(Clone, Copy)]
enum CreateHow {
    Unchecked,
    Guarded,
    Exclusive([u8; 8]),
}
#[derive(Clone)]
enum Operation<'a> {
    PutRootFh,
    PutFh(&'a FileHandle),
    SaveFh,
    RestoreFh,
    Lookup(FsNameBuf),
    GetFh,
    GetAttr,
    Verify {
        fsid_major: u64,
        fsid_minor: u64,
        fileid: u64,
    },
    VerifyChange(u64),
    GetAclCapabilities,
    GetSupportedAttrs,
    ExchangeId(&'a [u8]),
    CreateSession {
        clientid: u64,
        sequenceid: u32,
        slots: u32,
        callback_auth: &'a RpcSysAuth,
    },
    BindConnToSession {
        sessionid: [u8; 16],
        direction: u32,
    },
    Sequence {
        sessionid: [u8; 16],
        slot: u32,
        sequence: u32,
    },
    ReclaimComplete,
    // Reserved for the in-progress lease-renewal path.
    #[allow(dead_code)]
    Renew(u64),
    Open {
        seqid: u32,
        reclaim: bool,
        clientid: u64,
        owner: u64,
        share_access: u32,
        share_deny: u32,
        name: FsNameBuf,
    },
    OpenCreate {
        seqid: u32,
        clientid: u64,
        owner: u64,
        share_access: u32,
        name: FsNameBuf,
        mode: u32,
        attr_owner: Option<&'a [u8]>,
        attr_group: Option<&'a [u8]>,
        attr_acl: Option<&'a [u8]>,
        create_how: CreateHow,
    },
    OpenAttr(bool),
    Lock {
        open_seqid: u32,
        lock_seqid: u32,
        reclaim: bool,
        stateid: [u8; 16],
        clientid: u64,
        owner: u64,
        offset: u64,
        length: u64,
        write: bool,
    },
    LockT {
        clientid: u64,
        offset: u64,
        length: u64,
        write: bool,
    },
    LockU {
        seqid: u32,
        stateid: [u8; 16],
        offset: u64,
        length: u64,
        write: bool,
    },
    CreateDir {
        name: FsNameBuf,
        mode: u32,
        owner: Option<&'a [u8]>,
        group: Option<&'a [u8]>,
        acl: Option<&'a [u8]>,
    },
    CreateSymlink {
        name: FsNameBuf,
        target: &'a [u8],
        mode: u32,
        owner: Option<&'a [u8]>,
        group: Option<&'a [u8]>,
        acl: Option<&'a [u8]>,
    },
    Remove(FsNameBuf),
    Rename {
        old: FsNameBuf,
        new: FsNameBuf,
    },
    Link(FsNameBuf),
    ReadDir {
        cookie: u64,
        verifier: [u8; 8],
        max_bytes: u32,
    },
    SetAttr {
        stateid: [u8; 16],
        mode: Option<u32>,
        size: Option<u64>,
        owner: Option<&'a [u8]>,
        group: Option<&'a [u8]>,
        acl: Option<&'a [u8]>,
    },
    Read {
        stateid: [u8; 16],
        offset: u64,
        count: u32,
    },
    ReadLink,
    Write {
        stateid: [u8; 16],
        offset: u64,
        stability: StableHow,
        data: &'a [u8],
    },
    Commit {
        offset: u64,
        count: u32,
    },
    Close {
        sequence: u32,
        stateid: [u8; 16],
    },
    DelegReturn {
        stateid: [u8; 16],
    },
}
impl Operation<'_> {
    fn opcode(&self) -> u32 {
        match self {
            Self::PutRootFh => OP_PUTROOTFH,
            Self::PutFh(_) => OP_PUTFH,
            Self::SaveFh => OP_SAVEFH,
            Self::RestoreFh => OP_RESTOREFH,
            Self::Lookup(_) => OP_LOOKUP,
            Self::GetFh => OP_GETFH,
            Self::GetAttr | Self::GetAclCapabilities | Self::GetSupportedAttrs => OP_GETATTR,
            Self::Verify { .. } | Self::VerifyChange(_) => OP_VERIFY,
            Self::ExchangeId(_) => OP_EXCHANGE_ID,
            Self::CreateSession { .. } => OP_CREATE_SESSION,
            Self::BindConnToSession { .. } => OP_BIND_CONN_TO_SESSION,
            Self::Sequence { .. } => OP_SEQUENCE,
            Self::ReclaimComplete => OP_RECLAIM_COMPLETE,
            Self::Renew(_) => OP_RENEW,
            Self::Open { .. } | Self::OpenCreate { .. } => OP_OPEN,
            Self::OpenAttr(_) => OP_OPENATTR,
            Self::Lock { .. } => OP_LOCK,
            Self::LockT { .. } => OP_LOCKT,
            Self::LockU { .. } => OP_LOCKU,
            Self::CreateDir { .. } | Self::CreateSymlink { .. } => OP_CREATE,
            Self::Remove(_) => OP_REMOVE,
            Self::Rename { .. } => OP_RENAME,
            Self::Link(_) => OP_LINK,
            Self::ReadDir { .. } => OP_READDIR,
            Self::SetAttr { .. } => OP_SETATTR,
            Self::Read { .. } => OP_READ,
            Self::ReadLink => OP_READLINK,
            Self::Write { .. } => OP_WRITE,
            Self::Commit { .. } => OP_COMMIT,
            Self::Close { .. } => OP_CLOSE,
            Self::DelegReturn { .. } => OP_DELEGRETURN,
        }
    }
    fn encode(&self, x: &mut Xdr) {
        match self {
            Self::PutRootFh => x.u32(OP_PUTROOTFH),
            Self::PutFh(v) => {
                x.u32(OP_PUTFH);
                x.opaque(v.as_bytes())
            }
            Self::SaveFh => x.u32(OP_SAVEFH),
            Self::RestoreFh => x.u32(OP_RESTOREFH),
            Self::Lookup(v) => {
                x.u32(OP_LOOKUP);
                x.opaque(v.as_bytes())
            }
            Self::GetFh => x.u32(OP_GETFH),
            Self::GetAttr => {
                x.u32(OP_GETATTR);
                x.u32(2);
                x.u32(
                    (1 << 1)
                        | (1 << 3)
                        | (1 << 4)
                        | (1 << 8)
                        | (1 << 10)
                        | (1 << 20)
                        | (1 << 21)
                        | (1 << 22)
                        | (1 << 23)
                        | (1 << 29),
                );
                x.u32(
                    (1 << (33 - 32))
                        | (1 << (35 - 32))
                        | (1 << (36 - 32))
                        | (1 << (37 - 32))
                        | (1 << (38 - 32))
                        | (1 << (39 - 32))
                        | (1 << (40 - 32))
                        | (1 << (42 - 32))
                        | (1 << (43 - 32))
                        | (1 << (44 - 32)),
                )
            }
            Self::Verify {
                fsid_major,
                fsid_minor,
                fileid,
            } => {
                let mut attrs = Xdr::default();
                attrs.u64(*fsid_major);
                attrs.u64(*fsid_minor);
                attrs.u64(*fileid);
                x.u32(OP_VERIFY);
                x.u32(1);
                x.u32((1 << 8) | (1 << 20));
                x.opaque(&attrs.0)
            }
            Self::VerifyChange(change) => {
                let mut attrs = Xdr::default();
                attrs.u64(*change);
                x.u32(OP_VERIFY);
                x.u32(1);
                x.u32(1 << 3);
                x.opaque(&attrs.0)
            }
            Self::GetAclCapabilities => {
                x.u32(OP_GETATTR);
                x.u32(2);
                x.u32(
                    (1 << 1)
                        | (1 << 3)
                        | (1 << 4)
                        | (1 << 8)
                        | (1 << 12)
                        | (1 << 13)
                        | (1 << 20)
                        | (1 << 21)
                        | (1 << 22)
                        | (1 << 23)
                        | (1 << 29),
                );
                x.u32(
                    (1 << (33 - 32))
                        | (1 << (35 - 32))
                        | (1 << (36 - 32))
                        | (1 << (37 - 32))
                        | (1 << (38 - 32))
                        | (1 << (39 - 32))
                        | (1 << (40 - 32))
                        | (1 << (42 - 32))
                        | (1 << (43 - 32))
                        | (1 << (44 - 32)),
                )
            }
            Self::GetSupportedAttrs => {
                x.u32(OP_GETATTR);
                x.u32(1);
                x.u32(1);
            }
            Self::ExchangeId(owner) => {
                x.u32(OP_EXCHANGE_ID);
                x.u64(0);
                x.opaque(owner);
                x.u32(0);
                x.u32(0);
                x.u32(0)
            }
            Self::CreateSession {
                clientid,
                sequenceid,
                slots,
                callback_auth,
            } => {
                x.u32(OP_CREATE_SESSION);
                x.u64(*clientid);
                x.u32(*sequenceid);
                x.u32(0);
                for _ in 0..2 {
                    x.u32(0);
                    x.u32(1 << 20);
                    x.u32(1 << 20);
                    x.u32(1 << 20);
                    x.u32(MAX_COMPOUND_OPERATIONS as u32);
                    x.u32(*slots);
                    x.u32(0);
                }
                // Linux v6.18 advertises one AUTH_SYS callback_sec_parms4 even
                // when the forechannel itself uses RPCSEC_GSS. A zero-length
                // array disables authenticated callbacks on compliant servers.
                x.u32(0x4000_0000);
                x.u32(1);
                x.u32(AUTH_SYS);
                x.u32(0);
                x.opaque(&callback_auth.machine_name);
                x.u32(callback_auth.uid);
                x.u32(callback_auth.gid);
                x.u32(callback_auth.groups.len() as u32);
                for group in &callback_auth.groups {
                    x.u32(*group);
                }
            }
            Self::BindConnToSession {
                sessionid,
                direction,
            } => {
                x.u32(OP_BIND_CONN_TO_SESSION);
                x.fixed(sessionid);
                x.u32(*direction);
                x.u32(0)
            }
            Self::Sequence {
                sessionid,
                slot,
                sequence,
            } => {
                x.u32(OP_SEQUENCE);
                x.fixed(sessionid);
                x.u32(*sequence);
                x.u32(*slot);
                x.u32(0);
                x.u32(0)
            }
            Self::ReclaimComplete => {
                x.u32(OP_RECLAIM_COMPLETE);
                x.u32(0)
            }
            Self::Renew(id) => {
                x.u32(OP_RENEW);
                x.u64(*id)
            }
            Self::Open {
                seqid,
                reclaim,
                clientid,
                owner,
                share_access,
                share_deny,
                name,
            } => {
                x.u32(OP_OPEN);
                x.u32(*seqid);
                x.u32(*share_access);
                x.u32(*share_deny);
                x.u64(*clientid);
                x.opaque(&owner.to_be_bytes());
                x.u32(0);
                if *reclaim {
                    x.u32(1);
                    x.u32(0)
                } else {
                    x.u32(0);
                    x.opaque(name.as_bytes())
                }
            }
            Self::OpenCreate {
                seqid,
                clientid,
                owner,
                share_access,
                name,
                mode,
                attr_owner,
                attr_group,
                attr_acl,
                create_how,
            } => {
                x.u32(OP_OPEN);
                x.u32(*seqid);
                x.u32(*share_access);
                x.u32(0);
                x.u64(*clientid);
                x.opaque(&owner.to_be_bytes());
                x.u32(1);
                match create_how {
                    CreateHow::Exclusive(verifier) => {
                        x.u32(2);
                        x.fixed(verifier);
                    }
                    CreateHow::Unchecked | CreateHow::Guarded => {
                        x.u32(if matches!(create_how, CreateHow::Guarded) {
                            1
                        } else {
                            0
                        });
                        let mut first = 0u32;
                        let mut second = 1 << (33 - 32);
                        let mut attrs = Xdr::default();
                        if let Some(acl) = attr_acl {
                            first |= 1 << 12;
                            attrs.0.extend_from_slice(acl);
                        }
                        attrs.u32(*mode);
                        if let Some(owner) = attr_owner {
                            second |= 1 << (36 - 32);
                            attrs.opaque(owner);
                        }
                        if let Some(group) = attr_group {
                            second |= 1 << (37 - 32);
                            attrs.opaque(group);
                        }
                        x.u32(2);
                        x.u32(first);
                        x.u32(second);
                        x.opaque(&attrs.0);
                    }
                }
                x.u32(0);
                x.opaque(name.as_bytes())
            }
            Self::OpenAttr(create) => {
                x.u32(OP_OPENATTR);
                x.u32(*create as u32)
            }
            Self::Lock {
                open_seqid,
                lock_seqid,
                reclaim,
                stateid,
                clientid,
                owner,
                offset,
                length,
                write,
            } => {
                x.u32(OP_LOCK);
                x.u32(if *write { 2 } else { 1 });
                x.u32(*reclaim as u32);
                x.u64(*offset);
                x.u64(*length);
                x.u32(1);
                x.u32(*open_seqid);
                x.fixed(stateid);
                x.u32(*lock_seqid);
                x.u64(*clientid);
                x.opaque(&owner.to_be_bytes())
            }
            Self::LockT {
                clientid,
                offset,
                length,
                write,
            } => {
                x.u32(OP_LOCKT);
                x.u32(if *write { 2 } else { 1 });
                x.u64(*offset);
                x.u64(*length);
                x.u64(*clientid);
                x.opaque(&0u64.to_be_bytes())
            }
            Self::LockU {
                seqid,
                stateid,
                offset,
                length,
                write,
            } => {
                x.u32(OP_LOCKU);
                x.u32(if *write { 2 } else { 1 });
                x.u32(*seqid);
                x.u64(*offset);
                x.u64(*length);
                x.fixed(stateid)
            }
            Self::CreateDir {
                name,
                mode,
                owner,
                group,
                acl,
            } => {
                x.u32(OP_CREATE);
                x.u32(2);
                x.opaque(name.as_bytes());
                encode_create_attrs(x, *mode, *owner, *group, *acl)
            }
            Self::CreateSymlink {
                name,
                target,
                mode,
                owner,
                group,
                acl,
            } => {
                x.u32(OP_CREATE);
                x.u32(5);
                x.opaque(name.as_bytes());
                x.opaque(target);
                encode_create_attrs(x, *mode, *owner, *group, *acl)
            }
            Self::Remove(name) => {
                x.u32(OP_REMOVE);
                x.opaque(name.as_bytes())
            }
            Self::Rename { old, new } => {
                x.u32(OP_RENAME);
                x.opaque(old.as_bytes());
                x.opaque(new.as_bytes())
            }
            Self::Link(name) => {
                x.u32(OP_LINK);
                x.opaque(name.as_bytes())
            }
            Self::ReadDir {
                cookie,
                verifier,
                max_bytes,
            } => {
                x.u32(OP_READDIR);
                x.u64(*cookie);
                x.fixed(verifier);
                x.u32(*max_bytes);
                x.u32(*max_bytes);
                x.u32(1);
                x.u32((1 << 1) | (1 << 20))
            }
            Self::SetAttr {
                stateid,
                mode,
                size,
                owner,
                group,
                acl,
            } => {
                x.u32(OP_SETATTR);
                x.fixed(stateid);
                let mut first = 0u32;
                let mut second = 0u32;
                let mut values = Xdr::default();
                if let Some(acl) = acl {
                    first |= 1 << 12;
                    values.0.extend_from_slice(acl)
                }
                if let Some(size) = size {
                    first |= 1 << 4;
                    values.u64(*size)
                }
                if let Some(mode) = mode {
                    second |= 1 << (33 - 32);
                    values.u32(*mode)
                }
                if let Some(owner) = owner {
                    second |= 1 << (36 - 32);
                    values.opaque(owner)
                }
                if let Some(group) = group {
                    second |= 1 << (37 - 32);
                    values.opaque(group)
                }
                x.u32(2);
                x.u32(first);
                x.u32(second);
                x.opaque(&values.0)
            }
            Self::Read {
                stateid,
                offset,
                count,
            } => {
                x.u32(OP_READ);
                x.fixed(stateid);
                x.u64(*offset);
                x.u32(*count)
            }
            Self::ReadLink => x.u32(OP_READLINK),
            Self::Write {
                stateid,
                offset,
                stability,
                data,
            } => {
                x.u32(OP_WRITE);
                x.fixed(stateid);
                x.u64(*offset);
                x.u32(*stability as u32);
                x.opaque(data)
            }
            Self::Commit { offset, count } => {
                x.u32(OP_COMMIT);
                x.u64(*offset);
                x.u32(*count)
            }
            Self::Close { sequence, stateid } => {
                x.u32(OP_CLOSE);
                x.u32(*sequence);
                x.fixed(stateid)
            }
            Self::DelegReturn { stateid } => {
                x.u32(OP_DELEGRETURN);
                x.fixed(stateid)
            }
        }
    }
}
fn encode_create_attrs(
    x: &mut Xdr,
    mode: u32,
    owner: Option<&[u8]>,
    group: Option<&[u8]>,
    acl: Option<&[u8]>,
) {
    let mut first = 0u32;
    let mut second = 1 << (33 - 32);
    let mut values = Xdr::default();
    if let Some(acl) = acl {
        first |= 1 << 12;
        values.0.extend_from_slice(acl)
    }
    values.u32(mode);
    if let Some(owner) = owner {
        second |= 1 << (36 - 32);
        values.opaque(owner)
    }
    if let Some(group) = group {
        second |= 1 << (37 - 32);
        values.opaque(group)
    }
    x.u32(2);
    x.u32(first);
    x.u32(second);
    x.opaque(&values.0)
}
#[derive(Default)]
struct Xdr(Vec<u8>);
impl Xdr {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes())
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes())
    }
    fn fixed(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v)
    }
    fn opaque(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.fixed(v);
        self.0.resize((self.0.len() + 3) & !3, 0)
    }
}
fn decode_record(record: &[u8]) -> NfsResult<Vec<u8>> {
    let mut at = 0usize;
    let mut out = Vec::new();
    loop {
        let marker = record.get(at..at + 4).ok_or(NfsError::Malformed)?;
        let marker = u32::from_be_bytes(marker.try_into().map_err(|_| NfsError::Malformed)?);
        at = at.checked_add(4).ok_or(NfsError::Malformed)?;
        let length = (marker & 0x7fff_ffff) as usize;
        let end = at.checked_add(length).ok_or(NfsError::Length)?;
        out.extend_from_slice(record.get(at..end).ok_or(NfsError::Malformed)?);
        at = end;
        if marker & 0x8000_0000 != 0 {
            if at != record.len() {
                return Err(NfsError::Malformed);
            }
            return Ok(out);
        }
    }
}
struct XdrIn<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> XdrIn<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn take(&mut self, n: usize) -> NfsResult<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or(NfsError::Malformed)?;
        let out = self.bytes.get(self.at..end).ok_or(NfsError::Malformed)?;
        self.at = end;
        Ok(out)
    }
    fn u32(&mut self) -> NfsResult<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| NfsError::Malformed)?,
        ))
    }
    fn u64(&mut self) -> NfsResult<u64> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().map_err(|_| NfsError::Malformed)?,
        ))
    }
    fn opaque(&mut self) -> NfsResult<&'a [u8]> {
        let n = self.u32()? as usize;
        let out = self.take(n)?;
        self.take((4 - n % 4) % 4)?;
        Ok(out)
    }
}
struct Reply {
    top_status: u32,
    operation_count: u32,
    items: Vec<(u32, Vec<u8>)>,
    encoded: Vec<u8>,
    sequence_flags: u32,
    sequence_sessionid: Option<[u8; 16]>,
    sequence_id: Option<u32>,
    sequence_slot: Option<u32>,
    sequence_highest: Option<u32>,
    sequence_target_highest: Option<u32>,
    terminal_error: Option<NfsError>,
}
impl Reply {
    fn parse(bytes: &[u8]) -> NfsResult<Self> {
        let mut r = XdrIn::new(bytes);
        let status = r.u32()?;
        let _ = r.opaque()?;
        let operation_count = r.u32()?;
        let count = usize::try_from(operation_count).map_err(|_| NfsError::Length)?;
        // Every nfs_resop4 has an opcode and status word.  Check this before
        // Vec::with_capacity so a malicious count cannot turn a short reply
        // into a giant allocation, overflow, or OOM path.
        let minimum = count.checked_mul(8).ok_or(NfsError::Length)?;
        let remaining = bytes.len().checked_sub(r.at).ok_or(NfsError::Malformed)?;
        if count > MAX_COMPOUND_OPERATIONS {
            return Err(NfsError::Length);
        }
        if minimum > remaining {
            return Err(NfsError::Malformed);
        }
        // A non-NFS_OK COMPOUND status commonly accompanies the successful
        // prefix plus the failing operation in resarray4.  Parse that array
        // exactly like an NFS_OK reply: discarding it loses a valid SEQUENCE
        // acknowledgement and turns ordinary operation errors into bogus
        // malformed transport failures.
        let mut items = Vec::with_capacity(count);
        let mut sequence_flags = 0;
        let mut sequence_sessionid = None;
        let mut sequence_id = None;
        let mut sequence_slot = None;
        let mut sequence_highest = None;
        let mut sequence_target_highest = None;
        let mut terminal_error = (status != NFS_OK).then_some(NfsError::Status(status));
        let mut last_operation_status = None;
        for index in 0..count {
            let opcode = r.u32()?;
            let operation_status = r.u32()?;
            last_operation_status = Some(operation_status);
            if operation_status != NFS_OK {
                if opcode == OP_LOCKT && operation_status == 10010 {
                    let offset = r.u64()?;
                    let length = r.u64()?;
                    let write = r.u32()? == 2;
                    let _clientid = r.u64()?;
                    let owner = r.opaque()?.to_vec();
                    terminal_error = Some(NfsError::Denied {
                        offset,
                        length,
                        write,
                        owner,
                    });
                } else {
                    terminal_error = Some(NfsError::Status(operation_status));
                }
                if r.at != bytes.len() {
                    return Err(NfsError::Malformed);
                }
                items.push((opcode, Vec::new()));
                break;
            }
            let start = r.at;
            match opcode {
                OP_SEQUENCE => {
                    if index != 0 {
                        return Err(NfsError::Malformed);
                    }
                    let sequence = r.take(36)?;
                    sequence_sessionid =
                        Some(sequence[..16].try_into().map_err(|_| NfsError::Malformed)?);
                    sequence_id = Some(u32::from_be_bytes(
                        sequence[16..20]
                            .try_into()
                            .map_err(|_| NfsError::Malformed)?,
                    ));
                    sequence_slot = Some(u32::from_be_bytes(
                        sequence[20..24]
                            .try_into()
                            .map_err(|_| NfsError::Malformed)?,
                    ));
                    sequence_highest = Some(u32::from_be_bytes(
                        sequence[24..28]
                            .try_into()
                            .map_err(|_| NfsError::Malformed)?,
                    ));
                    sequence_target_highest = Some(u32::from_be_bytes(
                        sequence[28..32]
                            .try_into()
                            .map_err(|_| NfsError::Malformed)?,
                    ));
                    sequence_flags = u32::from_be_bytes(
                        sequence[32..36]
                            .try_into()
                            .map_err(|_| NfsError::Malformed)?,
                    );
                }
                OP_PUTFH | OP_PUTROOTFH | OP_SAVEFH | OP_RESTOREFH | OP_LOOKUP | OP_OPENATTR
                | OP_VERIFY | OP_RECLAIM_COMPLETE | OP_RENEW | OP_CLOSE | OP_DELEGRETURN
                | OP_LOCKT => {}
                OP_GETFH => {
                    let _ = r.opaque()?;
                }
                OP_READ => {
                    let _ = r.u32()?;
                    let _ = r.opaque()?;
                }
                OP_WRITE => {
                    r.take(16)?;
                }
                OP_COMMIT => {
                    r.take(8)?;
                }
                OP_LOCKU => {
                    r.take(16)?;
                }
                OP_CREATE => {
                    r.take(20)?;
                    let words = r.u32()? as usize;
                    r.take(words.checked_mul(4).ok_or(NfsError::Length)?)?;
                }
                OP_REMOVE | OP_LINK => {
                    r.take(20)?;
                }
                OP_RENAME => {
                    r.take(40)?;
                }
                OP_SETATTR => {
                    let words = r.u32()? as usize;
                    r.take(words.checked_mul(4).ok_or(NfsError::Length)?)?;
                }
                OP_OPEN => {
                    skip_open_result(&mut r)?;
                }
                OP_LOCK => {
                    r.take(16)?;
                }
                OP_GETATTR
                | OP_READDIR
                | OP_EXCHANGE_ID
                | OP_CREATE_SESSION
                | OP_BIND_CONN_TO_SESSION
                    if index + 1 == count =>
                {
                    r.at = bytes.len()
                }
                _ => return Err(NfsError::Malformed),
            }
            items.push((opcode, bytes[start..r.at].to_vec()));
        }
        if r.at != bytes.len() {
            return Err(NfsError::Malformed);
        }
        // RFC COMPOUND status is the status of the final executed operation.
        // A contradictory envelope could otherwise make a failed SEQUENCE
        // look acknowledged, so reject it before any slot transition sees it.
        if count != 0 && last_operation_status != Some(status) {
            return Err(NfsError::Malformed);
        }
        Ok(Self {
            top_status: status,
            operation_count,
            items,
            encoded: bytes.to_vec(),
            sequence_flags,
            sequence_sessionid,
            sequence_id,
            sequence_slot,
            sequence_highest,
            sequence_target_highest,
            terminal_error,
        })
    }
    fn last(&self, opcode: u32) -> NfsResult<&[u8]> {
        self.items
            .iter()
            .rev()
            .find(|v| v.0 == opcode)
            .map(|v| v.1.as_slice())
            .ok_or(NfsError::Malformed)
    }
    fn bytes(&self) -> Vec<u8> {
        self.encoded.clone()
    }
}
fn parse_exchange(v: &Reply) -> NfsResult<ExchangeIdentity> {
    let mut r = XdrIn::new(v.last(OP_EXCHANGE_ID)?);
    Ok(ExchangeIdentity {
        clientid: r.u64()?,
        sequenceid: r.u32()?,
    })
}
fn parse_bind_connection(
    v: &Reply,
    sessionid: [u8; 16],
    direction: u32,
    use_conn_in_rdma: bool,
) -> NfsResult<()> {
    let mut r = XdrIn::new(v.last(OP_BIND_CONN_TO_SESSION)?);
    let returned: [u8; 16] = r.take(16)?.try_into().map_err(|_| NfsError::Malformed)?;
    let returned_direction = r.u32()?;
    let returned_rdma = r.u32()?;
    if r.at != r.bytes.len()
        || returned != sessionid
        || returned_direction != direction
        || returned_rdma > 1
        || (returned_rdma != 0) != use_conn_in_rdma
    {
        return Err(NfsError::Malformed);
    }
    Ok(())
}
fn parse_channel_attrs(r: &mut XdrIn<'_>) -> NfsResult<ChannelGrant> {
    let _headerpad = r.u32()?;
    let max_request = r.u32()?;
    let max_response = r.u32()?;
    let max_cached = r.u32()?;
    let max_operations = r.u32()?;
    let max_requests = r.u32()?;
    let rdma_words = r.u32()? as usize;
    r.take(rdma_words.checked_mul(4).ok_or(NfsError::Length)?)?;
    // A zero or self-contradictory grant cannot safely carry COMPOUNDs.
    let max_operations = usize::try_from(max_operations).map_err(|_| NfsError::Length)?;
    if max_request == 0
        || max_response == 0
        || max_cached == 0
        || max_operations == 0
        || max_requests == 0
    {
        return Err(NfsError::Malformed);
    }
    // A server may legally grant more than the client requested. Retain the
    // negotiated intersection rather than rejecting that valid up-rounding;
    // local admission remains bounded by what this client advertised.
    Ok(ChannelGrant {
        max_requests,
        max_operations: max_operations.min(MAX_COMPOUND_OPERATIONS),
    })
}
fn parse_create_session(v: &Reply) -> NfsResult<CreateSessionGrant> {
    let mut r = XdrIn::new(v.last(OP_CREATE_SESSION)?);
    let id: [u8; 16] = r.take(16)?.try_into().map_err(|_| NfsError::Malformed)?;
    let _sequenceid = r.u32()?;
    let _flags = r.u32()?;
    let fore = parse_channel_attrs(&mut r)?;
    let _back = parse_channel_attrs(&mut r)?;
    if r.at != r.bytes.len() {
        return Err(NfsError::Malformed);
    }
    Ok(CreateSessionGrant {
        id,
        fore_slots: fore.max_requests,
        fore_max_operations: fore.max_operations,
    })
}
// Reply parsers for the in-progress open/lock recovery path.
#[allow(dead_code)]
fn parse_open(v: &Reply) -> NfsResult<[u8; 16]> {
    XdrIn::new(v.last(OP_OPEN)?)
        .take(16)?
        .try_into()
        .map_err(|_| NfsError::Malformed)
}
fn parse_lock(v: &Reply) -> NfsResult<[u8; 16]> {
    let mut r = XdrIn::new(v.last(OP_LOCK)?);
    let stateid = r.take(16)?.try_into().map_err(|_| NfsError::Malformed)?;
    if r.at != r.bytes.len() {
        return Err(NfsError::Malformed);
    }
    Ok(stateid)
}
fn parse_getfh(v: &Reply) -> NfsResult<FileHandle> {
    FileHandle::new(XdrIn::new(v.last(OP_GETFH)?).opaque()?.to_vec())
}
fn parse_supported_attrs(v: &Reply) -> NfsResult<[u32; 2]> {
    let mut r = XdrIn::new(v.last(OP_GETATTR)?);
    if r.u32()? != 1 || r.u32()? != 1 {
        return Err(NfsError::Malformed);
    }
    let encoded = r.opaque()?;
    if r.at != r.bytes.len() {
        return Err(NfsError::Malformed);
    }
    let mut values = XdrIn::new(encoded);
    let words = values.u32()? as usize;
    if words > 8 {
        return Err(NfsError::Length);
    }
    let mut out = [0; 2];
    for index in 0..words {
        let word = values.u32()?;
        if index < 2 {
            out[index] = word
        }
    }
    if values.at != values.bytes.len() {
        return Err(NfsError::Malformed);
    }
    Ok(out)
}
fn parse_acl4(a: &mut XdrIn<'_>) -> NfsResult<Nfs4Acl> {
    let count = a.u32()?;
    let mut wire = Vec::new();
    wire.try_reserve(4).map_err(|_| NfsError::Transport)?;
    wire.extend_from_slice(&count.to_be_bytes());
    for _ in 0..count {
        let ty = a.u32()?;
        let flags = a.u32()?;
        let mask = a.u32()?;
        let who = a.opaque()?;
        wire.try_reserve(16usize.checked_add(who.len()).ok_or(NfsError::Length)?)
            .map_err(|_| NfsError::Transport)?;
        wire.extend_from_slice(&ty.to_be_bytes());
        wire.extend_from_slice(&flags.to_be_bytes());
        wire.extend_from_slice(&mask.to_be_bytes());
        wire.extend_from_slice(&(who.len() as u32).to_be_bytes());
        wire.extend_from_slice(who);
        while wire.len() % 4 != 0 {
            wire.push(0)
        }
    }
    Ok(Nfs4Acl(wire))
}
fn parse_acl_capabilities(v: &Reply, supported: [u32; 2]) -> NfsResult<NfsAclCapabilities> {
    let mut r = XdrIn::new(v.last(OP_GETATTR)?);
    if r.u32()? != 2 {
        return Err(NfsError::Malformed);
    }
    let first = r.u32()?;
    let second = r.u32()?;
    let encoded = r.opaque()?;
    if r.at != r.bytes.len() {
        return Err(NfsError::Malformed);
    }
    let mut a = XdrIn::new(encoded);
    let mut support = None;
    let mut root_acl = None;
    for bit in 0..32 {
        if first & (1 << bit) == 0 {
            continue;
        }
        match bit {
            1 => {
                let _ = a.u32()?;
            }
            3 | 4 | 20 => {
                let _ = a.u64()?;
            }
            8 => {
                let _ = a.u64()?;
                let _ = a.u64()?;
            }
            12 => root_acl = Some(parse_acl4(&mut a)?),
            13 => support = Some(a.u32()?),
            21 | 22 | 23 => {
                let _ = a.u64()?;
            }
            29 => {
                let _ = a.u32()?;
            }
            _ => return Err(NfsError::Malformed),
        }
    }
    for bit in 0..32 {
        if second & (1 << bit) == 0 {
            continue;
        }
        match bit + 32 {
            33 | 35 => {
                let _ = a.u32()?;
            }
            36 | 37 => {
                let _ = a.opaque()?;
            }
            38 | 39 | 40 | 42 | 43 | 44 => {
                let _ = a.u64()?;
            }
            _ => return Err(NfsError::Malformed),
        }
    }
    if a.at != a.bytes.len() {
        return Err(NfsError::Malformed);
    }
    Ok(NfsAclCapabilities {
        supported_attrs: supported,
        acl_support: support.ok_or(NfsError::Malformed)?,
        root_acl: root_acl.ok_or(NfsError::Malformed)?,
    })
}
fn parse_attr(v: &Reply) -> NfsResult<NfsAttr> {
    let mut r = XdrIn::new(v.last(OP_GETATTR)?);
    let words = r.u32()? as usize;
    if words != 2 {
        return Err(NfsError::Malformed);
    }
    let first = r.u32()?;
    let second = r.u32()?;
    let encoded = r.opaque()?;
    let mut a = XdrIn::new(encoded);
    let mut out = NfsAttr::default();
    for bit in 0..32 {
        if first & (1 << bit) == 0 {
            continue;
        }
        match bit {
            1 => out.kind = a.u32()?,
            3 => out.change = a.u64()?,
            4 => out.size = a.u64()?,
            8 => {
                out.fsid_major = a.u64()?;
                out.fsid_minor = a.u64()?
            }
            10 => out.lease_time = a.u32()?,
            20 => out.fileid = a.u64()?,
            21 | 22 | 23 => {
                let _ = a.u64()?;
            }
            29 => out.max_name = a.u32()?,
            _ => return Err(NfsError::Malformed),
        }
    }
    for bit in 0..32 {
        if second & (1 << bit) == 0 {
            continue;
        }
        match bit + 32 {
            33 => out.mode = a.u32()?,
            35 => out.nlink = a.u32()?,
            36 => out.owner = a.opaque()?.to_vec(),
            37 => out.owner_group = a.opaque()?.to_vec(),
            38 => out.quota_avail_hard = a.u64()?,
            39 => out.quota_avail_soft = a.u64()?,
            40 => out.quota_used = a.u64()?,
            42 => out.space_avail = a.u64()?,
            43 => out.space_free = a.u64()?,
            44 => out.space_total = a.u64()?,
            _ => return Err(NfsError::Malformed),
        }
    }
    Ok(out)
}
fn parse_read(v: &Reply) -> NfsResult<ReadResult> {
    let mut r = XdrIn::new(v.last(OP_READ)?);
    let eof = r.u32()? != 0;
    Ok(ReadResult {
        eof,
        data: r.opaque()?.to_vec(),
    })
}
fn parse_readlink(v: &Reply) -> NfsResult<Vec<u8>> {
    XdrIn::new(v.last(OP_READLINK)?)
        .opaque()
        .map(|bytes| bytes.to_vec())
}
fn parse_write(v: &Reply) -> NfsResult<WriteResult> {
    let mut r = XdrIn::new(v.last(OP_WRITE)?);
    let count = r.u32()?;
    let committed = match r.u32()? {
        0 => StableHow::Unstable,
        1 => StableHow::DataSync,
        2 => StableHow::FileSync,
        _ => return Err(NfsError::Malformed),
    };
    Ok(WriteResult {
        count,
        committed,
        verifier: r.take(8)?.try_into().map_err(|_| NfsError::Malformed)?,
    })
}
fn parse_commit(v: &Reply) -> NfsResult<[u8; 8]> {
    XdrIn::new(v.last(OP_COMMIT)?)
        .take(8)?
        .try_into()
        .map_err(|_| NfsError::Malformed)
}
fn parse_open_created(v: &Reply) -> NfsResult<bool> {
    let mut r = XdrIn::new(v.last(OP_OPEN)?);
    let _stateid = r.take(16)?;
    let atomic = r.u32()? != 0;
    let before = r.u64()?;
    let after = r.u64()?;
    Ok(atomic && before != after)
}
fn parse_open_attrset(v: &Reply) -> NfsResult<[u32; 2]> {
    let mut r = XdrIn::new(v.last(OP_OPEN)?);
    r.take(16)?;
    r.take(20)?;
    let words = r.u32()? as usize;
    if words > 8 {
        return Err(NfsError::Malformed);
    }
    let mut attrs = [0; 2];
    for index in 0..words {
        let word = r.u32()?;
        if index < 2 {
            attrs[index] = word
        }
    }
    Ok(attrs)
}
fn parse_create_attrset(v: &Reply) -> NfsResult<[u32; 2]> {
    let mut r = XdrIn::new(v.last(OP_CREATE)?);
    r.take(20)?;
    let words = r.u32()? as usize;
    if words > 8 {
        return Err(NfsError::Malformed);
    }
    let mut attrs = [0; 2];
    for index in 0..words {
        let word = r.u32()?;
        if index < 2 {
            attrs[index] = word
        }
    }
    if r.at != r.bytes.len() {
        return Err(NfsError::Malformed);
    }
    Ok(attrs)
}
// Reply parser for the in-progress setattr path.
#[allow(dead_code)]
fn parse_setattr_attrset(v: &Reply) -> NfsResult<[u32; 2]> {
    let mut r = XdrIn::new(v.last(OP_SETATTR)?);
    let words = r.u32()? as usize;
    if words > 8 {
        return Err(NfsError::Malformed);
    }
    let mut attrs = [0; 2];
    for index in 0..words {
        let word = r.u32()?;
        if index < 2 {
            attrs[index] = word
        }
    }
    if r.at != r.bytes.len() {
        return Err(NfsError::Malformed);
    }
    Ok(attrs)
}
fn skip_open_result(r: &mut XdrIn<'_>) -> NfsResult<()> {
    let _stateid = r.take(16)?;
    let _atomic = r.u32()?;
    let _before = r.u64()?;
    let _after = r.u64()?;
    let _flags = r.u32()?;
    let words = r.u32()? as usize;
    if words > 8 {
        return Err(NfsError::Malformed);
    }
    r.take(words.checked_mul(4).ok_or(NfsError::Length)?)?;
    match r.u32()? {
        0 => {}
        1 => {
            let _stateid = r.take(16)?;
            let _recall = r.u32()?;
            parse_nfsace(r)?;
        }
        2 => {
            let _stateid = r.take(16)?;
            let _recall = r.u32()?;
            match r.u32()? {
                1 => {
                    let _filesize = r.u64()?;
                }
                2 => {
                    let _blocks = r.u32()?;
                    let _bytes_per_block = r.u32()?;
                }
                _ => return Err(NfsError::Malformed),
            };
            parse_nfsace(r)?;
        }
        3 => {
            let _why = r.u32()?;
        }
        _ => return Err(NfsError::Malformed),
    }
    Ok(())
}
fn parse_open_result(v: &Reply) -> NfsResult<([u8; 16], Option<[u8; 16]>)> {
    let mut r = XdrIn::new(v.last(OP_OPEN)?);
    let stateid = r.take(16)?.try_into().map_err(|_| NfsError::Malformed)?;
    let _atomic = r.u32()?;
    let _before = r.u64()?;
    let _after = r.u64()?;
    let _flags = r.u32()?;
    let words = r.u32()? as usize;
    if words > 8 {
        return Err(NfsError::Malformed);
    }
    for _ in 0..words {
        let _ = r.u32()?;
    }
    let delegation = match r.u32()? {
        0 => None,
        1 => {
            let stateid = r.take(16)?.try_into().map_err(|_| NfsError::Malformed)?;
            let _recall = r.u32()?;
            parse_nfsace(&mut r)?;
            Some(stateid)
        }
        2 => {
            let stateid = r.take(16)?.try_into().map_err(|_| NfsError::Malformed)?;
            let _recall = r.u32()?;
            match r.u32()? {
                1 => {
                    let _filesize = r.u64()?;
                }
                2 => {
                    let _blocks = r.u32()?;
                    let _bytes_per_block = r.u32()?;
                }
                _ => return Err(NfsError::Malformed),
            }
            parse_nfsace(&mut r)?;
            Some(stateid)
        }
        3 => {
            let _why = r.u32()?;
            None
        }
        _ => return Err(NfsError::Malformed),
    };
    if r.at != r.bytes.len() {
        return Err(NfsError::Malformed);
    }
    Ok((stateid, delegation))
}
fn parse_nfsace(r: &mut XdrIn<'_>) -> NfsResult<()> {
    let _type = r.u32()?;
    let _flags = r.u32()?;
    let _mask = r.u32()?;
    let _who = r.opaque()?;
    Ok(())
}
fn parse_locku(v: &Reply) -> NfsResult<[u8; 16]> {
    XdrIn::new(v.last(OP_LOCKU)?)
        .take(16)?
        .try_into()
        .map_err(|_| NfsError::Malformed)
}
fn parse_rename_change(v: &Reply) -> NfsResult<RenameChange> {
    let mut r = XdrIn::new(v.last(OP_RENAME)?);
    r.take(20)?;
    let target_atomic = r.u32()? != 0;
    let target_before = r.u64()?;
    let target_after = r.u64()?;
    if r.at != r.bytes.len() {
        return Err(NfsError::Malformed);
    }
    Ok(RenameChange {
        target_atomic,
        target_before,
        target_after,
    })
}
fn parse_readdir(v: &Reply) -> NfsResult<ReadDirResult> {
    let mut r = XdrIn::new(v.last(OP_READDIR)?);
    let verifier: [u8; 8] = r.take(8)?.try_into().map_err(|_| NfsError::Malformed)?;
    let mut entries = Vec::new();
    while r.u32()? != 0 {
        let cookie = r.u64()?;
        let name = FsNameBuf::from_vec(r.opaque()?.to_vec()).map_err(|_| NfsError::Malformed)?;
        let words = r.u32()? as usize;
        if words != 1 {
            return Err(NfsError::Malformed);
        }
        let bits = r.u32()?;
        let data = r.opaque()?;
        let mut a = XdrIn::new(data);
        let mut kind = 0;
        let mut fileid = 0;
        if bits & (1 << 1) != 0 {
            kind = a.u32()?
        }
        if bits & (1 << 20) != 0 {
            fileid = a.u64()?
        }
        entries.push(ReadDirEntry {
            name,
            cookie,
            kind,
            fileid,
        });
    }
    Ok(ReadDirResult {
        verifier,
        eof: r.u32()? != 0,
        entries,
    })
}

// VFS adapter.  Remote objects retain an NFS filehandle, never a pathname;
// every per-open object owns a distinct OPEN stateid and is released at the
// OFD boundary.
/// A filehandle is the server's opaque, stable object discriminator.  FNV-1a
/// is used only to fit it into VFS' fixed-width ObjectKey generation field;
/// it deliberately excludes FATTR4_CHANGE, which is a coherency version and
/// must not turn writes/metadata updates into a different inode identity.
fn nfs_filehandle_generation(handle: &FileHandle) -> u64 {
    handle
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
        })
}
fn nfs_filesystem_identity(attr: &NfsAttr) -> u64 {
    attr.fsid_major.rotate_left(29) ^ attr.fsid_minor
}
/// Preserve the provider-capability result at the VFS boundary.  In
/// particular, an attribute absent from FATTR4_SUPPORTED_ATTRS is not an I/O
/// failure: callers must fail before publishing the name with the same
/// unsupported result as the remote CREATE arm.
fn nfs_vfs<T>(result: NfsResult<T>) -> VfsResult<T> {
    result.map_err(|error| match error {
        // A NOWAIT slot probe is advisory until compound atomically marks a
        // slot busy.  A competing RPC may win that slot meanwhile; preserve the
        // retryable admission result so the syscall layer returns EAGAIN rather
        // than misreporting an I/O failure.
        NfsError::WouldBlock => VfsError::WouldBlock,
        // RFC 5661 status values are Linux-visible errno, not generic I/O.
        NfsError::Status(1) => VfsError::OperationNotPermitted,
        NfsError::Status(2) => VfsError::NotFound,
        NfsError::Status(13) => VfsError::PermissionDenied,
        NfsError::Status(17) => VfsError::AlreadyExists,
        NfsError::Status(18) => VfsError::CrossesDevices,
        NfsError::Status(20) => VfsError::NotADirectory,
        NfsError::Status(21) => VfsError::IsADirectory,
        NfsError::Status(22) => VfsError::InvalidInput,
        NfsError::Status(30) => VfsError::ReadOnlyFilesystem,
        NfsError::Status(63) => VfsError::NameTooLong,
        NfsError::Status(66) => VfsError::DirectoryNotEmpty,
        NfsError::Status(70) | NfsError::Status(10001) => VfsError::NotFound,
        NfsError::Status(10004) => VfsError::OperationNotSupported,
        NfsError::Status(10006) => VfsError::ResourceBusy,
        _ => VfsError::Io,
    })
}
const POSIX_ACL_VERSION: u32 = 0x0002;
const POSIX_ACL_USER_OBJ: u16 = 0x01;
const POSIX_ACL_USER: u16 = 0x02;
const POSIX_ACL_GROUP_OBJ: u16 = 0x04;
const POSIX_ACL_GROUP: u16 = 0x08;
const POSIX_ACL_MASK: u16 = 0x10;
const POSIX_ACL_OTHER: u16 = 0x20;
const ACE4_ACCESS_ALLOWED: u32 = 0;
const ACE4_ACCESS_DENIED: u32 = 1;
const ACE4_FILE_INHERIT: u32 = 1;
const ACE4_DIRECTORY_INHERIT: u32 = 2;
const ACE4_INHERIT_ONLY: u32 = 8;
const ACE4_IDENTIFIER_GROUP: u32 = 0x40;
const ACE4_READ_DATA: u32 = 1;
const ACE4_WRITE_DATA: u32 = 2;
const ACE4_APPEND_DATA: u32 = 4;
// ACE4 mask bits reserved for the in-progress full ACL mapping; the current
// profile deliberately grants only the DAC data rights below.
#[allow(dead_code)]
const ACE4_READ_NAMED_ATTRS: u32 = 8;
#[allow(dead_code)]
const ACE4_WRITE_NAMED_ATTRS: u32 = 16;
const ACE4_EXECUTE: u32 = 32;
#[allow(dead_code)]
const ACE4_READ_ATTRIBUTES: u32 = 128;
#[allow(dead_code)]
const ACE4_WRITE_ATTRIBUTES: u32 = 256;
#[allow(dead_code)]
const ACE4_READ_ACL: u32 = 0x0002_0000;
#[allow(dead_code)]
const ACE4_SYNCHRONIZE: u32 = 0x0010_0000;

/// The deliberate POSIX-to-NFSv4 rights profile.  It contains only the DAC
/// data/append/execute rights represented by a POSIX rwx bit; metadata, ACL
/// administration, named attributes and SYNCHRONIZE are not silently granted.
fn posix_to_nfs4_minimal_dac_rights(perm: u16) -> u32 {
    (if perm & 4 != 0 { ACE4_READ_DATA } else { 0 })
        | (if perm & 2 != 0 {
            ACE4_WRITE_DATA | ACE4_APPEND_DATA
        } else {
            0
        })
        | (if perm & 1 != 0 { ACE4_EXECUTE } else { 0 })
}
fn push_acl4_ace(
    out: &mut Vec<u8>,
    ace_type: u32,
    flags: u32,
    mask: u32,
    who: &[u8],
) -> VfsResult<()> {
    out.try_reserve(16usize.checked_add(who.len()).ok_or(VfsError::NoMemory)?)
        .map_err(|_| VfsError::NoMemory)?;
    out.extend_from_slice(&ace_type.to_be_bytes());
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&mask.to_be_bytes());
    out.extend_from_slice(&(who.len() as u32).to_be_bytes());
    out.extend_from_slice(who);
    out.resize((out.len() + 3) & !3, 0);
    Ok(())
}
/// Translates the Linux POSIX ACL xattr representation prepared by the VFS
/// into an RFC 5661 `acl4` value.  It deliberately produces the NFS wire
/// value consumed by FATTR4_ACL, never a `system.posix_acl_*` named attribute.
fn append_posix_acl4(
    mount: &NfsMount,
    xattr: &[u8],
    inherit: bool,
    out: &mut Vec<u8>,
    count: &mut u32,
) -> VfsResult<()> {
    if xattr.len() < 4
        || (xattr.len() - 4) % 8 != 0
        || u32::from_le_bytes(xattr[..4].try_into().map_err(|_| VfsError::InvalidInput)?)
            != POSIX_ACL_VERSION
    {
        return Err(VfsError::InvalidInput);
    }
    let mut entries = Vec::new();
    for raw in xattr[4..].chunks_exact(8) {
        let tag = u16::from_le_bytes(raw[..2].try_into().map_err(|_| VfsError::InvalidInput)?);
        let perm = u16::from_le_bytes(raw[2..4].try_into().map_err(|_| VfsError::InvalidInput)?);
        let id = u32::from_le_bytes(raw[4..8].try_into().map_err(|_| VfsError::InvalidInput)?);
        if perm & !7 != 0 {
            return Err(VfsError::InvalidInput);
        }
        entries.push((tag, perm, id));
    }
    let extended = entries
        .iter()
        .any(|(tag, ..)| matches!(*tag, POSIX_ACL_USER | POSIX_ACL_GROUP));
    // ACL4 evaluates named groups as ordered principals while POSIX combines
    // all matching groups before applying ACL_MASK.  This provider has no
    // server-specific proof that can preserve that union, so do not guess.
    if entries.iter().any(|(tag, ..)| *tag == POSIX_ACL_GROUP) {
        return Err(VfsError::OperationNotSupported);
    }
    let acl_mask = entries
        .iter()
        .find(|(tag, ..)| *tag == POSIX_ACL_MASK)
        .map(|(_, perm, _)| *perm);
    if extended != acl_mask.is_some() {
        return Err(VfsError::InvalidInput);
    }
    let inherit_flags = if inherit {
        ACE4_FILE_INHERIT | ACE4_DIRECTORY_INHERIT | ACE4_INHERIT_ONLY
    } else {
        0
    };
    for (tag, mut perm, id) in entries {
        let (flags, who) = match tag {
            POSIX_ACL_USER_OBJ if id == u32::MAX => (inherit_flags, b"OWNER@".to_vec()),
            POSIX_ACL_USER if id != u32::MAX => (inherit_flags, nfs_vfs(mount.uid_to_owner(id))?),
            POSIX_ACL_GROUP_OBJ if id == u32::MAX => {
                (inherit_flags | ACE4_IDENTIFIER_GROUP, b"GROUP@".to_vec())
            }
            POSIX_ACL_GROUP if id != u32::MAX => (
                inherit_flags | ACE4_IDENTIFIER_GROUP,
                nfs_vfs(mount.gid_to_group(id))?,
            ),
            POSIX_ACL_OTHER if id == u32::MAX => (inherit_flags, b"EVERYONE@".to_vec()),
            POSIX_ACL_MASK if id == u32::MAX => continue,
            _ => return Err(VfsError::InvalidInput),
        };
        if matches!(tag, POSIX_ACL_USER | POSIX_ACL_GROUP | POSIX_ACL_GROUP_OBJ) {
            perm &= acl_mask.unwrap_or(7);
        }
        // ACL4 does not have POSIX's mutually-exclusive user/group/other
        // classes.  OWNER@ and named users need a preceding denial for bits
        // absent from their selected POSIX class, before broad ACEs can see
        // them.  Groups are different: POSIX grants the union of *all*
        // matching owning/named groups and then applies ACL_MASK.  Emit their
        // masked ALLOW ACEs without per-group DENY entries, otherwise the
        // first matching group would incorrectly block a later group's grant.
        let allowed = posix_to_nfs4_minimal_dac_rights(perm);
        let denied =
            (ACE4_READ_DATA | ACE4_WRITE_DATA | ACE4_APPEND_DATA | ACE4_EXECUTE) & !allowed;
        if denied != 0 {
            push_acl4_ace(out, ACE4_ACCESS_DENIED, flags, denied, &who)?;
            *count = count.checked_add(1).ok_or(VfsError::NoMemory)?;
        }
        push_acl4_ace(out, ACE4_ACCESS_ALLOWED, flags, allowed, &who)?;
        *count = count.checked_add(1).ok_or(VfsError::NoMemory)?;
    }
    Ok(())
}
fn nfs4_acl_from_prepared(
    mount: &NfsMount,
    access: Option<&[u8]>,
    default_acl: Option<&[u8]>,
) -> VfsResult<Option<Vec<u8>>> {
    if access.is_none() && default_acl.is_none() {
        return Ok(None);
    }
    // NFSv4 has no POSIX default-ACL object.  Inheritable ACEs are only safe
    // when a server advertises a proof-specific inheritance contract; the
    // generic ACLSUPPORT bits prove ALLOW/DENY only, not inheritance.
    if default_acl.is_some() {
        return Err(VfsError::OperationNotSupported);
    }
    let _ = nfs_vfs(mount.acl_capabilities())?;
    let mut body = Vec::new();
    body.try_reserve(32).map_err(|_| VfsError::NoMemory)?;
    let mut count = 0;
    append_posix_acl4(
        mount,
        access.ok_or(VfsError::InvalidInput)?,
        false,
        &mut body,
        &mut count,
    )?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve(4usize.checked_add(body.len()).ok_or(VfsError::NoMemory)?)
        .map_err(|_| VfsError::NoMemory)?;
    encoded.extend_from_slice(&count.to_be_bytes());
    encoded.extend_from_slice(&body);
    Ok(Some(encoded))
}
fn nfs_type(kind: u32) -> NodeType {
    match kind {
        2 => NodeType::Directory,
        5 => NodeType::Symlink,
        3 => NodeType::BlockDevice,
        4 => NodeType::CharacterDevice,
        6 => NodeType::Socket,
        7 => NodeType::Fifo,
        _ => NodeType::RegularFile,
    }
}
fn nfs_metadata(mount: &NfsMount, attr: NfsAttr) -> VfsResult<Metadata> {
    let uid = nfs_vfs(mount.owner_to_uid(&attr.owner))?;
    let gid = nfs_vfs(mount.group_to_gid(&attr.owner_group))?;
    Ok(Metadata {
        device: 0,
        inode: attr.fileid,
        nlink: attr.nlink as u64,
        mode: NodePermission::from_bits_truncate((attr.mode & 0o7777) as u16),
        node_type: nfs_type(attr.kind),
        uid,
        gid,
        project_id: 0,
        size: attr.size,
        block_size: 4096,
        blocks: (attr.size + 511) / 512,
        rdev: Default::default(),
        atime: Timestamp::ZERO,
        btime: Timestamp::ZERO,
        mtime: Timestamp::ZERO,
        ctime: Timestamp::ZERO,
    })
}

pub struct NfsFilesystem {
    mount: Arc<NfsMount>,
    root: Mutex<Option<DirEntry>>,
    self_ref: Mutex<Option<Weak<NfsFilesystem>>>,
    nodes: Mutex<Vec<Weak<NfsNode>>>,
    node_data: Mutex<Vec<(FileHandle, u64, u64, u64, Arc<NodeUserData>)>>,
}
impl NfsFilesystem {
    pub fn mount(mount: Arc<NfsMount>) -> NfsResult<Filesystem> {
        mount.install_self_ref(&mount);
        let root_fh = mount.root_filehandle()?;
        let attr = mount.root_attrs()?;
        if nfs_type(attr.kind) != NodeType::Directory {
            return Err(NfsError::Malformed);
        }
        let fs = Arc::try_new(Self {
            mount,
            root: Mutex::new(None),
            self_ref: Mutex::new(None),
            nodes: Mutex::new(Vec::new()),
            node_data: Mutex::new(Vec::new()),
        })
        .map_err(|_| NfsError::Transport)?;
        *fs.self_ref.lock() = Some(Arc::downgrade(&fs));
        let lease_seconds = attr.lease_time;
        let root = fs
            .entry_for(root_fh, attr, Reference::root(), None, None)
            .map_err(|_| NfsError::Transport)?;
        *fs.root.lock() = Some(root);
        if let Err(error) = fs
            .mount
            .start_lease_worker(lease_seconds)
            .and_then(|_| fs.mount.start_recall_worker())
        {
            fs.mount.set_callback_listener_alive(false);
            return Err(error);
        }
        match Filesystem::try_new(fs.clone()) {
            Ok(filesystem) => Ok(filesystem),
            Err(_) => {
                fs.mount.set_callback_listener_alive(false);
                Err(NfsError::Transport)
            }
        }
    }
    fn entry_for(
        self: &Arc<Self>,
        fh: FileHandle,
        attr: NfsAttr,
        reference: Reference,
        parent: Option<FileHandle>,
        name: Option<FsNameBuf>,
    ) -> VfsResult<DirEntry> {
        let node_type = nfs_type(attr.kind);
        let user_data = {
            let mut data = self.node_data.lock();
            if let Some((_, _, _, _, cell)) =
                data.iter().find(|(known, major, minor, fileid, _)| {
                    *known == fh
                        && *major == attr.fsid_major
                        && *minor == attr.fsid_minor
                        && *fileid == attr.fileid
                })
            {
                cell.clone()
            } else {
                let cell = Arc::try_new(NodeUserData::default()).map_err(|_| VfsError::NoMemory)?;
                data.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                data.push((
                    fh.clone(),
                    attr.fsid_major,
                    attr.fsid_minor,
                    attr.fileid,
                    cell.clone(),
                ));
                cell
            }
        };
        let node = Arc::try_new(NfsNode {
            fs: self.clone(),
            fh,
            attr: Mutex::new(attr),
            entry: Mutex::new(None),
            parent,
            name,
            epoch: AtomicU64::new(1),
            user_data,
        })
        .map_err(|_| VfsError::NoMemory)?;
        self.nodes.lock().push(Arc::downgrade(&node));
        if node_type == NodeType::Directory {
            Ok(DirEntry::new_dir(
                |weak| {
                    *node.entry.lock() = Some(weak);
                    DirNode::new(node)
                },
                reference,
            ))
        } else {
            Ok(DirEntry::new_file(
                FileNode::new(node),
                node_type,
                reference,
            ))
        }
    }
    fn live(&self) {
        self.nodes.lock().retain(|node| node.strong_count() != 0);
        self.node_data
            .lock()
            .retain(|(_, _, _, _, cell)| Arc::strong_count(cell) > 1)
    }
}
impl FilesystemOps for NfsFilesystem {
    fn name(&self) -> &str {
        "nfs4"
    }
    fn root_dir(&self) -> DirEntry {
        self.root.lock().clone().expect("published NFS root")
    }
    fn stat(&self) -> VfsResult<StatFs> {
        let attr = nfs_vfs(self.mount.statfs())?;
        Ok(StatFs {
            fs_type: 0x6969,
            block_size: 4096,
            blocks: attr.space_total / 4096,
            blocks_free: attr.space_free / 4096,
            blocks_available: attr.space_avail / 4096,
            file_count: 0,
            free_file_count: 0,
            name_length: attr.max_name,
            fragment_size: 4096,
            mount_flags: 0,
        })
    }
    fn flush(&self) -> VfsResult<()> {
        let root = nfs_vfs(self.mount.root_filehandle())?;
        nfs_vfs(self.mount.commit_pending(&root))
    }
    fn unmount(&self) {
        self.mount.stop_lease_worker();
        self.root.lock().take();
        self.live();
        self.mount.set_callback_listener_alive(false)
    }
}
impl Drop for NfsFilesystem {
    fn drop(&mut self) {
        // A reader retains the transport while blocked in TCP receive.  The
        // final filesystem-owner edge must therefore close it even when VFS
        // teardown reaches Drop without a separate unmount callback.
        self.mount.set_callback_listener_alive(false);
    }
}
struct NfsNode {
    fs: Arc<NfsFilesystem>,
    fh: FileHandle,
    attr: Mutex<NfsAttr>,
    entry: Mutex<Option<WeakDirEntry>>,
    parent: Option<FileHandle>,
    name: Option<FsNameBuf>,
    epoch: AtomicU64,
    user_data: Arc<NodeUserData>,
}
impl NfsNode {
    fn parent_entry(&self) -> VfsResult<DirEntry> {
        self.entry
            .lock()
            .as_ref()
            .and_then(WeakDirEntry::upgrade)
            .ok_or(VfsError::NotFound)
    }
    fn refresh(&self) -> VfsResult<NfsAttr> {
        let attr = if let (Some(parent), Some(name)) = (&self.parent, &self.name) {
            nfs_vfs(self.fs.mount.lookup_attrs(Some(parent), name))?.1
        } else {
            nfs_vfs(self.fs.mount.root_attrs())?
        };
        *self.attr.lock() = attr.clone();
        Ok(attr)
    }
    /// Compare the server's opaque handle, not a change attribute or VFS
    /// dentry pointer, before publishing a prepared namespace mutation.
    fn expect_child(&self, name: &FsName, expected: &DirEntry) -> VfsResult<()> {
        let expected = expected
            .downcast::<NfsNode>()
            .map_err(|_| VfsError::CrossesDevices)?;
        if !Arc::ptr_eq(&self.fs, &expected.fs) {
            return Err(VfsError::CrossesDevices);
        }
        let actual = nfs_vfs(self.fs.mount.lookup(Some(&self.fh), name))?;
        if actual != expected.fh {
            return Err(VfsError::NotFound);
        }
        Ok(())
    }
    fn prepared_created_entry(
        &self,
        options: &NamedCreateOptions,
        handle: &FileHandle,
        attr: NfsAttr,
        name: &FsName,
    ) -> VfsResult<DirEntry> {
        let result = (|| {
            let parent = self.parent_entry()?;
            let fs = self
                .fs
                .self_ref
                .lock()
                .as_ref()
                .and_then(Weak::upgrade)
                .ok_or(VfsError::Io)?;
            fs.entry_for(
                handle.clone(),
                attr,
                Reference::try_new(Some(parent), name)?,
                Some(self.fh.clone()),
                Some(name.to_owned()),
            )
        })();
        match result {
            Ok(entry) => match options.install_initial_data(&entry) {
                Ok(()) => Ok(entry),
                Err(error) => {
                    self.fs.mount.rollback_created(&self.fh, name, handle);
                    Err(error)
                }
            },
            Err(error) => {
                self.fs.mount.rollback_created(&self.fh, name, handle);
                Err(error)
            }
        }
    }
}
impl NodeOps for NfsNode {
    fn inode(&self) -> u64 {
        self.attr.lock().fileid
    }
    fn object_key(&self) -> ObjectKey {
        let attr = self.attr.lock();
        ObjectKey::new(
            nfs_filesystem_identity(&attr),
            attr.fileid,
            nfs_filehandle_generation(&self.fh),
        )
    }
    fn metadata(&self) -> VfsResult<Metadata> {
        nfs_metadata(&self.fs.mount, self.refresh()?)
    }
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        nfs_vfs(self.fs.mount.setattr(
            &self.fh,
            [0; 16],
            update.mode.map(|m| m.bits() as u32),
            None,
        ))?;
        if let Some((uid, gid)) = update.owner {
            nfs_vfs(self.fs.mount.setattr_owner(&self.fh, [0; 16], uid, gid))?;
        }
        self.epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn filesystem(&self) -> &dyn FilesystemOps {
        self.fs.as_ref()
    }
    fn sync(&self, _: bool) -> VfsResult<()> {
        nfs_vfs(self.fs.mount.commit_pending(&self.fh))
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(self.user_data.as_ref())
    }
    fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
        Some(self)
    }
    fn quota_ops(&self) -> Option<&dyn QuotaOps> {
        Some(self)
    }
}
impl QuotaOps for NfsNode {
    fn quota_usage(&self) -> VfsResult<QuotaUsage> {
        let quota = nfs_vfs(self.fs.mount.quota(&self.fh))?;
        Ok(QuotaUsage {
            hard_available: Some(quota.hard_available),
            soft_available: Some(quota.soft_available),
            used: quota.used,
        })
    }
}
impl XattrProvider for NfsNode {
    fn get_xattr(&self, name: &[u8]) -> VfsResult<Vec<u8>> {
        let name = FsNameBuf::from_vec(name.to_vec())?;
        nfs_vfs(self.fs.mount.get_named_attr(&self.fh, &name))
    }
    fn list_xattrs(&self) -> VfsResult<Vec<u8>> {
        let names = nfs_vfs(self.fs.mount.list_named_attrs(&self.fh))?;
        let mut out = Vec::new();
        for name in names {
            out.try_reserve(name.as_bytes().len() + 1)
                .map_err(|_| VfsError::NoMemory)?;
            out.extend_from_slice(name.as_bytes());
            out.push(0)
        }
        Ok(out)
    }
    fn set_xattr(&self, name: &[u8], value: &[u8], mode: XattrSetMode) -> VfsResult<()> {
        let name = FsNameBuf::from_vec(name.to_vec())?;
        nfs_vfs(self.fs.mount.set_named_attr(&self.fh, &name, value, mode))
    }
    fn remove_xattr(&self, name: &[u8]) -> VfsResult<()> {
        let name = FsNameBuf::from_vec(name.to_vec())?;
        nfs_vfs(self.fs.mount.remove_named_attr(&self.fh, &name))
    }
}
impl Pollable for NfsNode {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }
    fn register<'a>(
        &'a self,
        _: &mut Context<'_>,
        _: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::empty()
    }
}
impl DirNodeOps for NfsNode {
    fn lookup(&self, name: &FsName) -> VfsResult<DirEntry> {
        let (fh, attr) = nfs_vfs(self.fs.mount.lookup_attrs(Some(&self.fh), name))?;
        let parent = self.parent_entry()?;
        let fs = self
            .fs
            .self_ref
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(VfsError::Io)?;
        fs.entry_for(
            fh,
            attr,
            Reference::try_new(Some(parent), name)?,
            Some(self.fh.clone()),
            Some(name.to_owned()),
        )
    }
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let reply = nfs_vfs(self.fs.mount.read_dir(&self.fh, offset, [0; 8], 64 * 1024))?;
        let mut count = 0;
        for entry in reply.entries {
            if !sink.accept(
                &entry.name,
                entry.fileid,
                nfs_type(entry.kind),
                entry.cookie,
            ) {
                break;
            }
            count += 1;
        }
        Ok(count)
    }
    fn namespace_epoch(&self) -> u64 {
        self.epoch
            .load(Ordering::Acquire)
            .max(self.fs.mount.coherency_epoch())
    }
    fn supports_named_create(&self, node_type: NodeType) -> bool {
        matches!(node_type, NodeType::Directory | NodeType::RegularFile)
    }
    fn supports_symlink(&self) -> bool {
        true
    }
    fn create_symlink(
        &self,
        name: &FsName,
        target: &axfs_ng_vfs::FsPath,
        permission: NodePermission,
        user: Option<(u32, u32)>,
    ) -> VfsResult<DirEntry> {
        let owner = match user
            .map(|(uid, _)| self.fs.mount.uid_to_owner(uid))
            .transpose()
        {
            Ok(owner) => owner,
            Err(_) => return Err(VfsError::Io),
        };
        let group = match user
            .map(|(_, gid)| self.fs.mount.gid_to_group(gid))
            .transpose()
        {
            Ok(group) => group,
            Err(_) => return Err(VfsError::Io),
        };
        let fh = nfs_vfs(self.fs.mount.create_symlink_with_attrs(
            &self.fh,
            name,
            target,
            permission.bits() as u32,
            owner.as_deref(),
            group.as_deref(),
            None,
        ))?;
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let attr = nfs_vfs(self.fs.mount.attrs(&fh))?;
        let parent = self.parent_entry()?;
        let fs = self
            .fs
            .self_ref
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(VfsError::Io)?;
        fs.entry_for(
            fh,
            attr,
            Reference::try_new(Some(parent), name)?,
            Some(self.fh.clone()),
            Some(name.to_owned()),
        )
    }
    fn create_symlink_prepared(
        &self,
        name: &FsName,
        target: &axfs_ng_vfs::FsPath,
        options: &NamedCreateOptions,
    ) -> VfsResult<DirEntry> {
        if options.node_type != NodeType::Symlink
            || options.initial_attributes.project_id.is_some()
            || options.initial_attributes.project_inherit
        {
            return Err(VfsError::OperationNotSupported);
        }
        let acl = nfs4_acl_from_prepared(
            &self.fs.mount,
            options.initial_attributes.access_acl.as_deref(),
            options.initial_attributes.default_acl.as_deref(),
        )?;
        let owner = options
            .owner
            .map(|(uid, _)| self.fs.mount.uid_to_owner(uid))
            .transpose()
            .map_err(|_| VfsError::Io)?;
        let group = options
            .owner
            .map(|(_, gid)| self.fs.mount.gid_to_group(gid))
            .transpose()
            .map_err(|_| VfsError::Io)?;
        let fh = nfs_vfs(self.fs.mount.create_symlink_with_attrs(
            &self.fh,
            name,
            target,
            options.permission.bits() as u32,
            owner.as_deref(),
            group.as_deref(),
            acl.as_deref(),
        ))?;
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let attr = nfs_vfs(self.fs.mount.attrs(&fh))?;
        let parent = self.parent_entry()?;
        let fs = self
            .fs
            .self_ref
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(VfsError::Io)?;
        fs.entry_for(
            fh,
            attr,
            Reference::try_new(Some(parent), name)?,
            Some(self.fh.clone()),
            Some(name.to_owned()),
        )
    }
    fn supports_unlink(&self) -> bool {
        true
    }
    fn supports_rmdir(&self) -> bool {
        true
    }
    fn supports_rename(&self) -> bool {
        true
    }
    fn create_named(
        &self,
        name: &FsName,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        // NFSv4 has no project-id fattr. Reject only an actually requested
        // inherited project, rather than hiding all remote creates.
        if options.initial_attributes.project_id.is_some()
            || options.initial_attributes.project_inherit
        {
            return Err(VfsError::OperationNotSupported);
        }
        let acl = nfs4_acl_from_prepared(
            &self.fs.mount,
            options.initial_attributes.access_acl.as_deref(),
            options.initial_attributes.default_acl.as_deref(),
        )?;
        if disposition == CreateDisposition::OpenOrCreate
            && options.node_type == NodeType::RegularFile
        {
            let owner = options
                .owner
                .map(|(uid, _)| self.fs.mount.uid_to_owner(uid))
                .transpose()
                .map_err(|_| VfsError::Io)?;
            let group = options
                .owner
                .map(|(_, gid)| self.fs.mount.gid_to_group(gid))
                .transpose()
                .map_err(|_| VfsError::Io)?;
            let (open, created, attr) = nfs_vfs(self.fs.mount.open_or_create_file_with_attrs(
                &self.fh,
                name,
                options.permission.bits() as u32,
                3,
                owner.as_deref(),
                group.as_deref(),
                acl.as_deref(),
            ))?;
            let fh = open.handle.clone();
            if let Err(error) = nfs_vfs(self.fs.mount.close(&open)) {
                if created {
                    self.fs.mount.rollback_created(&self.fh, name, &fh);
                }
                return Err(error);
            };
            self.epoch.fetch_add(1, Ordering::AcqRel);
            if created {
                let entry = self.prepared_created_entry(options, &fh, attr, name)?;
                return Ok(CreateOutcome {
                    entry,
                    created: true,
                });
            }
            let parent = self.parent_entry()?;
            let fs = self
                .fs
                .self_ref
                .lock()
                .as_ref()
                .and_then(Weak::upgrade)
                .ok_or(VfsError::Io)?;
            return Ok(CreateOutcome {
                entry: fs.entry_for(
                    fh,
                    attr,
                    Reference::try_new(Some(parent), name)?,
                    Some(self.fh.clone()),
                    Some(name.to_owned()),
                )?,
                created: false,
            });
        }
        if disposition == CreateDisposition::OpenOrCreate
            && options.node_type == NodeType::Directory
        {
            let owner = options
                .owner
                .map(|(uid, _)| self.fs.mount.uid_to_owner(uid))
                .transpose()
                .map_err(|_| VfsError::Io)?;
            let group = options
                .owner
                .map(|(_, gid)| self.fs.mount.gid_to_group(gid))
                .transpose()
                .map_err(|_| VfsError::Io)?;
            // NFS CREATE has no "open existing directory" arm.  Linearize
            // by attempting CREATE first; only its definitive EXIST result
            // admits a LOOKUP, and a concurrent removal restarts CREATE.
            for _ in 0..8 {
                match self.fs.mount.create_dir_with_attrs(
                    &self.fh,
                    name,
                    options.permission.bits() as u32,
                    owner.as_deref(),
                    group.as_deref(),
                    acl.as_deref(),
                ) {
                    Ok(fh) => {
                        let attr = match nfs_vfs(self.fs.mount.attrs(&fh)) {
                            Ok(attr) => attr,
                            Err(error) => {
                                self.fs.mount.rollback_created(&self.fh, name, &fh);
                                return Err(error);
                            }
                        };
                        self.epoch.fetch_add(1, Ordering::AcqRel);
                        let entry = self.prepared_created_entry(options, &fh, attr, name)?;
                        return Ok(CreateOutcome {
                            entry,
                            created: true,
                        });
                    }
                    Err(NfsError::Status(17)) => {
                        match self.fs.mount.lookup_attrs(Some(&self.fh), name) {
                            Ok((fh, attr)) => {
                                let parent = self.parent_entry()?;
                                let fs = self
                                    .fs
                                    .self_ref
                                    .lock()
                                    .as_ref()
                                    .and_then(Weak::upgrade)
                                    .ok_or(VfsError::Io)?;
                                return Ok(CreateOutcome {
                                    entry: fs.entry_for(
                                        fh,
                                        attr,
                                        Reference::try_new(Some(parent), name)?,
                                        Some(self.fh.clone()),
                                        Some(name.to_owned()),
                                    )?,
                                    created: false,
                                });
                            }
                            Err(NfsError::Status(2)) => continue,
                            Err(error) => return nfs_vfs(Err(error)),
                        }
                    }
                    Err(error) => return nfs_vfs(Err(error)),
                }
            }
            return Err(VfsError::WouldBlock);
        }
        if disposition == CreateDisposition::OpenOrCreate {
            match self.fs.mount.lookup_attrs(Some(&self.fh), name) {
                Ok((fh, attr)) => {
                    let parent = self.parent_entry()?;
                    let fs = self
                        .fs
                        .self_ref
                        .lock()
                        .as_ref()
                        .and_then(Weak::upgrade)
                        .ok_or(VfsError::Io)?;
                    return Ok(CreateOutcome {
                        entry: fs.entry_for(
                            fh,
                            attr,
                            Reference::try_new(Some(parent), name)?,
                            Some(self.fh.clone()),
                            Some(name.to_owned()),
                        )?,
                        created: false,
                    });
                }
                Err(NfsError::Status(2)) => {}
                Err(_) => return Err(VfsError::Io),
            }
        }
        let created = match options.node_type {
            NodeType::Directory => {
                let owner = options
                    .owner
                    .map(|(uid, _)| self.fs.mount.uid_to_owner(uid))
                    .transpose()
                    .map_err(|_| VfsError::Io)?;
                let group = options
                    .owner
                    .map(|(_, gid)| self.fs.mount.gid_to_group(gid))
                    .transpose()
                    .map_err(|_| VfsError::Io)?;
                self.fs.mount.create_dir_with_attrs(
                    &self.fh,
                    name,
                    options.permission.bits() as u32,
                    owner.as_deref(),
                    group.as_deref(),
                    acl.as_deref(),
                )
            }
            NodeType::RegularFile => {
                let owner = match options
                    .owner
                    .map(|(uid, _)| self.fs.mount.uid_to_owner(uid))
                    .transpose()
                {
                    Ok(owner) => owner,
                    Err(_) => return Err(VfsError::Io),
                };
                let group = match options
                    .owner
                    .map(|(_, gid)| self.fs.mount.gid_to_group(gid))
                    .transpose()
                {
                    Ok(group) => group,
                    Err(_) => return Err(VfsError::Io),
                };
                self.fs
                    .mount
                    .create_file_with_acl_attrs(
                        &self.fh,
                        name,
                        options.permission.bits() as u32,
                        3,
                        owner.as_deref(),
                        group.as_deref(),
                        acl.as_deref(),
                    )
                    .and_then(|open| {
                        self.fs.mount.close(&open)?;
                        Ok(open.handle)
                    })
            }
            _ => return Err(VfsError::OperationNotSupported),
        };
        let fh = match created {
            Ok(fh) => fh,
            Err(NfsError::Status(17)) if disposition == CreateDisposition::OpenOrCreate => {
                let (fh, attr) = nfs_vfs(self.fs.mount.lookup_attrs(Some(&self.fh), name))?;
                let parent = self.parent_entry()?;
                let fs = self
                    .fs
                    .self_ref
                    .lock()
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .ok_or(VfsError::Io)?;
                return Ok(CreateOutcome {
                    entry: fs.entry_for(
                        fh,
                        attr,
                        Reference::try_new(Some(parent), name)?,
                        Some(self.fh.clone()),
                        Some(name.to_owned()),
                    )?,
                    created: false,
                });
            }
            Err(_) => return Err(VfsError::Io),
        };
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let attr = match nfs_vfs(self.fs.mount.attrs(&fh)) {
            Ok(attr) => attr,
            Err(error) => {
                self.fs.mount.rollback_created(&self.fh, name, &fh);
                return Err(error);
            }
        };
        let entry = self.prepared_created_entry(options, &fh, attr, name)?;
        Ok(CreateOutcome {
            entry,
            created: true,
        })
    }
    fn link(&self, name: &FsName, node: &DirEntry) -> VfsResult<DirEntry> {
        let target = node
            .downcast::<NfsNode>()
            .map_err(|_| VfsError::CrossesDevices)?;
        if !Arc::ptr_eq(&self.fs, &target.fs) {
            return Err(VfsError::CrossesDevices);
        }
        // LINK's SAVEFH/PUTFH sequence returns the linked object's FATTR4 in
        // the same namespace transaction.  Reuse the opaque filehandle; a
        // pathname lookup here would be a second, racy operation.
        let attr = nfs_vfs(self.fs.mount.link(&self.fh, name, &target.fh))?;
        *target.attr.lock() = attr.clone();
        target.epoch.fetch_add(1, Ordering::AcqRel);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let parent = self.parent_entry()?;
        let fs = self
            .fs
            .self_ref
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(VfsError::Io)?;
        fs.entry_for(
            target.fh.clone(),
            attr,
            Reference::try_new(Some(parent), name)?,
            Some(self.fh.clone()),
            Some(name.to_owned()),
        )
    }
    fn unlink(&self, request: UnlinkRequest<'_>) -> VfsResult<()> {
        let _namespace = self.fs.mount.namespace_ops.lock();
        let (actual, attr) = nfs_vfs(self.fs.mount.lookup_attrs(Some(&self.fh), request.name))?;
        if request.is_dir != (nfs_type(attr.kind) == NodeType::Directory) {
            return Err(if request.is_dir {
                VfsError::NotADirectory
            } else {
                VfsError::IsADirectory
            });
        }
        if let Some(expected) = request.expected {
            let expected = expected
                .downcast::<NfsNode>()
                .map_err(|_| VfsError::CrossesDevices)?;
            if !Arc::ptr_eq(&self.fs, &expected.fs) {
                return Err(VfsError::CrossesDevices);
            }
            if actual != expected.fh {
                return Err(VfsError::NotFound);
            }
            let expected_attr = expected.attr.lock().clone();
            nfs_vfs(
                self.fs
                    .mount
                    .remove_verified(&self.fh, request.name, &expected_attr),
            )?;
        } else {
            nfs_vfs(self.fs.mount.remove(&self.fh, request.name))?;
        }
        self.epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn rename(&self, request: RenameRequest<'_>) -> VfsResult<()> {
        let dst = request
            .dst_dir
            .downcast::<NfsNode>()
            .map_err(|_| VfsError::CrossesDevices)?;
        let source = request
            .src
            .downcast::<NfsNode>()
            .map_err(|_| VfsError::CrossesDevices)?;
        if !Arc::ptr_eq(&self.fs, &dst.fs) || !Arc::ptr_eq(&self.fs, &source.fs) {
            return Err(VfsError::CrossesDevices);
        }
        let _namespace = self.fs.mount.namespace_ops.lock();
        self.expect_child(request.src_name, request.src)?;
        let source_attr = source.attr.lock().clone();
        let destination = match request.dst {
            Some(expected) => {
                dst.expect_child(request.dst_name, expected)?;
                let expected = expected
                    .downcast::<NfsNode>()
                    .map_err(|_| VfsError::CrossesDevices)?;
                Some(expected.attr.lock().clone())
            }
            None => None,
        };
        let target_before = if destination.is_none() {
            Some(nfs_vfs(self.fs.mount.attrs(&dst.fh))?.change)
        } else {
            None
        };
        let change = nfs_vfs(self.fs.mount.rename_verified(
            &self.fh,
            request.src_name,
            &source_attr,
            &dst.fh,
            request.dst_name,
            destination.as_ref(),
            target_before,
        ))?;
        if let Some(before) = target_before {
            if !change.target_atomic
                || change.target_before != before
                || change.target_after == before
            {
                return Err(VfsError::WouldBlock);
            }
        }
        self.epoch.fetch_add(1, Ordering::AcqRel);
        dst.epoch.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}
impl FileNodeOps for NfsNode {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if nfs_type(self.attr.lock().kind) != NodeType::Symlink {
            return Err(VfsError::BadFileDescriptor);
        }
        let target = nfs_vfs(self.fs.mount.readlink(&self.fh))?;
        let start = (offset as usize).min(target.len());
        let count = buf.len().min(target.len() - start);
        buf[..count].copy_from_slice(&target[start..start + count]);
        Ok(count)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::BadFileDescriptor)
    }
    fn append(&self, _: &[u8]) -> VfsResult<(usize, u64)> {
        Err(VfsError::BadFileDescriptor)
    }
    fn set_len(&self, len: u64) -> VfsResult<()> {
        nfs_vfs(self.fs.mount.setattr(&self.fh, [0; 16], None, Some(len)))
    }
    fn set_symlink(&self, _: &axfs_ng_vfs::FsPath) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }
    fn open_handle(
        &self,
        read: bool,
        write: bool,
        _flags: u32,
    ) -> VfsResult<Option<Arc<dyn FileNodeOps>>> {
        let parent = self
            .parent
            .as_ref()
            .ok_or(VfsError::OperationNotSupported)?;
        let name = self.name.as_ref().ok_or(VfsError::OperationNotSupported)?;
        let share_access = (if read { 1 } else { 0 }) | (if write { 2 } else { 0 });
        let state = nfs_vfs(self.fs.mount.open(parent, name, share_access, 0))?;
        let handle = Arc::try_new(NfsOpenFile {
            fs: self.fs.clone(),
            state,
            attr: Mutex::new(self.attr.lock().clone()),
            read,
            write,
            locks: Mutex::new(Vec::new()),
        })
        .map_err(|_| VfsError::NoMemory)?;
        Ok(Some(handle))
    }
}
struct NfsOpenFile {
    fs: Arc<NfsFilesystem>,
    state: OpenState,
    attr: Mutex<NfsAttr>,
    read: bool,
    write: bool,
    locks: Mutex<Vec<(u64, FileLock, LockState)>>,
}
fn checked_server_transfer(returned: usize, requested: usize) -> VfsResult<usize> {
    if returned <= requested {
        Ok(returned)
    } else {
        Err(VfsError::Io)
    }
}
impl NodeOps for NfsOpenFile {
    fn inode(&self) -> u64 {
        self.attr.lock().fileid
    }
    fn object_key(&self) -> ObjectKey {
        let attr = self.attr.lock();
        ObjectKey::new(
            nfs_filesystem_identity(&attr),
            attr.fileid,
            nfs_filehandle_generation(&self.state.handle),
        )
    }
    fn metadata(&self) -> VfsResult<Metadata> {
        nfs_metadata(&self.fs.mount, self.attr.lock().clone())
    }
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        let stateid = nfs_vfs(self.fs.mount.current_open_stateid(&self.state))?;
        nfs_vfs(self.fs.mount.setattr(
            &self.state.handle,
            stateid,
            update.mode.map(|m| m.bits() as u32),
            None,
        ))?;
        if let Some((uid, gid)) = update.owner {
            nfs_vfs(
                self.fs
                    .mount
                    .setattr_owner(&self.state.handle, stateid, uid, gid),
            )?;
        }
        Ok(())
    }
    fn filesystem(&self) -> &dyn FilesystemOps {
        self.fs.as_ref()
    }
    fn sync(&self, _: bool) -> VfsResult<()> {
        nfs_vfs(self.fs.mount.commit_pending(&self.state.handle))
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}
impl Pollable for NfsOpenFile {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }
    fn register<'a>(
        &'a self,
        _: &mut Context<'_>,
        _: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::empty()
    }
}
impl FileNodeOps for NfsOpenFile {
    fn nowait_read_admit(&self, _offset: u64, _length: usize) -> VfsResult<NowaitAdmission> {
        // A free session slot does not make the synchronous RPC reply ready.
        Ok(NowaitAdmission::WouldBlock)
    }
    fn nowait_write_admit(&self, _offset: u64, _length: usize) -> VfsResult<NowaitAdmission> {
        Ok(NowaitAdmission::WouldBlock)
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if !self.read {
            return Err(VfsError::BadFileDescriptor);
        }
        let requested = buf.len().min(u32::MAX as usize);
        let stateid = nfs_vfs(self.fs.mount.current_open_stateid(&self.state))?;
        let reply = nfs_vfs(self.fs.mount.read(
            &self.state.handle,
            stateid,
            offset,
            requested as u32,
        ))?;
        let count = checked_server_transfer(reply.data.len(), requested)?;
        buf[..count].copy_from_slice(&reply.data);
        Ok(count)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if !self.write {
            return Err(VfsError::BadFileDescriptor);
        }
        let stateid = nfs_vfs(self.fs.mount.current_open_stateid(&self.state))?;
        let result = nfs_vfs(self.fs.mount.write(
            &self.state.handle,
            stateid,
            offset,
            StableHow::Unstable,
            buf,
        ))?;
        checked_server_transfer(result.count as usize, buf.len())
    }
    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        if !self.write {
            return Err(VfsError::BadFileDescriptor);
        }
        let offset = self.attr.lock().size;
        let count = self.write_at(buf, offset)?;
        self.attr.lock().size = offset.saturating_add(count as u64);
        Ok((count, offset.saturating_add(count as u64)))
    }
    fn set_len(&self, len: u64) -> VfsResult<()> {
        if !self.write {
            return Err(VfsError::BadFileDescriptor);
        }
        let stateid = nfs_vfs(self.fs.mount.current_open_stateid(&self.state))?;
        nfs_vfs(
            self.fs
                .mount
                .setattr(&self.state.handle, stateid, None, Some(len)),
        )
    }
    fn set_symlink(&self, _: &axfs_ng_vfs::FsPath) -> VfsResult<()> {
        Err(VfsError::OperationNotSupported)
    }
    fn release_handle(&self) -> VfsResult<()> {
        nfs_vfs(self.fs.mount.commit_pending(&self.state.handle))?;
        let mut held = self.locks.lock();
        while let Some((_owner, lock, state)) = held.last().cloned() {
            let length = if lock.end == u64::MAX {
                0
            } else {
                lock.end.saturating_sub(lock.start).saturating_add(1)
            };
            nfs_vfs(self.fs.mount.unlock(
                &self.state.handle,
                &state,
                lock.start,
                length,
                lock.kind == 1,
            ))?;
            held.pop();
        }
        nfs_vfs(self.fs.mount.close(&self.state))
    }
}
impl LockOps for NfsOpenFile {
    fn get_lock(&self, _owner: u64, lock: FileLock) -> VfsResult<FileLock> {
        let length = if lock.end == u64::MAX {
            0
        } else {
            lock.end
                .checked_sub(lock.start)
                .and_then(|n| n.checked_add(1))
                .ok_or(VfsError::InvalidInput)?
        };
        match self
            .fs
            .mount
            .test_lock(&self.state.handle, lock.start, length, lock.kind == 1)
        {
            Ok(()) => Ok(FileLock { kind: 2, ..lock }),
            Err(NfsError::Denied {
                offset,
                length,
                write,
                ..
            }) => Ok(FileLock {
                start: offset,
                end: if length == 0 {
                    u64::MAX
                } else {
                    offset.saturating_add(length - 1)
                },
                kind: if write { 1 } else { 0 },
                pid: 0,
            }),
            Err(error) => nfs_vfs(Err(error)),
        }
    }
    fn set_lock(&self, owner: u64, lock: FileLock, _wait: bool) -> VfsResult<()> {
        let length = if lock.end == u64::MAX {
            0
        } else {
            lock.end
                .checked_sub(lock.start)
                .and_then(|n| n.checked_add(1))
                .ok_or(VfsError::InvalidInput)?
        };
        let mut held = self.locks.lock();
        if lock.kind == 2 {
            for index in (0..held.len()).rev() {
                let (existing_owner, existing, state) = &held[index];
                if *existing_owner == owner
                    && existing.start == lock.start
                    && existing.end == lock.end
                {
                    nfs_vfs(self.fs.mount.unlock(
                        &self.state.handle,
                        state,
                        lock.start,
                        length,
                        existing.kind == 1,
                    ))?;
                    held.remove(index);
                }
            }
            return Ok(());
        }
        let state = nfs_vfs(
            self.fs
                .mount
                .lock(&self.state, lock.start, length, lock.kind == 1),
        )?;
        held.push((owner, lock, state));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small scripted transport for protocol-state tests.  It records the
    /// complete record rather than decoded operations, which is the property
    /// a surviving-session replay must preserve.
    struct ScriptTransport {
        calls: Arc<Mutex<Vec<Vec<u8>>>>,
        replies: Arc<Mutex<Vec<NfsResult<Vec<u8>>>>>,
    }

    impl ScriptTransport {
        fn new(replies: Vec<NfsResult<Vec<u8>>>) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                replies: Arc::new(Mutex::new(replies)),
            }
        }
    }

    impl RpcTransport for ScriptTransport {
        fn call(&self, record: &[u8]) -> NfsResult<Vec<u8>> {
            self.calls.lock().push(record.to_vec());
            self.replies
                .lock()
                .remove(0)
                .map(|body| rpc_reply(record, body))
        }

        fn cancel(&self, _xid: u32) {}

        fn reconnect(&self, _callback: Weak<NfsMount>) -> NfsResult<Arc<dyn RpcTransport>> {
            Ok(Arc::new(Self {
                calls: self.calls.clone(),
                replies: self.replies.clone(),
            }))
        }
    }

    fn request_xid(record: &[u8]) -> u32 {
        let call = decode_record(record).unwrap();
        u32::from_be_bytes(call[..4].try_into().unwrap())
    }

    fn request_opcode(record: &[u8]) -> u32 {
        let call = decode_record(record).unwrap();
        let mut xdr = XdrIn::new(&call);
        for _ in 0..6 {
            xdr.u32().unwrap();
        }
        // Both AUTH_NONE and AUTH_SYS use XDR opaque authentication bodies;
        // parse their variable lengths instead of assuming a fixed header.
        xdr.u32().unwrap();
        xdr.opaque().unwrap();
        xdr.u32().unwrap();
        xdr.opaque().unwrap();
        xdr.opaque().unwrap(); // COMPOUND tag
        xdr.u32().unwrap(); // minorversion
        xdr.u32().unwrap(); // argarray count
        xdr.u32().unwrap()
    }

    fn request_sequence_tail_opcode(record: &[u8]) -> u32 {
        let call = decode_record(record).unwrap();
        let mut xdr = XdrIn::new(&call);
        for _ in 0..6 {
            xdr.u32().unwrap();
        }
        xdr.u32().unwrap();
        xdr.opaque().unwrap();
        xdr.u32().unwrap();
        xdr.opaque().unwrap();
        xdr.opaque().unwrap();
        xdr.u32().unwrap();
        assert_eq!(xdr.u32().unwrap(), 2);
        assert_eq!(xdr.u32().unwrap(), OP_SEQUENCE);
        xdr.take(16).unwrap();
        for _ in 0..4 {
            xdr.u32().unwrap();
        }
        xdr.u32().unwrap()
    }

    fn rpc_reply(request: &[u8], body: Vec<u8>) -> Vec<u8> {
        let mut rpc = Xdr::default();
        rpc.u32(request_xid(request));
        rpc.u32(RPC_REPLY);
        rpc.u32(RPC_ACCEPTED);
        rpc.u32(AUTH_NONE);
        rpc.u32(0);
        rpc.u32(RPC_SUCCESS);
        rpc.fixed(&body);
        let mut record = Xdr::default();
        record.u32(0x8000_0000 | rpc.0.len() as u32);
        record.fixed(&rpc.0);
        record.0
    }

    fn compound_error(status: u32, opcode: u32) -> Vec<u8> {
        let mut body = Xdr::default();
        body.u32(status);
        body.opaque(b"");
        body.u32(1);
        body.u32(opcode);
        body.u32(status);
        body.0
    }

    fn top_level_delay() -> Vec<u8> {
        let mut body = Xdr::default();
        body.u32(DELAY);
        body.opaque(b"");
        body.u32(0);
        body.0
    }

    fn bind_success(sessionid: [u8; 16]) -> Vec<u8> {
        let mut body = Xdr::default();
        body.u32(NFS_OK);
        body.opaque(b"");
        body.u32(1);
        body.u32(OP_BIND_CONN_TO_SESSION);
        body.u32(NFS_OK);
        body.fixed(&sessionid);
        body.u32(3);
        body.u32(0);
        body.0
    }

    fn exchange_success(clientid: u64, sequenceid: u32) -> Vec<u8> {
        let mut body = Xdr::default();
        body.u32(NFS_OK);
        body.opaque(b"");
        body.u32(1);
        body.u32(OP_EXCHANGE_ID);
        body.u32(NFS_OK);
        body.u64(clientid);
        body.u32(sequenceid);
        body.0
    }

    fn channel_attrs(body: &mut Xdr) {
        body.u32(0);
        body.u32(4096);
        body.u32(4096);
        body.u32(4096);
        body.u32(MAX_COMPOUND_OPERATIONS as u32);
        body.u32(1);
        body.u32(0);
    }

    fn create_session_success(sessionid: [u8; 16]) -> Vec<u8> {
        let mut body = Xdr::default();
        body.u32(NFS_OK);
        body.opaque(b"");
        body.u32(1);
        body.u32(OP_CREATE_SESSION);
        body.u32(NFS_OK);
        body.fixed(&sessionid);
        body.u32(1);
        body.u32(0);
        channel_attrs(&mut body);
        channel_attrs(&mut body);
        body.0
    }

    fn sequence_success(sessionid: [u8; 16], slot: u32, sequence: u32, tail: u32) -> Vec<u8> {
        let mut body = Xdr::default();
        body.u32(NFS_OK);
        body.opaque(b"");
        body.u32(2);
        body.u32(OP_SEQUENCE);
        body.u32(NFS_OK);
        body.fixed(&sessionid);
        body.u32(sequence);
        body.u32(slot);
        body.u32(slot);
        body.u32(slot);
        body.u32(0);
        body.u32(tail);
        body.u32(NFS_OK);
        body.0
    }

    fn sequence_tail_error(
        sessionid: [u8; 16],
        slot: u32,
        sequence: u32,
        tail: u32,
        status: u32,
    ) -> Vec<u8> {
        let mut body = Xdr::default();
        body.u32(status);
        body.opaque(b"");
        body.u32(2);
        body.u32(OP_SEQUENCE);
        body.u32(NFS_OK);
        body.fixed(&sessionid);
        body.u32(sequence);
        body.u32(slot);
        body.u32(slot);
        body.u32(slot);
        body.u32(0);
        body.u32(tail);
        body.u32(status);
        body.0
    }

    fn mounted_session_with_auth(
        transport: Arc<dyn RpcTransport>,
        sessionid: [u8; 16],
        auth: RpcAuth,
    ) -> Arc<NfsMount> {
        let mount = Arc::new(NfsMount::new(transport, auth));
        mount.install_self_ref(&mount);
        *mount.session.lock() = Some(Session {
            clientid: 7,
            exchange_sequence: 1,
            id: sessionid,
            max_operations: MAX_COMPOUND_OPERATIONS,
            slots: vec![Slot {
                id: 0,
                sequence: 1,
                unusable: false,
                lifecycle: SlotLifecycle::Free,
            }],
            highest_slot: 0,
            target_highest_slot: 0,
            reclaiming: false,
            replay_barrier: false,
        });
        mount
    }

    fn mounted_session(transport: Arc<dyn RpcTransport>, sessionid: [u8; 16]) -> Arc<NfsMount> {
        mounted_session_with_auth(transport, sessionid, RpcAuth::None)
    }

    #[test]
    fn exclusive_open_create_encodes_only_create_verifier() {
        let name = FsNameBuf::from_vec(b"new".to_vec()).unwrap();
        let mut xdr = Xdr::default();
        Operation::OpenCreate {
            seqid: 7,
            clientid: 9,
            owner: 11,
            share_access: 3,
            name,
            mode: 0o600,
            attr_owner: Some(b"owner"),
            attr_group: Some(b"group"),
            attr_acl: None,
            create_how: CreateHow::Exclusive([0x5a; 8]),
        }
        .encode(&mut xdr);
        assert_eq!(&xdr.0[40..44], &2u32.to_be_bytes());
        assert_eq!(&xdr.0[44..52], &[0x5a; 8]);
        assert_eq!(&xdr.0[52..56], &0u32.to_be_bytes());
        assert_eq!(xdr.0.len(), 60);
    }

    #[test]
    fn server_transfer_cannot_exceed_requested_buffer() {
        assert_eq!(checked_server_transfer(4, 4), Ok(4));
        assert_eq!(checked_server_transfer(5, 4), Err(VfsError::Io));
    }

    #[test]
    fn compound_error_with_sequence_prefix_retires_and_reuses_slot() {
        let sessionid = [0x33; 16];
        let transport = Arc::new(ScriptTransport::new(Vec::new()));
        let mount = mounted_session(transport.clone(), sessionid);
        *transport.replies.lock() = vec![Ok(sequence_tail_error(
            sessionid,
            0,
            1,
            OP_PUTROOTFH,
            10004,
        ))];
        assert!(matches!(
            mount.compound(&[Operation::PutRootFh]),
            Err(NfsError::Status(10004))
        ));
        let session = mount.session.lock();
        let slot = &session.as_ref().unwrap().slots[0];
        assert_eq!(slot.sequence, 2);
        assert!(matches!(slot.lifecycle, SlotLifecycle::Free));
        drop(session);
        assert!(mount.nowait_rpc_admit());
    }

    #[test]
    fn top_level_delay_replays_exact_wire_and_releases_slot_once() {
        let sessionid = [0x51; 16];
        // The first reply leaves no SEQUENCE acknowledgement.  Recovery must
        // bind the replacement connection and retransmit precisely that first
        // record before it can make the slot available again.
        let transport = Arc::new(ScriptTransport::new(Vec::new()));
        let mount = mounted_session(transport.clone(), sessionid);
        let mut replies = Vec::new();
        for _ in 0..=8 {
            replies.push(Ok(top_level_delay()));
        }
        replies.push(Ok(bind_success(sessionid)));
        replies.push(Ok(sequence_success(sessionid, 0, 1, OP_PUTROOTFH)));
        *transport.replies.lock() = replies;
        // Responses are framed by the scripted transport using each call's
        // received XID, so this test also catches an accidental XID rewrite.
        let reply = mount.compound(&[Operation::PutRootFh]).unwrap();
        assert_eq!(reply.top_status, NFS_OK);
        let calls = transport.calls.lock().clone();
        assert_eq!(calls.len(), 11);
        assert_eq!(request_sequence_tail_opcode(&calls[0]), OP_PUTROOTFH);
        assert!(calls[..9].iter().all(|call| call == &calls[0]));
        assert_eq!(request_opcode(&calls[9]), OP_BIND_CONN_TO_SESSION);
        assert_eq!(calls[0], calls[10]);
        assert!(mount.nowait_rpc_admit());
        assert!(matches!(
            mount.take_replayed_reply(sessionid, 0, 1, request_xid(&calls[0])),
            Err(NfsError::SessionLost)
        ));
    }

    #[test]
    fn bind_session_failure_escalates_to_full_session_recovery() {
        let old_sessionid = [0x42; 16];
        let replacement_sessionid = [0x43; 16];
        let transport = Arc::new(ScriptTransport::new(Vec::new()));
        let options = NfsMountOptions::default();
        let mount = mounted_session_with_auth(
            transport.clone(),
            old_sessionid,
            RpcAuth::Sys(options.auth_sys.clone()),
        );
        mount.session.lock().as_mut().unwrap().replay_barrier = true;
        *mount.options.lock() = Some(options);
        *transport.replies.lock() = vec![
            Ok(compound_error(BADSESSION, OP_BIND_CONN_TO_SESSION)),
            Ok(exchange_success(9, 2)),
            Ok(create_session_success(replacement_sessionid)),
            Ok(bind_success(replacement_sessionid)),
            Ok(sequence_success(
                replacement_sessionid,
                0,
                1,
                OP_RECLAIM_COMPLETE,
            )),
        ];
        assert_eq!(mount.recover(), Ok(()));
        let calls = transport.calls.lock();
        assert_eq!(calls.len(), 5);
        assert_eq!(request_opcode(&calls[0]), OP_BIND_CONN_TO_SESSION);
        assert_eq!(request_opcode(&calls[1]), OP_EXCHANGE_ID);
        assert_eq!(request_opcode(&calls[2]), OP_CREATE_SESSION);
        assert_eq!(request_opcode(&calls[3]), OP_BIND_CONN_TO_SESSION);
        let session = mount.session.lock();
        let session = session.as_ref().unwrap();
        assert_eq!(session.id, replacement_sessionid);
        assert!(!session.replay_barrier && !session.reclaiming);
        assert!(!mount.lease_faulted.load(Ordering::Acquire));
    }
}
