use core::bstr::ByteStr;

use axerrno::LinuxResult;
use axfs_ng_vfs::NodePermission;
use axnet::{
    RecvOptions, SocketOps,
    unix::{DgramTransport, UnixSocket},
};
use axtask::current;

use crate::task::AsThread;

pub fn bind_dev_log() -> LinuxResult<()> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let server = UnixSocket::new(DgramTransport::new()?, proc_data.net_ns.unix_namespace());
    let credentials = proc_data.fs_dac_credentials();
    crate::file::unix_socket::bind_path(
        &server,
        crate::file::unix_socket::try_path("/dev/log")?,
        &credentials,
        NodePermission::from_bits_truncate(0o666),
        0,
    )?;
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
    );
    Ok(())
}
