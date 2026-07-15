use alloc::sync::Arc;
use core::marker::PhantomData;

use axerrno::{AxError, AxResult};
use axnet::{
    SocketAcceptIdentity, SocketAcceptReservation, SocketAddrEx, unix::UnixEndpointIdentity,
};

use super::{
    af_alg::AfAlgSocket,
    desc::{FileDescription, OfdIoStatus},
    fd_table::get_file_description,
    fs::File,
    net::Socket,
    netlink::NetlinkSocket,
};
use crate::task::NetworkNamespace;

/// Concrete socket backend retained by one pinned open file description.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SocketBackendKind {
    Network,
    Netlink,
    AfAlg,
}

/// Validated, kernel-owned socket address passed to policy. The original byte
/// length remains in the external typed context, so no userspace pointer is
/// retained here.
#[derive(Clone, Debug)]
pub(crate) enum PreparedSocketAddress {
    Network(SocketAddrEx),
    Netlink(super::netlink::SockaddrNl),
    AfAlg(super::af_alg::SockAddrAlg),
    Unspecified,
}

/// Copied message-layout facts shared by send and receive policy leaves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreparedSocketMessage {
    flags: u32,
    iov_count: usize,
    name_length: usize,
    control_length: usize,
    ancillary_items: usize,
}

impl PreparedSocketMessage {
    pub(crate) const fn new(
        flags: u32,
        iov_count: usize,
        name_length: usize,
        control_length: usize,
        ancillary_items: usize,
    ) -> Self {
        Self {
            flags,
            iov_count,
            name_length,
            control_length,
            ancillary_items,
        }
    }

    pub(crate) const fn flags(&self) -> u32 {
        self.flags
    }

    pub(crate) const fn iov_count(&self) -> usize {
        self.iov_count
    }

    pub(crate) const fn name_length(&self) -> usize {
        self.name_length
    }

    pub(crate) const fn control_length(&self) -> usize {
        self.control_length
    }

    pub(crate) const fn ancillary_items(&self) -> usize {
        self.ancillary_items
    }
}

/// Frozen policy projection of one exact retained open file description.
#[derive(Clone, Copy)]
pub(crate) struct SocketSecurityRef<'a> {
    description: &'a Arc<FileDescription>,
    backend: SocketBackendKind,
    net_namespace: Option<&'a Arc<NetworkNamespace>>,
}

impl SocketSecurityRef<'_> {
    pub(crate) fn ofd_identity(&self) -> u64 {
        self.description.id().get()
    }

    pub(crate) const fn backend(&self) -> SocketBackendKind {
        self.backend
    }

    pub(crate) const fn net_namespace(&self) -> Option<&Arc<NetworkNamespace>> {
        self.net_namespace
    }
}

/// Exact backend object reserved for accept before queue mutation is committed.
#[derive(Clone, Copy)]
pub(crate) struct PendingSocketSecurityRef<'a> {
    identity: SocketAcceptIdentity,
    net_namespace: &'a Arc<NetworkNamespace>,
    _reservation: PhantomData<&'a ()>,
}

#[derive(Clone, Copy)]
pub(crate) enum AcceptedSocketSecurityRef<'a> {
    Description(SocketSecurityRef<'a>),
    Pending(PendingSocketSecurityRef<'a>),
}

impl<'a> PendingSocketSecurityRef<'a> {
    pub(crate) fn new(
        reservation: &'a SocketAcceptReservation<'_>,
        net_namespace: &'a Arc<NetworkNamespace>,
    ) -> Self {
        Self {
            identity: reservation.identity(),
            net_namespace,
            _reservation: PhantomData,
        }
    }

    pub(crate) const fn identity(&self) -> SocketAcceptIdentity {
        self.identity
    }

    pub(crate) const fn net_namespace(&self) -> &Arc<NetworkNamespace> {
        self.net_namespace
    }
}

/// Exact Unix listener, pending server endpoint, or datagram peer retained by
/// a Layer 1 reservation for the duration of one policy call.
#[derive(Clone, Copy)]
pub(crate) struct UnixEndpointSecurityRef<'a> {
    identity: UnixEndpointIdentity,
    net_namespace: &'a Arc<NetworkNamespace>,
    _reservation: PhantomData<&'a ()>,
}

impl<'a> UnixEndpointSecurityRef<'a> {
    pub(crate) fn new<T: ?Sized>(
        identity: UnixEndpointIdentity,
        net_namespace: &'a Arc<NetworkNamespace>,
        _reservation: &'a T,
    ) -> Self {
        Self {
            identity,
            net_namespace,
            _reservation: PhantomData,
        }
    }

    pub(crate) const fn identity(&self) -> UnixEndpointIdentity {
        self.identity
    }

