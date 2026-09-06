use core::time::Duration;

use axerrno::{AxError, AxResult};
use enum_dispatch::enum_dispatch;

/// A deferred socket fault, independent of any operating-system errno.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SocketFault {
    ConnectionRefused = 1,
    ConnectionReset = 2,
    Other = 3,
    TimedOut = 4,
}

impl SocketFault {
    pub const fn as_ax_error(self) -> AxError {
        match self {
            Self::ConnectionRefused => AxError::ConnectionRefused,
            Self::ConnectionReset => AxError::ConnectionReset,
            Self::Other => AxError::Io,
            Self::TimedOut => AxError::TimedOut,
        }
    }

    pub fn from_ax_error(error: AxError) -> Self {
        if error == AxError::ConnectionRefused {
            Self::ConnectionRefused
        } else if error == AxError::ConnectionReset {
            Self::ConnectionReset
        } else if error == AxError::TimedOut {
            Self::TimedOut
        } else {
            Self::Other
        }
    }

    pub const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::ConnectionRefused),
            2 => Some(Self::ConnectionReset),
            3 => Some(Self::Other),
            4 => Some(Self::TimedOut),
            _ => None,
        }
    }
}

macro_rules! define_options {
    ($($name:ident($value:ty),)*) => {
        /// Operation to get a socket option.
        ///
        /// See [`Configurable::get_option`].
        #[allow(missing_docs)]
        pub enum GetSocketOption<'a> {
            $(
                $name(&'a mut $value),
            )*
        }

        /// Operation to set a socket option.
        ///
        /// See [`Configurable::set_option`].
        #[allow(missing_docs)]
        #[derive(Clone, Copy)]
        pub enum SetSocketOption<'a> {
            $(
                $name(&'a $value),
            )*
        }
    };
}

/// Identity snapshot associated with a local socket peer.
#[derive(Default, Debug, Clone, Copy, Eq, PartialEq)]
pub struct SocketCredentials {
    /// Process ID.
    pub pid: u32,
    /// User ID.
    pub uid: u32,
    /// Group ID.
    pub gid: u32,
}
impl SocketCredentials {
    /// Credentials reported when no connected peer snapshot exists.
    pub const UNKNOWN: Self = Self::new(0, u32::MAX, u32::MAX);

    /// Create a new credential snapshot.
    pub const fn new(pid: u32, uid: u32, gid: u32) -> Self {
        SocketCredentials { pid, uid, gid }
    }
}

define_options! {
    // ---- Socket level options (SO_*) ----
    ReuseAddress(bool),
    Error(Option<SocketFault>),
    DontRoute(bool),
    SendBuffer(usize),
    ReceiveBuffer(usize),
    KeepAlive(bool),
    SendTimeout(Duration),
    ReceiveTimeout(Duration),
    SendBufferForce(usize),
    ReceiveBufferForce(usize),
    PassCredentials(bool),
    PeerCredentials(SocketCredentials),

    // --- TCP level options (TCP_*) ----
    NoDelay(bool),
    MaxSegment(usize),

    // ---- IP level options (IP_*) ----
    Ttl(u8),
    Ipv6Only(bool),

    // ---- Extra options ----
    NonBlocking(bool),
}

/// Trait for configurable socket-like objects.
#[enum_dispatch]
pub trait Configurable {
    /// Returns whether the socket is in non-blocking mode.
    fn nonblocking(&self) -> bool;

    /// Get a socket option, returns `true` if the socket supports the option.
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> AxResult<bool>;
    /// Set a socket option, returns `true` if the socket supports the option.
    fn set_option_inner(&self, opt: SetSocketOption) -> AxResult<bool>;

    /// Get a socket option. Dispatches to [`Configurable::get_option_inner`].
    fn get_option(&self, mut opt: GetSocketOption) -> AxResult {
        self.get_option_inner(&mut opt).and_then(|supported| {
            if !supported {
                Err(AxError::OperationNotSupported)
            } else {
                Ok(())
            }
        })
    }
    /// Set a socket option. Dispatches to [`Configurable::set_option_inner`].
    fn set_option(&self, opt: SetSocketOption) -> AxResult {
        self.set_option_inner(opt).and_then(|supported| {
            if !supported {
                Err(AxError::OperationNotSupported)
            } else {
                Ok(())
            }
        })
    }
}
