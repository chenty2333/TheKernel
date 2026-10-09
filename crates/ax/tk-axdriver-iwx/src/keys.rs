//! Pairwise/group key programming from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::{CMD_ASYNC, CMD_WANT_RESPONSE, CommandError, EncodedCommand, HostCommand};

pub const SEC_KEY_COMMAND: u8 = 0x18;
pub const ADD_STA_KEY_COMMAND: u8 = 0x17;
pub const MGMT_MCAST_KEY_COMMAND: u8 = 0x1f;
pub const DATA_PATH_GROUP: u8 = 0x05;
pub const SEC_KEY_CIPHER_CCMP: u32 = 0x02;
pub const SEC_KEY_FLAG_MFP: u32 = 0x20;
pub const SEC_KEY_FLAG_MCAST: u32 = 0x40;
pub const STA_KEY_FLAG_CCM: u16 = 0x02;
pub const STA_KEY_FLAG_WEP_KEY_MAP: u16 = 1 << 3;
pub const STA_KEY_FLAG_KEY_ID_SHIFT: u16 = 8;
pub const STA_KEY_FLAG_KEY_ID_MASK: u16 = 3 << STA_KEY_FLAG_KEY_ID_SHIFT;
pub const STA_KEY_NOT_VALID: u16 = 1 << 11;
pub const STA_KEY_FLAG_MULTICAST: u16 = 1 << 14;
pub const STA_KEY_FLAG_MFP: u16 = 1 << 15;
pub const ADD_STA_SUCCESS: u32 = 1;
pub const ADD_STA_STATUS_MASK: u32 = 0xff;
pub const NODE_HAVE_PAIRWISE_KEY: u8 = 1 << 0;
pub const NODE_HAVE_GROUP_KEY: u8 = 1 << 1;
pub const NODE_HAVE_INTEGRITY_GROUP_KEY: u8 = 1 << 2;
pub const SETKEY_QUEUE_CAPACITY: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCipher {
    Ccmp,
    Bip,
    Other(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyConfig {
    pub cipher: KeyCipher,
    pub key_id: u8,
    pub key: [u8; 32],
    pub key_len: u8,
    pub tx_sequence: u64,
    pub mgmt_rx_sequence: u64,
    pub group: bool,
    pub integrity_group: bool,
    pub node_mfp: bool,
    pub is_pairwise_key_slot: bool,
}

impl Default for KeyConfig {
    fn default() -> Self {
        Self {
            cipher: KeyCipher::Other(0),
            key_id: 0,
            key: [0; 32],
            key_len: 0,
            tx_sequence: 0,
            mgmt_rx_sequence: 0,
            group: false,
            integrity_group: false,
            node_mfp: false,
            is_pairwise_key_slot: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    Command(CommandError),
    InvalidIgtk,
    InvalidKeyLength,
    InvalidResponse,
    InvalidStation,
}
impl From<CommandError> for KeyError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

/// Build the MLD SEC_KEY add/remove command and context flags.
// upstream: if_iwx.c iwx_mld_set_sta_key_cmd()
pub fn mld_station_key_command(
    key: &KeyConfig,
    station_id: u8,
    remove: bool,
    slot: u8,
) -> Result<EncodedCommand, KeyError> {
    if key.key_len > 32 {
        return Err(KeyError::InvalidKeyLength);
    }
    if station_id >= 32 {
        return Err(KeyError::InvalidStation);
    }
    let mut flags = SEC_KEY_CIPHER_CCMP;
    if key.group {
        flags |= SEC_KEY_FLAG_MCAST;
    } else if key.integrity_group {
        flags |= SEC_KEY_FLAG_MCAST | SEC_KEY_FLAG_MFP;
    } else if key.node_mfp {
        flags |= SEC_KEY_FLAG_MFP;
    }
    let mut payload = [0u8; 80];
    payload[0..4].copy_from_slice(&(if remove { 3u32 } else { 1 }).to_le_bytes());
    payload[4..8].copy_from_slice(&(1u32 << station_id).to_le_bytes());
    payload[8..12].copy_from_slice(&u32::from(key.key_id).to_le_bytes());
    payload[12..16].copy_from_slice(&flags.to_le_bytes());
    payload[16..16 + usize::from(key.key_len)]
        .copy_from_slice(&key.key[..usize::from(key.key_len)]);
    payload[72..80].copy_from_slice(&key.tx_sequence.to_le_bytes());
    let command = HostCommand {
        id: (u32::from(DATA_PATH_GROUP) << 8) | u32::from(SEC_KEY_COMMAND),
        flags: if remove { CMD_ASYNC } else { 0 },
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, 0)?)
}

/// Build legacy ADD_STA_KEY API-v2 command and response ticket.
// upstream: if_iwx.c iwx_add_sta_key_cmd()
pub fn legacy_station_key_command(
    key: &KeyConfig,
    station_id: u8,
    slot: u8,
) -> Result<EncodedCommand, KeyError> {
    if key.key_len > 32 {
        return Err(KeyError::InvalidKeyLength);
    }
    let mut payload = [0u8; 76];
    payload[0] = station_id;
    payload[1] = if key.group { 1 } else { 0 };
    let mut flags = STA_KEY_FLAG_CCM
        | STA_KEY_FLAG_WEP_KEY_MAP
        | ((u16::from(key.key_id) << STA_KEY_FLAG_KEY_ID_SHIFT) & STA_KEY_FLAG_KEY_ID_MASK);
    if key.group {
        flags |= STA_KEY_FLAG_MULTICAST;
    } else if key.node_mfp {
        flags |= STA_KEY_FLAG_MFP;
    }
    payload[2..4].copy_from_slice(&flags.to_le_bytes());
    payload[4..4 + usize::from(key.key_len)].copy_from_slice(&key.key[..usize::from(key.key_len)]);
    payload[68..76].copy_from_slice(&key.tx_sequence.to_le_bytes());
    let command = HostCommand {
        id: u32::from(ADD_STA_KEY_COMMAND),
        flags: CMD_WANT_RESPONSE,
        response_capacity: 8,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, 0)?)
}

/// Validate ADD_STA_KEY reply status using the station command status mask.
// upstream: if_iwx.c iwx_add_sta_key_cmd()
pub fn validate_legacy_key_response(response: &[u8]) -> Result<(), KeyError> {
    let status = response.get(..4).ok_or(KeyError::InvalidResponse)?;
    if u32::from_le_bytes(status.try_into().unwrap()) & ADD_STA_STATUS_MASK == ADD_STA_SUCCESS {
        Ok(())
    } else {
        Err(KeyError::InvalidResponse)
    }
}

/// Build management multicast key v1/v2 add/remove layouts for BIP IGTKs.
// upstream: if_iwx.c iwx_set_sta_igtk()
pub fn igtk_command(
    key: &KeyConfig,
    station_id: u8,
    multi_queue_rx: bool,
    remove: bool,
    slot: u8,
) -> Result<EncodedCommand, KeyError> {
    if key.is_pairwise_key_slot
        || !(4..=7).contains(&key.key_id)
        || key.cipher != KeyCipher::Bip
        || key.key_len > 32
    {
        return Err(KeyError::InvalidIgtk);
    }
    let mut payload = alloc::vec![0u8; if multi_queue_rx { 52 } else { 68 }];
    let flags = if remove {
        u32::from(STA_KEY_NOT_VALID)
    } else if key.cipher == KeyCipher::Bip {
        u32::from(STA_KEY_FLAG_CCM)
    } else {
        return Err(KeyError::InvalidIgtk);
    };
    payload[0..4].copy_from_slice(&flags.to_le_bytes());
    if !remove {
        payload[4..4 + usize::from(key.key_len)]
            .copy_from_slice(&key.key[..usize::from(key.key_len)]);
    }
    let key_id_offset = if multi_queue_rx { 36 } else { 52 };
    payload[key_id_offset..key_id_offset + 4].copy_from_slice(&u32::from(key.key_id).to_le_bytes());
    payload[key_id_offset + 4..key_id_offset + 8]
        .copy_from_slice(&u32::from(station_id).to_le_bytes());
    if !remove {
        payload[key_id_offset + 8..key_id_offset + 16]
            .copy_from_slice(&key.mgmt_rx_sequence.to_le_bytes());
    }
    let command = HostCommand {
        id: u32::from(MGMT_MCAST_KEY_COMMAND),
        flags: if remove { CMD_ASYNC } else { 0 },
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, 0)?)
}

/// Select the legacy key command or the newer MLD key command by firmware API.
// upstream: if_iwx.c iwx_add_sta_key()
pub fn station_key_command(
    key: &KeyConfig,
    station_id: u8,
    security_key_version: u8,
    multi_queue_rx: bool,
    slot: u8,
) -> Result<EncodedCommand, KeyError> {
    if security_key_version != 0 && security_key_version != 99 {
        mld_station_key_command(key, station_id, false, slot)
    } else if key.integrity_group {
        igtk_command(key, station_id, multi_queue_rx, false, slot)
    } else {
        legacy_station_key_command(key, station_id, slot)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyTracking {
    pub installed_mask: u8,
    pub group_tx_key: u8,
    pub igtk_id: u8,
    pub port_valid: bool,
    pub tx_mgmt_protected: bool,
    pub link_up: bool,
}
impl Default for KeyTracking {
    fn default() -> Self {
        Self {
            installed_mask: 0,
            group_tx_key: 0,
            igtk_id: 0,
            port_valid: false,
            tx_mgmt_protected: false,
            link_up: false,
        }
    }
}

/// Apply successful key installation bookkeeping and open the RSN port when complete.
// upstream: if_iwx.c iwx_add_sta_key()
pub fn key_install_succeeded(tracking: &mut KeyTracking, key: &KeyConfig) {
    if key.integrity_group {
        tracking.installed_mask |= NODE_HAVE_INTEGRITY_GROUP_KEY;
        tracking.igtk_id = key.key_id;
    } else if key.group {
        tracking.installed_mask |= NODE_HAVE_GROUP_KEY;
        tracking.group_tx_key = key.key_id;
    } else {
        tracking.installed_mask |= NODE_HAVE_PAIRWISE_KEY;
    }
    let needed = NODE_HAVE_PAIRWISE_KEY
        | NODE_HAVE_GROUP_KEY
        | if key.node_mfp {
            NODE_HAVE_INTEGRITY_GROUP_KEY
        } else {
            0
        };
    if tracking.installed_mask & needed == needed {
        if key.node_mfp {
            tracking.tx_mgmt_protected = true;
        }
        tracking.port_valid = true;
        tracking.link_up = true;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetKeyDecision {
    SoftwareFallback,
    Queued,
    QueueFull,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SetKeyQueue {
    entries: [Option<(u8, KeyConfig)>; SETKEY_QUEUE_CAPACITY],
    head: usize,
    tail: usize,
    count: usize,
}

fn queue_key_install(queue: &mut SetKeyQueue, station_id: u8, key: KeyConfig) -> SetKeyDecision {
    if key.cipher != KeyCipher::Ccmp && !key.integrity_group {
        return SetKeyDecision::SoftwareFallback;
    }
    if queue.count >= SETKEY_QUEUE_CAPACITY {
        return SetKeyDecision::QueueFull;
    }
    queue.entries[queue.head] = Some((station_id, key));
    queue.head = (queue.head + 1) % SETKEY_QUEUE_CAPACITY;
    queue.count += 1;
    SetKeyDecision::Queued
}

/// Select software crypto or enqueue hardware CCMP/IGTK setup, retaining fallback group state.
// upstream: if_iwx.c iwx_set_key()
pub fn set_key<E>(
    queue: &mut SetKeyQueue,
    tracking: &mut KeyTracking,
    station_id: u8,
    key: KeyConfig,
    mut software_crypto: impl FnMut(&KeyConfig) -> Result<(), E>,
) -> Result<SetKeyDecision, E> {
    if key.cipher != KeyCipher::Ccmp && !key.integrity_group {
        software_crypto(&key)?;
        if key.group {
            tracking.installed_mask |= NODE_HAVE_GROUP_KEY;
        }
        return Ok(SetKeyDecision::SoftwareFallback);
    }
    Ok(queue_key_install(queue, station_id, key))
}

/// Process deferred key requests until an error/shutdown, clearing each consumed slot.
// upstream: if_iwx.c iwx_setkey_task()
pub fn drain_key_install_queue<E>(
    queue: &mut SetKeyQueue,
    shutdown: bool,
    mut install: impl FnMut(u8, KeyConfig) -> Result<(), E>,
) -> Result<usize, E> {
    let mut error = None;
    while queue.count > 0 {
        if shutdown || error.is_some() {
            break;
        }
        let (station, key) = queue.entries[queue.tail]
            .take()
            .expect("occupied key task slot");
        error = install(station, key).err();
        queue.tail = (queue.tail + 1) % SETKEY_QUEUE_CAPACITY;
        queue.count -= 1;
    }
    if let Some(error) = error {
        Err(error)
    } else {
        Ok(queue.count)
    }
}

/// Build legacy asynchronous key deletion, leaving actual command failures non-fatal.
// upstream: if_iwx.c iwx_delete_key()
pub fn legacy_delete_key_command(
    key: &KeyConfig,
    station_id: u8,
    slot: u8,
) -> Result<EncodedCommand, KeyError> {
    if key.key_len > 32 {
        return Err(KeyError::InvalidKeyLength);
    }
    let mut payload = [0u8; 76];
    let mut flags = STA_KEY_NOT_VALID
        | STA_KEY_FLAG_WEP_KEY_MAP
        | ((u16::from(key.key_id) << STA_KEY_FLAG_KEY_ID_SHIFT) & STA_KEY_FLAG_KEY_ID_MASK);
    if key.group {
        flags |= STA_KEY_FLAG_MULTICAST;
    }
    payload[2..4].copy_from_slice(&flags.to_le_bytes());
    payload[4..4 + usize::from(key.key_len)].copy_from_slice(&key.key[..usize::from(key.key_len)]);
    payload[0] = station_id;
    payload[1] = if key.group { 1 } else { 0 };
    let command = HostCommand {
        id: u32::from(ADD_STA_KEY_COMMAND),
        flags: CMD_ASYNC,
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, 0)?)
}

/// Select hardware deletion only for supported keys on an active firmware STA.
// upstream: if_iwx.c iwx_delete_key()
pub fn delete_key_command(
    key: &KeyConfig,
    station_id: u8,
    station_active: bool,
    security_key_version: u8,
    multi_queue_rx: bool,
    slot: u8,
) -> Result<Option<EncodedCommand>, KeyError> {
    if key.cipher != KeyCipher::Ccmp && !key.integrity_group {
        return Ok(None);
    }
    if !station_active {
        return Ok(None);
    }
    let command = if security_key_version != 0 && security_key_version != 99 {
        mld_station_key_command(key, station_id, true, slot)?
    } else if key.integrity_group {
        igtk_command(key, station_id, multi_queue_rx, true, slot)?
    } else {
        legacy_delete_key_command(key, station_id, slot)?
    };
    Ok(Some(command))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> KeyConfig {
        KeyConfig {
            cipher: KeyCipher::Ccmp,
            key_id: 1,
            key: [0x5a; 32],
            key_len: 16,
            tx_sequence: 0x1122,
            mgmt_rx_sequence: 0x3344,
            group: false,
            integrity_group: false,
            node_mfp: true,
            is_pairwise_key_slot: false,
        }
    }

    #[test]
    fn mld_and_legacy_key_commands_match_offsets_and_status() {
        let pair = key();
        let mld = mld_station_key_command(&pair, 0, false, 0).unwrap();
        assert_eq!(mld.bytes.len(), 88);
        assert_eq!(&mld.bytes[8..12], &1u32.to_le_bytes());
        assert_eq!(&mld.bytes[12..16], &1u32.to_le_bytes());
        assert_eq!(
            &mld.bytes[20..24],
            &(SEC_KEY_CIPHER_CCMP | SEC_KEY_FLAG_MFP).to_le_bytes()
        );
        assert_eq!(&mld.bytes[80..88], &pair.tx_sequence.to_le_bytes());
        let legacy = legacy_station_key_command(&pair, 0, 0).unwrap();
        assert_eq!(legacy.bytes.len(), 84);
        assert_eq!(legacy.bytes[8], 0);
        assert_eq!(legacy.bytes[9], 0);
        assert_eq!(
            u16::from_le_bytes(legacy.bytes[10..12].try_into().unwrap()),
            STA_KEY_FLAG_CCM | STA_KEY_FLAG_WEP_KEY_MAP | STA_KEY_FLAG_MFP | (1 << 8)
        );
        assert_eq!(&legacy.bytes[76..84], &pair.tx_sequence.to_le_bytes());
        assert_eq!(validate_legacy_key_response(&1u32.to_le_bytes()), Ok(()));
        assert_eq!(
            validate_legacy_key_response(&2u32.to_le_bytes()),
            Err(KeyError::InvalidResponse)
        );
    }

    #[test]
    fn igtk_version_and_install_bookkeeping_follow_mfp_rules() {
        let mut igtk = key();
        igtk.cipher = KeyCipher::Bip;
        igtk.integrity_group = true;
        igtk.group = false;
        igtk.key_id = 4;
        let v1 = igtk_command(&igtk, 0, false, false, 0).unwrap();
        let v2 = igtk_command(&igtk, 0, true, false, 0).unwrap();
        assert_eq!(v1.bytes.len(), 76);
        assert_eq!(v2.bytes.len(), 60);
        assert_eq!(&v1.bytes[60..64], &4u32.to_le_bytes());
        assert_eq!(&v2.bytes[44..48], &4u32.to_le_bytes());
        for key_id in 4..=7 {
            let big_or_integrity_key = KeyConfig { key_id, ..igtk };
            let command = igtk_command(&big_or_integrity_key, 0, true, false, 0).unwrap();
            assert_eq!(&command.bytes[44..48], &u32::from(key_id).to_le_bytes());
        }
        assert!(
            igtk_command(
                &KeyConfig {
                    is_pairwise_key_slot: true,
                    ..igtk
                },
                0,
                true,
                false,
                0
            )
            .is_err()
        );
        let mut tracking = KeyTracking::default();
        key_install_succeeded(&mut tracking, &igtk);
        assert!(!tracking.port_valid);
        key_install_succeeded(
            &mut tracking,
            &KeyConfig {
                group: true,
                integrity_group: false,
                ..igtk
            },
        );
        assert!(!tracking.port_valid);
        key_install_succeeded(
            &mut tracking,
            &KeyConfig {
                integrity_group: false,
                ..igtk
            },
        );
        assert!(tracking.port_valid && tracking.tx_mgmt_protected && tracking.link_up);
        let v2_selected = station_key_command(&igtk, 0, 99, true, 0).unwrap();
        assert_eq!(v2_selected.bytes.len(), 60);
        assert!(
            delete_key_command(&igtk, 0, false, 99, true, 0)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            delete_key_command(&igtk, 0, true, 99, true, 0)
                .unwrap()
                .unwrap()
                .flags,
            CMD_ASYNC
        );
    }

    #[test]
    fn deferred_key_queue_wraps_and_stops_after_error_or_shutdown() {
        let mut queue = SetKeyQueue::default();
        let pair = key();
        assert_eq!(
            queue_key_install(&mut queue, 0, pair),
            SetKeyDecision::Queued
        );
        assert_eq!(
            queue_key_install(&mut queue, 0, pair),
            SetKeyDecision::Queued
        );
        assert_eq!(
            queue_key_install(&mut queue, 0, pair),
            SetKeyDecision::Queued
        );
        assert_eq!(
            queue_key_install(&mut queue, 0, pair),
            SetKeyDecision::QueueFull
        );
        assert_eq!(
            drain_key_install_queue(&mut queue, false, |_, _| Ok::<_, ()>(())),
            Ok(0)
        );
        assert_eq!(
            queue_key_install(&mut queue, 0, pair),
            SetKeyDecision::Queued
        );
        assert_eq!(
            drain_key_install_queue(&mut queue, true, |_, _| Ok::<_, ()>(())),
            Ok(1)
        );
        let mut software = KeyConfig {
            cipher: KeyCipher::Other(7),
            group: true,
            ..pair
        };
        let mut tracking = KeyTracking::default();
        assert_eq!(
            set_key(&mut queue, &mut tracking, 0, software, |_| Ok::<_, ()>(())),
            Ok(SetKeyDecision::SoftwareFallback)
        );
        assert_ne!(tracking.installed_mask & NODE_HAVE_GROUP_KEY, 0);
        software.group = false;
        assert_eq!(
            set_key(&mut queue, &mut tracking, 0, software, |_| Err("software")),
            Err("software")
        );
    }
}
