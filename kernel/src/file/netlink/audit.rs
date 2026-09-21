//! `file::netlink` subsections; see the parent `mod.rs` for the module map.

use super::*;

/// Deliver one policy decision through the generic NETLINK_AUDIT transport.
/// The record is a normal netlink frame with Linux's
/// `AUDIT_LANDLOCK_ACCESS` type, so
/// listeners receive an ordered kernel-originated datagram instead of a
/// private side channel.
pub(crate) fn emit_landlock_audit(sequence: u64, event: AuditLandlockDenied) {
    let mut text = alloc::format!(
        "audit({sequence}): landlock_blocker={} landlock_access=0x{:x} landlock_domain={} \
         exec={}\0",
        event.blocker,
        event.access,
        event.domain_id,
        u8::from(event.on_exec),
    )
    .into_bytes();
    let mut message = vec![0; size_of::<NlMsgHdr>()];
    message.append(&mut text);
    let header = NlMsgHdr {
        nlmsg_len: message.len() as u32,
        nlmsg_type: AUDIT_LANDLOCK_ACCESS,
        nlmsg_flags: 0,
        nlmsg_seq: sequence as u32,
        nlmsg_pid: 0,
    };
    write_struct(&mut message[..size_of::<NlMsgHdr>()], &header);
    let mut sockets = AUDIT_SOCKETS.lock();
    sockets.retain(|weak| {
        let Some(socket) = weak.upgrade() else {
            return false;
        };
        if socket.subscribed_to(AUDIT_GROUP) {
            let mut copy = Vec::new();
            if copy.try_reserve_exact(message.len()).is_ok() {
                copy.extend_from_slice(&message);
                socket.enqueue_kernel_from(copy, AUDIT_GROUP, Some(KERNEL_UEVENT_CREDENTIALS));
            } else {
                socket.note_queue_drop();
            }
        }
        true
    });
}

/// Deliver one seccomp event using Linux's `AUDIT_SECCOMP` message type.
/// Credentials are captured as kernel credentials with the queued datagram,
/// not sampled later from a potentially unrelated receiver.
pub(crate) fn emit_seccomp_audit(sequence: u64, event: AuditSeccompDecision) {
    let mut text = alloc::format!(
        "audit({sequence}): arch={:#x} syscall={} ip={:#x} code={:#x} pid={}\0",
        event.architecture,
        event.syscall,
        event.instruction_pointer,
        event.action,
        event.pid,
    )
    .into_bytes();
    let mut message = vec![0; size_of::<NlMsgHdr>()];
    message.append(&mut text);
    let header = NlMsgHdr {
        nlmsg_len: message.len() as u32,
        nlmsg_type: AUDIT_SECCOMP,
        nlmsg_flags: 0,
        nlmsg_seq: sequence as u32,
        nlmsg_pid: 0,
    };
    write_struct(&mut message[..size_of::<NlMsgHdr>()], &header);
    let mut sockets = AUDIT_SOCKETS.lock();
    sockets.retain(|weak| {
        let Some(socket) = weak.upgrade() else {
            return false;
        };
        if socket.subscribed_to(AUDIT_GROUP) {
            let mut copy = Vec::new();
            if copy.try_reserve_exact(message.len()).is_ok() {
                copy.extend_from_slice(&message);
                socket.enqueue_kernel_from(copy, AUDIT_GROUP, Some(KERNEL_UEVENT_CREDENTIALS));
            } else {
                socket.note_queue_drop();
            }
        }
        true
    });
}

/// Establish the network namespace to which all kernel kobject uevents are
/// broadcast.  Boot registers init-net before publishing devices; repeating
/// that registration is harmless, while replacing it is rejected.
pub(crate) fn register_init_network_namespace(net_ns: &Arc<NetworkNamespace>) -> AxResult {
    let mut init_net_ns = INIT_NETWORK_NAMESPACE.lock();
    match init_net_ns.as_ref() {
        Some(existing) if Arc::ptr_eq(existing, net_ns) => Ok(()),
        Some(_) => Err(AxError::AlreadyExists),
        None => {
            *init_net_ns = Some(net_ns.clone());
            Ok(())
        }
    }
}

/// Audit endpoints remain global even though ordinary netlink families are
/// network-namespace scoped.  Compare object identity rather than the owner
/// user namespace: an unshared network namespace can be owned by init-user
/// and is still not permitted to host an audit listener.
pub(crate) fn is_initial_network_namespace(net_ns: &Arc<NetworkNamespace>) -> bool {
    INIT_NETWORK_NAMESPACE
        .lock()
        .as_ref()
        .is_some_and(|initial| Arc::ptr_eq(initial, net_ns))
}

