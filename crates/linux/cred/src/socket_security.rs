//! Policy-neutral typed contexts for Linux socket security hook leaves.
//!
//! These values describe the immutable inputs visible at the Linux v6.18
//! `socket_*` and `unix_*` LSM leaves. They do not look up descriptors, copy
//! userspace addresses, decode transport objects, call `current()`, dispatch a
//! security-module registry, or map policy failures into errno values.
//!
//! The embedding kernel freezes each socket, address, and prepared-message
//! object before constructing a context. Those objects remain opaque here and
//! are only borrowed for the duration of one policy call. In particular, a
//! prepared send-message snapshot must itself retain the normalized/raw
//! message flags visible through Linux's `msghdr`; the `socket_sendmsg` leaf
//! has no separate flags argument. The `socket_recvmsg` leaf does, so
//! [`SocketReceiveMessageContext`] retains that raw value independently.

use crate::{Credential, UserNamespaceView};

const SOCKET_TYPE_MASK: i32 = 0x0f;

/// Normalized creation facts shared by socket-create and post-create hooks.
///
/// The socket type is the base type after the embedding adapter has validated
/// and removed descriptor flags such as nonblocking and close-on-exec. Family
/// and protocol remain raw signed Linux hook values, and `kernel_origin`
/// preserves whether the request originated inside the kernel.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SocketCreateSpec {
    family: i32,
    socket_type: i32,
    protocol: i32,
    kernel_origin: bool,
}

impl SocketCreateSpec {
    /// Constructs a creation spec from an already flag-free socket type.
    ///
    /// Returns `None` when `socket_type` is negative or contains any bit
    /// outside Linux's base socket-type mask. Raw flag validation and removal
    /// remain adapter responsibilities; this constructor prevents a flagged
    /// type from entering the typed leaf contract.
    pub const fn try_new(
        family: i32,
        socket_type: i32,
        protocol: i32,
        kernel_origin: bool,
    ) -> Option<Self> {
        if socket_type < 0 || socket_type & !SOCKET_TYPE_MASK != 0 {
            return None;
        }
        Some(Self {
            family,
            socket_type,
            protocol,
            kernel_origin,
        })
    }

    /// Returns the raw protocol family passed to the Linux hook leaf.
    pub const fn family(self) -> i32 {
        self.family
    }

    /// Returns the validated flag-free base socket type.
    pub const fn socket_type(self) -> i32 {
        self.socket_type
    }

    /// Returns the raw protocol value passed to the Linux hook leaf.
    pub const fn protocol(self) -> i32 {
        self.protocol
    }

    /// Reports whether this is a kernel-originated socket creation.
    pub const fn kernel_origin(self) -> bool {
        self.kernel_origin
    }
}

/// A nonnegative backlog already clamped by the embedding network policy.
///
/// Linux clamps the raw listen backlog against the target network namespace's
/// `somaxconn` before invoking the security leaf. This value records only that
/// prepared result; it neither owns the namespace policy nor chooses the cap.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SocketListenBacklog(i32);

impl SocketListenBacklog {
    /// Constructs a value from a consumer-clamped, nonnegative backlog.
    pub const fn try_from_clamped(backlog: i32) -> Option<Self> {
        if backlog < 0 {
            None
        } else {
            Some(Self(backlog))
        }
    }

    /// Returns the clamped backlog observed by the hook leaf.
    pub const fn get(self) -> i32 {
        self.0
    }
}

/// Raw socket-option selector observed by get-option and set-option leaves.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SocketOption {
    level: i32,
    name: i32,
}

impl SocketOption {
    /// Binds the raw level and option name selected by the consumer.
    pub const fn new(level: i32, name: i32) -> Self {
        Self { level, name }
    }

    /// Returns the raw socket-option level.
    pub const fn level(self) -> i32 {
        self.level
    }

    /// Returns the raw socket-option name.
    pub const fn name(self) -> i32 {
        self.name
    }
}

struct SocketFacts<'a, N: UserNamespaceView, S: ?Sized> {
    actor: &'a Credential<N>,
    socket: &'a S,
}

impl<'a, N: UserNamespaceView, S: ?Sized> SocketFacts<'a, N, S> {
    const fn new(actor: &'a Credential<N>, socket: &'a S) -> Self {
        Self { actor, socket }
    }

    const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    const fn socket(&self) -> &'a S {
        self.socket
    }
}

