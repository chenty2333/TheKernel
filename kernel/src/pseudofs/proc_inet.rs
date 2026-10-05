//! Linux-shaped mandatory inet table columns from live transport observations.
use alloc::{format, string::String, sync::Arc};
use core::fmt::Write;

use axfs_ng_vfs::{VfsError, VfsResult};

use crate::{
    file::{
        current_file_operation_security_credential,
        netlink::{SocketDiagRecord, diagnostic_records},
    },
    task::NetworkNamespace,
};

fn address(bytes: &[u8; 16], family: u16) -> String {
    let mut out = String::new();
    let words = if family == 2 { 1 } else { 4 };
    for word in bytes[..words * 4].as_chunks::<4>().0 {
        let _ = write!(out, "{:08X}", u32::from_ne_bytes(*word));
    }
    out
}
fn row(index: usize, entry: &SocketDiagRecord) -> String {
    // Linux proc TCP does not display its delayed-ACK timer, and listeners'
    // transmit data queue is empty (inet_diag wqueue instead means backlog).
    let timer = if entry.timer == 5 { 0 } else { entry.timer };
    let when = if timer == 0 {
        0
    } else {
        entry.expires_ms as u64 * 100 / 1000
    };
    let send = if entry.state == 10 {
        0
    } else {
        entry.send_queue
    };
    let width = if entry.protocol == 17 { 5 } else { 4 };
    format!(
        "{index:width$}: {}:{:04X} {}:{:04X} {:02X} {send:08X}:{:08X} {timer:02X}:{when:08X} \
         {:08X} {:5} {:8} {}\n",
        address(&entry.src, entry.family),
        entry.sport,
        address(&entry.dst, entry.family),
        entry.dport,
        entry.state,
        entry.receive_queue,
        entry.retransmit_timeouts,
        entry.uid,
        entry.probes_sent,
        entry.inode
    )
}
fn header(family: u16) -> String {
    if family == 2 {
        format!(
            "{:<149}\n",
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  \
             timeout inode"
        )
    } else {
        String::from(
            "  sl  local_address                         remote_address                        st \
             tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n",
        )
    }
}
pub(super) fn tcp(namespace: &Arc<NetworkNamespace>, family: u16) -> VfsResult<String> {
    let actor = current_file_operation_security_credential().ok_or(VfsError::Io)?;
    let records = diagnostic_records(namespace, false, &actor, 1 << 6)?;
    let mut out = header(family);
    for (index, entry) in records
        .iter()
        .filter(|entry| entry.family == family && entry.protocol == 6 && entry.state != 7)
        .enumerate()
    {
        let row = row(index, entry);
        if family == 2 {
            let _ = writeln!(out, "{:<149}", row.trim_end_matches('\n'));
        } else {
            out.push_str(&row);
        }
    }
    // Native sock-lifetime/congestion observations for Linux's trailing
    // fields are not available yet; expose the real consumer prefix only,
    // not fabricated tail statistics or a claim of complete per-state grammar.
    Ok(out)
}

fn udp_header(family: u16) -> String {
    if family == 2 {
        format!(
            "{:<127}\n",
            "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  \
             timeout inode ref pointer drops"
        )
    } else {
        String::from(
            "  sl  local_address                         remote_address                        st \
             tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n",
        )
    }
}

pub(super) fn udp(namespace: &Arc<NetworkNamespace>, family: u16) -> VfsResult<String> {
    let actor = current_file_operation_security_credential().ok_or(VfsError::Io)?;
    let records = diagnostic_records(namespace, false, &actor, 1 << 17)?;
    let mut out = udp_header(family);
    for (index, entry) in records
        .iter()
        .filter(|entry| entry.family == family && entry.protocol == 17)
        .enumerate()
    {
        let row = row(index, entry);
        if family == 2 {
            let _ = writeln!(out, "{:<127}", row.trim_end_matches('\n'));
        } else {
            out.push_str(&row);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_word_addresses_and_mandatory_column_units_match_linux() {
        let mut ipv4 = [0; 16];
        ipv4[..4].copy_from_slice(&[127, 0, 0, 1]);
        let mut ipv6 = [0; 16];
        ipv6[15] = 1;
        assert_eq!(address(&ipv4, 2), "0100007F");
        assert_eq!(address(&ipv6, 10), "00000000000000000000000001000000");
        assert_eq!(header(2).len(), 150);
        assert!(header(2).contains("rem_address"));
        assert!(header(10).contains("remote_address"));
        // The wire integer order is fixed by x86_64-only project scope.
    }
    #[test]
    fn udp_header_preserves_linux_family_labels_and_ipv4_width() {
        assert_eq!(udp_header(2).len(), 128);
        assert!(udp_header(2).contains("rem_address"));
        assert!(udp_header(10).contains("remote_address"));
        assert!(udp_header(10).ends_with("inode ref pointer drops\n"));
    }
    #[test]
    fn row_preserves_live_mandatory_columns_listener_queue_and_timer_rules() {
        let mut entry = SocketDiagRecord {
            family: 2,
            protocol: 6,
            state: 1,
            cookie: 1,
            sport: 1234,
            dport: 80,
            src: [0; 16],
            dst: [0; 16],
            ifindex: 0,
            receive_queue: 9,
            send_queue: 17,
            uid: 1000,
            inode: 0x1_0000_0002,
            timer: 1,
            expires_ms: 999,
            retransmit_timeouts: 3,
            probes_sent: 2,
            retransmit_delay_ms: 1000,
            ack_delay_ms: 10,
        };
        entry.src[..4].copy_from_slice(&[127, 0, 0, 1]);
        let text = row(0, &entry);
        let fields: alloc::vec::Vec<_> = text.split_whitespace().collect();
        assert_eq!(
            fields,
            [
                "0:",
                "0100007F:04D2",
                "00000000:0050",
                "01",
                "00000011:00000009",
                "01:00000063",
                "00000003",
                "1000",
                "2",
                "4294967298"
            ]
        );
        entry.state = 10;
        entry.timer = 5;
        let text = row(0, &entry);
        let fields: alloc::vec::Vec<_> = text.split_whitespace().collect();
        assert_eq!(fields[4], "00000000:00000009");
        assert_eq!(fields[5], "00:00000000");
        entry.protocol = 17;
        entry.state = 7;
        entry.timer = 0;
        entry.send_queue = 6;
        entry.receive_queue = 19;
        entry.retransmit_timeouts = 0;
        entry.probes_sent = 0;
        let text = row(0, &entry);
        assert!(text.starts_with("    0:"));
        let fields: alloc::vec::Vec<_> = text.split_whitespace().collect();
        assert_eq!(fields[3], "07");
        assert_eq!(fields[4], "00000006:00000013");
        assert_eq!(&fields[5..7], &["00:00000000", "00000000"]);
    }
}