    pub(crate) const fn net_namespace(&self) -> &Arc<NetworkNamespace> {
        self.net_namespace
    }
}

/// One stable open file description for a complete socket operation.
///
/// The numeric descriptor is resolved exactly once. Backend classification and
/// `O_PATH`/`ENOTSOCK` decisions are then derived only from that retained
/// description, so a `CLONE_FILES` sibling cannot redirect a later branch by
/// closing and reusing the descriptor number.
pub(crate) struct PinnedSocketDescription {
    description: Arc<FileDescription>,
    backend: AxResult<SocketBackendKind>,
    io_status: OfdIoStatus,
}

impl PinnedSocketDescription {
    /// Pins and validates a socket description at syscall entry.
    pub(crate) fn from_fd(fd: i32) -> AxResult<Self> {
        Self::from_lookup(|| get_file_description(fd))
    }

    /// Pins the description while deferring backend error reporting.
    ///
    /// Linux `connect(2)` retains the fd before copying the address but reports
    /// `ENOTSOCK` only after that copy. The saved classification still refers to
    /// this exact description and never performs a second fd-table lookup.
    pub(crate) fn pin_fd(fd: i32) -> AxResult<Self> {
        Self::pin_with(|| get_file_description(fd))
    }

    /// Stabilizes an already allocated but unpublished socket description.
    pub(crate) fn from_description(description: Arc<FileDescription>) -> AxResult<Self> {
        Self::from_lookup(|| Ok(description))
    }

    fn from_lookup(lookup: impl FnOnce() -> AxResult<Arc<FileDescription>>) -> AxResult<Self> {
        let pinned = Self::pin_with(lookup)?;
        pinned.backend()?;
        Ok(pinned)
    }

    fn pin_with(lookup: impl FnOnce() -> AxResult<Arc<FileDescription>>) -> AxResult<Self> {
        let description = lookup()?;
        let io_status = description.io_status_snapshot();
        let backend = Self::classify(&description, io_status);
        Ok(Self {
            description,
            backend,
            io_status,
        })
    }

    fn classify(
        description: &FileDescription,
        io_status: OfdIoStatus,
    ) -> AxResult<SocketBackendKind> {
        if io_status.path_only() {
            return Err(AxError::BadFileDescriptor);
        }
        if description.inner.downcast_ref::<Socket>().is_some() {
            return Ok(SocketBackendKind::Network);
        }
        if description.inner.downcast_ref::<NetlinkSocket>().is_some() {
            return Ok(SocketBackendKind::Netlink);
        }
        if description.inner.downcast_ref::<AfAlgSocket>().is_some() {
            return Ok(SocketBackendKind::AfAlg);
        }
        if description
            .inner
            .downcast_ref::<File>()
            .is_some_and(|file| file.inner().is_path())
        {
            return Err(AxError::BadFileDescriptor);
        }
        Err(AxError::NotASocket)
    }

    pub(crate) const fn backend(&self) -> AxResult<SocketBackendKind> {
        self.backend
    }

    pub(crate) const fn nonblocking(&self) -> bool {
        self.io_status.nonblocking()
    }

    pub(crate) fn network(&self) -> AxResult<&Socket> {
        if self.backend()? != SocketBackendKind::Network {
            return Err(AxError::NotASocket);
        }
        self.description
            .inner
            .downcast_ref::<Socket>()
            .ok_or(AxError::BadState)
    }

    pub(crate) fn netlink(&self) -> AxResult<&NetlinkSocket> {
        if self.backend()? != SocketBackendKind::Netlink {
            return Err(AxError::NotASocket);
        }
        self.description
            .inner
            .downcast_ref::<NetlinkSocket>()
            .ok_or(AxError::BadState)
    }

    pub(crate) fn af_alg(&self) -> AxResult<&AfAlgSocket> {
        if self.backend()? != SocketBackendKind::AfAlg {
            return Err(AxError::NotASocket);
        }
        self.description
            .inner
            .downcast_ref::<AfAlgSocket>()
            .ok_or(AxError::BadState)
    }