/// Immutable input to the pre-allocation `socket_create` policy leaf.
///
/// This context has no socket object because Linux invokes the leaf before
/// allocating one. It is intentionally non-`Copy`, and its fields are private
/// so external consumers cannot replace the actor or forge a flagged type.
///
/// ```compile_fail
/// use thekernel_linux_cred::{SocketCreateContext, UserNamespaceView};
///
/// fn inspect_private_fields<N: UserNamespaceView>(context: SocketCreateContext<'_, N>) {
///     let SocketCreateContext { actor, spec } = context;
///     let _ = (actor, spec);
/// }
/// ```
///
/// ```compile_fail
/// use thekernel_linux_cred::{SocketCreateContext, UserNamespaceView};
///
/// fn duplicate<N: UserNamespaceView>(context: SocketCreateContext<'_, N>) {
///     let moved = context;
///     let _ = context.actor();
///     let _ = moved;
/// }
/// ```
pub struct SocketCreateContext<'a, N: UserNamespaceView> {
    actor: &'a Credential<N>,
    spec: SocketCreateSpec,
}

impl<'a, N: UserNamespaceView> SocketCreateContext<'a, N> {
    /// Binds one immutable actor to one normalized creation request.
    pub const fn new(actor: &'a Credential<N>, spec: SocketCreateSpec) -> Self {
        Self { actor, spec }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Returns the normalized creation request.
    pub const fn spec(&self) -> SocketCreateSpec {
        self.spec
    }
}

/// Immutable input to the post-allocation `socket_post_create` policy leaf.
pub struct SocketPostCreateContext<'a, N: UserNamespaceView, S: ?Sized> {
    facts: SocketFacts<'a, N, S>,
    spec: SocketCreateSpec,
}

impl<'a, N: UserNamespaceView, S: ?Sized> SocketPostCreateContext<'a, N, S> {
    /// Binds the exact actor and created socket to the original create spec.
    pub const fn new(
        actor: &'a Credential<N>,
        created_socket: &'a S,
        spec: SocketCreateSpec,
    ) -> Self {
        Self {
            facts: SocketFacts::new(actor, created_socket),
            spec,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.facts.actor()
    }

    /// Borrows the newly created opaque socket object.
    pub const fn created_socket(&self) -> &'a S {
        self.facts.socket()
    }

    /// Returns the same normalized request admitted at create time.
    pub const fn spec(&self) -> SocketCreateSpec {
        self.spec
    }
}

/// Immutable input to the `socket_socketpair` policy leaf.
pub struct SocketPairContext<'a, N: UserNamespaceView, FirstSocket: ?Sized, SecondSocket: ?Sized> {
    actor: &'a Credential<N>,
    first_socket: &'a FirstSocket,
    second_socket: &'a SecondSocket,
}

impl<'a, N: UserNamespaceView, FirstSocket: ?Sized, SecondSocket: ?Sized>
    SocketPairContext<'a, N, FirstSocket, SecondSocket>
{
    /// Binds the actor and both newly allocated socket endpoints in order.
    pub const fn new(
        actor: &'a Credential<N>,
        first_socket: &'a FirstSocket,
        second_socket: &'a SecondSocket,
    ) -> Self {
        Self {
            actor,
            first_socket,
            second_socket,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the first socket endpoint supplied to the leaf.
    pub const fn first_socket(&self) -> &'a FirstSocket {
        self.first_socket
    }

    /// Borrows the second socket endpoint supplied to the leaf.
    pub const fn second_socket(&self) -> &'a SecondSocket {
        self.second_socket
    }
}

/// Immutable input to the `socket_bind` policy leaf.
pub struct SocketBindContext<'a, N: UserNamespaceView, S: ?Sized, A: ?Sized> {
    facts: SocketFacts<'a, N, S>,
    address: &'a A,
    address_length: usize,
}

impl<'a, N: UserNamespaceView, S: ?Sized, A: ?Sized> SocketBindContext<'a, N, S, A> {
    /// Binds a socket to the exact prepared address snapshot and byte length.
    pub const fn new(
        actor: &'a Credential<N>,
        socket: &'a S,
        address: &'a A,
        address_length: usize,
    ) -> Self {
        Self {
            facts: SocketFacts::new(actor, socket),
            address,
            address_length,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.facts.actor()
    }

    /// Borrows the exact opaque socket object.
    pub const fn socket(&self) -> &'a S {
        self.facts.socket()
    }

