//! Linux AF_BLUETOOTH HCI socket shell for device discovery and monitor setup.
//! Protocol framing follows the Bluetooth Core and Linux UAPI; no GPL net stack
//! implementation is copied here.
use alloc::{
    borrow::Cow,
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axio::IoBuf;
use axpoll::{IoEvents, PollRegistration, PollSet, Pollable, PreparedPollRegistration};
use bytemuck::{Pod, Zeroable};
use linux_raw_sys::net::{SOCK_RAW, sockaddr};
use spin::Mutex as SpinMutex;

use super::{FileLike, IoDst, IoSrc, IoctlContext, Kstat, PseudoInode, try_pseudo_inode_path};
use crate::mm::UserMemoryCapability;

pub const AF_BLUETOOTH: u32 = 31;
pub const BTPROTO_HCI: u32 = 1;
const HCIDEVUP: u32 = 0x4004_48c9;
const HCIDEVDOWN: u32 = 0x4004_48ca;
const HCIGETDEVLIST: u32 = 0x8004_48d2;
const HCIGETDEVINFO: u32 = 0x8004_48d3;
const HCI_DEV_NONE: u16 = 0xffff;
const HCI_CHANNEL_MONITOR: u16 = 2;
const HCI_CHANNEL_CONTROL: u16 = 3;
const HCI_OPT_DATA_DIR: u32 = 1;
const HCI_OPT_TIME_STAMP: u32 = 3;
const HCI_OPT_PASS_CREDENTIALS: u32 = 0x1_0000;
pub(crate) const HCI_CMSG_DIR: u32 = 1;
pub(crate) const HCI_CMSG_TSTAMP: u32 = 2;
const MAX_HCI_PACKET: usize = 65_536;
#[cfg(feature = "input")]
const SMP_PAIR_TIMEOUT: core::time::Duration = core::time::Duration::from_secs(30);
#[cfg(feature = "input")]
const SMP_PAIR_TIMEOUT_NANOS: u64 = 30_000_000_000;
const HCI_UP: u32 = 1 << 0;
#[cfg(feature = "input")]
const MAX_SMP_SESSIONS: usize = 8;

#[cfg(feature = "input")]
struct PendingSmpPairing {
    index: u16,
    address: [u8; 6],
    address_type: u8,
    io_capability: u8,
    started_at: u64,
    peer_auth_req: Option<u8>,
}

#[cfg(feature = "input")]
struct SmpSession {
    index: u16,
    handle: u16,
    address: [u8; 6],
    address_type: u8,
    initiator: axdriver::bluetooth_smp::Initiator,
    ltk: Option<[u8; 16]>,
    negotiated: Option<axdriver::bluetooth_smp::Negotiated>,
    encrypted: bool,
    peer_irk: Option<[u8; 16]>,
    peer_identity: Option<(u8, [u8; 6])>,
    peer_ediv: Option<u16>,
    peer_rand: Option<[u8; 8]>,
    peer_ltk: Option<[u8; 16]>,
    peer_encryption_info: bool,
    peer_master_ident: bool,
    peer_identity_info: bool,
    peer_identity_address: bool,
    peer_distribution: u8,
    local_distribution: u8,
    distribution_started: bool,
    pending_confirmation: bool,
    pending_passkey: bool,
    started_at: u64,
}

#[cfg(feature = "input")]
static SMP_PENDING: SpinMutex<Vec<PendingSmpPairing>> = SpinMutex::new(Vec::new());
#[cfg(feature = "input")]
static SMP_SESSIONS: SpinMutex<Vec<SmpSession>> = SpinMutex::new(Vec::new());
#[cfg(feature = "input")]
static SMP_ACL_REASSEMBLER: SpinMutex<axdriver::BluetoothAclReassembler> =
    SpinMutex::new(axdriver::BluetoothAclReassembler::new());

#[cfg(feature = "input")]
type UsbAdapter = Arc<SpinMutex<axdriver::UsbBluetoothHci>>;
#[cfg(not(feature = "input"))]
type UsbAdapter = ();

struct BoundChannel {
    channel: u16,
    adapter: Option<UsbAdapter>,
}

#[cfg(feature = "input")]
fn map_transport_error(error: axdriver::BluetoothError) -> AxError {
    match error {
        axdriver::BluetoothError::NoDevice => LinuxError::ENODEV.into(),
        axdriver::BluetoothError::Again => LinuxError::EAGAIN.into(),
        axdriver::BluetoothError::Busy => LinuxError::EBUSY.into(),
        axdriver::BluetoothError::NotUp => LinuxError::ENETDOWN.into(),
        axdriver::BluetoothError::Truncated | axdriver::BluetoothError::InvalidLength => {
            AxError::InvalidInput
        }
        axdriver::BluetoothError::Unsupported => LinuxError::EOPNOTSUPP.into(),
        axdriver::BluetoothError::NoMemory => AxError::NoMemory,
    }
}

#[cfg(feature = "input")]
fn usb_adapter(index: u16) -> Option<UsbAdapter> {
    axdriver::bluetooth_devices()
        .into_iter()
        .find(|adapter| adapter.lock().index() == index)
}

#[cfg(feature = "input")]
fn monitor_adapter(index: u16) -> Option<UsbAdapter> {
    if index == HCI_DEV_NONE {
        // A globally bound monitor socket observes the first controller; the
        // target platform has one CNVi HCI interface.
        axdriver::bluetooth_devices().into_iter().next()
    } else {
        usb_adapter(index)
    }
}

fn encode_dev_req(index: u16, is_up: bool) -> [u8; 8] {
    // Linux `struct hci_dev_req` has u16 dev_id followed by aligned u32
    // dev_opt; preserve the two padding bytes in its native ABI layout.
    let mut request = [0u8; 8];
    request[..2].copy_from_slice(&index.to_ne_bytes());
    request[4..].copy_from_slice(&(if is_up { HCI_UP } else { 0 }).to_ne_bytes());
    request
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct SockaddrHci {
    family: u16,
    device: u16,
    channel: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct HciDevStats {
    err_rx: u32,
    err_tx: u32,
    cmd_tx: u32,
    evt_rx: u32,
    acl_tx: u32,
    acl_rx: u32,
    sco_tx: u32,
    sco_rx: u32,
    byte_rx: u32,
    byte_tx: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct HciDevInfo {
    dev_id: u16,
    name: [u8; 8],
    bdaddr: [u8; 6],
    flags: u32,
    device_type: u8,
    features: [u8; 8],
    reserved: [u8; 3],
    packet_type: u32,
    link_policy: u32,
    link_mode: u32,
    acl_mtu: u16,
    acl_packets: u16,
    sco_mtu: u16,
    sco_packets: u16,
    stats: HciDevStats,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_channel_binds_without_hardware_but_device_channels_fail() {
        let socket = HciSocket::new();
        assert!(
            socket
                .bind(SockaddrHci {
                    family: AF_BLUETOOTH as u16,
                    device: HCI_DEV_NONE,
                    channel: HCI_CHANNEL_MONITOR
                })
                .is_ok()
        );
        assert!(
            HciSocket::new()
                .bind(SockaddrHci {
                    family: AF_BLUETOOTH as u16,
                    device: 0,
                    channel: HCI_CHANNEL_MONITOR,
                })
                .is_ok()
        );
        // `socket` is already bound to the monitor channel above, so a device
        // channel needs a fresh socket. RAW (0) on a missing device is ENODEV.
        assert_eq!(
            HciSocket::new().bind(SockaddrHci {
                family: AF_BLUETOOTH as u16,
                device: 0,
                channel: 0
            }),
            Err(LinuxError::ENODEV.into())
        );
        assert_eq!(
            HciSocket::validate_socket_type(SOCK_RAW as u32, BTPROTO_HCI),
            Ok(())
        );
    }

    #[test]
    fn management_event_fanout_reaches_each_control_socket() {
        let first = HciSocket::new();
        let second = HciSocket::new();
        let address = SockaddrHci {
            family: AF_BLUETOOTH as u16,
            device: HCI_DEV_NONE,
            channel: HCI_CHANNEL_CONTROL,
        };
        first.bind(address).unwrap();
        second.bind(address).unwrap();
        let event = alloc::vec![6, 0, 0, 0, 4, 0, 1, 0, 0, 0];
        fanout_mgmt_event(event.clone());
        assert_eq!(first.control.rx.lock().pop_front(), Some(event.clone()));
        assert_eq!(second.control.rx.lock().pop_front(), Some(event));
    }

    #[test]
    fn hci_dev_list_records_include_aligned_flags() {
        assert_eq!(
            encode_dev_req(0x1234, false),
            [0x34, 0x12, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(encode_dev_req(0x1234, true), [0x34, 0x12, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn management_read_version_and_empty_index_list_match_wire_abi() {
        // Controller-less commands are sent with MGMT_INDEX_NONE (0xffff) and
        // the Command Complete reply echoes that index.
        let version = management_response(&[1, 0, 0xff, 0xff, 0, 0]).unwrap();
        assert_eq!(&version[..9], &[1, 0, 0xff, 0xff, 6, 0, 1, 0, 0]);
        assert_eq!(&version[9..], &[1, 0, 0]);

        let indices = management_response(&[3, 0, 0xff, 0xff, 0, 0]).unwrap();
        assert_eq!(&indices[..9], &[1, 0, 0xff, 0xff, 5, 0, 3, 0, 0]);
        assert_eq!(&indices[9..], &[0, 0]);

        // Linux rejects a real index on these commands with INVALID_INDEX.
        let bound = management_response(&[1, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(bound, &[1, 0, 0, 0, 3, 0, 1, 0, 0x11]);
    }

    #[test]
    fn management_read_info_returns_invalid_index_without_device() {
        let info = management_response(&[4, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(&info[..9], &[1, 0, 0, 0, 3, 0, 4, 0, 0x11]);
        assert_eq!(info.len(), 9);
    }

    #[test]
    fn cancel_pair_device_has_linux_management_wire_shape() {
        let reply = management_response(&[0x1a, 0, 0, 0, 7, 0, 1, 2, 3, 4, 5, 6, 2]).unwrap();
        assert_eq!(&reply[..9], &[1, 0, 0, 0, 3, 0, 0x1a, 0, 0x11]);
        assert_eq!(reply.len(), 9);
    }

    #[test]
    fn passkey_management_commands_validate_wire_parameters_without_device() {
        let reply = management_response(&[
            0x20, 0, 0, 0, 11, 0, 1, 2, 3, 4, 5, 6, 1, 0x3f, 0x42, 0x0f, 0,
        ])
        .unwrap();
        assert_eq!(&reply[..9], &[1, 0, 0, 0, 3, 0, 0x20, 0, 0x11]);
        let rejected = management_response(&[0x21, 0, 0, 0, 7, 0, 1, 2, 3, 4, 5, 6, 1]).unwrap();
        assert_eq!(&rejected[..9], &[1, 0, 0, 0, 3, 0, 0x21, 0, 0x11]);
        let out_of_range = management_response(&[
            0x20, 0, 0, 0, 11, 0, 1, 2, 3, 4, 5, 6, 1, 0x40, 0x42, 0x0f, 0x01,
        ])
        .unwrap();
        assert_eq!(out_of_range[8], 0x0d);
    }

    #[cfg(feature = "input")]
    #[test]
    fn smp_timeout_uses_thirty_second_monotonic_deadline() {
        assert!(!smp_timeout_expired(30_000_000_000, 1_000_000_000));
        assert!(smp_timeout_expired(31_000_000_000, 1_000_000_000));
        assert!(!smp_timeout_expired(5, 6));
    }

    #[test]
    fn management_commands_advertise_only_supported_operations() {
        let commands = management_response(&[2, 0, 0xff, 0xff, 0, 0]).unwrap();
        assert_eq!(u16::from_le_bytes([commands[9], commands[10]]), 25);
        assert_eq!(u16::from_le_bytes([commands[11], commands[12]]), 13);
        assert_eq!(commands.len(), 9 + 4 + 2 * 38);
        assert_eq!(
            &commands[13..63],
            &[
                3, 0, 4, 0, 5, 0, 6, 0, 7, 0, 9, 0, 11, 0, 13, 0, 18, 0, 19, 0, 20, 0, 22, 0, 23,
                0, 24, 0, 25, 0, 26, 0, 28, 0, 29, 0, 32, 0, 33, 0, 35, 0, 36, 0, 42, 0, 47, 0, 48,
                0,
            ]
        );
        assert_eq!(
            &commands[63..],
            &[
                6, 0, 9, 0, 0x0a, 0, 0x0b, 0, 0x0c, 0, 0x0e, 0, 0x0f, 0, 0x10, 0, 0x11, 0, 0x12, 0,
                0x13, 0, 0x17, 0, 0x18, 0
            ]
        );
    }

    #[test]
    fn management_key_load_records_are_length_and_field_checked() {
        let mut link_keys = alloc::vec![0u8; 28];
        link_keys[1] = 1;
        link_keys[3 + 24] = 16;
        assert!(valid_load_link_keys(&link_keys));
        link_keys[3 + 6] = 3;
        assert!(!valid_load_link_keys(&link_keys));

        let mut ltks = alloc::vec![0u8; 38];
        ltks[0] = 1;
        ltks[2 + 9] = 16;
        assert!(valid_load_long_term_keys(&ltks));
        ltks[2 + 9] = 6;
        assert!(!valid_load_long_term_keys(&ltks));

        let mut irks = alloc::vec![0u8; 25];
        irks[0] = 1;
        assert!(valid_load_irks(&irks));
        irks[8] = 3;
        assert!(!valid_load_irks(&irks));
    }

    #[cfg(feature = "input")]
    #[test]
    fn management_event_header_uses_little_endian_lengths() {
        assert_eq!(
            mgmt_event_packet(0x1234, 2, &[1, 2]).unwrap(),
            [0x34, 0x12, 2, 0, 2, 0, 1, 2]
        );
    }

    #[test]
    fn hci_monitor_socket_options_round_trip_and_reject_bad_values() {
        let socket = HciSocket::new();
        assert_eq!(socket.get_hci_option(HCI_OPT_DATA_DIR), Ok(0));
        assert_eq!(socket.get_hci_option(HCI_OPT_TIME_STAMP), Ok(0));
        assert_eq!(socket.get_hci_option(HCI_OPT_PASS_CREDENTIALS), Ok(0));
        socket.set_hci_option(HCI_OPT_DATA_DIR, 1).unwrap();
        socket.set_hci_option(HCI_OPT_TIME_STAMP, 1).unwrap();
        socket.set_hci_option(HCI_OPT_PASS_CREDENTIALS, 1).unwrap();
        assert_eq!(socket.get_hci_option(HCI_OPT_DATA_DIR), Ok(1));
        assert_eq!(socket.get_hci_option(HCI_OPT_TIME_STAMP), Ok(1));
        assert_eq!(socket.get_hci_option(HCI_OPT_PASS_CREDENTIALS), Ok(1));
        assert!(socket.set_hci_option(HCI_OPT_TIME_STAMP, 2).is_err());
        assert!(socket.get_hci_option(2).is_err());
    }

    // fanout_hci_management_events() is only compiled with the `input` feature;
    // without it the function is a no-op, so this test is gated the same way.
    #[cfg(feature = "input")]
    #[test]
    fn inquiry_result_is_fanned_out_as_device_found() {
        let socket = HciSocket::new();
        socket
            .bind(SockaddrHci {
                family: AF_BLUETOOTH as u16,
                device: HCI_DEV_NONE,
                channel: HCI_CHANNEL_CONTROL,
            })
            .unwrap();
        let event = [0x02, 14, 1, 1, 2, 3, 4, 5, 6, 1, 0, 0, 0, 0, 0, 0];
        fanout_hci_management_events(3, &event);
        let actual = socket.control.rx.lock().pop_front().unwrap();
        assert_eq!(&actual[..6], &[0x12, 0, 3, 0, 14, 0]);
        assert_eq!(&actual[6..13], &[1, 2, 3, 4, 5, 6, 0]);
        assert_eq!(actual[13], 0); // RSSI unavailable in legacy Inquiry Result.
        assert_eq!(&actual[14..], &[0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn pairing_prompts_are_translated_to_mgmt_events() {
        let socket = HciSocket::new();
        socket
            .bind(SockaddrHci {
                family: AF_BLUETOOTH as u16,
                device: HCI_DEV_NONE,
                channel: HCI_CHANNEL_CONTROL,
            })
            .unwrap();
        fanout_hci_management_events(4, &[0x16, 6, 1, 2, 3, 4, 5, 6]);
        let pin = socket.control.rx.lock().pop_front().unwrap();
        assert_eq!(&pin[..6], &[0x0e, 0, 4, 0, 8, 0]);
        assert_eq!(&pin[6..], &[1, 2, 3, 4, 5, 6, 0, 0]);

        fanout_hci_management_events(4, &[0x33, 10, 1, 2, 3, 4, 5, 6, 0x78, 0x56, 0x34, 0x12]);
        let confirm = socket.control.rx.lock().pop_front().unwrap();
        assert_eq!(&confirm[..6], &[0x0f, 0, 4, 0, 12, 0]);
        assert_eq!(
            &confirm[6..],
            &[1, 2, 3, 4, 5, 6, 0, 1, 0x78, 0x56, 0x34, 0x12]
        );
    }

    #[test]
    fn management_command_complete_encodes_status_and_settings_payload() {
        let response = management_command_complete(4, 7, 0, &0x212u32.to_le_bytes()).unwrap();
        assert_eq!(&response, &[1, 0, 4, 0, 7, 0, 7, 0, 0, 0x12, 0x02, 0, 0]);
    }

    #[test]
    fn management_recognizes_bluez_startup_ops_and_returns_no_device_status() {
        let requests: &[&[u8]] = &[
            &[6, 0, 0, 0, 3, 0, 0, 0, 0],                   // SET_DISCOVERABLE
            &[7, 0, 0, 0, 1, 0, 1],                         // SET_CONNECTABLE
            &[9, 0, 0, 0, 1, 0, 1],                         // SET_BONDABLE
            &[11, 0, 0, 0, 1, 0, 1],                        // SET_SSP
            &[13, 0, 0, 0, 1, 0, 1],                        // SET_LE
            &[0x18, 0, 0, 0, 1, 0, 3],                      // SET_IO_CAPABILITY
            &[0x17, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0],    // PIN_CODE_NEG_REPLY
            &[0x1c, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0],    // USER_CONFIRM_REPLY
            &[0x1d, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0],    // USER_CONFIRM_NEG_REPLY
            &[0x2a, 0, 0, 0, 1, 0, 1],                      // SET_BREDR
            &[18, 0, 0, 0, 3, 0, 0, 0, 0],                  // LOAD_LINK_KEYS, empty set
            &[19, 0, 0, 0, 2, 0, 0, 0],                     // LOAD_LONG_TERM_KEYS, empty set
            &[0x30, 0, 0, 0, 2, 0, 0, 0],                   // LOAD_IRKS, empty set
            &[0x19, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 3], // PAIR_DEVICE
            &[0x14, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0],    // DISCONNECT
            &[0x23, 0, 0, 0, 1, 0, 1],                      // START_DISCOVERY, BR/EDR
            &[0x24, 0, 0, 0, 1, 0, 1],                      // STOP_DISCOVERY, BR/EDR
            &[0x24, 0, 0, 0, 1, 0, 1],                      // STOP_DISCOVERY, BR/EDR
        ];
        for request in requests {
            let response = management_response(request).unwrap();
            assert_eq!(
                response[8],
                0x11,
                "opcode={:#06x}",
                u16::from_le_bytes([request[0], request[1]])
            );
        }
        let mut pin_reply = [0u8; 30];
        pin_reply[0] = 0x16;
        pin_reply[4] = 24;
        pin_reply[13] = 1;
        assert_eq!(management_response(&pin_reply).unwrap()[8], 0x11);
        let mut privacy = [0u8; 23];
        privacy[0] = 0x2f;
        privacy[4] = 17;
        privacy[6] = 1;
        assert_eq!(management_response(&privacy).unwrap()[8], 0x11);

        let invalid_connectable = management_response(&[7, 0, 0, 0, 1, 0, 2]).unwrap();
        assert_eq!(invalid_connectable[8], 0x0d);
        let invalid_discovery = management_response(&[0x23, 0, 0, 0, 1, 0, 0]).unwrap();
        assert_eq!(invalid_discovery[8], 0x0d);
    }
}

pub struct HciSocket {
    inode: PseudoInode,
    nonblocking: AtomicBool,
    binding: SpinMutex<Option<BoundChannel>>,
    control: Arc<MgmtSubscriber>,
    socket_options: SpinMutex<HciSocketOptions>,
}

#[derive(Default)]
struct HciSocketOptions {
    data_direction: i32,
    timestamps: i32,
    pass_credentials: i32,
}

pub(crate) struct HciReceiveInfo {
    pub(crate) length: usize,
    pub(crate) timestamp_nanos: u64,
    pub(crate) cmsg_mask: u8,
    pub(crate) pass_credentials: bool,
}

struct MgmtSubscriber {
    rx: SpinMutex<VecDeque<Vec<u8>>>,
    readiness: Arc<PollSet<32>>,
}

static MGMT_SUBSCRIBERS: spin::Once<SpinMutex<Vec<Weak<MgmtSubscriber>>>> = spin::Once::new();

fn register_mgmt_subscriber(subscriber: &Arc<MgmtSubscriber>) {
    MGMT_SUBSCRIBERS
        .call_once(|| SpinMutex::new(Vec::new()))
        .lock()
        .push(Arc::downgrade(subscriber));
}

fn fanout_mgmt_event(event: Vec<u8>) {
    let Some(registry) = MGMT_SUBSCRIBERS.get() else {
        return;
    };
    let mut registry = registry.lock();
    registry.retain(|subscriber| {
        let Some(subscriber) = subscriber.upgrade() else {
            return false;
        };
        let mut queue = subscriber.rx.lock();
        if queue.len() < 64 {
            let mut copy = Vec::new();
            if copy.try_reserve_exact(event.len()).is_ok() {
                copy.extend_from_slice(&event);
                queue.push_back(copy);
                drop(queue);
                subscriber.readiness.wake();
            }
        }
        true
    });
}

impl HciSocket {
    pub(crate) fn new() -> Self {
        Self {
            inode: PseudoInode::socket(),
            nonblocking: AtomicBool::new(false),
            binding: SpinMutex::new(None),
            control: Arc::new(MgmtSubscriber {
                rx: SpinMutex::new(VecDeque::new()),
                readiness: Arc::new(PollSet::new()),
            }),
            socket_options: SpinMutex::new(HciSocketOptions::default()),
        }
    }

    /// Retain HCI capture and SOL_SOCKET toggles used by monitor clients.
    pub(crate) fn set_hci_option(&self, option: u32, value: i32) -> AxResult<()> {
        if !matches!(value, 0 | 1) {
            return Err(AxError::InvalidInput);
        }
        let mut options = self.socket_options.lock();
        match option {
            HCI_OPT_DATA_DIR => options.data_direction = value,
            HCI_OPT_TIME_STAMP => options.timestamps = value,
            HCI_OPT_PASS_CREDENTIALS => options.pass_credentials = value,
            _ => return Err(LinuxError::ENOPROTOOPT.into()),
        }
        Ok(())
    }

    pub(crate) fn get_hci_option(&self, option: u32) -> AxResult<i32> {
        let options = self.socket_options.lock();
        match option {
            HCI_OPT_DATA_DIR => Ok(options.data_direction),
            HCI_OPT_TIME_STAMP => Ok(options.timestamps),
            HCI_OPT_PASS_CREDENTIALS => Ok(options.pass_credentials),
            _ => Err(LinuxError::ENOPROTOOPT.into()),
        }
    }

    pub(crate) fn receive_into(&self, out: &mut [u8]) -> AxResult<HciReceiveInfo> {
        let (channel, adapter) = self
            .binding
            .lock()
            .as_ref()
            .map(|binding| (binding.channel, binding.adapter.clone()))
            .ok_or(LinuxError::ENODEV)?;
        if channel == HCI_CHANNEL_CONTROL {
            let packet = self
                .control
                .rx
                .lock()
                .pop_front()
                .ok_or(LinuxError::EAGAIN)?;
            if packet.len() > out.len() {
                return Err(AxError::InvalidInput);
            }
            out[..packet.len()].copy_from_slice(&packet);
            return Ok(HciReceiveInfo {
                length: packet.len(),
                timestamp_nanos: crate::time::wall_time_nanos(),
                cmsg_mask: 0,
                pass_credentials: self.get_hci_option(HCI_OPT_PASS_CREDENTIALS)? != 0,
            });
        }
        #[cfg(feature = "input")]
        if let Some(adapter) = &adapter {
            let (length, timestamp_nanos) = adapter
                .lock()
                .receive_channel_packet(channel, out, self.nonblocking.load(Ordering::Acquire))
                .map_err(map_transport_error)?;
            let options = self.socket_options.lock();
            let pass_credentials = options.pass_credentials != 0;
            let cmsg_mask = if matches!(channel, 0 | 1) {
                (u8::from(options.data_direction != 0) * HCI_CMSG_DIR as u8)
                    | (u8::from(options.timestamps != 0) * HCI_CMSG_TSTAMP as u8)
            } else {
                0
            };
            return Ok(HciReceiveInfo {
                length,
                timestamp_nanos,
                cmsg_mask,
                pass_credentials,
            });
        }
        Err(LinuxError::ENODEV.into())
    }

    pub(crate) fn receive_packet(&self) -> AxResult<(Vec<u8>, HciReceiveInfo)> {
        let capacity = MAX_HCI_PACKET.checked_add(6).ok_or(AxError::InvalidInput)?;
        let mut packet = Vec::new();
        packet
            .try_reserve_exact(capacity)
            .map_err(|_| AxError::NoMemory)?;
        packet.resize(capacity, 0);
        let info = self.receive_into(&mut packet)?;
        packet.truncate(info.length);
        Ok((packet, info))
    }

    pub(crate) fn validate_socket_type(ty: u32, protocol: u32) -> AxResult<()> {
        if ty != SOCK_RAW as u32 {
            return Err(LinuxError::ESOCKTNOSUPPORT.into());
        }
        if protocol != BTPROTO_HCI {
            return Err(LinuxError::EPROTONOSUPPORT.into());
        }
        Ok(())
    }

    pub(crate) fn bind(&self, address: SockaddrHci) -> AxResult<()> {
        if address.family as u32 != AF_BLUETOOTH {
            return Err(AxError::InvalidInput);
        }
        // HCI monitor is global and remains bindable without an adapter. The
        // HCI_DEV_NONE spelling is accepted as on Linux; raw/user need a device.
        if !matches!(
            address.channel,
            0 | 1 | HCI_CHANNEL_MONITOR | HCI_CHANNEL_CONTROL
        ) {
            return Err(LinuxError::EPROTONOSUPPORT.into());
        }
        let mut current = self.binding.lock();
        if current.is_some() {
            return Err(LinuxError::EINVAL.into());
        }
        let adapter = if address.channel == HCI_CHANNEL_CONTROL {
            if address.device != HCI_DEV_NONE {
                return Err(AxError::InvalidInput);
            }
            None
        } else if address.channel == HCI_CHANNEL_MONITOR {
            #[cfg(feature = "input")]
            let adapter = monitor_adapter(address.device);
            #[cfg(not(feature = "input"))]
            let adapter: Option<UsbAdapter> = None;
            #[cfg(feature = "input")]
            if let Some(adapter) = &adapter {
                adapter
                    .lock()
                    .open_channel(address.channel)
                    .map_err(map_transport_error)?;
            }
            adapter
        } else {
            if address.device == HCI_DEV_NONE {
                return Err(AxError::InvalidInput);
            }
            #[cfg(feature = "input")]
            let adapter = usb_adapter(address.device).ok_or(LinuxError::ENODEV)?;
            #[cfg(not(feature = "input"))]
            return Err(LinuxError::ENODEV.into());
            #[cfg(feature = "input")]
            {
                adapter
                    .lock()
                    .open_channel(address.channel)
                    .map_err(map_transport_error)?;
                Some(adapter)
            }
        };
        *current = Some(BoundChannel {
            channel: address.channel,
            adapter,
        });
        if address.channel == HCI_CHANNEL_CONTROL {
            register_mgmt_subscriber(&self.control);
        }
        Ok(())
    }

    fn close_binding(&self) {
        if let Some(binding) = self.binding.lock().take() {
            #[cfg(feature = "input")]
            if let Some(adapter) = binding.adapter {
                adapter.lock().close_channel(binding.channel);
            }
        }
    }

    fn device_operation(&self, index: u16, up: bool) -> AxResult<()> {
        #[cfg(feature = "input")]
        {
            let adapter = usb_adapter(index).ok_or(LinuxError::ENODEV)?;
            adapter
                .lock()
                .set_device_up(up)
                .map_err(map_transport_error)
        }
        #[cfg(not(feature = "input"))]
        {
            let _ = (index, up);
            Err(LinuxError::ENODEV.into())
        }
    }

    fn device_info(&self, index: u16) -> AxResult<HciDevInfo> {
        #[cfg(feature = "input")]
        {
            let adapter = usb_adapter(index).ok_or(LinuxError::ENODEV)?;
            let adapter = adapter.lock();
            let capabilities = adapter.capabilities();
            let mut info = HciDevInfo::zeroed();
            info.dev_id = index;
            info.bdaddr = adapter.address();
            info.name[..3].copy_from_slice(b"hci");
            let mut value = index;
            let mut digits = [0u8; 5];
            let mut count = 0;
            if value == 0 {
                digits[0] = b'0';
                count = 1;
            }
            while value != 0 {
                digits[count] = b'0' + (value % 10) as u8;
                value /= 10;
                count += 1;
            }
            for i in 0..count {
                info.name[3 + i] = digits[count - i - 1];
            }
            info.flags = if adapter.is_up() { 1 } else { 0 };
            info.device_type = 1; // HCI_USB
            info.features = capabilities.features;
            info.acl_mtu = capabilities.acl_mtu;
            info.acl_packets = capabilities.acl_packets;
            info.sco_mtu = capabilities.sco_mtu;
            info.sco_packets = capabilities.sco_packets;
            let stats = adapter.statistics();
            info.stats = HciDevStats {
                err_rx: stats.err_rx,
                err_tx: stats.err_tx,
                cmd_tx: stats.cmd_tx,
                evt_rx: stats.evt_rx,
                acl_tx: stats.acl_tx,
                acl_rx: stats.acl_rx,
                sco_tx: stats.sco_tx,
                sco_rx: stats.sco_rx,
                byte_rx: stats.byte_rx,
                byte_tx: stats.byte_tx,
            };
            Ok(info)
        }
        #[cfg(not(feature = "input"))]
        {
            let _ = index;
            Err(LinuxError::ENODEV.into())
        }
    }

    pub(crate) fn read_from_user(
        &self,
        capability: &UserMemoryCapability,
        address: *const sockaddr,
        len: usize,
    ) -> AxResult<SockaddrHci> {
        if len < core::mem::size_of::<SockaddrHci>() {
            return Err(AxError::InvalidInput);
        }
        capability
            .read_value(address.cast())
            .map_err(crate::mm::map_usercopy_error)
    }
}

impl Pollable for HciSocket {
    fn poll(&self) -> IoEvents {
        let bound_channel = self.binding.lock().as_ref().map(|binding| binding.channel);
        if bound_channel == Some(HCI_CHANNEL_CONTROL) {
            drain_bluetooth_management_events();
        }
        let binding = self.binding.lock();
        let Some(binding) = binding.as_ref() else {
            return IoEvents::WRITABLE;
        };
        let mut events = IoEvents::WRITABLE;
        if binding.channel == HCI_CHANNEL_CONTROL {
            if !self.control.rx.lock().is_empty() {
                events |= IoEvents::READABLE;
            }
            return events;
        }
        #[cfg(feature = "input")]
        if let Some(adapter) = &binding.adapter
            && adapter.lock().receive_ready(binding.channel)
        {
            events |= IoEvents::READABLE;
        }
        events
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        if !events.intersects(IoEvents::READABLE) {
            return axpoll::PollRegistration::empty();
        }
        let binding = self.binding.lock();
        let Some(binding) = binding.as_ref() else {
            return axpoll::PollRegistration::empty();
        };
        if binding.channel == HCI_CHANNEL_CONTROL {
            #[cfg(feature = "input")]
            {
                let adapters = axdriver::bluetooth_devices();
                let mut prepared = PreparedPollRegistration::try_new(adapters.len() + 1)?;
                prepared.arm(&self.control.readiness, context.waker())?;
                for adapter in adapters {
                    prepared.arm_owned(adapter.lock().receive_readiness(), context.waker())?;
                }
                return prepared.commit();
            }
            #[cfg(not(feature = "input"))]
            return PollRegistration::single(&self.control.readiness, context.waker());
        }
        #[cfg(feature = "input")]
        if let Some(adapter) = &binding.adapter {
            let readiness = adapter.lock().receive_readiness();
            return PollRegistration::single_owned(readiness, context.waker());
        }
        axpoll::PollRegistration::empty()
    }
}

impl FileLike for HciSocket {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        let channel = self
            .binding
            .lock()
            .as_ref()
            .map(|binding| binding.channel)
            .ok_or(LinuxError::ENODEV)?;
        if channel == HCI_CHANNEL_CONTROL {
            drain_bluetooth_management_events();
        }
        let (packet, _) = self.receive_packet()?;
        dst.write(&packet)
    }
    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        let channel = self
            .binding
            .lock()
            .as_ref()
            .map(|binding| binding.channel)
            .ok_or(LinuxError::ENODEV)?;
        if channel == HCI_CHANNEL_CONTROL {
            return self.write_management_command(src);
        }
        let binding = self.binding.lock();
        let binding = binding.as_ref().ok_or(LinuxError::ENODEV)?;
        #[cfg(feature = "input")]
        if let Some(adapter) = &binding.adapter {
            if binding.channel == HCI_CHANNEL_MONITOR {
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            let length = src.remaining().min(MAX_HCI_PACKET);
            let mut packet = Vec::new();
            packet
                .try_reserve_exact(length)
                .map_err(|_| AxError::NoMemory)?;
            packet.resize(length, 0);
            let read = src.read(&mut packet)?;
            packet.truncate(read);
            adapter
                .lock()
                .send_channel_packet(binding.channel, &packet)
                .map_err(map_transport_error)?;
            return Ok(read);
        }
        let _ = binding;
        Err(LinuxError::ENODEV.into())
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(self.inode.stat())
    }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        match cmd {
            HCIGETDEVLIST => {
                let requested = context
                    .user_memory()
                    .read_value(arg as *const u16)
                    .map_err(crate::mm::map_usercopy_error)?;
                #[cfg(feature = "input")]
                let devices = axdriver::bluetooth_devices();
                #[cfg(not(feature = "input"))]
                let devices: Vec<()> = Vec::new();
                let count = usize::from(requested).min(devices.len());
                for (slot, adapter) in devices.iter().take(count).enumerate() {
                    #[cfg(feature = "input")]
                    let (index, is_up) = {
                        let adapter = adapter.lock();
                        (adapter.index(), adapter.is_up())
                    };
                    #[cfg(not(feature = "input"))]
                    let (index, is_up) = {
                        let _ = adapter;
                        (0u16, false)
                    };
                    let request = encode_dev_req(index, is_up);
                    context
                        .user_memory()
                        .write_bytes(
                            arg.checked_add(4 + slot * request.len())
                                .ok_or(AxError::InvalidInput)?,
                            &request,
                        )
                        .map_err(crate::mm::map_usercopy_error)?;
                }
                context
                    .user_memory()
                    .write_value(arg as *mut u16, count as u16)
                    .map_err(crate::mm::map_usercopy_error)?;
                Ok(0)
            }
            HCIDEVUP => self.device_operation(arg as u16, true).map(|()| 0),
            HCIDEVDOWN => self.device_operation(arg as u16, false).map(|()| 0),
            HCIGETDEVINFO => {
                let index = context
                    .user_memory()
                    .read_value(arg as *const u16)
                    .map_err(crate::mm::map_usercopy_error)?;
                let info = self.device_info(index)?;
                context
                    .user_memory()
                    .write_value(arg as *mut HciDevInfo, info)
                    .map_err(crate::mm::map_usercopy_error)?;
                Ok(0)
            }
            _ => Err(AxError::NotATty),
        }
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        try_pseudo_inode_path("socket", self.inode.inode())
    }
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }
    fn set_nonblocking(&self, value: bool) -> AxResult {
        self.nonblocking.store(value, Ordering::Release);
        Ok(())
    }
    fn final_close(&self) {
        self.close_binding();
    }
}

fn drain_bluetooth_management_events() {
    #[cfg(feature = "input")]
    for adapter in axdriver::bluetooth_devices() {
        let adapter_device = adapter.clone();
        let mut adapter = adapter.lock();
        let _ = adapter.pump_receive();
        let index = adapter.index();
        let mut events = Vec::new();
        while let Some(event) = adapter.pop_management_event() {
            events.push(event);
        }
        let mut acls = Vec::new();
        while let Some(acl) = adapter.pop_management_acl() {
            acls.push(acl);
        }
        drop(adapter);
        for event in events {
            fanout_hci_management_events(index, &event);
        }
        for acl in acls {
            process_smp_acl(index, &adapter_device, &acl);
        }
    }
}

impl Default for HciSocket {
    fn default() -> Self {
        Self::new()
    }
}

impl HciSocket {
    fn write_management_command(&self, src: &mut IoSrc) -> AxResult<usize> {
        let length = src.remaining().min(MAX_HCI_PACKET);
        if length < 6 {
            return Err(AxError::InvalidInput);
        }
        let mut packet = Vec::new();
        packet
            .try_reserve_exact(length)
            .map_err(|_| AxError::NoMemory)?;
        packet.resize(length, 0);
        let read = src.read(&mut packet)?;
        packet.truncate(read);
        let (response, setting_event) = management_controller_command(&packet)?
            .unwrap_or((management_response(&packet)?, None));
        let mut queue = self.control.rx.lock();
        if queue.len() >= 64 {
            return Err(LinuxError::ENOBUFS.into());
        }
        queue.push_back(response);
        drop(queue);
        if let Some(event) = setting_event {
            fanout_mgmt_event(event);
        }
        self.control.readiness.wake();
        Ok(read)
    }
}

#[cfg(feature = "input")]
fn mgmt_event_packet(code: u16, index: u16, parameters: &[u8]) -> AxResult<Vec<u8>> {
    let length = u16::try_from(parameters.len()).map_err(|_| AxError::InvalidInput)?;
    let mut packet = Vec::new();
    packet
        .try_reserve_exact(6 + parameters.len())
        .map_err(|_| AxError::NoMemory)?;
    packet.extend_from_slice(&code.to_le_bytes());
    packet.extend_from_slice(&index.to_le_bytes());
    packet.extend_from_slice(&length.to_le_bytes());
    packet.extend_from_slice(parameters);
    Ok(packet)
}

#[cfg(feature = "input")]
fn start_pending_le_smp(index: u16, handle: u16, peer: [u8; 6], hci_address_type: u8) {
    let Some(adapter) = usb_adapter(index) else {
        return;
    };
    let resolved_identity = adapter
        .lock()
        .management_resolve_private_address(hci_address_type.saturating_add(1), peer);
    let Some(pending) = SMP_PENDING.lock().iter().position(|entry| {
        entry.index == index
            && ((entry.address == peer && entry.address_type == hci_address_type.saturating_add(1))
                || resolved_identity.is_some_and(|identity| {
                    entry.address == identity.1 && entry.address_type == identity.0
                }))
    }) else {
        return;
    };
    let pending = SMP_PENDING.lock().remove(pending);
    let started_at = axhal::time::monotonic_time_nanos();
    let local = adapter.lock().address();
    let mut local_address = [0u8; 7];
    local_address[1..].copy_from_slice(&local);
    local_address[1..].reverse();
    let mut peer_address = [0u8; 7];
    peer_address[0] = hci_address_type & 1;
    peer_address[1..].copy_from_slice(&peer);
    peer_address[1..].reverse();
    let mut entropy = [0u8; 368];
    if crate::random::fill_secure(&mut entropy).is_err() {
        emit_smp_auth_failed(index, peer, pending.address_type, 0x03);
        return;
    }
    let local = axdriver::bluetooth_smp::PairingFeatures {
        io_capability: pending.io_capability,
        oob_data: false,
        auth_req: 0x09
            | pending.peer_auth_req.unwrap_or(0)
            | if matches!(pending.io_capability, 0 | 1 | 2 | 4)
                || pending.peer_auth_req.is_some_and(|auth| auth & 0x04 != 0)
            {
                0x04
            } else {
                0
            },
        max_key_size: 16,
        initiator_key_distribution: 0x03, // encryption + identity
        responder_key_distribution: 0x03,
    };
    let passkey_randoms = core::array::from_fn(|round| {
        let start = 48 + round * 16;
        entropy[start..start + 16].try_into().unwrap_or([0; 16])
    });
    let initiator = match axdriver::bluetooth_smp::Initiator::new_with_passkey_entropy(
        local,
        local_address,
        peer_address,
        entropy[..16].try_into().unwrap_or([0; 16]),
        entropy[16..48].try_into().unwrap_or([0; 32]),
        passkey_randoms,
    ) {
        Ok(initiator) => initiator,
        Err(_) => {
            emit_smp_auth_failed(index, peer, pending.address_type, 0x03);
            return;
        }
    };
    let request = initiator.request_pdu();
    {
        let mut sessions = SMP_SESSIONS.lock();
        sessions.retain(|session| session.index != index || session.handle != handle);
        if sessions.len() >= MAX_SMP_SESSIONS || sessions.try_reserve(1).is_err() {
            drop(sessions);
            emit_smp_auth_failed(index, peer, pending.address_type, 0x07);
            return;
        }
        sessions.push(SmpSession {
            index,
            handle,
            address: pending.address,
            address_type: pending.address_type,
            initiator,
            ltk: None,
            negotiated: None,
            encrypted: false,
            peer_irk: None,
            peer_identity: None,
            peer_ediv: None,
            peer_rand: None,
            peer_ltk: None,
            peer_encryption_info: false,
            peer_master_ident: false,
            peer_identity_info: false,
            peer_identity_address: false,
            peer_distribution: 0,
            local_distribution: 0,
            distribution_started: false,
            pending_confirmation: false,
            pending_passkey: false,
            started_at,
        });
    }
    if !schedule_smp_pair_timeout(
        index,
        handle,
        pending.address,
        pending.address_type,
        started_at,
        adapter.clone(),
    ) {
        SMP_SESSIONS
            .lock()
            .retain(|session| session.index != index || session.handle != handle);
        emit_smp_auth_failed(index, pending.address, pending.address_type, 0x08);
        return;
    }
    if adapter
        .lock()
        .management_send_smp(handle, &request)
        .is_err()
    {
        SMP_SESSIONS
            .lock()
            .retain(|session| session.index != index || session.handle != handle);
        emit_smp_auth_failed(index, peer, pending.address_type, 0x03);
    }
}

#[cfg(feature = "input")]
fn schedule_smp_pair_timeout(
    index: u16,
    handle: u16,
    address: [u8; 6],
    address_type: u8,
    started_at: u64,
    adapter: UsbAdapter,
) -> bool {
    axtask::spawn_with_name(
        move || {
            if axtask::sleep(SMP_PAIR_TIMEOUT).is_err()
                || !smp_timeout_expired(axhal::time::monotonic_time_nanos(), started_at)
            {
                return;
            }
            let expired = {
                let mut sessions = SMP_SESSIONS.lock();
                let Some(position) = sessions.iter().position(|session| {
                    session.index == index
                        && session.handle == handle
                        && session.started_at == started_at
                }) else {
                    return;
                };
                sessions.remove(position)
            };
            let _ = adapter.lock().management_send_smp(handle, &[0x05, 0x08]);
            let _ = adapter
                .lock()
                .management_disconnect(expired.address, expired.address_type);
            emit_smp_auth_failed(index, address, address_type, 0x08);
        },
        alloc::string::String::from("bt-smp-timeout"),
    )
    .is_ok()
}

#[cfg(feature = "input")]
fn smp_timeout_expired(now: u64, started_at: u64) -> bool {
    now.saturating_sub(started_at) >= SMP_PAIR_TIMEOUT_NANOS
}

#[cfg(feature = "input")]
fn emit_smp_auth_failed(index: u16, address: [u8; 6], address_type: u8, reason: u8) {
    let mut parameters = Vec::with_capacity(8);
    parameters.extend_from_slice(&address);
    parameters.push(address_type);
    parameters.push(reason);
    if let Ok(event) = mgmt_event_packet(0x0011, index, &parameters) {
        fanout_mgmt_event(event);
    }
}

#[cfg(feature = "input")]
fn process_smp_acl(index: u16, adapter: &UsbAdapter, packet: &[u8]) {
    let Some(reassembled) = SMP_ACL_REASSEMBLER.lock().push(packet) else {
        return;
    };
    let packet = reassembled.as_slice();
    if packet.len() < 8 {
        return;
    }
    let handle_flags = u16::from_le_bytes([packet[0], packet[1]]);
    let handle = handle_flags & 0x0fff;
    let acl_length = usize::from(u16::from_le_bytes([packet[2], packet[3]]));
    let l2cap_length = usize::from(u16::from_le_bytes([packet[4], packet[5]]));
    let cid = u16::from_le_bytes([packet[6], packet[7]]);
    if cid != axdriver::bluetooth_smp::SMP_CID
        || acl_length != packet.len() - 4
        || l2cap_length != acl_length - 4
    {
        return;
    }
    let pdu = &packet[8..];
    if axdriver::bluetooth_smp::parse_security_request(pdu).is_ok()
        && !SMP_SESSIONS
            .lock()
            .iter()
            .any(|session| session.index == index && session.handle == handle)
    {
        let (peer, address_type, io_capability) = {
            let adapter_guard = adapter.lock();
            let Some((peer, address_type)) = adapter_guard.management_peer_for_handle(handle)
            else {
                return;
            };
            (peer, address_type, adapter_guard.management_io_capability())
        };
        match adapter
            .lock()
            .management_start_encryption_from_loaded_ltk(handle, peer, address_type)
        {
            Ok(true) => return,
            Err(_) => {
                emit_smp_auth_failed(index, peer, address_type, 0x08);
                return;
            }
            Ok(false) => {}
        }
        let started_at = axhal::time::monotonic_time_nanos();
        let mut pending = SMP_PENDING.lock();
        pending.retain(|entry| entry.index != index || entry.address != peer);
        if pending.len() < MAX_SMP_SESSIONS && pending.try_reserve(1).is_ok() {
            pending.push(PendingSmpPairing {
                index,
                address: peer,
                address_type,
                io_capability,
                started_at,
                peer_auth_req: axdriver::bluetooth_smp::parse_security_request(pdu).ok(),
            });
            drop(pending);
            start_pending_le_smp(index, handle, peer, address_type.saturating_sub(1));
        }
        return;
    }
    let phase_three = SMP_SESSIONS.lock().iter().any(|session| {
        session.index == index
            && session.handle == handle
            && session.encrypted
            && session.initiator.distributing_keys()
    });
    if phase_three {
        process_smp_key_distribution(index, handle, adapter, pdu);
        return;
    }
    let action = {
        let mut sessions = SMP_SESSIONS.lock();
        let Some(session) = sessions
            .iter_mut()
            .find(|session| session.index == index && session.handle == handle)
        else {
            return;
        };
        session.initiator.handle(pdu)
    };
    match action {
        axdriver::bluetooth_smp::Action::Send(pdu) => {
            if adapter.lock().management_send_smp(handle, &pdu).is_err() {
                SMP_SESSIONS
                    .lock()
                    .retain(|session| session.index != index || session.handle != handle);
            }
        }
        axdriver::bluetooth_smp::Action::ConfirmNumeric(value) => {
            let parameters = {
                let mut sessions = SMP_SESSIONS.lock();
                sessions
                    .iter_mut()
                    .find(|session| session.index == index && session.handle == handle)
                    .map(|session| {
                        session.pending_confirmation = true;
                        let mut parameters = Vec::with_capacity(12);
                        parameters.extend_from_slice(&session.address);
                        parameters.push(session.address_type);
                        parameters.push(1); // confirm_hint: user must accept
                        parameters.extend_from_slice(&value.to_le_bytes());
                        parameters
                    })
            };
            if let Some(parameters) = parameters {
                if let Ok(event) = mgmt_event_packet(0x000f, index, &parameters) {
                    fanout_mgmt_event(event);
                }
            }
        }
        axdriver::bluetooth_smp::Action::RequestPasskey => {
            let parameters = {
                let mut sessions = SMP_SESSIONS.lock();
                sessions
                    .iter_mut()
                    .find(|session| session.index == index && session.handle == handle)
                    .map(|session| {
                        session.pending_passkey = true;
                        let mut parameters = Vec::with_capacity(7);
                        parameters.extend_from_slice(&session.address);
                        parameters.push(session.address_type);
                        parameters
                    })
            };
            if let Some(parameters) = parameters
                && let Ok(event) = mgmt_event_packet(0x0010, index, &parameters)
            {
                fanout_mgmt_event(event);
            }
        }
        axdriver::bluetooth_smp::Action::DisplayPasskey { passkey, pdu } => {
            let (address, address_type) = SMP_SESSIONS
                .lock()
                .iter()
                .find(|session| session.index == index && session.handle == handle)
                .map(|session| (session.address, session.address_type))
                .unwrap_or(([0; 6], 0));
            if adapter.lock().management_send_smp(handle, &pdu).is_ok() {
                let mut parameters = Vec::with_capacity(12);
                parameters.extend_from_slice(&address);
                parameters.push(address_type);
                parameters.extend_from_slice(&passkey.to_le_bytes());
                parameters.push(0); // no digits entered by the peer yet
                if let Ok(event) = mgmt_event_packet(0x0017, index, &parameters) {
                    fanout_mgmt_event(event);
                }
            }
        }
        axdriver::bluetooth_smp::Action::StartEncryption(key) => {
            {
                let mut sessions = SMP_SESSIONS.lock();
                if let Some(session) = sessions
                    .iter_mut()
                    .find(|session| session.index == index && session.handle == handle)
                {
                    session.negotiated = session.initiator.negotiated();
                    if session
                        .negotiated
                        .is_some_and(|negotiated| negotiated.secure_connections)
                    {
                        session.ltk = session.initiator.long_term_key();
                    }
                    if let Some(negotiated) = session.negotiated {
                        let encryption_key_mask = if negotiated.secure_connections {
                            !0x01
                        } else {
                            0xff
                        };
                        session.local_distribution =
                            negotiated.initiator_key_distribution & encryption_key_mask;
                        session.peer_distribution =
                            negotiated.responder_key_distribution & encryption_key_mask;
                    }
                }
            }
            if adapter
                .lock()
                .management_start_encryption(handle, &reverse_key(key))
                .is_err()
            {
                SMP_SESSIONS
                    .lock()
                    .retain(|session| session.index != index || session.handle != handle);
            }
        }
        axdriver::bluetooth_smp::Action::Failed(error) => {
            let reason = smp_failure_reason(error);
            if error != axdriver::bluetooth_smp::PairingError::PeerFailed {
                let _ = adapter.lock().management_send_smp(handle, &[0x05, reason]);
            }
            let mut parameters = Vec::with_capacity(8);
            if let Some(session) = SMP_SESSIONS
                .lock()
                .iter()
                .find(|session| session.index == index && session.handle == handle)
            {
                parameters.extend_from_slice(&session.address);
                parameters.push(session.address_type);
                parameters.push(reason);
            }
            SMP_SESSIONS
                .lock()
                .retain(|session| session.index != index || session.handle != handle);
            if parameters.len() == 8
                && let Ok(event) = mgmt_event_packet(0x0011, index, &parameters)
            {
                fanout_mgmt_event(event);
            }
        }
        axdriver::bluetooth_smp::Action::Paired { .. } => {}
    }
}

#[cfg(feature = "input")]
fn smp_failure_reason(error: axdriver::bluetooth_smp::PairingError) -> u8 {
    use axdriver::bluetooth_smp::PairingError;
    match error {
        PairingError::ConfirmFailed => 0x04,
        PairingError::DhKeyCheckFailed | PairingError::InvalidPublicKey => 0x0b,
        PairingError::AuthenticationUnsupported => 0x03,
        PairingError::UserRejected => 0x0c,
        PairingError::InvalidPdu | PairingError::InvalidFeatures => 0x0a,
        PairingError::PeerFailed | PairingError::InvalidState => 0x08,
    }
}

#[cfg(feature = "input")]
fn smp_encryption_complete(index: u16, handle: u16, adapter: &UsbAdapter) {
    let (secure_connections, peer_distribution, local_distribution, key_size) = {
        let mut sessions = SMP_SESSIONS.lock();
        let Some(session) = sessions
            .iter_mut()
            .find(|session| session.index == index && session.handle == handle)
        else {
            return;
        };
        if session.initiator.encryption_complete(true).is_err() {
            return;
        }
        session.encrypted = true;
        let secure = session.negotiated.is_some_and(|n| n.secure_connections);
        let peer = if secure {
            session.peer_distribution & !0x01
        } else {
            session.peer_distribution
        };
        let local = if secure {
            session.local_distribution & !0x01
        } else {
            session.local_distribution
        };
        (
            secure,
            peer,
            local,
            session.negotiated.map_or(16, |n| n.key_size),
        )
    };
    let expected_peer = peer_distribution & 0x03;
    if expected_peer == 0 {
        send_local_smp_keys(
            index,
            handle,
            adapter,
            local_distribution,
            secure_connections,
            key_size,
        );
    }
}

#[cfg(feature = "input")]
fn process_smp_key_distribution(index: u16, handle: u16, adapter: &UsbAdapter, pdu: &[u8]) {
    if pdu.is_empty() {
        return;
    }
    let mut send_local = None;
    {
        let mut sessions = SMP_SESSIONS.lock();
        let Some(session) = sessions
            .iter_mut()
            .find(|session| session.index == index && session.handle == handle)
        else {
            return;
        };
        match pdu[0] {
            0x06 if pdu.len() == 17
                && session.peer_distribution & 1 != 0
                && !session.peer_encryption_info =>
            {
                session.peer_ltk = Some(pdu[1..].try_into().unwrap_or([0; 16]));
                session.peer_encryption_info = true;
            }
            0x07 if pdu.len() == 11
                && session.peer_distribution & 1 != 0
                && session.peer_encryption_info
                && !session.peer_master_ident =>
            {
                session.peer_ediv = Some(u16::from_le_bytes([pdu[1], pdu[2]]));
                session.peer_rand = Some(pdu[3..11].try_into().unwrap_or([0; 8]));
                session.peer_master_ident = true;
            }
            0x08 if pdu.len() == 17
                && session.peer_distribution & 2 != 0
                && (session.peer_distribution & 1 == 0
                    || (session.peer_encryption_info && session.peer_master_ident))
                && !session.peer_identity_info =>
            {
                session.peer_irk = Some(pdu[1..].try_into().unwrap_or([0; 16]));
                session.peer_identity_info = true;
            }
            0x09 if pdu.len() == 8
                && session.peer_distribution & 2 != 0
                && session.peer_identity_info
                && !session.peer_identity_address
                && pdu[1] <= 1
                && (pdu[1] == 0 || pdu[7] & 0xc0 == 0xc0) =>
            {
                session.peer_identity = Some((pdu[1] + 1, pdu[2..8].try_into().unwrap_or([0; 6])));
                session.peer_identity_address = true;
            }
            _ => return,
        }
        let expected_peer = session.peer_distribution;
        let enc_done =
            expected_peer & 1 == 0 || (session.peer_encryption_info && session.peer_master_ident);
        let id_done =
            expected_peer & 2 == 0 || (session.peer_identity_info && session.peer_identity_address);
        if enc_done && id_done && !session.distribution_started {
            session.distribution_started = true;
            send_local = Some((
                session.local_distribution,
                session.negotiated.is_some_and(|n| n.secure_connections),
                session.negotiated.map_or(16, |n| n.key_size),
            ));
        }
    }
    if let Some((local_distribution, secure_connections, key_size)) = send_local {
        send_local_smp_keys(
            index,
            handle,
            adapter,
            local_distribution,
            secure_connections,
            key_size,
        );
    }
}

#[cfg(feature = "input")]
fn send_local_smp_keys(
    index: u16,
    handle: u16,
    adapter: &UsbAdapter,
    local_distribution: u8,
    secure_connections: bool,
    key_size: u8,
) {
    let send_encryption = local_distribution & 1 != 0 && !secure_connections;
    let send_identity = local_distribution & 2 != 0;
    if !send_encryption && !send_identity {
        complete_smp_bond(index, handle, adapter);
        return;
    }
    let mut entropy = [0u8; 42];
    if send_encryption && crate::random::fill_secure(&mut entropy).is_err() {
        SMP_SESSIONS
            .lock()
            .retain(|s| s.index != index || s.handle != handle);
        return;
    }
    let mut local_irk = [0u8; 16];
    if send_identity {
        local_irk = adapter.lock().management_local_irk();
        if local_irk == [0; 16] {
            if crate::random::fill_secure(&mut local_irk).is_err()
                || adapter
                    .lock()
                    .management_store_local_irk(local_irk)
                    .is_err()
            {
                SMP_SESSIONS
                    .lock()
                    .retain(|s| s.index != index || s.handle != handle);
                return;
            }
        }
    }
    let mut pdus = Vec::new();
    if send_encryption {
        let mut encryption = Vec::with_capacity(17);
        encryption.push(0x06);
        let mut local_ltk = [0u8; 16];
        local_ltk.copy_from_slice(&entropy[16..32]);
        local_ltk[usize::from(key_size.clamp(7, 16))..].fill(0);
        encryption.extend_from_slice(&local_ltk);
        pdus.push(encryption);
        let mut master = Vec::with_capacity(11);
        master.push(0x07);
        master.extend_from_slice(&entropy[32..34]);
        master.extend_from_slice(&entropy[34..42]);
        pdus.push(master);
    }
    if send_identity {
        let mut identity_key = Vec::with_capacity(17);
        identity_key.push(0x08);
        identity_key.extend_from_slice(&local_irk);
        pdus.push(identity_key);
        let mut identity = [0u8; 8];
        identity[0] = 0x09;
        identity[1] = 0; // local public identity
        identity[2..].copy_from_slice(&adapter.lock().address());
        pdus.push(identity.to_vec());
    }
    for pdu in pdus {
        if adapter.lock().management_send_smp(handle, &pdu).is_err() {
            SMP_SESSIONS
                .lock()
                .retain(|s| s.index != index || s.handle != handle);
            return;
        }
    }
    complete_smp_bond(index, handle, adapter);
}

#[cfg(feature = "input")]
fn complete_smp_bond(index: u16, handle: u16, adapter: &UsbAdapter) {
    let Some(session_index) = SMP_SESSIONS
        .lock()
        .iter()
        .position(|session| session.index == index && session.handle == handle)
    else {
        return;
    };
    let (address, address_type, ltk, peer_ltk, ediv, rand, identity, irk, negotiated) = {
        let sessions = SMP_SESSIONS.lock();
        let session = &sessions[session_index];
        (
            session.address,
            session.address_type,
            session.ltk,
            session.peer_ltk,
            session.peer_ediv.unwrap_or(0),
            session.peer_rand.unwrap_or([0; 8]),
            session.peer_identity,
            session.peer_irk,
            session.negotiated,
        )
    };
    let Some(negotiated) = negotiated else { return };
    let bond = negotiated.bonding;
    let key_address = identity.map_or(address, |(_, address)| address);
    let key_address_type = identity.map_or(address_type, |(kind, _)| kind);
    if let Some((identity_type, identity_address)) = identity {
        adapter
            .lock()
            .management_set_identity_for_handle(handle, identity_address, identity_type);
    }
    let mut ltk_record = None;
    let mut ltk_event = None;
    let key = if negotiated.secure_connections {
        ltk.map(reverse_key)
    } else {
        peer_ltk
    };
    if let Some(key) = key {
        let mut record = [0u8; 36];
        record[..6].copy_from_slice(&key_address);
        record[6] = key_address_type;
        record[7] = if negotiated.secure_connections { 2 } else { 0 };
        record[8] = 1; // local controller is the LE Central
        record[9] = negotiated.key_size;
        record[10..12].copy_from_slice(&ediv.to_le_bytes());
        record[12..20].copy_from_slice(&rand);
        record[20..].copy_from_slice(&key);
        if bond && identity.is_some() {
            ltk_record = Some(record);
        }
        let mut parameters = Vec::with_capacity(37);
        parameters.push(u8::from(bond && identity.is_some()));
        parameters.extend_from_slice(&key_address);
        parameters.push(key_address_type);
        parameters.push(if negotiated.secure_connections { 2 } else { 0 });
        parameters.push(1);
        parameters.push(negotiated.key_size);
        parameters.extend_from_slice(&ediv.to_le_bytes());
        parameters.extend_from_slice(&rand);
        parameters.extend_from_slice(&key);
        ltk_event = Some(parameters);
    }
    let mut irk_record = None;
    let mut irk_event = None;
    if let (Some(irk), Some((identity_type, identity_address))) = (irk, identity) {
        let mut record = [0u8; 23];
        record[..6].copy_from_slice(&identity_address);
        record[6] = identity_type;
        record[7..].copy_from_slice(&irk);
        if bond {
            irk_record = Some(record);
        }
        let mut parameters = Vec::with_capacity(30);
        parameters.push(u8::from(bond));
        if address_type == 2 && address[5] & 0xc0 == 0x40 {
            parameters.extend_from_slice(&address);
        } else {
            parameters.extend_from_slice(&[0; 6]);
        }
        parameters.extend_from_slice(&identity_address);
        parameters.push(identity_type);
        parameters.extend_from_slice(&irk);
        irk_event = Some(parameters);
    }
    if adapter
        .lock()
        .management_store_pairing_keys(ltk_record, irk_record)
        .is_err()
    {
        SMP_SESSIONS
            .lock()
            .retain(|s| s.index != index || s.handle != handle);
        return;
    }
    if let Some(parameters) = irk_event
        && let Ok(event) = mgmt_event_packet(0x0018, index, &parameters)
    {
        fanout_mgmt_event(event);
    }
    if let Some(parameters) = ltk_event
        && let Ok(event) = mgmt_event_packet(0x000a, index, &parameters)
    {
        fanout_mgmt_event(event);
    }
    SMP_SESSIONS
        .lock()
        .retain(|s| s.index != index || s.handle != handle);
}

#[cfg(feature = "input")]
fn reverse_key(mut key: [u8; 16]) -> [u8; 16] {
    key.reverse();
    key
}

/// Convert controller events that carry the management API's address/state
/// records. SMP key events are produced by the host pairing engine, not by a
/// raw LE HCI event that omits key material.
#[cfg(feature = "input")]
fn fanout_hci_management_events(index: u16, event: &[u8]) {
    if event.len() < 2 || event.len() != usize::from(event[1]) + 2 {
        return;
    }
    let publish = |code, parameters: &[u8]| {
        if let Ok(packet) = mgmt_event_packet(code, index, parameters) {
            fanout_mgmt_event(packet);
        }
    };
    if event[0] == 0x08 && event.len() >= 5 && event[2] == 0 {
        let handle = u16::from_le_bytes([event[3], event[4]]) & 0x0fff;
        if let Some(adapter) = usb_adapter(index) {
            smp_encryption_complete(index, handle, &adapter);
        }
    } else if event[0] == 0x08 && event.len() >= 5 && event[2] != 0 {
        let handle = u16::from_le_bytes([event[3], event[4]]) & 0x0fff;
        let failed = {
            let mut sessions = SMP_SESSIONS.lock();
            let position = sessions
                .iter()
                .position(|session| session.index == index && session.handle == handle);
            position.map(|position| sessions.remove(position))
        };
        if let Some(session) = failed {
            emit_smp_auth_failed(index, session.address, session.address_type, event[2]);
        }
    } else if event[0] == 0x05 && event.len() >= 6 {
        let handle = u16::from_le_bytes([event[3], event[4]]) & 0x0fff;
        let disconnected = {
            let mut sessions = SMP_SESSIONS.lock();
            let position = sessions
                .iter()
                .position(|session| session.index == index && session.handle == handle);
            position.map(|position| sessions.remove(position))
        };
        if let Some(session) = disconnected {
            emit_smp_auth_failed(index, session.address, session.address_type, 0x08);
        }
        SMP_ACL_REASSEMBLER.lock().clear_handle(handle);
    }
    if event[0] == 0x3e && event.len() >= 21 && event[2] == 0x01 && event[3] == 0 {
        let handle = u16::from_le_bytes([event[4], event[5]]) & 0x0fff;
        let mut address = [0; 6];
        address.copy_from_slice(&event[8..14]);
        start_pending_le_smp(index, handle, address, event[7]);
    } else if event[0] == 0x3e && event.len() >= 21 && event[2] == 0x01 && event[3] != 0 {
        let address_type = event[7].saturating_add(1);
        let mut address = [0; 6];
        address.copy_from_slice(&event[8..14]);
        SMP_PENDING.lock().retain(|entry| {
            entry.index != index || entry.address != address || entry.address_type != address_type
        });
        emit_smp_auth_failed(index, address, address_type, event[3]);
    }
    match event[0] {
        // HCI Connection Complete -> Device Connected.
        0x03 if event.len() >= 11 && event[2] == 0 => {
            let mut parameters = Vec::with_capacity(13);
            parameters.extend_from_slice(&event[5..11]);
            parameters.push(0); // BR/EDR address
            parameters.extend_from_slice(&0u32.to_le_bytes()); // flags
            parameters.extend_from_slice(&0u16.to_le_bytes()); // EIR length
            publish(0x000b, &parameters);
        }
        // LE Connection Complete -> Device Connected.
        0x3e if event.len() >= 21 && event[2] == 0x01 && event[3] == 0 => {
            let address_type = match event[7] {
                0 => 1, // public LE address
                1 => 2, // random LE address
                _ => return,
            };
            let handle = u16::from_le_bytes([event[4], event[5]]) & 0x0fff;
            let mut peer = [0; 6];
            peer.copy_from_slice(&event[8..14]);
            let identity = (address_type == 2)
                .then(|| usb_adapter(index))
                .flatten()
                .and_then(|adapter| {
                    adapter
                        .lock()
                        .management_resolve_private_address(address_type, peer)
                });
            let (peer, address_type) = if let Some((identity_type, identity)) = identity {
                if let Some(adapter) = usb_adapter(index) {
                    adapter.lock().management_set_identity_for_handle(
                        handle,
                        identity,
                        identity_type,
                    );
                }
                (identity, identity_type)
            } else {
                (peer, address_type)
            };
            let mut parameters = Vec::with_capacity(13);
            parameters.extend_from_slice(&peer);
            parameters.push(address_type);
            parameters.extend_from_slice(&0u32.to_le_bytes());
            parameters.extend_from_slice(&0u16.to_le_bytes());
            publish(0x000b, &parameters);
        }
        // HCI Disconnection Complete -> Device Disconnected.
        0x05 if event.len() >= 6 && event[2] == 0 => {
            let handle = u16::from_le_bytes([event[3], event[4]]) & 0x0fff;
            if let Some(adapter) = usb_adapter(index)
                && let Some((address, address_type)) = {
                    let mut adapter = adapter.lock();
                    adapter
                        .take_management_peer_for_handle(handle)
                        .or_else(|| adapter.management_peer_for_handle(handle))
                }
            {
                let mut parameters = Vec::with_capacity(8);
                parameters.extend_from_slice(&address);
                parameters.push(address_type);
                parameters.push(match event[5] {
                    0x08 => 0x01, // timeout
                    0x16 => 0x02, // local host
                    0x13 => 0x03, // remote
                    0x05 => 0x04, // authentication failure
                    _ => 0,
                });
                publish(0x000c, &parameters);
            }
        }
        // HCI Authentication Complete -> Authentication Failed.
        0x06 if event.len() >= 5 && event[2] != 0 => {
            let handle = u16::from_le_bytes([event[3], event[4]]) & 0x0fff;
            if let Some(adapter) = usb_adapter(index)
                && let Some((address, address_type)) =
                    adapter.lock().management_peer_for_handle(handle)
            {
                let mut parameters = Vec::with_capacity(8);
                parameters.extend_from_slice(&address);
                parameters.push(address_type);
                parameters.push(event[2]);
                publish(0x0011, &parameters);
            }
        }
        // HCI PIN Code Request -> BlueZ PIN prompt.
        0x16 if event.len() == 8 => {
            let mut parameters = Vec::with_capacity(8);
            parameters.extend_from_slice(&event[2..8]);
            parameters.push(0); // BR/EDR
            parameters.push(0); // legacy PIN request is not marked secure
            publish(0x000e, &parameters);
        }
        // HCI User Confirmation Request -> BlueZ numeric-comparison prompt.
        0x33 if event.len() == 12 => {
            let mut parameters = Vec::with_capacity(12);
            parameters.extend_from_slice(&event[2..8]);
            parameters.push(0); // BR/EDR
            parameters.push(1); // confirmation is required
            parameters.extend_from_slice(&event[8..12]);
            publish(0x000f, &parameters);
        }
        // HCI Encryption Change can also report an LE security failure.
        0x08 if event.len() >= 5 && event[2] != 0 => {
            let handle = u16::from_le_bytes([event[3], event[4]]) & 0x0fff;
            if let Some(adapter) = usb_adapter(index)
                && let Some((address, address_type)) =
                    adapter.lock().management_peer_for_handle(handle)
            {
                let mut parameters = Vec::with_capacity(8);
                parameters.extend_from_slice(&address);
                parameters.push(address_type);
                parameters.push(event[2]);
                publish(0x0011, &parameters);
            }
        }
        // HCI Link Key Notification contains the key bytes for classic links.
        0x18 if event.len() >= 25 => {
            let mut parameters = Vec::with_capacity(26);
            parameters.push(1); // persistent key
            parameters.extend_from_slice(&event[2..8]);
            parameters.push(0); // BR/EDR
            parameters.push(event[24]); // key type
            parameters.extend_from_slice(&event[8..24]);
            parameters.push(0); // PIN length is not carried in the HCI event.
            publish(0x0009, &parameters);
        }
        // HCI Inquiry Result: report each 13-byte response without optional EIR.
        0x02 if event.len() >= 3 => {
            let count = usize::from(event[2]);
            if event.len() != 3 + count * 13 {
                return;
            }
            for record in event[3..].chunks_exact(13) {
                let mut parameters = Vec::with_capacity(14);
                parameters.extend_from_slice(&record[..6]);
                parameters.extend_from_slice(&[0, 0]); // address type, RSSI unavailable
                parameters.extend_from_slice(&0u32.to_le_bytes()); // flags
                parameters.extend_from_slice(&0u16.to_le_bytes()); // EIR length
                publish(0x0012, &parameters);
            }
        }
        // HCI Inquiry Result with RSSI has 14-byte response records.
        0x22 if event.len() >= 3 => {
            let count = usize::from(event[2]);
            if event.len() != 3 + count * 14 {
                return;
            }
            for record in event[3..].chunks_exact(14) {
                let mut parameters = Vec::with_capacity(14);
                parameters.extend_from_slice(&record[..6]);
                parameters.push(0); // BR/EDR address
                parameters.push(record[13]); // RSSI
                parameters.extend_from_slice(&0u32.to_le_bytes());
                parameters.extend_from_slice(&0u16.to_le_bytes());
                publish(0x0012, &parameters);
            }
        }
        // LE Advertising Report carries the advertising data and RSSI.
        0x3e if event.len() >= 4 && event[2] == 0x02 => {
            let count = usize::from(event[3]);
            let mut at = 4usize;
            for _ in 0..count {
                let Some(fixed) = event.get(at..at + 9) else {
                    return;
                };
                let address_type = match fixed[1] {
                    0 => 1,
                    1 => 2,
                    _ => return,
                };
                let data_len = usize::from(fixed[8]);
                let Some(record) = event.get(at..at + 10 + data_len) else {
                    return;
                };
                let mut parameters = Vec::with_capacity(14 + data_len);
                parameters.extend_from_slice(&record[2..8]);
                parameters.push(address_type);
                parameters.push(record[9 + data_len]); // RSSI
                parameters.extend_from_slice(&0u32.to_le_bytes()); // flags
                parameters.extend_from_slice(&(data_len as u16).to_le_bytes());
                parameters.extend_from_slice(&record[9..9 + data_len]);
                publish(0x0012, &parameters);
                at += 10 + data_len;
            }
            if at != event.len() {
                return;
            }
        }
        _ => {}
    }
}

#[cfg(not(feature = "input"))]
fn fanout_hci_management_events(_index: u16, _event: &[u8]) {}

fn management_response(request: &[u8]) -> AxResult<Vec<u8>> {
    const MGMT_INDEX_NONE: u16 = 0xffff;
    const CMD_COMPLETE: u16 = 1;
    const READ_VERSION: u16 = 1;
    const READ_COMMANDS: u16 = 2;
    const READ_INDEX_LIST: u16 = 3;
    const READ_INFO: u16 = 4;
    const SET_POWERED: u16 = 5;
    const SET_DISCOVERABLE: u16 = 6;
    const SET_CONNECTABLE: u16 = 7;
    const SET_BONDABLE: u16 = 9;
    const SET_SSP: u16 = 11;
    const SET_LE: u16 = 13;
    const SET_IO_CAPABILITY: u16 = 0x18;
    const PIN_CODE_REPLY: u16 = 0x16;
    const PIN_CODE_NEG_REPLY: u16 = 0x17;
    const USER_CONFIRM_REPLY: u16 = 0x1c;
    const USER_CONFIRM_NEG_REPLY: u16 = 0x1d;
    const USER_PASSKEY_REPLY: u16 = 0x20;
    const USER_PASSKEY_NEG_REPLY: u16 = 0x21;
    const LOAD_LINK_KEYS: u16 = 18;
    const LOAD_LONG_TERM_KEYS: u16 = 19;
    const LOAD_IRKS: u16 = 0x30;
    const PAIR_DEVICE: u16 = 0x19;
    const CANCEL_PAIR_DEVICE: u16 = 0x1a;
    const DISCONNECT: u16 = 0x14;
    const SET_BREDR: u16 = 0x2a;
    const SET_PRIVACY: u16 = 0x2f;
    const START_DISCOVERY: u16 = 0x23;
    const STOP_DISCOVERY: u16 = 0x24;
    const UNKNOWN_COMMAND: u8 = 1;
    const NOT_SUPPORTED: u8 = 0x0c;
    const INVALID_PARAMS: u8 = 0x0d;
    const INVALID_INDEX: u8 = 0x11;
    const SUPPORTED_SETTINGS: u32 =
        (1 << 0) | (1 << 1) | (1 << 3) | (1 << 4) | (1 << 6) | (1 << 7) | (1 << 9) | (1 << 13);
    if request.len() < 6 {
        return Err(AxError::InvalidInput);
    }
    let opcode = u16::from_le_bytes([request[0], request[1]]);
    let index = u16::from_le_bytes([request[2], request[3]]);
    let parameter_len = usize::from(u16::from_le_bytes([request[4], request[5]]));
    if request.len() != 6 + parameter_len {
        return Err(AxError::InvalidInput);
    }
    let parameters = &request[6..];
    let mut status = 0u8;
    let mut data = Vec::new();
    match opcode {
        // Linux hci_mgmt_cmd() requires MGMT_INDEX_NONE for HCI_MGMT_NO_HDEV
        // commands (mgmt.c read_version/read_commands/read_index_list) and
        // answers any other index with MGMT_STATUS_INVALID_INDEX.
        READ_VERSION | READ_COMMANDS | READ_INDEX_LIST if index != MGMT_INDEX_NONE => {
            status = INVALID_INDEX;
        }
        READ_VERSION if parameters.is_empty() => data.extend_from_slice(&[1, 0, 0]),
        READ_COMMANDS if parameters.is_empty() => {
            // Linux excludes READ_VERSION/READ_COMMANDS from the advertised
            // per-controller command table. Keep this list truthful: only
            // operations implemented by this HCI socket are exposed.
            let commands = [
                READ_INDEX_LIST,
                READ_INFO,
                SET_POWERED,
                SET_DISCOVERABLE,
                SET_CONNECTABLE,
                SET_BONDABLE,
                SET_SSP,
                SET_LE,
                LOAD_LINK_KEYS,
                LOAD_LONG_TERM_KEYS,
                DISCONNECT,
                PIN_CODE_REPLY,
                PIN_CODE_NEG_REPLY,
                SET_IO_CAPABILITY,
                PAIR_DEVICE,
                CANCEL_PAIR_DEVICE,
                USER_CONFIRM_REPLY,
                USER_CONFIRM_NEG_REPLY,
                USER_PASSKEY_REPLY,
                USER_PASSKEY_NEG_REPLY,
                START_DISCOVERY,
                STOP_DISCOVERY,
                SET_BREDR,
                SET_PRIVACY,
                LOAD_IRKS,
            ];
            let events = [
                6u16, 0x09, 0x0a, 0x0b, 0x0c, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x17, 0x18,
            ];
            data.extend_from_slice(&(commands.len() as u16).to_le_bytes());
            data.extend_from_slice(&(events.len() as u16).to_le_bytes());
            for item in commands.into_iter().chain(events) {
                data.extend_from_slice(&item.to_le_bytes());
            }
        }
        READ_INDEX_LIST if parameters.is_empty() => {
            #[cfg(feature = "input")]
            let devices = axdriver::bluetooth_devices();
            #[cfg(not(feature = "input"))]
            let devices: Vec<()> = Vec::new();
            data.extend_from_slice(&(devices.len() as u16).to_le_bytes());
            #[cfg(feature = "input")]
            for adapter in devices {
                data.extend_from_slice(&adapter.lock().index().to_le_bytes());
            }
        }
        READ_INFO if parameters.is_empty() => {
            #[cfg(feature = "input")]
            let adapter = axdriver::bluetooth_devices()
                .into_iter()
                .find(|adapter| adapter.lock().index() == index);
            #[cfg(feature = "input")]
            if let Some(adapter) = adapter {
                let adapter = adapter.lock();
                let address = adapter.address();
                let capabilities = adapter.capabilities();
                // mgmt_rp_read_info, with only currently observed controller
                // properties populated. Unimplemented capabilities stay clear.
                data.extend_from_slice(&address);
                data.push(capabilities.hci_version);
                data.extend_from_slice(&capabilities.manufacturer.to_le_bytes());
                data.extend_from_slice(&SUPPORTED_SETTINGS.to_le_bytes());
                data.extend_from_slice(&adapter.management_settings().to_le_bytes());
                data.extend_from_slice(&[0; 3 + 249 + 11]);
            } else {
                status = INVALID_INDEX;
            }
            #[cfg(not(feature = "input"))]
            {
                let _ = index;
                status = INVALID_INDEX;
            }
        }
        SET_POWERED if parameters.len() == 1 && parameters[0] <= 1 => {
            #[cfg(feature = "input")]
            {
                let adapter = axdriver::bluetooth_devices()
                    .into_iter()
                    .find(|adapter| adapter.lock().index() == index);
                if let Some(adapter) = adapter {
                    if adapter.lock().set_device_up(parameters[0] != 0).is_err() {
                        status = 3;
                    }
                } else {
                    status = INVALID_INDEX;
                }
            }
            #[cfg(not(feature = "input"))]
            {
                status = INVALID_INDEX;
            }
        }
        SET_DISCOVERABLE if parameters.len() == 3 && parameters[0] <= 1 => {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        SET_IO_CAPABILITY if parameters.len() == 1 && parameters[0] <= 4 => {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        SET_PRIVACY if parameters.len() == 17 && parameters[0] <= 1 => {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        SET_CONNECTABLE | SET_BONDABLE | SET_SSP | SET_LE | SET_BREDR
            if parameters.len() == 1 && parameters[0] <= 1 =>
        {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        LOAD_LINK_KEYS if valid_load_link_keys(parameters) => {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        LOAD_LONG_TERM_KEYS if valid_load_long_term_keys(parameters) => {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        LOAD_IRKS if valid_load_irks(parameters) => {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        PAIR_DEVICE if parameters.len() == 8 && parameters[6] <= 2 && parameters[7] <= 4 => {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        CANCEL_PAIR_DEVICE if parameters.len() == 7 && parameters[6] <= 2 => {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        DISCONNECT if parameters.len() == 7 && parameters[6] <= 2 => {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        PIN_CODE_REPLY if parameters.len() == 24 && parameters[6] == 0 && parameters[7] <= 16 => {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        PIN_CODE_NEG_REPLY if parameters.len() == 7 && parameters[6] == 0 => {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        USER_CONFIRM_REPLY | USER_CONFIRM_NEG_REPLY
            if parameters.len() == 7 && parameters[6] <= 2 =>
        {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        USER_PASSKEY_REPLY
            if parameters.len() == 11
                && parameters[6] <= 2
                && u32::from_le_bytes(parameters[7..11].try_into().unwrap_or([0; 4]))
                    <= 999_999 =>
        {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        USER_PASSKEY_NEG_REPLY if parameters.len() == 7 && parameters[6] <= 2 => {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        START_DISCOVERY if parameters.len() == 1 && matches!(parameters[0], 1 | 6) => {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        STOP_DISCOVERY if parameters.len() == 1 && matches!(parameters[0], 1 | 6) => {
            status = management_controller_status(index, INVALID_INDEX, NOT_SUPPORTED);
        }
        _ if matches!(
            opcode,
            READ_VERSION
                | READ_COMMANDS
                | READ_INDEX_LIST
                | READ_INFO
                | SET_POWERED
                | SET_DISCOVERABLE
                | SET_CONNECTABLE
                | SET_BONDABLE
                | SET_SSP
                | SET_LE
                | SET_IO_CAPABILITY
                | SET_BREDR
                | SET_PRIVACY
                | LOAD_LINK_KEYS
                | LOAD_LONG_TERM_KEYS
                | LOAD_IRKS
                | PAIR_DEVICE
                | CANCEL_PAIR_DEVICE
                | DISCONNECT
                | PIN_CODE_REPLY
                | PIN_CODE_NEG_REPLY
                | USER_CONFIRM_REPLY
                | USER_CONFIRM_NEG_REPLY
                | USER_PASSKEY_REPLY
                | USER_PASSKEY_NEG_REPLY
                | START_DISCOVERY
                | STOP_DISCOVERY
        ) =>
        {
            status = INVALID_PARAMS;
        }
        _ => status = UNKNOWN_COMMAND,
    }
    if status != 0 {
        data.clear();
    }
    let mut response = Vec::new();
    response
        .try_reserve_exact(9 + data.len())
        .map_err(|_| AxError::NoMemory)?;
    response.extend_from_slice(&CMD_COMPLETE.to_le_bytes());
    response.extend_from_slice(&index.to_le_bytes());
    response.extend_from_slice(&(3 + data.len() as u16).to_le_bytes());
    response.extend_from_slice(&opcode.to_le_bytes());
    response.push(status);
    response.extend_from_slice(&data);
    Ok(response)
}

#[cfg(feature = "input")]
fn management_controller_command(request: &[u8]) -> AxResult<Option<(Vec<u8>, Option<Vec<u8>>)>> {
    const SET_POWERED: u16 = 5;
    const SET_DISCOVERABLE: u16 = 6;
    const SET_CONNECTABLE: u16 = 7;
    const SET_BONDABLE: u16 = 9;
    const SET_SSP: u16 = 11;
    const SET_LE: u16 = 13;
    const SET_IO_CAPABILITY: u16 = 0x18;
    const PIN_CODE_REPLY: u16 = 0x16;
    const PIN_CODE_NEG_REPLY: u16 = 0x17;
    const USER_CONFIRM_REPLY: u16 = 0x1c;
    const USER_CONFIRM_NEG_REPLY: u16 = 0x1d;
    const USER_PASSKEY_REPLY: u16 = 0x20;
    const USER_PASSKEY_NEG_REPLY: u16 = 0x21;
    const LOAD_LINK_KEYS: u16 = 18;
    const LOAD_LONG_TERM_KEYS: u16 = 19;
    const LOAD_IRKS: u16 = 0x30;
    const PAIR_DEVICE: u16 = 0x19;
    const CANCEL_PAIR_DEVICE: u16 = 0x1a;
    const DISCONNECT: u16 = 0x14;
    const SET_BREDR: u16 = 0x2a;
    const SET_PRIVACY: u16 = 0x2f;
    const START_DISCOVERY: u16 = 0x23;
    const STOP_DISCOVERY: u16 = 0x24;
    const MGMT_SETTING_CONNECTABLE: u32 = 1 << 1;
    const MGMT_SETTING_DISCOVERABLE: u32 = 1 << 3;
    const MGMT_SETTING_BONDABLE: u32 = 1 << 4;
    const MGMT_SETTING_SSP: u32 = 1 << 6;
    const MGMT_SETTING_LE: u32 = 1 << 9;
    const MGMT_SETTING_BREDR: u32 = 1 << 7;
    const INVALID_INDEX: u8 = 0x11;
    const NOT_SUPPORTED: u8 = 0x0c;
    const FAILED: u8 = 0x03;

    if request.len() < 6 {
        return Err(AxError::InvalidInput);
    }
    let opcode = u16::from_le_bytes([request[0], request[1]]);
    let index = u16::from_le_bytes([request[2], request[3]]);
    let length = usize::from(u16::from_le_bytes([request[4], request[5]]));
    if request.len() != 6 + length {
        return Err(AxError::InvalidInput);
    }
    let parameters = &request[6..];
    if (opcode == LOAD_LINK_KEYS && valid_load_link_keys(parameters))
        || (opcode == LOAD_LONG_TERM_KEYS && valid_load_long_term_keys(parameters))
        || (opcode == LOAD_IRKS && valid_load_irks(parameters))
    {
        let Some(adapter) = usb_adapter(index) else {
            return Ok(None);
        };
        let status = match adapter.lock().management_load_keys(opcode, parameters) {
            Ok(()) => 0,
            Err(axdriver::BluetoothError::NoMemory) => 7,
            Err(axdriver::BluetoothError::InvalidLength) => 0x0d,
            Err(_) => FAILED,
        };
        return Ok(Some((
            management_command_complete(index, opcode, status, &[])?,
            None,
        )));
    }
    if opcode == CANCEL_PAIR_DEVICE && parameters.len() == 7 && parameters[6] <= 2 {
        let Some(adapter) = usb_adapter(index) else {
            return Ok(None);
        };
        let mut address = [0u8; 6];
        address.copy_from_slice(&parameters[..6]);
        let address_type = parameters[6];
        SMP_PENDING.lock().retain(|entry| {
            entry.index != index || entry.address != address || entry.address_type != address_type
        });
        let session = {
            let mut sessions = SMP_SESSIONS.lock();
            let position = sessions.iter().position(|session| {
                session.index == index
                    && session.address == address
                    && session.address_type == address_type
            });
            position.map(|position| sessions.remove(position))
        };
        let result = if let Some(session) = session {
            let _ = adapter
                .lock()
                .management_send_smp(session.handle, &[0x05, 0x08]);
            adapter
                .lock()
                .management_cancel_pair_device(address, address_type)
        } else {
            adapter
                .lock()
                .management_cancel_pair_device(address, address_type)
        };
        let status = match result {
            Ok(()) => 0,
            Err(axdriver::BluetoothError::NotUp) => 0x0f,
            Err(axdriver::BluetoothError::NoDevice) => INVALID_INDEX,
            Err(axdriver::BluetoothError::Unsupported) => 0x02,
            Err(_) => FAILED,
        };
        let data = if status == 0 {
            parameters.to_vec()
        } else {
            Vec::new()
        };
        return Ok(Some((
            management_command_complete(index, opcode, status, &data)?,
            None,
        )));
    }
    if opcode == PAIR_DEVICE && parameters.len() == 8 && parameters[6] <= 2 && parameters[7] <= 4 {
        let Some(adapter) = usb_adapter(index) else {
            return Ok(None);
        };
        let mut address = [0; 6];
        address.copy_from_slice(&parameters[..6]);
        let pending_le = parameters[6] != 0;
        if pending_le {
            let mut pending = SMP_PENDING.lock();
            if pending.len() >= MAX_SMP_SESSIONS || pending.try_reserve(1).is_err() {
                return Ok(Some((
                    management_command_complete(index, opcode, 7, &[])?,
                    None,
                )));
            }
            pending.retain(|entry| {
                entry.index != index
                    || entry.address != address
                    || entry.address_type != parameters[6]
            });
            pending.push(PendingSmpPairing {
                index,
                address,
                address_type: parameters[6],
                io_capability: parameters[7],
                started_at: axhal::time::monotonic_time_nanos(),
                peer_auth_req: None,
            });
        }
        let status =
            match adapter
                .lock()
                .management_pair_device(address, parameters[6], parameters[7])
            {
                Ok(()) => 0,
                Err(axdriver::BluetoothError::NotUp) => 0x0f,
                Err(axdriver::BluetoothError::NoDevice) => INVALID_INDEX,
                Err(axdriver::BluetoothError::Busy) => 0x0a,
                Err(axdriver::BluetoothError::Unsupported) => NOT_SUPPORTED,
                Err(_) => 0x04,
            };
        if status != 0 && pending_le {
            SMP_PENDING.lock().retain(|entry| {
                entry.index != index
                    || entry.address != address
                    || entry.address_type != parameters[6]
            });
        }
        if status == 0 && pending_le {
            let started_at = SMP_PENDING
                .lock()
                .iter()
                .find(|entry| {
                    entry.index == index
                        && entry.address == address
                        && entry.address_type == parameters[6]
                })
                .map(|entry| entry.started_at);
            if let Some(started_at) = started_at
                && !schedule_pending_pair_timeout(
                    index,
                    address,
                    parameters[6],
                    started_at,
                    adapter.clone(),
                )
            {
                SMP_PENDING.lock().retain(|entry| {
                    entry.index != index
                        || entry.address != address
                        || entry.address_type != parameters[6]
                });
                let _ = adapter
                    .lock()
                    .management_cancel_pair_device(address, parameters[6]);
                emit_smp_auth_failed(index, address, parameters[6], 0x08);
                return Ok(Some((
                    management_command_complete(index, opcode, 7, &[])?,
                    None,
                )));
            }
        }
        let data = if status == 0 {
            parameters[..7].to_vec()
        } else {
            Vec::new()
        };
        return Ok(Some((
            management_command_complete(index, opcode, status, &data)?,
            None,
        )));
    }
    if opcode == DISCONNECT && parameters.len() == 7 && parameters[6] <= 2 {
        let Some(adapter) = usb_adapter(index) else {
            return Ok(None);
        };
        let mut address = [0; 6];
        address.copy_from_slice(&parameters[..6]);
        let status = match adapter.lock().management_disconnect(address, parameters[6]) {
            Ok(()) => 0,
            Err(axdriver::BluetoothError::NotUp) => 0x0f,
            Err(axdriver::BluetoothError::NoDevice) => INVALID_INDEX,
            Err(axdriver::BluetoothError::Busy) => 0x0a,
            Err(axdriver::BluetoothError::Unsupported) => 0x02,
            Err(_) => FAILED,
        };
        let data = if status == 0 {
            parameters.to_vec()
        } else {
            Vec::new()
        };
        return Ok(Some((
            management_command_complete(index, opcode, status, &data)?,
            None,
        )));
    }
    if opcode == SET_IO_CAPABILITY && parameters.len() == 1 && parameters[0] <= 4 {
        let Some(adapter) = usb_adapter(index) else {
            return Ok(None);
        };
        let status = match adapter.lock().management_set_io_capability(parameters[0]) {
            Ok(()) => 0,
            Err(axdriver::BluetoothError::NoDevice) => INVALID_INDEX,
            Err(axdriver::BluetoothError::InvalidLength) => 0x0d,
            Err(_) => FAILED,
        };
        return Ok(Some((
            management_command_complete(index, opcode, status, &[])?,
            None,
        )));
    }
    if opcode == SET_PRIVACY && parameters.len() == 17 && parameters[0] <= 1 {
        let Some(adapter) = usb_adapter(index) else {
            return Ok(None);
        };
        let mut local_irk = [0; 16];
        local_irk.copy_from_slice(&parameters[1..17]);
        let (status, settings) = {
            let mut adapter = adapter.lock();
            let previous = adapter.management_settings();
            match adapter.management_set_privacy(parameters[0] != 0, local_irk) {
                Ok(settings) => (0, Some((previous, settings))),
                Err(axdriver::BluetoothError::NotUp) => (0x0f, None),
                Err(axdriver::BluetoothError::Unsupported) => (NOT_SUPPORTED, None),
                Err(axdriver::BluetoothError::NoDevice) => (INVALID_INDEX, None),
                Err(_) => (FAILED, None),
            }
        };
        let mut data = Vec::new();
        if let Some((_, settings)) = settings {
            data.extend_from_slice(&settings.to_le_bytes());
        }
        let response = management_command_complete(index, opcode, status, &data)?;
        let event = if status == 0 {
            settings_event(
                index,
                settings
                    .filter(|(previous, current)| previous != current)
                    .map(|(_, current)| current),
            )?
        } else {
            None
        };
        return Ok(Some((response, event)));
    }
    if opcode == USER_PASSKEY_REPLY || opcode == USER_PASSKEY_NEG_REPLY {
        let valid = if opcode == USER_PASSKEY_REPLY {
            parameters.len() == 11
                && parameters[6] <= 2
                && u32::from_le_bytes(parameters[7..11].try_into().unwrap_or([0; 4])) <= 999_999
        } else {
            parameters.len() == 7 && parameters[6] <= 2
        };
        if !valid {
            return Ok(None);
        }
        let Some(adapter) = usb_adapter(index) else {
            return Ok(None);
        };
        let mut address = [0; 6];
        address.copy_from_slice(&parameters[..6]);
        let address_type = parameters[6];
        let session_action = if address_type == 0 {
            None
        } else {
            let mut sessions = SMP_SESSIONS.lock();
            let Some(session) = sessions.iter_mut().find(|session| {
                session.index == index
                    && session.address == address
                    && session.address_type == address_type
                    && session.pending_passkey
            }) else {
                return Ok(Some((
                    management_command_complete(index, opcode, 0x02, &[])?,
                    None,
                )));
            };
            session.pending_passkey = false;
            if opcode == USER_PASSKEY_REPLY {
                Some((
                    session.handle,
                    session.initiator.provide_passkey(u32::from_le_bytes(
                        parameters[7..11].try_into().unwrap_or([0; 4]),
                    )),
                ))
            } else {
                let handle = session.handle;
                sessions.retain(|entry| entry.index != index || entry.handle != handle);
                Some((
                    handle,
                    axdriver::bluetooth_smp::Action::Failed(
                        axdriver::bluetooth_smp::PairingError::UserRejected,
                    ),
                ))
            }
        };
        let status = if let Some((handle, action)) = session_action {
            match action {
                axdriver::bluetooth_smp::Action::Send(pdu) => {
                    if adapter.lock().management_send_smp(handle, &pdu).is_ok() {
                        0
                    } else {
                        FAILED
                    }
                }
                axdriver::bluetooth_smp::Action::Failed(_) => {
                    let _ = adapter.lock().management_send_smp(handle, &[0x05, 0x01]);
                    emit_smp_auth_failed(index, address, address_type, 0x01);
                    0
                }
                _ => FAILED,
            }
        } else {
            NOT_SUPPORTED
        };
        let data = if status == 0 {
            parameters[..7].to_vec()
        } else {
            Vec::new()
        };
        return Ok(Some((
            management_command_complete(index, opcode, status, &data)?,
            None,
        )));
    }
    if opcode == USER_CONFIRM_REPLY || opcode == USER_CONFIRM_NEG_REPLY {
        if parameters.len() != 7 || parameters[6] > 2 {
            return Ok(None);
        }
        let Some(adapter) = usb_adapter(index) else {
            return Ok(None);
        };
        let mut address = [0; 6];
        address.copy_from_slice(&parameters[..6]);
        if parameters[6] != 0 {
            let pairing = {
                let mut sessions = SMP_SESSIONS.lock();
                sessions
                    .iter_mut()
                    .find(|session| {
                        session.index == index
                            && session.address == address
                            && session.address_type == parameters[6]
                            && session.pending_confirmation
                    })
                    .map(|session| {
                        session.pending_confirmation = false;
                        (
                            session.handle,
                            session
                                .initiator
                                .user_confirmation(opcode == USER_CONFIRM_REPLY),
                        )
                    })
            };
            let status = match pairing {
                Some((handle, axdriver::bluetooth_smp::Action::Send(pdu))) => {
                    match adapter.lock().management_send_smp(handle, &pdu) {
                        Ok(()) => 0,
                        Err(_) => FAILED,
                    }
                }
                Some((handle, axdriver::bluetooth_smp::Action::Failed(error))) => {
                    let reason = smp_failure_reason(error);
                    if error != axdriver::bluetooth_smp::PairingError::PeerFailed {
                        let _ = adapter.lock().management_send_smp(handle, &[0x05, reason]);
                    }
                    SMP_SESSIONS
                        .lock()
                        .retain(|session| session.index != index || session.handle != handle);
                    emit_smp_auth_failed(index, address, parameters[6], reason);
                    0
                }
                _ => 0x02, // no pending LE confirmation
            };
            let data = if status == 0 {
                parameters.to_vec()
            } else {
                Vec::new()
            };
            return Ok(Some((
                management_command_complete(index, opcode, status, &data)?,
                None,
            )));
        }
        let status = match adapter
            .lock()
            .management_user_confirmation(address, opcode == USER_CONFIRM_REPLY)
        {
            Ok(()) => 0,
            Err(axdriver::BluetoothError::NotUp) => 0x0f,
            Err(axdriver::BluetoothError::NoDevice) => INVALID_INDEX,
            Err(axdriver::BluetoothError::Unsupported) => NOT_SUPPORTED,
            Err(_) => FAILED,
        };
        let data = if status == 0 {
            parameters.to_vec()
        } else {
            Vec::new()
        };
        return Ok(Some((
            management_command_complete(index, opcode, status, &data)?,
            None,
        )));
    }
    if opcode == PIN_CODE_REPLY || opcode == PIN_CODE_NEG_REPLY {
        let expected_len = if opcode == PIN_CODE_REPLY { 24 } else { 7 };
        if parameters.len() != expected_len || parameters[6] != 0 {
            return Ok(None);
        }
        let pin = if opcode == PIN_CODE_REPLY {
            let pin_len = usize::from(parameters[7]);
            if pin_len == 0 || pin_len > 16 {
                return Ok(None);
            }
            Some(&parameters[8..8 + pin_len])
        } else {
            None
        };
        let Some(adapter) = usb_adapter(index) else {
            return Ok(None);
        };
        let mut address = [0; 6];
        address.copy_from_slice(&parameters[..6]);
        let status = match adapter.lock().management_pin_code_reply(address, pin) {
            Ok(()) => 0,
            Err(axdriver::BluetoothError::NotUp) => 0x0f,
            Err(axdriver::BluetoothError::NoDevice) => INVALID_INDEX,
            Err(axdriver::BluetoothError::InvalidLength) => 0x0d,
            Err(_) => FAILED,
        };
        let data = if status == 0 {
            parameters[..7].to_vec()
        } else {
            Vec::new()
        };
        return Ok(Some((
            management_command_complete(index, opcode, status, &data)?,
            None,
        )));
    }
    if opcode == START_DISCOVERY || opcode == STOP_DISCOVERY {
        if parameters.len() != 1 || !matches!(parameters[0], 1 | 6) {
            return Ok(None);
        }
        let Some(adapter) = usb_adapter(index) else {
            return Ok(None);
        };
        let start = opcode == START_DISCOVERY;
        let discovery_type = parameters[0];
        let result = adapter.lock().management_discovery(discovery_type, start);
        let (status, data, event) = match result {
            Ok(changed) => (
                0,
                alloc::vec![discovery_type],
                changed.then_some((discovery_type, u8::from(start))),
            ),
            Err(axdriver::BluetoothError::NoDevice) => (INVALID_INDEX, Vec::new(), None),
            Err(axdriver::BluetoothError::Unsupported) => (NOT_SUPPORTED, Vec::new(), None),
            Err(_) => (FAILED, Vec::new(), None),
        };
        let response = management_command_complete(index, opcode, status, &data)?;
        let event = if let Some((kind, discovering)) = event {
            Some(discovering_event(index, kind, discovering)?)
        } else {
            None
        };
        return Ok(Some((response, event)));
    }
    if opcode == SET_POWERED {
        if parameters.len() != 1 || parameters[0] > 1 {
            return Ok(None);
        }
        let Some(adapter) = usb_adapter(index) else {
            return Ok(None);
        };
        let (status, settings) = {
            let mut adapter = adapter.lock();
            let previous = adapter.management_settings();
            match adapter.set_device_up(parameters[0] != 0) {
                Ok(()) => (0, Some((previous, adapter.management_settings()))),
                Err(axdriver::BluetoothError::Unsupported) => (NOT_SUPPORTED, None),
                Err(axdriver::BluetoothError::NoDevice) => (INVALID_INDEX, None),
                Err(_) => (FAILED, None),
            }
        };
        let mut data = Vec::new();
        if let Some((_, current)) = settings {
            data.extend_from_slice(&current.to_le_bytes());
        }
        let response = management_command_complete(index, opcode, status, &data)?;
        let event = if status == 0 {
            settings_event(
                index,
                settings
                    .filter(|(previous, current)| previous != current)
                    .map(|(_, current)| current),
            )?
        } else {
            None
        };
        return Ok(Some((response, event)));
    }
    let (setting, enabled) = match opcode {
        SET_DISCOVERABLE
            if parameters.len() == 3 && parameters[0] <= 1 && parameters[1..] == [0, 0] =>
        {
            (MGMT_SETTING_DISCOVERABLE, parameters[0] != 0)
        }
        SET_CONNECTABLE | SET_BONDABLE | SET_SSP | SET_LE | SET_BREDR
            if parameters.len() == 1 && parameters[0] <= 1 =>
        {
            let setting = match opcode {
                SET_CONNECTABLE => MGMT_SETTING_CONNECTABLE,
                SET_BONDABLE => MGMT_SETTING_BONDABLE,
                SET_SSP => MGMT_SETTING_SSP,
                SET_LE => MGMT_SETTING_LE,
                SET_BREDR => MGMT_SETTING_BREDR,
                _ => unreachable!(),
            };
            (setting, parameters[0] != 0)
        }
        _ => return Ok(None),
    };
    let adapter = usb_adapter(index);
    let Some(adapter) = adapter else {
        return Ok(None);
    };
    let (status, settings) = {
        let mut adapter = adapter.lock();
        let previous = adapter.management_settings();
        match adapter.set_management_setting(setting, enabled) {
            Ok(settings) => (0, Some((previous, settings))),
            Err(axdriver::BluetoothError::Unsupported) => (NOT_SUPPORTED, None),
            Err(axdriver::BluetoothError::NotUp) => (FAILED, None),
            Err(axdriver::BluetoothError::NoDevice) => (INVALID_INDEX, None),
            Err(_) => (FAILED, None),
        }
    };
    let mut data = Vec::new();
    if let Some((_, settings)) = settings {
        data.extend_from_slice(&settings.to_le_bytes());
    }
    let response = management_command_complete(index, opcode, status, &data)?;
    let event = if status == 0 {
        settings_event(
            index,
            settings
                .filter(|(previous, current)| previous != current)
                .map(|(_, current)| current),
        )?
    } else {
        None
    };
    Ok(Some((response, event)))
}

#[cfg(feature = "input")]
fn schedule_pending_pair_timeout(
    index: u16,
    address: [u8; 6],
    address_type: u8,
    started_at: u64,
    adapter: UsbAdapter,
) -> bool {
    axtask::spawn_with_name(
        move || {
            if axtask::sleep(SMP_PAIR_TIMEOUT).is_err()
                || !smp_timeout_expired(axhal::time::monotonic_time_nanos(), started_at)
            {
                return;
            }
            let expired = {
                let mut pending = SMP_PENDING.lock();
                let Some(position) = pending.iter().position(|entry| {
                    entry.index == index
                        && entry.address == address
                        && entry.address_type == address_type
                        && entry.started_at == started_at
                }) else {
                    return;
                };
                pending.remove(position)
            };
            let _ = adapter
                .lock()
                .management_cancel_pair_device(expired.address, expired.address_type);
            emit_smp_auth_failed(index, address, address_type, 0x08);
        },
        alloc::string::String::from("bt-pair-timeout"),
    )
    .is_ok()
}

#[cfg(not(feature = "input"))]
fn management_controller_command(_request: &[u8]) -> AxResult<Option<(Vec<u8>, Option<Vec<u8>>)>> {
    Ok(None)
}

fn management_command_complete(
    index: u16,
    opcode: u16,
    status: u8,
    data: &[u8],
) -> AxResult<Vec<u8>> {
    let length = 3usize
        .checked_add(data.len())
        .ok_or(AxError::InvalidInput)?;
    if length > u16::MAX as usize {
        return Err(AxError::InvalidInput);
    }
    let mut response = Vec::new();
    response
        .try_reserve_exact(6 + length)
        .map_err(|_| AxError::NoMemory)?;
    response.extend_from_slice(&1u16.to_le_bytes());
    response.extend_from_slice(&index.to_le_bytes());
    response.extend_from_slice(&(length as u16).to_le_bytes());
    response.extend_from_slice(&opcode.to_le_bytes());
    response.push(status);
    response.extend_from_slice(data);
    Ok(response)
}

#[cfg(feature = "input")]
fn settings_event(index: u16, settings: Option<u32>) -> AxResult<Option<Vec<u8>>> {
    let Some(settings) = settings else {
        return Ok(None);
    };
    let mut event = Vec::new();
    event.try_reserve_exact(10).map_err(|_| AxError::NoMemory)?;
    event.extend_from_slice(&6u16.to_le_bytes());
    event.extend_from_slice(&index.to_le_bytes());
    event.extend_from_slice(&4u16.to_le_bytes());
    event.extend_from_slice(&settings.to_le_bytes());
    Ok(Some(event))
}

#[cfg(feature = "input")]
fn discovering_event(index: u16, discovery_type: u8, discovering: u8) -> AxResult<Vec<u8>> {
    let mut event = Vec::new();
    event.try_reserve_exact(8).map_err(|_| AxError::NoMemory)?;
    event.extend_from_slice(&0x13u16.to_le_bytes());
    event.extend_from_slice(&index.to_le_bytes());
    event.extend_from_slice(&2u16.to_le_bytes());
    event.extend_from_slice(&[discovery_type, discovering]);
    Ok(event)
}

#[cfg(not(feature = "input"))]
fn discovering_event(_index: u16, _discovery_type: u8, _discovering: u8) -> AxResult<Vec<u8>> {
    Ok(Vec::new())
}

#[cfg(not(feature = "input"))]
fn settings_event(_index: u16, _settings: Option<u32>) -> AxResult<Option<Vec<u8>>> {
    Ok(None)
}

fn management_controller_status(index: u16, invalid_index: u8, unsupported: u8) -> u8 {
    #[cfg(feature = "input")]
    {
        if usb_adapter(index).is_some() {
            unsupported
        } else {
            invalid_index
        }
    }
    #[cfg(not(feature = "input"))]
    {
        let _ = (index, unsupported);
        invalid_index
    }
}

fn valid_load_link_keys(parameters: &[u8]) -> bool {
    if parameters.len() < 3 || parameters[0] > 1 {
        return false;
    }
    let count = usize::from(u16::from_le_bytes([parameters[1], parameters[2]]));
    count.checked_mul(25).and_then(|size| size.checked_add(3)) == Some(parameters.len())
        && parameters[3..]
            .chunks_exact(25)
            .all(|key| key[6] <= 2 && key[7] <= 8 && key[24] <= 16)
}

fn valid_load_long_term_keys(parameters: &[u8]) -> bool {
    if parameters.len() < 2 {
        return false;
    }
    let count = usize::from(u16::from_le_bytes([parameters[0], parameters[1]]));
    count.checked_mul(36).and_then(|size| size.checked_add(2)) == Some(parameters.len())
        && parameters[2..]
            .chunks_exact(36)
            .all(|key| key[6] <= 2 && key[7] <= 4 && key[8] <= 1 && (7..=16).contains(&key[9]))
}

fn valid_load_irks(parameters: &[u8]) -> bool {
    if parameters.len() < 2 {
        return false;
    }
    let count = usize::from(u16::from_le_bytes([parameters[0], parameters[1]]));
    let Some(expected) = count.checked_mul(23).and_then(|bytes| bytes.checked_add(2)) else {
        return false;
    };
    if parameters.len() != expected {
        return false;
    }
    parameters[2..].chunks_exact(23).all(|irk| irk[6] <= 2)
}
