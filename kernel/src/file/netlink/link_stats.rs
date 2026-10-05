//! Linux link-statistics layout backed by the network device's observations.
use super::*;

const IFLA_STATS: u16 = 7;
const IFLA_STATS64: u16 = 23;
const IFLA_PAD: u16 = 42;

fn fields(stats: axnet::DeviceStats) -> [u64; 25] {
    let mut fields = [0; 25];
    fields[..8].copy_from_slice(&[
        stats.rx_packets,
        stats.tx_packets,
        stats.rx_bytes,
        stats.tx_bytes,
        stats.rx_errors,
        stats.tx_errors,
        stats.rx_dropped,
        stats.tx_dropped,
    ]);
    // This provider does not classify detailed hardware errors, multicast,
    // collisions, compressed packets or nohandler/otherhost drops. Like a
    // Linux driver without those optional counters, their fields remain 0.
    fields
}

pub(super) fn append_attributes(payload: &mut Vec<u8>, stats: axnet::DeviceStats) {
    let fields = fields(stats);
    let mut narrow = [0; 100];
    let mut wide = [0; 200];
    for (index, field) in fields.into_iter().enumerate() {
        narrow[index * 4..index * 4 + 4].copy_from_slice(&(field as u32).to_ne_bytes());
        wide[index * 8..index * 8 + 8].copy_from_slice(&field.to_ne_bytes());
    }
    push_attr(payload, IFLA_STATS, &narrow);
    // Account for the attribute header; the enclosing nlmsg header is 16B.
    if !(payload.len() + 4).is_multiple_of(8) {
        push_attr(payload, IFLA_PAD, &[]);
    }
    push_attr(payload, IFLA_STATS64, &wide);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_stat_layouts_preserve_observed_counts_and_low_word_conversion() {
        let stats = axnet::DeviceStats {
            rx_bytes: 0x1_0000_0002,
            rx_packets: 3,
            rx_errors: 5,
            rx_dropped: 7,
            tx_bytes: 11,
            tx_packets: 13,
            tx_errors: 17,
            tx_dropped: 19,
        };
        let mut payload = Vec::new();
        append_attributes(&mut payload, stats);
        let mut narrow = None;
        let mut wide = None;
        for_each_rtattr(&payload, |kind, value| {
            match kind {
                IFLA_STATS => narrow = Some(value),
                IFLA_STATS64 => wide = Some(value),
                _ => {}
            }
            Ok(())
        })
        .unwrap();
        let (narrow, wide) = (narrow.unwrap(), wide.unwrap());
        assert_eq!((narrow.len(), wide.len()), (100, 200));
        for (index, field) in fields(stats).into_iter().enumerate() {
            assert_eq!(
                &narrow[index * 4..index * 4 + 4],
                &(field as u32).to_ne_bytes()
            );
            assert_eq!(&wide[index * 8..index * 8 + 8], &field.to_ne_bytes());
        }
    }
}
