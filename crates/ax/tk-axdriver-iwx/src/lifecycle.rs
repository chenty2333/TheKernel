//! Multicast setup command from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::{CommandError, EncodedCommand, HostCommand};

pub const MCAST_FILTER_COMMAND: u8 = 0xd0;
pub const MCAST_FILTER_PAYLOAD_BYTES: usize = 12;

/// Build a pass-all multicast filter associated with the current BSSID.
// upstream: if_iwx.c iwx_allow_mcast()
pub fn allow_multicast_command(bssid: [u8; 6], slot: u8) -> Result<EncodedCommand, CommandError> {
    let mut payload = [0u8; MCAST_FILTER_PAYLOAD_BYTES];
    payload[0] = 1;
    payload[1] = 0;
    payload[2] = 0;
    payload[3] = 1;
    payload[4..10].copy_from_slice(&bssid);
    let command = HostCommand {
        id: u32::from(MCAST_FILTER_COMMAND),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    EncodedCommand::encode(&command, slot, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multicast_filter_payload_sets_own_port_pass_all_and_bssid() {
        let command = allow_multicast_command([1, 2, 3, 4, 5, 6], 0).unwrap();
        assert_eq!(command.bytes.len(), 8 + MCAST_FILTER_PAYLOAD_BYTES);
        assert_eq!(&command.bytes[8..12], &[1, 0, 0, 1]);
        assert_eq!(&command.bytes[12..18], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(&command.bytes[18..20], &[0, 0]);
    }
}