    /// Borrows the consumer-prepared opaque address snapshot.
    pub const fn address(&self) -> &'a A {
        self.address
    }

    /// Returns the prepared address byte length passed to the leaf.
    pub const fn address_length(&self) -> usize {
        self.address_length
    }
}

/// Immutable input to the `socket_connect` policy leaf.
pub struct SocketConnectContext<'a, N: UserNamespaceView, S: ?Sized, A: ?Sized> {
    facts: SocketFacts<'a, N, S>,
    address: &'a A,
    address_length: usize,
}

impl<'a, N: UserNamespaceView, S: ?Sized, A: ?Sized> SocketConnectContext<'a, N, S, A> {
    /// Binds a socket to the exact prepared peer-address snapshot and length.
    pub const fn new(
        actor: &'a Credential<N>,
        socket: &'a S,
        address: &'a A,
        address_length: usize,
    ) -> Self {
        Self {
            facts: SocketFacts::new(actor, socket),
            address,
            address_length,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.facts.actor()
    }

    /// Borrows the exact opaque socket object.
    pub const fn socket(&self) -> &'a S {
        self.facts.socket()
    }

    /// Borrows the consumer-prepared opaque peer-address snapshot.
    pub const fn address(&self) -> &'a A {
        self.address
    }

    /// Returns the prepared peer-address byte length passed to the leaf.
    pub const fn address_length(&self) -> usize {
        self.address_length
    }
}

/// Immutable input to the `socket_listen` policy leaf.
pub struct SocketListenContext<'a, N: UserNamespaceView, S: ?Sized> {
    facts: SocketFacts<'a, N, S>,
    backlog: SocketListenBacklog,
}

impl<'a, N: UserNamespaceView, S: ?Sized> SocketListenContext<'a, N, S> {
    /// Binds a socket to the backlog already clamped by network policy.
    pub const fn new(
        actor: &'a Credential<N>,
        socket: &'a S,
        backlog: SocketListenBacklog,
    ) -> Self {
        Self {
            facts: SocketFacts::new(actor, socket),
            backlog,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.facts.actor()
    }

    /// Borrows the exact opaque listening socket object.
    pub const fn socket(&self) -> &'a S {
        self.facts.socket()
    }

    /// Returns the consumer-clamped listen backlog.
    pub const fn backlog(&self) -> SocketListenBacklog {
        self.backlog
    }
}

/// Immutable input to the `socket_accept` policy leaf.
pub struct SocketAcceptContext<'a, N: UserNamespaceView, ListeningSocket: ?Sized, NewSocket: ?Sized>
{
    actor: &'a Credential<N>,
    listening_socket: &'a ListeningSocket,
    new_socket: &'a NewSocket,
}

impl<'a, N: UserNamespaceView, ListeningSocket: ?Sized, NewSocket: ?Sized>
    SocketAcceptContext<'a, N, ListeningSocket, NewSocket>
{
    /// Binds the actor, listening endpoint, and pre-accept new socket in order.
    pub const fn new(
        actor: &'a Credential<N>,
        listening_socket: &'a ListeningSocket,
        new_socket: &'a NewSocket,
    ) -> Self {
        Self {
            actor,
            listening_socket,
            new_socket,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the existing listening socket.
    pub const fn listening_socket(&self) -> &'a ListeningSocket {
        self.listening_socket
    }

    /// Borrows the newly allocated socket passed to the accept leaf.
    pub const fn new_socket(&self) -> &'a NewSocket {
        self.new_socket
    }
}

/// Immutable input to the `socket_sendmsg` policy leaf.
pub struct SocketSendMessageContext<'a, N: UserNamespaceView, S: ?Sized, M: ?Sized> {
    facts: SocketFacts<'a, N, S>,
    prepared_message: &'a M,
    size: usize,
}

impl<'a, N: UserNamespaceView, S: ?Sized, M: ?Sized> SocketSendMessageContext<'a, N, S, M> {
    /// Binds a socket to one prepared send-message snapshot and payload size.
    ///
    /// `prepared_message` must freeze any normalized/raw send flags because
    /// Linux's send-message security leaf has no separate flags parameter.
    pub const fn new(
        actor: &'a Credential<N>,
        socket: &'a S,
        prepared_message: &'a M,
        size: usize,
    ) -> Self {
        Self {
            facts: SocketFacts::new(actor, socket),
            prepared_message,
            size,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.facts.actor()
    }

    /// Borrows the exact opaque socket object.
    pub const fn socket(&self) -> &'a S {
        self.facts.socket()
    }