/// Publish a kobject uevent exclusively to the boot-established init network
/// namespace.  Before boot has registered init-net, there can be no
/// publishable device listener, so retain the historical best-effort behavior
/// and drop the notification.
pub(crate) fn emit_init_net_kobject_uevent(
    action: &str,
    devpath: &str,
    subsystem: &str,
    extra_environment: &[(&str, &str)],
) -> AxResult<Option<u64>> {
    let init_net_ns = INIT_NETWORK_NAMESPACE.lock().clone();
    let Some(init_net_ns) = init_net_ns else {
        return Ok(None);
    };
    emit_kobject_uevent(&init_net_ns, action, devpath, subsystem, extra_environment).map(Some)
}

/// Publish a kernel kobject uevent to NETLINK_KOBJECT_UEVENT group 1.
///
/// The payload follows the Linux wire format: an action/path header followed
/// by NUL-separated environment strings, with a globally monotonic SEQNUM.
/// This is intentionally independent from the route netlink request parser.
pub(crate) fn emit_kobject_uevent(
    net_ns: &NetworkNamespace,
    action: &str,
    devpath: &str,
    subsystem: &str,
    extra_environment: &[(&str, &str)],
) -> AxResult<u64> {
    if action.is_empty()
        || devpath.is_empty()
        || subsystem.is_empty()
        || action.contains('\0')
        || devpath.contains('\0')
        || subsystem.contains('\0')
        || extra_environment
            .iter()
            .any(|(key, value)| key.is_empty() || key.contains('\0') || value.contains('\0'))
    {
        return Err(AxError::InvalidInput);
    }

    // A single sender domain keeps sequence allocation and delivery ordered:
    // listeners can never receive SEQNUM n + 1 before n.
    let _send_guard = KOBJECT_UEVENT_SEND_LOCK.lock();
    let sequence = KOBJECT_UEVENT_SEQNUM.fetch_add(1, Ordering::Relaxed) + 1;
    let sequence_text = sequence.to_string();
    let mut payload_len = action
        .len()
        .checked_add(1)
        .and_then(|len| len.checked_add(devpath.len()))
        .and_then(|len| len.checked_add(1))
        .ok_or(AxError::NoMemory)?;
    for (key, value) in [
        ("ACTION", action),
        ("DEVPATH", devpath),
        ("SUBSYSTEM", subsystem),
        ("SEQNUM", sequence_text.as_str()),
    ]
    .into_iter()
    .chain(extra_environment.iter().copied())
    {
        payload_len = payload_len
            .checked_add(key.len())
            .and_then(|len| len.checked_add(1))
            .and_then(|len| len.checked_add(value.len()))
            .and_then(|len| len.checked_add(1))
            .ok_or(AxError::NoMemory)?;
    }
    if admit_kernel_netlink_message(payload_len) == NetlinkWriteAdmission::MessageTooLarge {
        return Err(LinuxError::EMSGSIZE.into());
    }
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(payload_len)
        .map_err(|_| AxError::NoMemory)?;
    append_uevent_field(&mut payload, action)?;
    payload.push(b'@');
    append_uevent_field(&mut payload, devpath)?;
    payload.push(0);
    append_uevent_assignment(&mut payload, "ACTION", action)?;
    append_uevent_assignment(&mut payload, "DEVPATH", devpath)?;
    append_uevent_assignment(&mut payload, "SUBSYSTEM", subsystem)?;
    append_uevent_assignment(&mut payload, "SEQNUM", &sequence_text)?;
    for &(key, value) in extra_environment {
        append_uevent_assignment(&mut payload, key, value)?;
    }
    debug_assert_eq!(payload.len(), payload_len);

    broadcast_uevent_to_namespace(net_ns, &payload, KERNEL_UEVENT_CREDENTIALS, None);
    Ok(sequence)
}

pub(crate) fn broadcast_user_uevent(
    net_ns: &NetworkNamespace,
    payload: &[u8],
    credentials: NetlinkCredentials,
) -> AxResult {
    // Keep synthetic and kernel-originated uevents in one sequence/delivery
    // domain, matching uevent_sock_mutex plus the global Linux sequence.
    let _send_guard = KOBJECT_UEVENT_SEND_LOCK.lock();
    broadcast_user_uevent_locked(net_ns, payload, credentials, None)
}

