mod addr;
mod cmsg;
mod io;
mod name;
mod opt;
mod packet;
mod socket;

use alloc::sync::Arc;

use axerrno::AxResult;
use axnet::options::UnixCredentials;
use axtask::current;

pub use self::{cmsg::*, io::*, name::*, opt::*, socket::*};
use crate::task::{AsThread, Cred, NetworkNamespace};

/// Keeps pure socket-output syscalls at the Linux hook boundary: policy sees
/// the exact pinned socket before the adapter reads any userspace output
/// length. A denial therefore cannot fault or otherwise touch that output.
fn import_socket_output_after_policy<T>(
    authorize: impl FnOnce() -> AxResult<()>,
    import_output: impl FnOnce() -> AxResult<T>,
) -> AxResult<T> {
    authorize()?;
    import_output()
}

/// One syscall-entry identity snapshot shared by socket policy and every
/// admitted side effect. Helpers must not resample `current()` after this value
/// has been created.
pub(super) struct SocketSyscallSnapshot {
    actor: Arc<Cred>,
    net_namespace: Arc<NetworkNamespace>,
    pid: u32,
    umask: u32,
    unix_credentials: UnixCredentials,
}

impl SocketSyscallSnapshot {
    pub(super) fn capture() -> Self {
        let current = current();
        let thread = current.as_thread();
        let actor = thread.current_cred();
        let pid = thread.proc_data.proc.pid() as u32;
        let ids = actor.ids();
        Self {
            actor,
            net_namespace: thread.proc_data.net_ns.clone(),
            pid,
            umask: thread.fs_context().lock().umask(),
            unix_credentials: UnixCredentials::new(pid, ids.euid.into_raw(), ids.egid.into_raw()),
        }
    }

    pub(super) fn actor(&self) -> &Arc<Cred> {
        &self.actor
    }

    pub(super) fn net_namespace(&self) -> &Arc<NetworkNamespace> {
        &self.net_namespace
    }

    pub(super) const fn pid(&self) -> u32 {
        self.pid
    }

    pub(super) const fn umask(&self) -> u32 {
        self.umask
    }

    pub(super) const fn unix_credentials(&self) -> UnixCredentials {
        self.unix_credentials
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use axerrno::AxError;

    use super::import_socket_output_after_policy;

    #[test]
    fn denied_socket_output_policy_wins_over_an_invalid_user_length_pointer() {
        let imports = Cell::new(0);
        let result = import_socket_output_after_policy(
            || Err(AxError::PermissionDenied),
            || {
                imports.set(imports.get() + 1);
                Err::<usize, _>(AxError::BadAddress)
            },
        );

        assert_eq!(result, Err(AxError::PermissionDenied));
        assert_eq!(imports.get(), 0);
    }

    #[test]
    fn admitted_socket_output_imports_the_user_length_once() {
        let imports = Cell::new(0);
        let result = import_socket_output_after_policy(
            || Ok(()),
            || {
                imports.set(imports.get() + 1);
                Ok(32usize)
            },
        );

        assert_eq!(result, Ok(32));
        assert_eq!(imports.get(), 1);
    }
}