    /// Borrows the consumer-prepared opaque message snapshot.
    pub const fn prepared_message(&self) -> &'a M {
        self.prepared_message
    }

    /// Returns the payload size visible at the send-message leaf.
    pub const fn size(&self) -> usize {
        self.size
    }
}

/// Immutable input to the `socket_recvmsg` policy leaf.
pub struct SocketReceiveMessageContext<'a, N: UserNamespaceView, S: ?Sized, M: ?Sized> {
    facts: SocketFacts<'a, N, S>,
    prepared_message: &'a M,
    size: usize,
    flags: i32,
}

impl<'a, N: UserNamespaceView, S: ?Sized, M: ?Sized> SocketReceiveMessageContext<'a, N, S, M> {
    /// Binds one prepared receive-message snapshot, size, and raw leaf flags.
    pub const fn new(
        actor: &'a Credential<N>,
        socket: &'a S,
        prepared_message: &'a M,
        size: usize,
        flags: i32,
    ) -> Self {
        Self {
            facts: SocketFacts::new(actor, socket),
            prepared_message,
            size,
            flags,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.facts.actor()
    }

    /// Borrows the exact opaque socket object.
    pub const fn socket(&self) -> &'a S {
        self.facts.socket()
    }

    /// Borrows the consumer-prepared opaque message snapshot.
    pub const fn prepared_message(&self) -> &'a M {
        self.prepared_message
    }

    /// Returns the receive capacity visible at the leaf.
    pub const fn size(&self) -> usize {
        self.size
    }

    /// Returns the raw flags passed separately to the receive-message leaf.
    pub const fn flags(&self) -> i32 {
        self.flags
    }
}

/// Immutable input to the `socket_getsockname` policy leaf.
pub struct SocketGetSockNameContext<'a, N: UserNamespaceView, S: ?Sized> {
    facts: SocketFacts<'a, N, S>,
}

impl<'a, N: UserNamespaceView, S: ?Sized> SocketGetSockNameContext<'a, N, S> {
    /// Binds the actor to the socket whose local name will be queried.
    pub const fn new(actor: &'a Credential<N>, socket: &'a S) -> Self {
        Self {
            facts: SocketFacts::new(actor, socket),
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.facts.actor()
    }

    /// Borrows the exact opaque socket object.
    pub const fn socket(&self) -> &'a S {
        self.facts.socket()
    }
}

/// Immutable input to the `socket_getpeername` policy leaf.
pub struct SocketGetPeerNameContext<'a, N: UserNamespaceView, S: ?Sized> {
    facts: SocketFacts<'a, N, S>,
}

impl<'a, N: UserNamespaceView, S: ?Sized> SocketGetPeerNameContext<'a, N, S> {
    /// Binds the actor to the socket whose peer name will be queried.
    pub const fn new(actor: &'a Credential<N>, socket: &'a S) -> Self {
        Self {
            facts: SocketFacts::new(actor, socket),
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.facts.actor()
    }

    /// Borrows the exact opaque socket object.
    pub const fn socket(&self) -> &'a S {
        self.facts.socket()
    }
}

/// Immutable input to the `socket_getsockopt` policy leaf.
pub struct SocketGetOptionContext<'a, N: UserNamespaceView, S: ?Sized> {
    facts: SocketFacts<'a, N, S>,
    option: SocketOption,
}

impl<'a, N: UserNamespaceView, S: ?Sized> SocketGetOptionContext<'a, N, S> {
    /// Binds the actor and socket to one raw option selector.
    pub const fn new(actor: &'a Credential<N>, socket: &'a S, option: SocketOption) -> Self {
        Self {
            facts: SocketFacts::new(actor, socket),
            option,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.facts.actor()
    }

    /// Borrows the exact opaque socket object.
    pub const fn socket(&self) -> &'a S {
        self.facts.socket()
    }

    /// Returns the raw socket-option selector.
    pub const fn option(&self) -> SocketOption {
        self.option
    }
}

/// Immutable input to the `socket_setsockopt` policy leaf.
pub struct SocketSetOptionContext<'a, N: UserNamespaceView, S: ?Sized> {
    facts: SocketFacts<'a, N, S>,
    option: SocketOption,
}