/// Caller already owns `KOBJECT_UEVENT_SEND_LOCK` through a typed Uevent
/// write permit or through the ordinary wrapper above.
fn broadcast_user_uevent_locked(
    net_ns: &NetworkNamespace,
    payload: &[u8],
    credentials: NetlinkCredentials,
    skip: Option<*const NetlinkSocket>,
) -> AxResult {
    let sequence = KOBJECT_UEVENT_SEQNUM.fetch_add(1, Ordering::Relaxed) + 1;
    let sequence_text = sequence.to_string();
    let suffix_len = "SEQNUM="
        .len()
        .checked_add(sequence_text.len())
        .and_then(|len| len.checked_add(1))
        .ok_or(AxError::NoMemory)?;
    let message_len = payload
        .len()
        .checked_add(suffix_len)
        .ok_or(AxError::NoMemory)?;
    if admit_kernel_netlink_message(message_len) == NetlinkWriteAdmission::MessageTooLarge {
        return Err(LinuxError::EMSGSIZE.into());
    }
    let mut message = Vec::new();
    message
        .try_reserve_exact(message_len)
        .map_err(|_| AxError::NoMemory)?;
    message.extend_from_slice(payload);
    append_uevent_assignment(&mut message, "SEQNUM", &sequence_text)?;
    debug_assert_eq!(message.len(), message_len);
    broadcast_uevent_to_namespace(net_ns, &message, credentials, skip);
    Ok(())
}

/// NOWAIT uevent delivery while the caller owns the global sender domain.
/// Peer state/queues are probed only; contended listeners observe a normal
/// multicast drop rather than making this source-consuming operation sleep.
/// The caller's own delivery is returned for its retained queue to enqueue.
pub(crate) fn broadcast_user_uevent_nowait_locked(
    net_ns: &NetworkNamespace,
    payload: &[u8],
    credentials: NetlinkCredentials,
    sender: *const NetlinkSocket,
    sockets: &mut Vec<Weak<NetlinkSocket>>,
) -> AxResult<Vec<u8>> {
    let sequence = KOBJECT_UEVENT_SEQNUM.fetch_add(1, Ordering::Relaxed) + 1;
    let sequence_text = sequence.to_string();
    let suffix_len = "SEQNUM="
        .len()
        .checked_add(sequence_text.len())
        .and_then(|len| len.checked_add(1))
        .ok_or(AxError::NoMemory)?;
    let message_len = payload
        .len()
        .checked_add(suffix_len)
        .ok_or(AxError::NoMemory)?;
    if admit_kernel_netlink_message(message_len) == NetlinkWriteAdmission::MessageTooLarge {
        return Err(LinuxError::EMSGSIZE.into());
    }
    let mut message = Vec::new();
    message
        .try_reserve_exact(message_len)
        .map_err(|_| AxError::NoMemory)?;
    message.extend_from_slice(payload);
    append_uevent_assignment(&mut message, "SEQNUM", &sequence_text)?;
    sockets.retain(|entry| {
        let Some(socket) = entry.upgrade() else {
            return false;
        };
        if core::ptr::eq(Arc::as_ptr(&socket), sender)
            || !core::ptr::eq(socket.net_ns.as_ref(), net_ns)
        {
            return true;
        }
        let Some(state) = socket.state.try_lock() else {
            // The NO_ENOBUFS bit itself is protected by this contended lock;
            // report the loss conservatively so userspace can rescan.
            socket.overrun.store(true, Ordering::Release);
            socket.poll_rx.wake();
            return true;
        };
        let subscribed = state.groups & u64::from(KOBJECT_UEVENT_GROUP) != 0;
        let suppress_enobufs = state.option_flags & (1 << NETLINK_NO_ENOBUFS) != 0;
        let rcvbuf = usize::try_from(state.sock.rcvbuf).unwrap_or(0);
        drop(state);
        if !subscribed {
            return true;
        }
        let Some(mut queue) = socket.queue.try_lock() else {
            if !suppress_enobufs {
                socket.overrun.store(true, Ordering::Release);
            }
            socket.poll_rx.wake();
            return true;
        };
        if admit_netlink_queue(
            queue.datagrams.len(),
            queue.bytes,
            message.len(),
            NETLINK_QUEUE_LIMIT,
            rcvbuf,
        ) == NetlinkQueueAdmission::Drop
        {
            if !suppress_enobufs {
                socket.overrun.store(true, Ordering::Release);
            }
            socket.poll_rx.wake();
            return true;
        }
        let mut copy = Vec::new();
        if copy.try_reserve_exact(message.len()).is_err() {
            if !suppress_enobufs {
                socket.overrun.store(true, Ordering::Release);
            }
            socket.poll_rx.wake();
            return true;
        }
        copy.extend_from_slice(&message);
        queue.bytes += copy.len();
        queue.datagrams.push_back(NetlinkDatagram {
            data: copy,
            source_port_id: 0,
            source_groups: KOBJECT_UEVENT_GROUP,
            credentials: Some(credentials),
        });
        drop(queue);
        socket.poll_rx.wake();
        true
    });
    Ok(message)
}

