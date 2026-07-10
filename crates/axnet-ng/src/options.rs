use core::time::Duration;

use axerrno::{AxError, AxResult, LinuxError};
use enum_dispatch::enum_dispatch;

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

/// Corresponds to `struct ucred` in Linux.
#[repr(C)]
#[derive(Default, Debug, Clone, Copy, Eq, PartialEq)]
pub struct UnixCredentials {
    /// Process ID.
    pub pid: u32,
    /// User ID.
    pub uid: u32,
    /// Group ID.
    pub gid: u32,
}
impl UnixCredentials {
    /// Credentials reported when Linux has no connected peer snapshot.
    pub const UNKNOWN: Self = Self::new(0, u32::MAX, u32::MAX);

    /// Create a new credential snapshot.
    pub const fn new(pid: u32, uid: u32, gid: u32) -> Self {
        UnixCredentials { pid, uid, gid }
    }
}

define_options! {
    // ---- Socket level options (SO_*) ----
    ReuseAddress(bool),
    Error(i32),
    DontRoute(bool),
    SendBuffer(usize),
    ReceiveBuffer(usize),
    KeepAlive(bool),
    SendTimeout(Duration),
    ReceiveTimeout(Duration),
    SendBufferForce(usize),
    ReceiveBufferForce(usize),
    PassCredentials(bool),
    PeerCredentials(UnixCredentials),

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
                Err(AxError::from(LinuxError::ENOPROTOOPT))
            } else {
                Ok(())
            }
        })
    }
    /// Set a socket option. Dispatches to [`Configurable::set_option_inner`].
    fn set_option(&self, opt: SetSocketOption) -> AxResult {
        self.set_option_inner(opt).and_then(|supported| {
            if !supported {
                Err(AxError::from(LinuxError::ENOPROTOOPT))
            } else {
                Ok(())
            }
        })
    }
}
