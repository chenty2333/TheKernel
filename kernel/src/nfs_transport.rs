//! NFSv4.1 record-marking transport over one retained TCP OFD.
//!
//! There is exactly one socket reader. Forechannel callers only serialize
//! writes; replies are demultiplexed by XID and server CALLs are answered by
//! that same reader before it consumes another record.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, Ordering};

use axfs::{NfsError, NfsMount, NfsResult, RpcTransport};
use axio::Cursor;
use axnet::{Shutdown, Socket as SocketInner, SocketAddrEx, SocketOps};
use axsync::Mutex;

use crate::{
    file::{FileHandle, Socket},
    task::NetworkNamespace,
};

const MAX_RPC_RECORD: usize = 1024 * 1024;
const RPC_CALL: u32 = 0;
const RPC_REPLY: u32 = 1;
const RPC_VERSION: u32 = 2;
const RPC_ACCEPTED: u32 = 0;
const RPC_SUCCESS: u32 = 0;
const AUTH_NONE: u32 = 0;
const AUTH_SYS: u32 = 1;
const RPCSEC_GSS: u32 = 6;
const NFS_CALLBACK_PROGRAM: u32 = 0x4000_0000;
const NFS_CALLBACK_VERSION: u32 = 1;
const NFS_CALLBACK_COMPOUND: u32 = 1;

fn take_be_word(bytes: &[u8], at: &mut usize) -> NfsResult<u32> {
    let end = at.checked_add(4).ok_or(NfsError::Malformed)?;
    let word = u32::from_be_bytes(
        bytes
            .get(*at..end)
            .ok_or(NfsError::Malformed)?
            .try_into()
            .map_err(|_| NfsError::Malformed)?,
    );
    *at = end;
    Ok(word)
}
struct ReplyWaiter {
    xid: u32,
    terminal: AtomicBool,
    reply: Mutex<Option<NfsResult<Vec<u8>>>>,
}
struct CallbackLifecycle {
    mount: Option<Weak<NfsMount>>,
    epoch: u64,
}