    pub(crate) fn security_ref(&self) -> AxResult<SocketSecurityRef<'_>> {
        let backend = self.backend()?;
        let net_namespace = match backend {
            SocketBackendKind::Network => Some(self.network()?.net_namespace()),
            SocketBackendKind::Netlink => Some(self.netlink()?.net_namespace()),
            SocketBackendKind::AfAlg => None,
        };
        Ok(SocketSecurityRef {
            description: &self.description,
            backend,
            net_namespace,
        })
    }

    pub(crate) fn into_description(self) -> Arc<FileDescription> {
        self.description
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{borrow::Cow, sync::Arc};
    use core::{cell::Cell, task::Context};
    use std::cell::RefCell;

    use axnet::{
        Socket as SocketInner,
        unix::{DgramTransport, UnixSocket},
    };
    use axpoll::{IoEvents, Pollable};
    use linux_raw_sys::general::O_PATH;

    use super::*;
    use crate::{
        file::{FileLike, Kstat},
        task::{NetworkNamespace, UserNamespace},
    };

    struct NonSocket;

    impl Pollable for NonSocket {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            _context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
            axpoll::PollRegistration::empty()
        }
    }

    impl FileLike for NonSocket {
        fn stat(&self) -> AxResult<Kstat> {
            Err(AxError::InvalidInput)
        }

        fn path(&self) -> AxResult<Cow<'_, str>> {
            Ok(Cow::Borrowed("non-socket"))
        }

        fn set_nonblocking(&self, _nonblocking: bool) -> AxResult<()> {
            Ok(())
        }
    }

    fn description(inner: Arc<dyn FileLike>, flags: u32) -> Arc<FileDescription> {
        FileDescription::new_with_flags(inner, flags).unwrap()
    }

    fn socket_descriptions() -> [Arc<FileDescription>; 3] {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let net_ns = NetworkNamespace::try_new_loopback_only(user_ns).unwrap();
        let unix = UnixSocket::new(
            DgramTransport::new().unwrap(),
            net_ns.stack().unix_namespace(),
        );
        let network = Socket::new(SocketInner::Unix(unix), net_ns.clone());
        let netlink = NetlinkSocket::try_new(0, net_ns).unwrap();
        let af_alg = AfAlgSocket::new_listener();
        [
            description(Arc::new(network), 0),
            description(netlink, 0),
            description(Arc::new(af_alg), 0),
        ]
    }

    #[test]
    fn classifies_every_supported_backend_from_one_description() {
        let descriptions = socket_descriptions();
        for (description, expected) in descriptions.into_iter().zip([
            SocketBackendKind::Network,
            SocketBackendKind::Netlink,
            SocketBackendKind::AfAlg,
        ]) {
            let lookups = Cell::new(0);
            let pinned = PinnedSocketDescription::from_lookup(|| {
                lookups.set(lookups.get() + 1);
                Ok(description)
            })
            .unwrap();
            assert_eq!(lookups.get(), 1);
            assert_eq!(pinned.backend(), Ok(expected));
        }
    }

    #[test]
    fn lookup_and_backend_errors_keep_their_linux_errno_priority() {
        let lookups = Cell::new(0);
        let missing = PinnedSocketDescription::from_lookup(|| {
            lookups.set(lookups.get() + 1);
            Err(AxError::BadFileDescriptor)
        });
        assert!(matches!(missing, Err(AxError::BadFileDescriptor)));
        assert_eq!(lookups.get(), 1);

        let non_socket = description(Arc::new(NonSocket), 0);
        let lookups = Cell::new(0);
        let error = PinnedSocketDescription::from_lookup(|| {
            lookups.set(lookups.get() + 1);
            Ok(non_socket)
        });
        assert!(matches!(error, Err(AxError::NotASocket)));
        assert_eq!(lookups.get(), 1);

        let path_only = description(Arc::new(NonSocket), O_PATH);
        let lookups = Cell::new(0);
        let error = PinnedSocketDescription::from_lookup(|| {
            lookups.set(lookups.get() + 1);
            Ok(path_only)
        });
        assert!(matches!(error, Err(AxError::BadFileDescriptor)));
        assert_eq!(lookups.get(), 1);
    }

    #[test]
    fn close_and_reuse_after_lookup_cannot_redirect_backend_classification() {
        let [original, _netlink, replacement] = socket_descriptions();
        let slot = RefCell::new(original.clone());
        let lookups = Cell::new(0);

        let pinned = PinnedSocketDescription::from_lookup(|| {
            lookups.set(lookups.get() + 1);
            let selected = slot.borrow().clone();
            slot.replace(replacement);
            Ok(selected)
        })
        .unwrap();

        assert_eq!(lookups.get(), 1);
        assert_eq!(pinned.backend(), Ok(SocketBackendKind::Network));
        assert!(Arc::ptr_eq(&pinned.description, &original));
        assert!(!Arc::ptr_eq(&pinned.description, &slot.borrow()));
    }

    #[test]
    fn deferred_connect_classification_uses_the_same_pinned_description() {
        let non_socket = description(Arc::new(NonSocket), 0);
        let lookups = Cell::new(0);
        let pinned = PinnedSocketDescription::pin_with(|| {
            lookups.set(lookups.get() + 1);
            Ok(non_socket)
        })
        .unwrap();

        assert_eq!(lookups.get(), 1);
        assert_eq!(pinned.backend(), Err(AxError::NotASocket));
        assert!(matches!(pinned.network(), Err(AxError::NotASocket)));
        assert_eq!(lookups.get(), 1);
    }
}
