//! Driver-private peer state initialized with OpenBSD's zeroed iwx node.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxvar.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::RxDuplicateState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IwxPeerNode {
    pub phy_context_id: Option<u8>,
    pub mac_address: [u8; 6],
    pub station_id: u16,
    pub station_color: u16,
    pub duplicate_state: RxDuplicateState,
    pub key_flags: u8,
}

impl Default for IwxPeerNode {
    fn default() -> Self {
        Self {
            phy_context_id: None,
            mac_address: [0; 6],
            station_id: 0,
            station_color: 0,
            duplicate_state: RxDuplicateState::new(),
            key_flags: 0,
        }
    }
}

/// Allocate a zero-initialized driver extension for an 802.11 peer.
// upstream: if_iwx.c iwx_node_alloc()
pub fn allocate_peer_node() -> IwxPeerNode {
    IwxPeerNode::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iwx_node_allocation_zeroes_driver_owned_extension() {
        let node = allocate_peer_node();
        assert_eq!(node.phy_context_id, None);
        assert_eq!(node.mac_address, [0; 6]);
        assert_eq!((node.station_id, node.station_color), (0, 0));
        assert_eq!(node.duplicate_state, RxDuplicateState::new());
        assert_eq!(node.key_flags, 0);
    }
}