/// Typed, stream-only RPC transport. UDP is deliberately rejected: NFSv4.1
/// sessions require ordered record-marked RPC over the retained connection.
pub(crate) struct NfsSocketTransport {
    socket: FileHandle<Socket>,
    cancelled: Mutex<Vec<u32>>,
    net_ns: Arc<NetworkNamespace>,
    peer: SocketAddrEx,
    creator: (
        Arc<crate::task::Cred>,
        crate::task::security::LandlockDomain,
    ),
    /// A send guard never spans a reply wait. The reader task owns reads.
    send: Mutex<()>,
    waiters: Mutex<Vec<Arc<ReplyWaiter>>>,
    closing: AtomicBool,
    reader_started: AtomicBool,
    callback_lifecycle: Mutex<CallbackLifecycle>,
}
impl NfsSocketTransport {
    pub(crate) fn try_new(socket: FileHandle<Socket>) -> NfsResult<Arc<Self>> {
        if !matches!(&socket.inner, SocketInner::Tcp(_)) {
            return Err(NfsError::Transport);
        }
        let net_ns = socket.net_namespace().clone();
        let peer = socket.peer_addr().map_err(|_| NfsError::Transport)?;
        let creator = socket
            .creator_security_snapshot()
            .map_err(|_| NfsError::Transport)?;
        Arc::try_new(Self {
            socket,
            cancelled: Mutex::new(Vec::new()),
            net_ns,
            peer,
            creator,
            send: Mutex::new(()),
            waiters: Mutex::new(Vec::new()),
            closing: AtomicBool::new(false),
            reader_started: AtomicBool::new(false),
            callback_lifecycle: Mutex::new(CallbackLifecycle {
                mount: None,
                epoch: 0,
            }),
        })
        .map_err(|_| NfsError::Transport)
    }
    /// Start the reader before the mount can issue its first forechannel RPC.
    pub(crate) fn install_callback_mount(self: &Arc<Self>, mount: Weak<NfsMount>) -> NfsResult<()> {
        {
            let mut lifecycle = self.callback_lifecycle.lock();
            if self.closing.load(Ordering::Acquire) {
                return Err(NfsError::Transport);
            }
            let requested = mount.upgrade().ok_or(NfsError::SessionLost)?;
            if let Some(existing) = lifecycle.mount.as_ref().and_then(Weak::upgrade) {
                if !Arc::ptr_eq(&existing, &requested) {
                    return Err(NfsError::Transport);
                }
            }
            lifecycle.mount = Some(Arc::downgrade(&requested));
            lifecycle.epoch = lifecycle.epoch.wrapping_add(1);
        }
        if !self.reader_started.swap(true, Ordering::AcqRel) {
            let weak = Arc::downgrade(self);
            if axtask::spawn_raw(
                move || Self::reader_task(weak),
                "nfs41_rpc_reader".into(),
                axconfig::TASK_STACK_SIZE,
            )
            .is_err()
            {
                self.reader_started.store(false, Ordering::Release);
                let mut lifecycle = self.callback_lifecycle.lock();
                lifecycle.mount.take();
                lifecycle.epoch = lifecycle.epoch.wrapping_add(1);
                return Err(NfsError::Transport);
            }
        }
        Ok(())
    }
    /// Complete every forecall during unmount/session teardown. A TCP read
    /// already inside the socket may unwind later, but can no longer retain a
    /// caller or receive a late callback.
    pub(crate) fn shutdown(&self) {
        if !self.closing.swap(true, Ordering::AcqRel) {
            let mut lifecycle = self.callback_lifecycle.lock();
            lifecycle.mount.take();
            lifecycle.epoch = lifecycle.epoch.wrapping_add(1);
            drop(lifecycle);
            self.abort_waiters();
            self.cancelled.lock().clear();
            let _ = self.socket.shutdown(Shutdown::Both);
        }
    }
    fn reader_task(weak: Weak<Self>) {
        loop {
            let Some(transport) = weak.upgrade() else {
                return;
            };
            if transport.closing.load(Ordering::Acquire) {
                return;
            }
            match transport.read_record() {
                Ok(record) => {
                    if transport.route_record(&record).is_err() {
                        transport.shutdown();
                        return;
                    }
                }
                Err(_) => {
                    transport.shutdown();
                    return;
                }
            }
        }
    }
    fn read_record(&self) -> NfsResult<Vec<u8>> {
        let mut record = Vec::new();
        loop {
            let mut marker = [0u8; 4];
            self.read_exact(&mut marker)?;
            let marker = u32::from_be_bytes(marker);
            let length = (marker & 0x7fff_ffff) as usize;
            if length > MAX_RPC_RECORD
                || record
                    .len()
                    .checked_add(length + 4)
                    .is_none_or(|size| size > MAX_RPC_RECORD)
            {
                return Err(NfsError::Length);
            }
            let start = record.len();
            record
                .try_reserve_exact(length + 4)
                .map_err(|_| NfsError::Transport)?;
            record.extend_from_slice(&marker.to_be_bytes());
            record.resize(start + 4 + length, 0);
            self.read_exact(&mut record[start + 4..])?;
            if marker & 0x8000_0000 != 0 {
                return Ok(record);
            }
        }
    }
    fn route_record(&self, record: &[u8]) -> NfsResult<()> {
        let payload = logical_payload(record)?;
        let mut at = 0;
        let xid = take_be_word(&payload, &mut at)?;
        match take_be_word(&payload, &mut at)? {
            RPC_REPLY => self.complete_waiter(xid, record),
            RPC_CALL => self.dispatch_inbound_callback_payload(&payload),
            _ => Err(NfsError::Malformed),
        }
    }
    fn complete_waiter(&self, xid: u32, record: &[u8]) -> NfsResult<()> {
        let waiter = {
            let waiters = self.waiters.lock();
            waiters.iter().find(|waiter| waiter.xid == xid).cloned()
        };
        let Some(waiter) = waiter else {
            let mut cancelled = self.cancelled.lock();
            return if let Some(index) = cancelled.iter().position(|pending| *pending == xid) {
                cancelled.remove(index);
                Ok(())
            } else {
                Err(NfsError::Malformed)
            };
        };
        let mut reply = Vec::new();
        reply
            .try_reserve_exact(record.len())
            .map_err(|_| NfsError::Transport)?;
        reply.extend_from_slice(record);
        if !waiter.terminal.swap(true, Ordering::AcqRel) {
            *waiter.reply.lock() = Some(Ok(reply));
            self.remove_waiter(xid);
        }
        Ok(())
    }
    fn dispatch_inbound_callback_payload(&self, payload: &[u8]) -> NfsResult<()> {
        let mut at = 0usize;
        let xid = take_be_word(payload, &mut at)?;
        if take_be_word(payload, &mut at)? != RPC_CALL
            || take_be_word(payload, &mut at)? != RPC_VERSION
        {
            return Err(NfsError::Malformed);
        }
        if take_be_word(payload, &mut at)? != NFS_CALLBACK_PROGRAM
            || take_be_word(payload, &mut at)? != NFS_CALLBACK_VERSION
            || take_be_word(payload, &mut at)? != NFS_CALLBACK_COMPOUND
        {
            return Err(NfsError::Malformed);
        }
        let credential_flavor = take_be_word(payload, &mut at)?;
        let credential_len = take_be_word(payload, &mut at)? as usize;
        let credential = payload
            .get(at..at.checked_add(credential_len).ok_or(NfsError::Malformed)?)
            .ok_or(NfsError::Malformed)?;
        at = at
            .checked_add((credential_len + 3) & !3)
            .ok_or(NfsError::Malformed)?;
        let call_through_credential = payload.get(..at).ok_or(NfsError::Malformed)?;
        let verifier_flavor = take_be_word(payload, &mut at)?;
        if !matches!(
            (credential_flavor, verifier_flavor),
            (RPCSEC_GSS, RPCSEC_GSS) | (AUTH_SYS, AUTH_NONE) | (AUTH_NONE, AUTH_NONE)
        ) {
            return Err(NfsError::Security);
        }
        let verifier_len = take_be_word(payload, &mut at)? as usize;
        if matches!(credential_flavor, AUTH_SYS | AUTH_NONE) && verifier_len != 0 {
            return Err(NfsError::Security);
        }
        let verifier = payload
            .get(at..at.checked_add(verifier_len).ok_or(NfsError::Malformed)?)
            .ok_or(NfsError::Malformed)?;
        at = at
            .checked_add((verifier_len + 3) & !3)
            .ok_or(NfsError::Malformed)?;
        let body = payload.get(at..).ok_or(NfsError::Malformed)?;
        let (mount, epoch) = {
            let lifecycle = self.callback_lifecycle.lock();
            if self.closing.load(Ordering::Acquire) {
                return Err(NfsError::SessionLost);
            }
            (
                lifecycle
                    .mount
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .ok_or(NfsError::SessionLost)?,
                lifecycle.epoch,
            )
        };
        let (sequence, result) = mount.authenticated_callback(
            credential_flavor,
            credential,
            verifier,
            call_through_credential,
            body,
        )?;
        {
            let lifecycle = self.callback_lifecycle.lock();
            if self.closing.load(Ordering::Acquire) || lifecycle.epoch != epoch {
                return Err(NfsError::SessionLost);
            }
        }
        // RFC 2203 DATA reply verifier authenticates this callback's
        // rpc_gss_cred_t sequence. The accepted header is then emitted with
        // the returned RPCSEC_GSS verifier and wrapped COMPOUND result.
        let reply_flavor = if credential_flavor == RPCSEC_GSS {
            RPCSEC_GSS
        } else {
            AUTH_NONE
        };
        let mut signed_prefix = Vec::new();
        signed_prefix
            .try_reserve_exact(16)
            .map_err(|_| NfsError::Transport)?;
        signed_prefix.extend_from_slice(&xid.to_be_bytes());
        signed_prefix.extend_from_slice(&RPC_REPLY.to_be_bytes());
        signed_prefix.extend_from_slice(&RPC_ACCEPTED.to_be_bytes());
        signed_prefix.extend_from_slice(&reply_flavor.to_be_bytes());
        let reply_verifier =
            mount.callback_reply_verifier(credential_flavor, sequence, &signed_prefix)?;
        let result = mount.wrap_callback_reply(credential_flavor, sequence, &result)?;
        let mut reply = Vec::new();
        let capacity = 40usize
            .checked_add(reply_verifier.len())
            .and_then(|n| n.checked_add(result.len()))
            .ok_or(NfsError::Length)?;
        reply
            .try_reserve_exact(capacity)
            .map_err(|_| NfsError::Transport)?;
        reply.extend_from_slice(&signed_prefix);
        reply.extend_from_slice(&(reply_verifier.len() as u32).to_be_bytes());
        reply.extend_from_slice(&reply_verifier);
        reply.resize((reply.len() + 3) & !3, 0);
        reply.extend_from_slice(&RPC_SUCCESS.to_be_bytes());
        reply.extend_from_slice(&result);
        self.write_framed(&reply)?;
        // CB_RECALL completion is deliberately deferred until the callback
        // reply is on the wire.  The mount worker may then issue COMMIT and
        // DELEGRETURN without blocking this transport's sole reader.
        mount.callback_reply_sent();
        Ok(())
    }
    fn cancelled(&self, xid: u32) -> bool {
        self.closing.load(Ordering::Acquire)
            || self.cancelled.lock().iter().any(|pending| *pending == xid)
    }
    fn write_forecall(&self, bytes: &[u8], xid: u32) -> NfsResult<()> {
        let len = bytes.len();
        let mut cursor = Cursor::new(bytes);
        while cursor.position() != len as u64 {
            if self.cancelled(xid) {
                return Err(NfsError::Transport);
            }
            let sent = self
                .socket
                .write_with_nonblocking(&mut cursor, false)
                .map_err(|_| NfsError::Transport)?;
            if sent == 0 {
                return Err(NfsError::Transport);
            }
        }
        Ok(())
    }
    fn write_callback(&self, bytes: &[u8]) -> NfsResult<()> {
        let mut cursor = Cursor::new(bytes);
        while cursor.position() != bytes.len() as u64 {
            if self.closing.load(Ordering::Acquire) {
                return Err(NfsError::Transport);
            }
            let sent = self
                .socket
                .write_with_nonblocking(&mut cursor, false)
                .map_err(|_| NfsError::Transport)?;
            if sent == 0 {
                return Err(NfsError::Transport);
            }
        }
        Ok(())
    }
    fn write_framed(&self, payload: &[u8]) -> NfsResult<()> {
        let length = u32::try_from(payload.len()).map_err(|_| NfsError::Length)?;
        let mut framed = Vec::new();
        framed
            .try_reserve_exact(payload.len() + 4)
            .map_err(|_| NfsError::Transport)?;
        framed.extend_from_slice(&(0x8000_0000 | length).to_be_bytes());
        framed.extend_from_slice(payload);
        let _send = self.send.lock();
        self.write_callback(&framed)
    }
    fn read_exact(&self, bytes: &mut [u8]) -> NfsResult<()> {
        let len = bytes.len();
        let mut cursor = Cursor::new(bytes);
        while cursor.position() != len as u64 {
            if self.closing.load(Ordering::Acquire) {
                return Err(NfsError::Transport);
            }
            let got = self
                .socket
                .read_with_nonblocking(&mut cursor, false)
                .map_err(|_| NfsError::Transport)?;
            if got == 0 {
                return Err(NfsError::Transport);
            }
        }
        Ok(())
    }
    fn register_waiter(&self, xid: u32) -> NfsResult<Arc<ReplyWaiter>> {
        if self.closing.load(Ordering::Acquire)
            || self.cancelled.lock().iter().any(|pending| *pending == xid)
        {
            return Err(NfsError::Transport);
        }
        let waiter = Arc::try_new(ReplyWaiter {
            xid,
            terminal: AtomicBool::new(false),
            reply: Mutex::new(None),
        })
        .map_err(|_| NfsError::Transport)?;
        let mut waiters = self.waiters.lock();
        waiters.try_reserve(1).map_err(|_| NfsError::Transport)?;
        if waiters.iter().any(|pending| pending.xid == xid) {
            return Err(NfsError::Malformed);
        }
        waiters.push(waiter.clone());
        Ok(waiter)
    }
    fn remove_waiter(&self, xid: u32) -> Option<Arc<ReplyWaiter>> {
        let mut waiters = self.waiters.lock();
        waiters
            .iter()
            .position(|waiter| waiter.xid == xid)
            .map(|index| waiters.remove(index))
    }
    fn abort_waiters(&self) {
        let waiters = core::mem::take(&mut *self.waiters.lock());
        for waiter in waiters {
            if !waiter.terminal.swap(true, Ordering::AcqRel) {
                *waiter.reply.lock() = Some(Err(NfsError::Transport));
            }
        }
    }
}
fn logical_payload(record: &[u8]) -> NfsResult<Vec<u8>> {
    let mut at = 0usize;
    let mut payload = Vec::new();
    loop {
        let marker = take_be_word(record, &mut at)?;
        let length = (marker & 0x7fff_ffff) as usize;
        let end = at.checked_add(length).ok_or(NfsError::Length)?;
        let fragment = record.get(at..end).ok_or(NfsError::Malformed)?;
        payload
            .try_reserve(fragment.len())
            .map_err(|_| NfsError::Transport)?;
        payload.extend_from_slice(fragment);
        at = end;
        if marker & 0x8000_0000 != 0 {
            if at != record.len() {
                return Err(NfsError::Malformed);
            }
            return Ok(payload);
        }
    }
}
impl RpcTransport for NfsSocketTransport {
    fn call(&self, record: &[u8]) -> NfsResult<Vec<u8>> {
        let xid = record
            .get(4..8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_be_bytes)
            .ok_or(NfsError::Malformed)?;
        if record.len() < 8 {
            return Err(NfsError::Malformed);
        }
        let waiter = self.register_waiter(xid)?;
        let send_result = {
            let _send = self.send.lock();
            self.write_forecall(record, xid)
        };
        if let Err(error) = send_result {
            self.remove_waiter(xid);
            return Err(error);
        }
        loop {
            if let Some(reply) = waiter.reply.lock().take() {
                return reply;
            }
            if self.cancelled(xid) {
                self.remove_waiter(xid);
                return Err(NfsError::Transport);
            }
            axtask::yield_now();
        }
    }
    fn cancel(&self, xid: u32) {
        let mut cancelled = self.cancelled.lock();
        let remembered = if cancelled.iter().any(|pending| *pending == xid) {
            true
        } else if cancelled.try_reserve(1).is_ok() {
            cancelled.push(xid);
            true
        } else {
            false
        };
        drop(cancelled);
        if !remembered {
            self.shutdown();
            return;
        }
        if let Some(waiter) = self.remove_waiter(xid) {
            if !waiter.terminal.swap(true, Ordering::AcqRel) {
                *waiter.reply.lock() = Some(Err(NfsError::Transport));
            }
        }
    }
    fn reconnect(&self, mount: Weak<NfsMount>) -> NfsResult<Arc<dyn RpcTransport>> {
        let socket = crate::syscall::reconnect_tcp_socket(
            self.net_ns.clone(),
            self.peer.clone(),
            self.creator.clone(),
        )
        .map_err(|_| NfsError::Transport)?;
        let transport = NfsSocketTransport::try_new(socket)?;
        transport.install_callback_mount(mount)?;
        Ok(transport)
    }
    fn shutdown(&self) {
        NfsSocketTransport::shutdown(self)
    }
}