fn broadcast_uevent_to_namespace(
    net_ns: &NetworkNamespace,
    payload: &[u8],
    credentials: NetlinkCredentials,
    skip: Option<*const NetlinkSocket>,
) {
    // Never retain the global listener registry while taking a socket-local
    // state or queue lock.  A userspace sender holds its own state/queue
    // before it takes the sender-domain lock, so registry -> peer lock here
    // would otherwise form a cross-sender cycle.
    let mut sockets = KOBJECT_UEVENT_SOCKETS.lock();
    let listeners = match collect_live_listeners(&mut sockets) {
        Ok(listeners) => listeners,
        Err(error) => {
            // Kernel-originated uevents are best effort.  OOM while taking a
            // snapshot must not hold the sender domain or make device
            // publication fail; retain only live registrations and drop this
            // multicast with a diagnostic.
            sockets.retain(|socket| socket.strong_count() != 0);
            warn!("dropping kobject uevent: cannot snapshot listeners: {error}");
            return;
        }
    };
    drop(sockets);

    for socket in listeners {
        if skip.is_some_and(|skip| core::ptr::eq(Arc::as_ptr(&socket), skip)) {
            continue;
        }
        if !core::ptr::eq(socket.net_ns.as_ref(), net_ns) {
            continue;
        }
        // This path runs with KOBJECT_UEVENT_SEND_LOCK held.  A synthetic
        // sender owns its own state before waiting for that lock, so listener
        // state and queue must be probed, never waited on.
        let Some(state) = socket.state.try_lock() else {
            // We cannot inspect NETLINK_NO_ENOBUFS without this lock.  Report
            // the loss conservatively so eudevd receives an ENOBUFS rescan
            // signal instead of silently missing device lifecycle events.
            socket.overrun.store(true, Ordering::Release);
            socket.poll_rx.wake();
            continue;
        };
        let subscribed = state.groups & u64::from(KOBJECT_UEVENT_GROUP) != 0;
        let suppress_enobufs = state.option_flags & (1 << NETLINK_NO_ENOBUFS) != 0;
        let rcvbuf = usize::try_from(state.sock.rcvbuf).unwrap_or(0);
        drop(state);
        if !subscribed {
            continue;
        }
        let Some(mut queue) = socket.queue.try_lock() else {
            if !suppress_enobufs {
                socket.overrun.store(true, Ordering::Release);
            }
            socket.poll_rx.wake();
            continue;
        };
        if admit_netlink_queue(
            queue.datagrams.len(),
            queue.bytes,
            payload.len(),
            NETLINK_QUEUE_LIMIT,
            rcvbuf,
        ) == NetlinkQueueAdmission::Drop
        {
            if !suppress_enobufs {
                socket.overrun.store(true, Ordering::Release);
            }
            drop(queue);
            socket.poll_rx.wake();
            continue;
        }
        // Keep per-socket buffers isolated: an allocation failure for one
        // listener never makes another listener observe its datagram.
        let mut message = Vec::new();
        if message.try_reserve_exact(payload.len()).is_err() {
            if !suppress_enobufs {
                socket.overrun.store(true, Ordering::Release);
            }
            drop(queue);
            socket.poll_rx.wake();
            continue;
        }
        message.extend_from_slice(payload);
        queue.bytes += message.len();
        queue.datagrams.push_back(NetlinkDatagram {
            data: message,
            source_port_id: 0,
            source_groups: KOBJECT_UEVENT_GROUP,
            credentials: Some(credentials),
        });
        drop(queue);
        socket.poll_rx.wake();
    }
}