impl<'a, N: UserNamespaceView, S: ?Sized> SocketSetOptionContext<'a, N, S> {
    /// Binds the actor and socket to one raw option selector.
    pub const fn new(actor: &'a Credential<N>, socket: &'a S, option: SocketOption) -> Self {
        Self {
            facts: SocketFacts::new(actor, socket),
            option,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.facts.actor()
    }

    /// Borrows the exact opaque socket object.
    pub const fn socket(&self) -> &'a S {
        self.facts.socket()
    }

    /// Returns the raw socket-option selector.
    pub const fn option(&self) -> SocketOption {
        self.option
    }
}

/// Immutable input to the `socket_shutdown` policy leaf.
pub struct SocketShutdownContext<'a, N: UserNamespaceView, S: ?Sized> {
    facts: SocketFacts<'a, N, S>,
    how: i32,
}

impl<'a, N: UserNamespaceView, S: ?Sized> SocketShutdownContext<'a, N, S> {
    /// Binds a socket to the raw shutdown direction observed by the leaf.
    ///
    /// The value is deliberately not converted to an enum: Linux security
    /// policy observes the raw `how` value before the transport operation.
    pub const fn new(actor: &'a Credential<N>, socket: &'a S, how: i32) -> Self {
        Self {
            facts: SocketFacts::new(actor, socket),
            how,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.facts.actor()
    }

    /// Borrows the exact opaque socket object.
    pub const fn socket(&self) -> &'a S {
        self.facts.socket()
    }

    /// Returns the raw shutdown `how` value.
    pub const fn how(&self) -> i32 {
        self.how
    }
}

/// Immutable input to the Unix-domain `unix_stream_connect` policy leaf.
///
/// The three socket roles intentionally have independent generic types. The
/// connecting endpoint, listening endpoint, and newly allocated accepted
/// endpoint cannot be interchanged without an explicit adapter decision.
pub struct UnixStreamConnectContext<
    'a,
    N: UserNamespaceView,
    ConnectingSocket: ?Sized,
    ListeningSocket: ?Sized,
    AcceptedSocket: ?Sized,
> {
    actor: &'a Credential<N>,
    connecting_socket: &'a ConnectingSocket,
    listening_socket: &'a ListeningSocket,
    accepted_socket: &'a AcceptedSocket,
}

impl<
    'a,
    N: UserNamespaceView,
    ConnectingSocket: ?Sized,
    ListeningSocket: ?Sized,
    AcceptedSocket: ?Sized,
> UnixStreamConnectContext<'a, N, ConnectingSocket, ListeningSocket, AcceptedSocket>
{
    /// Binds all three Unix stream connection roles in Linux leaf order.
    pub const fn new(
        actor: &'a Credential<N>,
        connecting_socket: &'a ConnectingSocket,
        listening_socket: &'a ListeningSocket,
        accepted_socket: &'a AcceptedSocket,
    ) -> Self {
        Self {
            actor,
            connecting_socket,
            listening_socket,
            accepted_socket,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the client-side connecting socket.
    pub const fn connecting_socket(&self) -> &'a ConnectingSocket {
        self.connecting_socket
    }

    /// Borrows the server-side listening socket.
    pub const fn listening_socket(&self) -> &'a ListeningSocket {
        self.listening_socket
    }

    /// Borrows the newly allocated server-side accepted socket.
    pub const fn accepted_socket(&self) -> &'a AcceptedSocket {
        self.accepted_socket
    }
}

/// Immutable input to the Unix-domain `unix_may_send` policy leaf.
pub struct UnixMaySendContext<
    'a,
    N: UserNamespaceView,
    SendingSocket: ?Sized,
    ReceivingSocket: ?Sized,
> {
    actor: &'a Credential<N>,
    sending_socket: &'a SendingSocket,
    receiving_socket: &'a ReceivingSocket,
}

impl<'a, N: UserNamespaceView, SendingSocket: ?Sized, ReceivingSocket: ?Sized>
    UnixMaySendContext<'a, N, SendingSocket, ReceivingSocket>
{
    /// Binds the sending and receiving Unix socket roles in Linux leaf order.
    pub const fn new(
        actor: &'a Credential<N>,
        sending_socket: &'a SendingSocket,
        receiving_socket: &'a ReceivingSocket,
    ) -> Self {
        Self {
            actor,
            sending_socket,
            receiving_socket,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the Unix socket sending the message.
    pub const fn sending_socket(&self) -> &'a SendingSocket {
        self.sending_socket
    }

    /// Borrows the Unix socket receiving the message.
    pub const fn receiving_socket(&self) -> &'a ReceivingSocket {
        self.receiving_socket
    }
}
