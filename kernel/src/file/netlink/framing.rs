//! The ordinary kernel netlink receiver stops when the next header fails
//! NLMSG_OK; trailing allocation padding is not a second malformed request.
//! Linux 7.2.3 net/netlink/af_netlink.c netlink_rcv_skb behavior reference.

const HEADER_BYTES: usize = 16;

pub(super) fn ordinary_prefix_len(data: &[u8]) -> usize {
    let mut offset = 0;
    while data.len() - offset >= HEADER_BYTES {
        let length = u32::from_ne_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        if length < HEADER_BYTES || length > data.len() - offset {
            break;
        }
        // A complete final message need not provide every alignment byte;
        // the Linux receiver consumes at most the bytes remaining in the skb.
        offset = offset
            .saturating_add(length.saturating_add(3) & !3)
            .min(data.len());
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dump_request_in_larger_zeroed_buffer_has_only_one_message() {
        let mut bytes = [0; 156];
        bytes[..4].copy_from_slice(&36u32.to_ne_bytes());
        assert_eq!(ordinary_prefix_len(&bytes), 36);
        assert_eq!(ordinary_prefix_len(&[0; 16]), 0);
        assert_eq!(ordinary_prefix_len(&[0; 3]), 0);
    }
    #[test]
    fn incomplete_tail_stops_without_reinterpreting_or_rejecting_valid_prefix() {
        let mut bytes = [0; 48];
        bytes[..4].copy_from_slice(&16u32.to_ne_bytes());
        bytes[16..20].copy_from_slice(&64u32.to_ne_bytes());
        assert_eq!(ordinary_prefix_len(&bytes), 16);
        bytes[16..20].copy_from_slice(&16u32.to_ne_bytes());
        assert_eq!(ordinary_prefix_len(&bytes[..35]), 32);
        bytes[..4].copy_from_slice(&17u32.to_ne_bytes());
        assert_eq!(ordinary_prefix_len(&bytes[..18]), 18);
    }
}
