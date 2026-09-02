use alloc::sync::Arc;
use core::bstr::ByteStr;

use axerrno::LinuxResult;
use axnet::{
    RecvOptions, SocketOps,
    unix::{DgramTransport, UnixNamespace, UnixSocket},
};

use crate::file::permission::VfsSecurityContext;

pub fn bind_dev_log(
    fs: &axfs::FsContext,
    security: &VfsSecurityContext,
    unix_namespace: Arc<UnixNamespace>,
) -> LinuxResult<()> {
    let server = UnixSocket::new(DgramTransport::new()?, unix_namespace);
    crate::file::unix_socket::bind_precreated_path(
        &server,
        crate::file::unix_socket::try_path(axfs_ng_vfs::FsPath::new(b"/dev/log"))?,
        security,
        fs,
    )?;
    // The socket is already bound at this point, so a discarded spawn failure
    // would leave `/dev/log` present with nothing draining it while this
    // function still reported success. Propagate instead: the caller treats a
    // failure to construct the boot device tree as fatal.
    axtask::spawn_with_name(
        move || {
            let mut buf = [0u8; 65536];
            loop {
                match server.recv(&mut buf[..], RecvOptions::default()) {
                    Ok(0) => break,
                    Ok(read) => {
                        let msg = ByteStr::new(buf[..read].trim_ascii_end());
                        if !msg.is_empty() {
                            info!("{msg}");
                        }
                    }
                    Err(err) => {
                        warn!("Failed to receive logs from client: {err:?}");
                        break;
                    }
                }
            }
        },
        "dev-log-server".into(),
    )?;
    Ok(())
}

#[cfg(test)]
const _: fn(&axfs::FsContext, &VfsSecurityContext, Arc<UnixNamespace>) -> LinuxResult<()> =
    bind_dev_log;
