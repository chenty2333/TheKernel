//! Linux-shaped live Unix OFD table; opaque addresses never expose kernel pointers.
use alloc::{format, sync::Arc, vec::Vec};

use axfs_ng_vfs::VfsResult;
use axnet::unix::UnixSocketAddr;

use crate::{
    file::netlink::{UnixSocketRecord, unix_records},
    task::NetworkNamespace,
};

fn row(out: &mut Vec<u8>, record: &UnixSocketRecord) {
    let flags = if record.snapshot.listening {
        0x10000
    } else {
        0
    };
    let state = if record.snapshot.connected { 3 } else { 1 };
    out.extend_from_slice(
        format!(
            "0000000000000000: {:08X} 00000000 {flags:08X} {:04X} {state:02X} {:5}",
            record.owners, record.snapshot.socket_type, record.inode,
        )
        .as_bytes(),
    );
    match &record.snapshot.address {
        UnixSocketAddr::Unnamed => {}
        UnixSocketAddr::Path(path) => {
            out.push(b' ');
            out.extend_from_slice(path);
        }
        UnixSocketAddr::Abstract(name) => {
            out.extend_from_slice(b" @");
            out.extend(
                name.iter()
                    .map(|byte| if *byte == 0 { b'@' } else { *byte }),
            );
        }
    }
    out.push(b'\n');
}
pub(super) fn snapshot(namespace: &Arc<NetworkNamespace>) -> VfsResult<Vec<u8>> {
    let mut out = Vec::from(&b"Num       RefCount Protocol Flags    Type St Inode Path\n"[..]);
    for record in unix_records(namespace)? {
        row(&mut out, &record);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn row_preserves_type_state_inode_and_raw_path_bytes() {
        let mut record = UnixSocketRecord {
            inode: 0x1_0000_0002,
            owners: 3,
            snapshot: axnet::unix::UnixDiagnosticSnapshot {
                socket_type: 5,
                connected: false,
                listening: true,
                address: UnixSocketAddr::Abstract(Arc::new(Vec::from(&b"a\0b\xff"[..]))),
            },
        };
        let mut out = Vec::new();
        row(&mut out, &record);
        assert_eq!(
            out,
            b"0000000000000000: 00000003 00000000 00010000 0005 01 4294967298 @a@b\xff\n"
        );
        record.snapshot.connected = true;
        record.snapshot.listening = false;
        record.snapshot.address = UnixSocketAddr::Path(Arc::new(Vec::from(&b"/raw-\xff-path"[..])));
        out.clear();
        row(&mut out, &record);
        assert!(out.ends_with(b"00000000 0005 03 4294967298 /raw-\xff-path\n"));
    }
}
