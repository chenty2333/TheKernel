//! Minimal Linux rfkill character device event snapshot for published WLANs.

use core::any::Any;

use axfs_ng_vfs::{NodeFlags, VfsError, VfsResult};

use crate::pseudofs::DeviceOps;

const RFKILL_EVENT_BYTES: usize = 8;
const RFKILL_TYPE_WLAN: u8 = 1;
const RFKILL_OP_ADD: u8 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RfkillEvent {
    index: u32,
    kind: u8,
    operation: u8,
    soft: u8,
    hard: u8,
}

fn encode_event(event: RfkillEvent) -> [u8; RFKILL_EVENT_BYTES] {
    let index = event.index.to_ne_bytes();
    [
        index[0], index[1], index[2], index[3], event.kind, event.operation, event.soft,
        event.hard,
    ]
}

pub(crate) struct Rfkill;

impl DeviceOps for Rfkill {
    fn read_at(&self, output: &mut [u8], offset: u64) -> VfsResult<usize> {
        if !offset.is_multiple_of(RFKILL_EVENT_BYTES as u64) {
            return Err(VfsError::InvalidInput);
        }
        if output.len() < RFKILL_EVENT_BYTES {
            return Err(VfsError::InvalidInput);
        }
        let interfaces = axnet::wireless_interfaces();
        let mut position = usize::try_from(offset / RFKILL_EVENT_BYTES as u64)
            .map_err(|_| VfsError::InvalidInput)?;
        let mut written = 0;
        while output.len() - written >= RFKILL_EVENT_BYTES {
            let Some(interface) = interfaces.get(position) else {
                break;
            };
            let event = encode_event(RfkillEvent {
                index: interface.ifindex,
                kind: RFKILL_TYPE_WLAN,
                operation: RFKILL_OP_ADD,
                soft: u8::from(interface.soft_blocked),
                hard: u8::from(interface.hard_blocked),
            });
            output[written..written + RFKILL_EVENT_BYTES].copy_from_slice(&event);
            written += RFKILL_EVENT_BYTES;
            position += 1;
        }
        Ok(written)
    }

    fn write_at(&self, _input: &[u8], _offset: u64) -> VfsResult<usize> {
        // State changes are not accepted until the wireless adapter can
        // serialize an rfkill transition with its hardware bring-up path.
        Err(VfsError::OperationNotSupported)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfkill_event_uses_linux_record_order_and_native_index_endian() {
        let event = encode_event(RfkillEvent {
            index: 0x1234_5678,
            kind: RFKILL_TYPE_WLAN,
            operation: RFKILL_OP_ADD,
            soft: 1,
            hard: 0,
        });
        assert_eq!(&event[..4], &0x1234_5678u32.to_ne_bytes());
        assert_eq!(&event[4..], &[RFKILL_TYPE_WLAN, RFKILL_OP_ADD, 1, 0]);
    }
}