/// Snapshot live listeners while holding only the registry lock.  Callers
/// must release that lock before touching socket-local state or queues.
fn collect_live_listeners(
    sockets: &mut Vec<Weak<NetlinkSocket>>,
) -> AxResult<Vec<Arc<NetlinkSocket>>> {
    let mut listeners = Vec::new();
    listeners
        .try_reserve(sockets.len())
        .map_err(|_| AxError::NoMemory)?;
    sockets.retain(|entry| {
        let Some(socket) = entry.upgrade() else {
            return false;
        };
        listeners.push(socket);
        true
    });
    Ok(listeners)
}

pub(crate) fn find_netlink_peer(
    protocol: u32,
    net_ns: &NetworkNamespace,
    port_id: u32,
    nowait: bool,
) -> AxResult<Option<Arc<NetlinkSocket>>> {
    // KOBJECT_UEVENT and USERSOCK are the netlink families in this kernel that
    // accept user-to-user datagrams (`netlink_unicast` resolving a port ID in
    // `nl_table[protocol].hash`).  Their weak listener registries provide a
    // lifetime pin without changing the deliberately metadata-only port
    // reservation table used by the kernel-service families.
    let registry = match protocol {
        NETLINK_KOBJECT_UEVENT => &*KOBJECT_UEVENT_SOCKETS,
        NETLINK_USERSOCK => &*USERSOCK_SOCKETS,
        _ => return Ok(None),
    };
    // Binding takes socket state and then the port registry.  Do not invert
    // that order by holding the transport registry while inspecting a peer's
    // state: clone live candidates first, then drop the registry lock.
    let mut sockets = if nowait {
        registry.try_lock().ok_or(AxError::WouldBlock)?
    } else {
        registry.lock()
    };
    let candidates = collect_live_listeners(&mut sockets)?;
    drop(sockets);

    for socket in candidates {
        let state = if nowait {
            socket.state.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            socket.state.lock()
        };
        let matches = state.bound
            && state.port_id == port_id
            && core::ptr::eq(socket.net_ns.as_ref(), net_ns);
        drop(state);
        if matches {
            return Ok(Some(socket));
        }
    }
    Ok(None)
}

/// Deliver one `NETLINK_USERSOCK` datagram to every subscriber of `group` in
/// this network namespace.  Linux's `netlink_broadcast` walks the same
/// protocol hash table and filters on `nlk->groups & group` after translating
/// the reported sender identity and, for `nl_pid != 0`, excluding the sender.
pub(crate) fn broadcast_netlink_usersock(
    sender: &NetlinkSocket,
    data: &[u8],
    source_port_id: u32,
    destination_port_id: u32,
    group: u32,
    credentials: NetlinkCredentials,
    nowait: bool,
) -> AxResult {
    let mut sockets = if nowait {
        USERSOCK_SOCKETS.try_lock().ok_or(AxError::WouldBlock)?
    } else {
        USERSOCK_SOCKETS.lock()
    };
    let candidates = collect_live_listeners(&mut sockets)?;
    drop(sockets);

    let mut delivered = false;
    for socket in candidates {
        // `do_one_broadcast()` skips the sender (`p->exclude_sk`) and the
        // socket the address names (`nlk->portid == p->portid`) before it
        // tests group membership (`net/netlink/af_netlink.c:1429-1431`).  The
        // named socket is skipped because `netlink_sendmsg` hands it the
        // `netlink_unicast()` copy instead, which is what makes a
        // `{nl_pid, nl_groups}` address deliver exactly one datagram to it.
        if core::ptr::eq(socket.as_ref(), sender) {
            continue;
        }
        if destination_port_id != 0 && socket.bound_port_id() == Some(destination_port_id) {
            continue;
        }
        if !Arc::ptr_eq(&socket.net_ns, &sender.net_ns) {
            continue;
        }
        if !socket.subscribed_to(group) {
            continue;
        }
        let mut copy = Vec::new();
        copy.try_reserve_exact(data.len())
            .map_err(|_| AxError::from(LinuxError::ENOBUFS))?;
        copy.extend_from_slice(data);
        // `do_one_broadcast()` reports one failed delivery without aborting
        // the round, so a full or contended queue on one subscriber costs it
        // the datagram (recorded as an overrun) but never starves the rest.
        if socket
            .enqueue_user_from(copy, source_port_id, credentials, nowait)
            .is_err()
        {
            socket.note_queue_drop();
            continue;
        }
        delivered = true;
    }
    if delivered {
        Ok(())
    } else {
        Err(LinuxError::ESRCH.into())
    }
}
