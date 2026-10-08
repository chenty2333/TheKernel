//! FreeBSD e1000 transmit/receive descriptor helpers.
//!
//! Translated from `sys/dev/e1000/em_txrx.c` in FreeBSD commit
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2016 Nicole Graziano <nicole@nextbsd.org>.
//! Copyright (c) 2017 Matthew Macy <mmacy@mattmacy.io>.

use alloc::vec::Vec;

use axdriver_base::{DevError, DevResult};

use super::{osdep::E1000RegisterIo, registers::*};

const ETHER_TYPE_IPV4: u16 = 0x0800;
const ETHER_TYPE_IPV6: u16 = 0x86dd;
const IP_CHECKSUM_OFFSET: u16 = 10;
const TCP_CHECKSUM_OFFSET: u16 = 16;
const UDP_CHECKSUM_OFFSET: u16 = 6;
const TSO_SENTINEL_BYTES: usize = 4;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmTxContextDescriptor {
    pub bytes: [u8; 16],
}

impl EmTxContextDescriptor {
    fn put_u8(&mut self, offset: usize, value: u8) {
        self.bytes[offset] = value;
    }
    fn put_u16(&mut self, offset: usize, value: u16) {
        self.bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn put_u32(&mut self, offset: usize, value: u32) {
        self.bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmTsoInput {
    pub ring_size: usize,
    pub producer: usize,
    pub ethernet_header_len: u16,
    pub ip_header_len: u16,
    pub tcp_header_len: u16,
    pub ether_type: u16,
    pub tso_segment_size: u16,
    pub packet_len: u32,
    pub base_txd_cmd: u32,
    pub mac: super::api::E1000MacType,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmTsoSetup {
    pub descriptor: EmTxContextDescriptor,
    pub txd_upper: u32,
    pub txd_lower: u32,
    pub next_producer: usize,
    pub tso_sentinel: bool,
}

/// upstream: em_txrx.c em_tso_setup()
pub fn em_tso_setup(input: EmTsoInput) -> EmTsoSetup {
    let mut descriptor = EmTxContextDescriptor::default();
    let header_len = input.ethernet_header_len + input.ip_header_len + input.tcp_header_len;
    let mut txd_upper = 0;
    let txd_lower = E1000_TXD_CMD_DEXT | E1000_TXD_DTYP_D | E1000_TXD_CMD_TSE;
    if input.ether_type == ETHER_TYPE_IPV4 {
        txd_upper = (E1000_TXD_POPTS_IXSM | E1000_TXD_POPTS_TXSM) << 8;
        descriptor.put_u16(2, input.ethernet_header_len + input.ip_header_len - 1);
    } else if input.ether_type == ETHER_TYPE_IPV6 {
        txd_upper = E1000_TXD_POPTS_TXSM << 8;
        descriptor.put_u16(2, 0);
    }
    descriptor.put_u8(0, input.ethernet_header_len as u8);
    descriptor.put_u8(1, (input.ethernet_header_len + IP_CHECKSUM_OFFSET) as u8);
    descriptor.put_u8(4, (input.ethernet_header_len + input.ip_header_len) as u8);
    descriptor.put_u16(6, 0);
    descriptor.put_u8(
        5,
        (input.ethernet_header_len + input.ip_header_len + TCP_CHECKSUM_OFFSET) as u8,
    );
    descriptor.put_u16(14, input.tso_segment_size);
    descriptor.put_u8(13, header_len as u8);
    let mut command =
        input.base_txd_cmd | E1000_TXD_CMD_DEXT | E1000_TXD_CMD_TSE | E1000_TXD_CMD_TCP;
    if input.ether_type == ETHER_TYPE_IPV4 {
        command |= E1000_TXD_CMD_IP;
    }
    descriptor.put_u32(
        8,
        command | input.packet_len.wrapping_sub(u32::from(header_len)),
    );
    let mut next_producer = input.producer + 1;
    if next_producer == input.ring_size {
        next_producer = 0;
    }
    EmTsoSetup {
        descriptor,
        txd_upper,
        txd_lower,
        next_producer,
        tso_sentinel: input.mac < super::api::E1000MacType::I82571,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmTxChecksumFlags {
    pub ipv4_header: bool,
    pub ipv4_tcp: bool,
    pub ipv4_udp: bool,
    pub ipv6_tcp: bool,
    pub ipv6_udp: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmTxChecksumCache {
    pub ethernet_header_len: u8,
    pub ip_header_len: u8,
    pub flags: EmTxChecksumFlags,
    pub txd_upper: u32,
    pub txd_lower: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmTxChecksumInput {
    pub ring_size: usize,
    pub producer: usize,
    pub ring_count: usize,
    pub ethernet_header_len: u8,
    pub ip_header_len: u8,
    pub flags: EmTxChecksumFlags,
    pub base_txd_cmd: u32,
    pub txd_upper: u32,
    pub txd_lower: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmTxChecksumSetup {
    pub descriptor: Option<EmTxContextDescriptor>,
    pub txd_upper: u32,
    pub txd_lower: u32,
    pub next_producer: usize,
    pub cache: EmTxChecksumCache,
}

/// upstream: em_txrx.c em_transmit_checksum_setup()
pub fn em_transmit_checksum_setup(
    input: EmTxChecksumInput,
    cache: EmTxChecksumCache,
) -> EmTxChecksumSetup {
    if input.ring_count == 1
        && cache.ethernet_header_len == input.ethernet_header_len
        && cache.ip_header_len == input.ip_header_len
        && cache.flags == input.flags
    {
        return EmTxChecksumSetup {
            descriptor: None,
            txd_upper: cache.txd_upper,
            txd_lower: cache.txd_lower,
            next_producer: input.producer,
            cache,
        };
    }
    let mut descriptor = EmTxContextDescriptor::default();
    let header_len = u16::from(input.ethernet_header_len) + u16::from(input.ip_header_len);
    let mut upper = input.txd_upper;
    let mut lower = input.txd_lower;
    let mut command = input.base_txd_cmd;
    descriptor.put_u8(0, input.ethernet_header_len);
    descriptor.put_u8(
        1,
        input
            .ethernet_header_len
            .wrapping_add(IP_CHECKSUM_OFFSET as u8),
    );
    if input.flags.ipv4_header {
        upper |= E1000_TXD_POPTS_IXSM << 8;
        descriptor.put_u16(2, header_len - 1);
        command |= E1000_TXD_CMD_IP;
    } else if input.flags.ipv6_tcp || input.flags.ipv6_udp {
        descriptor.put_u16(2, 0);
    }
    let tcp = input.flags.ipv4_tcp || input.flags.ipv6_tcp;
    let udp = input.flags.ipv4_udp || input.flags.ipv6_udp;
    if tcp || udp {
        upper |= E1000_TXD_POPTS_TXSM << 8;
        lower = E1000_TXD_CMD_DEXT | E1000_TXD_DTYP_D;
        descriptor.put_u8(4, header_len as u8);
        descriptor.put_u8(
            5,
            (header_len
                + if tcp {
                    TCP_CHECKSUM_OFFSET
                } else {
                    UDP_CHECKSUM_OFFSET
                }) as u8,
        );
        descriptor.put_u16(6, 0);
        if tcp {
            command |= E1000_TXD_CMD_TCP;
        }
    }
    descriptor.put_u32(8, E1000_TXD_CMD_IFCS | E1000_TXD_CMD_DEXT | command);
    let mut next_producer = input.producer + 1;
    if next_producer == input.ring_size {
        next_producer = 0;
    }
    let cache = EmTxChecksumCache {
        ethernet_header_len: input.ethernet_header_len,
        ip_header_len: input.ip_header_len,
        flags: input.flags,
        txd_upper: upper,
        txd_lower: lower,
    };
    EmTxChecksumSetup {
        descriptor: Some(descriptor),
        txd_upper: upper,
        txd_lower: lower,
        next_producer,
        cache,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmTxSegment {
    pub address: u64,
    pub length: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EmTxDescriptor {
    Context {
        ring_index: usize,
        descriptor: EmTxContextDescriptor,
    },
    Data {
        ring_index: usize,
        address: u64,
        lower: u32,
        upper: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmTxPacket {
    pub segments: Vec<EmTxSegment>,
    pub ring_size: usize,
    pub producer: usize,
    pub mac: super::api::E1000MacType,
    pub base_txd_cmd: u32,
    pub interrupt_requested: bool,
    pub vlan_tag: Option<u16>,
    pub checksums: EmTxChecksumFlags,
    pub tso_segment_size: Option<u16>,
    pub ethernet_header_len: u16,
    pub ip_header_len: u16,
    pub tcp_header_len: u16,
    pub ether_type: u16,
    pub packet_len: u32,
    pub report_status_pidx: usize,
    pub report_status_cidx: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmTxEncapResult {
    pub descriptors: Vec<EmTxDescriptor>,
    pub next_producer: usize,
    pub next_tso_sentinel: bool,
    pub report_status_index: Option<usize>,
    pub next_report_status_pidx: usize,
    pub bytes_accounted: u64,
    pub packets_accounted: u64,
    pub checksum_cache: EmTxChecksumCache,
}

fn advance(index: usize, ring_size: usize) -> usize {
    if index + 1 == ring_size { 0 } else { index + 1 }
}

pub trait EmTxPublishOps {
    fn publish_aim(&mut self) -> DevResult;
}

/// upstream: em_txrx.c em_isc_txd_flush()
pub fn em_isc_txd_flush<I: E1000RegisterIo, O: EmTxPublishOps>(
    io: &mut I,
    ops: &mut O,
    mac: super::api::E1000MacType,
    queue: u32,
    producer: u32,
) -> DevResult {
    io.write_register(tx_desc_tail(queue), producer)?;
    if mac >= super::api::E1000MacType::I82540 {
        ops.publish_aim()?;
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmTxCompletionRing {
    pub report_status: Vec<Option<usize>>,
    pub descriptor_status: Vec<u8>,
    pub report_status_cidx: usize,
    pub report_status_pidx: usize,
    pub processed_cidx: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmTxCompletionUpdate {
    pub credits: usize,
    pub report_status_cidx: usize,
    pub processed_cidx: usize,
}

/// upstream: em_txrx.c em_isc_txd_credits_update()
pub fn em_isc_txd_credits_update(
    ring: &EmTxCompletionRing,
    clear: bool,
) -> DevResult<EmTxCompletionUpdate> {
    let count = ring.report_status.len();
    if count == 0
        || !count.is_power_of_two()
        || ring.descriptor_status.len() != count
        || ring.report_status_cidx >= count
        || ring.report_status_pidx >= count
        || ring.processed_cidx >= count
    {
        return Err(DevError::InvalidParam);
    }
    let mut report_status_cidx = ring.report_status_cidx;
    let mut processed_cidx = ring.processed_cidx;
    if report_status_cidx == ring.report_status_pidx {
        return Ok(EmTxCompletionUpdate {
            credits: 0,
            report_status_cidx,
            processed_cidx,
        });
    }
    let mut current = ring.report_status[report_status_cidx].ok_or(DevError::BadState)?;
    if current >= count {
        return Err(DevError::InvalidParam);
    }
    if ring.descriptor_status[current] & E1000_TXD_STAT_DD as u8 == 0 {
        return Ok(EmTxCompletionUpdate {
            credits: 0,
            report_status_cidx,
            processed_cidx,
        });
    }
    if !clear {
        return Ok(EmTxCompletionUpdate {
            credits: 1,
            report_status_cidx,
            processed_cidx,
        });
    }
    let mut credits = 0;
    loop {
        let delta = if current >= processed_cidx {
            current - processed_cidx
        } else {
            current + count - processed_cidx
        };
        if delta == 0 {
            return Err(DevError::BadState);
        }
        credits += delta;
        processed_cidx = current;
        report_status_cidx = (report_status_cidx + 1) & (count - 1);
        if report_status_cidx == ring.report_status_pidx {
            break;
        }
        current = ring.report_status[report_status_cidx].ok_or(DevError::BadState)?;
        if current >= count {
            return Err(DevError::InvalidParam);
        }
        if ring.descriptor_status[current] & E1000_TXD_STAT_DD as u8 == 0 {
            break;
        }
    }
    Ok(EmTxCompletionUpdate {
        credits,
        report_status_cidx,
        processed_cidx,
    })
}

/// upstream: em_txrx.c em_isc_txd_encap()
pub fn em_isc_txd_encap(
    packet: &EmTxPacket,
    previous_tso_sentinel: bool,
    checksum_cache: EmTxChecksumCache,
    queue_count: usize,
) -> DevResult<EmTxEncapResult> {
    if packet.ring_size == 0
        || !packet.ring_size.is_power_of_two()
        || packet.producer >= packet.ring_size
        || packet.report_status_pidx >= packet.ring_size
        || packet.report_status_cidx >= packet.ring_size
        || packet.segments.is_empty()
        || packet.segments.iter().any(|segment| segment.length == 0)
    {
        return Err(DevError::InvalidParam);
    }
    let mut descriptors = Vec::new();
    let tso = packet.tso_segment_size.filter(|&size| size != 0);
    let mut txd_upper = 0;
    let mut txd_lower = 0;
    let mut producer = packet.producer;
    let mut next_checksum_cache = checksum_cache;
    let (sentinel, next_tso_sentinel) = if let Some(tso_segment_size) = tso {
        let setup = em_tso_setup(EmTsoInput {
            ring_size: packet.ring_size,
            producer,
            ethernet_header_len: packet.ethernet_header_len,
            ip_header_len: packet.ip_header_len,
            tcp_header_len: packet.tcp_header_len,
            ether_type: packet.ether_type,
            tso_segment_size,
            packet_len: packet.packet_len,
            base_txd_cmd: packet.base_txd_cmd,
            mac: packet.mac,
        });
        descriptors.push(EmTxDescriptor::Context {
            ring_index: producer,
            descriptor: setup.descriptor,
        });
        producer = setup.next_producer;
        txd_upper = setup.txd_upper;
        txd_lower = setup.txd_lower;
        (setup.tso_sentinel, setup.tso_sentinel)
    } else {
        let sentinel = previous_tso_sentinel && packet.segments.len() == 1;
        if packet.checksums.ipv4_header
            || packet.checksums.ipv4_tcp
            || packet.checksums.ipv4_udp
            || packet.checksums.ipv6_tcp
            || packet.checksums.ipv6_udp
        {
            let setup = em_transmit_checksum_setup(
                EmTxChecksumInput {
                    ring_size: packet.ring_size,
                    producer,
                    ring_count: queue_count,
                    ethernet_header_len: packet.ethernet_header_len as u8,
                    ip_header_len: packet.ip_header_len as u8,
                    flags: packet.checksums,
                    base_txd_cmd: packet.base_txd_cmd,
                    txd_upper,
                    txd_lower,
                },
                checksum_cache,
            );
            if let Some(descriptor) = setup.descriptor {
                descriptors.push(EmTxDescriptor::Context {
                    ring_index: producer,
                    descriptor,
                });
            }
            producer = setup.next_producer;
            txd_upper = setup.txd_upper;
            txd_lower = setup.txd_lower;
            next_checksum_cache = setup.cache;
        }
        (sentinel, false)
    };

    if let Some(vlan) = packet.vlan_tag {
        txd_upper |= u32::from(vlan) << 16;
        txd_lower |= E1000_TXD_CMD_VLE;
    }
    let mut last_data_index = producer;
    for (segment_index, segment) in packet.segments.iter().enumerate() {
        let is_last = segment_index + 1 == packet.segments.len();
        let split = sentinel && is_last && segment.length > 8;
        let first_len = if split {
            segment.length - TSO_SENTINEL_BYTES
        } else {
            segment.length
        };
        descriptors.push(EmTxDescriptor::Data {
            ring_index: producer,
            address: segment.address,
            lower: E1000_TXD_CMD_IFCS | packet.base_txd_cmd | txd_lower | first_len as u32,
            upper: txd_upper,
        });
        last_data_index = producer;
        producer = advance(producer, packet.ring_size);
        if split {
            descriptors.push(EmTxDescriptor::Data {
                ring_index: producer,
                address: segment.address + first_len as u64,
                lower: E1000_TXD_CMD_IFCS
                    | packet.base_txd_cmd
                    | txd_lower
                    | TSO_SENTINEL_BYTES as u32,
                upper: txd_upper,
            });
            last_data_index = producer;
            producer = advance(producer, packet.ring_size);
        }
    }
    if let Some(EmTxDescriptor::Data { lower, .. }) = descriptors
        .iter_mut()
        .rev()
        .find(|descriptor| matches!(descriptor, EmTxDescriptor::Data { .. }))
    {
        *lower |= E1000_TXD_CMD_EOP;
        if packet.interrupt_requested {
            *lower |= E1000_TXD_CMD_RS;
        }
    }
    let report_status_index = packet.interrupt_requested.then_some(last_data_index);
    let next_report_status_pidx = if packet.interrupt_requested {
        let next = (packet.report_status_pidx + 1) & (packet.ring_size - 1);
        if next == packet.report_status_cidx {
            return Err(DevError::BadState);
        }
        next
    } else {
        packet.report_status_pidx
    };
    let (bytes_accounted, packets_accounted) = if let Some(tso_mss) = tso {
        let header_len = u32::from(packet.ethernet_header_len)
            + u32::from(packet.ip_header_len)
            + u32::from(packet.tcp_header_len);
        if packet.packet_len > header_len {
            let payload = u64::from(packet.packet_len - header_len);
            let segments = payload.div_ceil(u64::from(tso_mss));
            (
                u64::from(packet.packet_len) + (segments - 1) * u64::from(header_len),
                segments,
            )
        } else {
            (u64::from(packet.packet_len), 1)
        }
    } else {
        (u64::from(packet.packet_len), 1)
    };
    Ok(EmTxEncapResult {
        descriptors,
        next_producer: producer,
        next_tso_sentinel,
        report_status_index,
        next_report_status_pidx,
        bytes_accounted,
        packets_accounted,
        checksum_cache: next_checksum_cache,
    })
}

const RX_FRAME_ERROR_MASK: u32 = 0x97;
const RX_EXT_FRAME_ERROR_MASK: u32 = 0x9700_0000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmLegacyRxDescriptor {
    pub address: u64,
    pub length: u16,
    pub checksum: u16,
    pub status: u8,
    pub errors: u8,
    pub special: u16,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmAdvancedRxDescriptor {
    pub address: u64,
    pub status_error: u32,
    pub length: u16,
    pub vlan: u16,
    pub packet_info: u32,
    pub rss_hash: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmRxFragment {
    pub index: usize,
    pub length: u16,
    pub free_list: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmRxStats {
    pub bytes: u64,
    pub packets: u64,
    pub dropped: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EmRxPacket {
    pub length: u32,
    pub fragments: Vec<EmRxFragment>,
    pub checksum: EmRxChecksum,
    pub vlan_tag: Option<u16>,
    pub flow_id: Option<u32>,
    pub rss_type: EmRssType,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmRxCapabilities {
    pub checksum: bool,
    pub vlan_tagging: bool,
}

fn rx_available<I, F>(descriptors: &[I], index: usize, budget: usize, mut status: F) -> usize
where
    F: FnMut(&I) -> (u32, u32),
{
    if descriptors.is_empty() || index >= descriptors.len() {
        return 0;
    }
    let mut count = 0;
    let mut cursor = index;
    let mut scanned = 0;
    while scanned < descriptors.len() && count <= budget {
        let (staterr, _) = status(&descriptors[cursor]);
        if staterr & E1000_RXD_STAT_DD == 0 {
            break;
        }
        cursor = advance(cursor, descriptors.len());
        scanned += 1;
        if staterr & E1000_RXD_STAT_EOP != 0 {
            count += 1;
        }
    }
    count
}

/// upstream: em_txrx.c lem_isc_rxd_refill()
pub fn lem_isc_rxd_refill(
    descriptors: &mut [EmLegacyRxDescriptor],
    start: usize,
    addresses: &[u64],
) -> DevResult<usize> {
    if descriptors.is_empty() || start >= descriptors.len() || addresses.len() > descriptors.len() {
        return Err(DevError::InvalidParam);
    }
    let mut cursor = start;
    for &address in addresses {
        descriptors[cursor].address = address;
        descriptors[cursor].status = 0;
        cursor = advance(cursor, descriptors.len());
    }
    Ok(cursor)
}

/// upstream: em_txrx.c em_isc_rxd_refill()
pub fn em_isc_rxd_refill(
    descriptors: &mut [EmAdvancedRxDescriptor],
    start: usize,
    addresses: &[u64],
) -> DevResult<usize> {
    if descriptors.is_empty() || start >= descriptors.len() || addresses.len() > descriptors.len() {
        return Err(DevError::InvalidParam);
    }
    let mut cursor = start;
    for &address in addresses {
        descriptors[cursor].address = address;
        descriptors[cursor].status_error = 0;
        cursor = advance(cursor, descriptors.len());
    }
    Ok(cursor)
}

pub trait EmRxPublishOps {
    fn publish_aim_rx(&mut self) -> DevResult;
}

/// upstream: em_txrx.c em_isc_rxd_flush()
pub fn em_isc_rxd_flush<I: E1000RegisterIo, O: EmRxPublishOps>(
    io: &mut I,
    ops: &mut O,
    mac: super::api::E1000MacType,
    queue: u32,
    producer: u32,
) -> DevResult {
    io.write_register(rx_desc_tail(queue), producer)?;
    if mac >= super::api::E1000MacType::I82540 {
        ops.publish_aim_rx()?;
    }
    Ok(())
}

/// upstream: em_txrx.c lem_isc_rxd_available()
pub fn lem_isc_rxd_available(
    descriptors: &[EmLegacyRxDescriptor],
    index: usize,
    budget: usize,
) -> usize {
    rx_available(descriptors, index, budget, |descriptor| {
        (u32::from(descriptor.status), u32::from(descriptor.errors))
    })
}

/// upstream: em_txrx.c em_isc_rxd_available()
pub fn em_isc_rxd_available(
    descriptors: &[EmAdvancedRxDescriptor],
    index: usize,
    budget: usize,
) -> usize {
    rx_available(descriptors, index, budget, |descriptor| {
        (descriptor.status_error, descriptor.status_error >> 24)
    })
}

/// upstream: em_txrx.c lem_isc_rxd_pkt_get()
pub fn lem_isc_rxd_pkt_get(
    descriptors: &mut [EmLegacyRxDescriptor],
    index: usize,
    capabilities: EmRxCapabilities,
    stats: &mut EmRxStats,
) -> DevResult<EmRxPacket> {
    if descriptors.is_empty() || index >= descriptors.len() {
        return Err(DevError::InvalidParam);
    }
    let mut packet = EmRxPacket::default();
    let mut cursor = index;
    loop {
        let descriptor = &mut descriptors[cursor];
        if u32::from(descriptor.status) & E1000_RXD_STAT_DD == 0 {
            return Err(DevError::BadState);
        }
        let status = descriptor.status;
        let errors = descriptor.errors;
        let length = descriptor.length;
        packet.length += u32::from(length);
        let eop = u32::from(status) & E1000_RXD_STAT_EOP != 0;
        if u32::from(errors) & RX_FRAME_ERROR_MASK != 0 {
            stats.dropped += 1;
            return Err(DevError::Io);
        }
        packet.fragments.push(EmRxFragment {
            index: cursor,
            length,
            free_list: 0,
        });
        descriptor.status = 0;
        if eop {
            if capabilities.checksum {
                packet.checksum = em_receive_checksum(u16::from(status), errors);
            }
            if capabilities.vlan_tagging && u32::from(status) & E1000_RXD_STAT_VP != 0 {
                packet.vlan_tag = Some(descriptor.special);
            }
            break;
        }
        cursor = advance(cursor, descriptors.len());
        if packet.fragments.len() == descriptors.len() {
            return Err(DevError::BadState);
        }
    }
    stats.bytes += u64::from(packet.length);
    stats.packets += 1;
    Ok(packet)
}

/// upstream: em_txrx.c em_isc_rxd_pkt_get()
pub fn em_isc_rxd_pkt_get(
    descriptors: &mut [EmAdvancedRxDescriptor],
    index: usize,
    capabilities: EmRxCapabilities,
    stats: &mut EmRxStats,
) -> DevResult<EmRxPacket> {
    if descriptors.is_empty() || index >= descriptors.len() {
        return Err(DevError::InvalidParam);
    }
    let mut packet = EmRxPacket::default();
    let mut cursor = index;
    loop {
        let descriptor = &mut descriptors[cursor];
        let staterr = descriptor.status_error;
        if staterr & E1000_RXD_STAT_DD == 0 {
            return Err(DevError::BadState);
        }
        let packet_info = descriptor.packet_info;
        let length = descriptor.length;
        packet.length += u32::from(length);
        let eop = staterr & E1000_RXD_STAT_EOP != 0;
        if staterr & RX_EXT_FRAME_ERROR_MASK != 0 {
            stats.dropped += 1;
            return Err(DevError::Io);
        }
        packet.fragments.push(EmRxFragment {
            index: cursor,
            length,
            free_list: 0,
        });
        descriptor.status_error &= !0xff;
        if eop {
            if capabilities.checksum {
                packet.checksum = em_receive_checksum(staterr as u16, (staterr >> 24) as u8);
            }
            if capabilities.vlan_tagging && staterr & E1000_RXD_STAT_VP != 0 {
                packet.vlan_tag = Some(descriptor.vlan);
            }
            packet.flow_id = Some(descriptor.rss_hash);
            packet.rss_type = em_determine_rsstype(packet_info);
            break;
        }
        cursor = advance(cursor, descriptors.len());
        if packet.fragments.len() == descriptors.len() {
            return Err(DevError::BadState);
        }
    }
    stats.bytes += u64::from(packet.length);
    stats.packets += 1;
    Ok(packet)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EmRxChecksum {
    pub ip_checked: bool,
    pub ip_valid: bool,
    pub data_valid: bool,
    pub pseudo_header: bool,
    pub data: u16,
}

/// upstream: em_txrx.c em_receive_checksum()
pub fn em_receive_checksum(status: u16, errors: u8) -> EmRxChecksum {
    if u32::from(status) & E1000_RXD_STAT_IXSM != 0
        || u32::from(errors) & (E1000_RXD_ERR_IPE | E1000_RXD_ERR_TCPE) != 0
    {
        return EmRxChecksum::default();
    }
    let ip = u32::from(status) & E1000_RXD_STAT_IPCS != 0;
    let l4 = u32::from(status) & (E1000_RXD_STAT_TCPCS | E1000_RXD_STAT_UDPCS) != 0;
    EmRxChecksum {
        ip_checked: ip,
        ip_valid: ip,
        data_valid: l4,
        pseudo_header: l4,
        data: if l4 { u16::MAX } else { 0 },
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EmRssType {
    #[default]
    None,
    TcpIpv4,
    Ipv4,
    TcpIpv6,
    Ipv6Ex,
    Ipv6,
    TcpIpv6Ex,
    UdpIpv4,
    UdpIpv6,
    UdpIpv6Ex,
}

/// upstream: em_txrx.c em_determine_rsstype()
pub fn em_determine_rsstype(packet_info: u32) -> EmRssType {
    match packet_info & 0x0f {
        0x01 => EmRssType::TcpIpv4,
        0x02 => EmRssType::Ipv4,
        0x03 => EmRssType::TcpIpv6,
        0x04 => EmRssType::Ipv6Ex,
        0x05 => EmRssType::Ipv6,
        0x06 => EmRssType::TcpIpv6Ex,
        0x07 => EmRssType::UdpIpv4,
        0x08 => EmRssType::UdpIpv6,
        0x09 => EmRssType::UdpIpv6Ex,
        _ => EmRssType::None,
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;

    use super::*;

    #[derive(Default)]
    struct RegisterMock(BTreeMap<u32, u32>);
    impl super::super::osdep::E1000RegisterIo for RegisterMock {
        fn read_register(&mut self, register: u32) -> DevResult<u32> {
            Ok(self.0.get(&register).copied().unwrap_or(0))
        }
        fn write_register(&mut self, register: u32, value: u32) -> DevResult {
            self.0.insert(register, value);
            Ok(())
        }
        fn delay_us(&mut self, _micros: u32) {}
        fn invalid_tail_write(&mut self, _direction: &'static str) {}
    }
    #[derive(Default)]
    struct AimMock(usize);
    impl EmTxPublishOps for AimMock {
        fn publish_aim(&mut self) -> DevResult {
            self.0 += 1;
            Ok(())
        }
    }
    impl EmRxPublishOps for AimMock {
        fn publish_aim_rx(&mut self) -> DevResult {
            self.0 += 1;
            Ok(())
        }
    }

    #[test]
    fn receive_checksum_flags_match_status_and_error_gates() {
        assert_eq!(
            em_receive_checksum((E1000_RXD_STAT_IPCS | E1000_RXD_STAT_TCPCS) as u16, 0),
            EmRxChecksum {
                ip_checked: true,
                ip_valid: true,
                data_valid: true,
                pseudo_header: true,
                data: u16::MAX,
            }
        );
        assert_eq!(
            em_receive_checksum(E1000_RXD_STAT_IPCS as u16, E1000_RXD_ERR_IPE as u8),
            EmRxChecksum::default()
        );
        assert_eq!(
            em_receive_checksum(E1000_RXD_STAT_IXSM as u16, 0),
            EmRxChecksum::default()
        );
    }

    #[test]
    fn rss_type_nibble_maps_upstream_protocol_values() {
        assert_eq!(em_determine_rsstype(0x101), EmRssType::TcpIpv4);
        assert_eq!(em_determine_rsstype(0x08), EmRssType::UdpIpv6);
        assert_eq!(em_determine_rsstype(0x0a), EmRssType::None);
    }

    #[test]
    fn tso_context_preserves_header_offsets_mss_and_legacy_sentinel_policy() {
        let setup = em_tso_setup(EmTsoInput {
            ring_size: 8,
            producer: 7,
            ethernet_header_len: 14,
            ip_header_len: 20,
            tcp_header_len: 20,
            ether_type: ETHER_TYPE_IPV4,
            tso_segment_size: 1460,
            packet_len: 4096,
            base_txd_cmd: E1000_TXD_CMD_IFCS,
            mac: super::super::api::E1000MacType::I82540,
        });
        assert_eq!(setup.next_producer, 0);
        assert!(setup.tso_sentinel);
        assert_eq!(
            setup.txd_lower,
            E1000_TXD_CMD_DEXT | E1000_TXD_DTYP_D | E1000_TXD_CMD_TSE
        );
        assert_eq!(
            setup.txd_upper,
            (E1000_TXD_POPTS_IXSM | E1000_TXD_POPTS_TXSM) << 8
        );
        assert_eq!(setup.descriptor.bytes[0..8], [14, 24, 33, 0, 34, 50, 0, 0]);
        assert_eq!(
            u16::from_le_bytes([setup.descriptor.bytes[14], setup.descriptor.bytes[15]]),
            1460
        );
        assert_eq!(setup.descriptor.bytes[13], 54);
        assert_eq!(
            u32::from_le_bytes(setup.descriptor.bytes[8..12].try_into().unwrap()),
            E1000_TXD_CMD_IFCS
                | E1000_TXD_CMD_DEXT
                | E1000_TXD_CMD_TSE
                | E1000_TXD_CMD_TCP
                | E1000_TXD_CMD_IP
                | (4096 - 54)
        );
    }

    #[test]
    fn checksum_context_reuses_only_matching_single_queue_state() {
        let flags = EmTxChecksumFlags {
            ipv4_header: true,
            ipv4_tcp: true,
            ..EmTxChecksumFlags::default()
        };
        let cache = EmTxChecksumCache {
            ethernet_header_len: 14,
            ip_header_len: 20,
            flags,
            txd_upper: 0x100,
            txd_lower: 0x200,
        };
        let reuse = em_transmit_checksum_setup(
            EmTxChecksumInput {
                ring_size: 8,
                producer: 3,
                ring_count: 1,
                ethernet_header_len: 14,
                ip_header_len: 20,
                flags,
                base_txd_cmd: 0,
                txd_upper: 0,
                txd_lower: 0,
            },
            cache,
        );
        assert!(reuse.descriptor.is_none());
        assert_eq!(
            (reuse.next_producer, reuse.txd_upper, reuse.txd_lower),
            (3, 0x100, 0x200)
        );

        let fresh = em_transmit_checksum_setup(
            EmTxChecksumInput {
                ring_size: 4,
                producer: 3,
                ring_count: 2,
                ethernet_header_len: 14,
                ip_header_len: 20,
                flags,
                base_txd_cmd: 0,
                txd_upper: 0,
                txd_lower: 0,
            },
            EmTxChecksumCache::default(),
        );
        let desc = fresh.descriptor.unwrap();
        assert_eq!(fresh.next_producer, 0);
        assert_eq!(desc.bytes[0..8], [14, 24, 33, 0, 34, 50, 0, 0]);
        assert_eq!(
            fresh.txd_upper,
            E1000_TXD_POPTS_IXSM << 8 | E1000_TXD_POPTS_TXSM << 8
        );
        assert_eq!(fresh.txd_lower, E1000_TXD_CMD_DEXT | E1000_TXD_DTYP_D);
    }

    #[test]
    fn tx_encapsulation_splits_legacy_tso_sentinel_and_accounts_wire_bytes() {
        let packet = EmTxPacket {
            segments: alloc::vec![EmTxSegment {
                address: 0x1000,
                length: 200
            }],
            ring_size: 8,
            producer: 6,
            mac: super::super::api::E1000MacType::I82540,
            base_txd_cmd: 0,
            interrupt_requested: true,
            vlan_tag: Some(0x123),
            checksums: EmTxChecksumFlags::default(),
            tso_segment_size: Some(100),
            ethernet_header_len: 14,
            ip_header_len: 20,
            tcp_header_len: 20,
            ether_type: ETHER_TYPE_IPV4,
            packet_len: 254,
            report_status_pidx: 3,
            report_status_cidx: 5,
        };
        let result = em_isc_txd_encap(&packet, false, EmTxChecksumCache::default(), 1).unwrap();
        assert_eq!(result.descriptors.len(), 3);
        assert_eq!(result.next_producer, 1);
        assert!(result.next_tso_sentinel);
        assert_eq!(result.report_status_index, Some(0));
        assert_eq!(result.next_report_status_pidx, 4);
        assert_eq!((result.bytes_accounted, result.packets_accounted), (308, 2));
        assert!(matches!(
            result.descriptors[0],
            EmTxDescriptor::Context { ring_index: 6, .. }
        ));
        assert_eq!(
            result.descriptors[1],
            EmTxDescriptor::Data {
                ring_index: 7,
                address: 0x1000,
                lower: E1000_TXD_CMD_IFCS
                    | E1000_TXD_CMD_DEXT
                    | E1000_TXD_DTYP_D
                    | E1000_TXD_CMD_TSE
                    | E1000_TXD_CMD_VLE
                    | 196,
                upper: (E1000_TXD_POPTS_IXSM | E1000_TXD_POPTS_TXSM) << 8 | (0x123 << 16),
            }
        );
        assert!(matches!(
            result.descriptors[2],
            EmTxDescriptor::Data { ring_index: 0, address: 0x10c4, lower, .. }
                if lower & (E1000_TXD_CMD_EOP | E1000_TXD_CMD_RS) == (E1000_TXD_CMD_EOP | E1000_TXD_CMD_RS)
        ));
    }

    #[test]
    fn tx_flush_and_report_status_credit_walk_match_source_boundaries() {
        let mut io = RegisterMock::default();
        let mut aim = AimMock::default();
        em_isc_txd_flush(
            &mut io,
            &mut aim,
            super::super::api::E1000MacType::I82540,
            0,
            7,
        )
        .unwrap();
        assert_eq!(io.0[&tx_desc_tail(0)], 7);
        assert_eq!(aim.0, 1);
        em_isc_txd_flush(
            &mut io,
            &mut aim,
            super::super::api::E1000MacType::I82543,
            1,
            2,
        )
        .unwrap();
        assert_eq!(io.0[&tx_desc_tail(1)], 2);
        assert_eq!(aim.0, 1);

        let ring = EmTxCompletionRing {
            report_status: alloc::vec![Some(2), Some(5), None, None, None, None, None, None],
            descriptor_status: alloc::vec![
                0,
                0,
                E1000_TXD_STAT_DD as u8,
                0,
                0,
                E1000_TXD_STAT_DD as u8,
                0,
                0
            ],
            report_status_cidx: 0,
            report_status_pidx: 2,
            processed_cidx: 0,
        };
        let poll = em_isc_txd_credits_update(&ring, false).unwrap();
        assert_eq!(poll.credits, 1);
        assert_eq!(poll.report_status_cidx, 0);
        let clear = em_isc_txd_credits_update(&ring, true).unwrap();
        assert_eq!(clear.credits, 5);
        assert_eq!(clear.report_status_cidx, 2);
        assert_eq!(clear.processed_cidx, 5);
    }

    #[test]
    fn rx_refill_available_and_flush_keep_legacy_advanced_ring_rules() {
        let mut legacy = alloc::vec![EmLegacyRxDescriptor::default(); 4];
        legacy[3].status = E1000_RXD_STAT_DD as u8;
        let next = lem_isc_rxd_refill(&mut legacy, 3, &[0x1000, 0x2000]).unwrap();
        assert_eq!(next, 1);
        assert_eq!(legacy[3].address, 0x1000);
        assert_eq!(legacy[0].address, 0x2000);
        assert_eq!(legacy[3].status, 0);
        legacy[0].status = (E1000_RXD_STAT_DD | E1000_RXD_STAT_EOP) as u8;
        legacy[1].status = (E1000_RXD_STAT_DD | E1000_RXD_STAT_EOP) as u8;
        assert_eq!(lem_isc_rxd_available(&legacy, 0, 0), 1);

        let mut advanced = alloc::vec![EmAdvancedRxDescriptor::default(); 4];
        advanced[3].status_error = E1000_RXD_STAT_DD;
        assert_eq!(em_isc_rxd_refill(&mut advanced, 3, &[0x3000]).unwrap(), 0);
        assert_eq!(advanced[3].address, 0x3000);
        assert_eq!(advanced[3].status_error, 0);
        advanced[3].status_error = E1000_RXD_STAT_DD | E1000_RXD_STAT_EOP;
        assert_eq!(em_isc_rxd_available(&advanced, 3, 0), 1);

        let mut io = RegisterMock::default();
        let mut aim = AimMock::default();
        em_isc_rxd_flush(
            &mut io,
            &mut aim,
            super::super::api::E1000MacType::I82540,
            1,
            2,
        )
        .unwrap();
        assert_eq!(io.0[&rx_desc_tail(1)], 2);
        assert_eq!(aim.0, 1);
    }

    #[test]
    fn rx_packet_get_accumulates_fragments_and_drops_frame_errors() {
        let mut descriptors = alloc::vec![EmLegacyRxDescriptor::default(); 4];
        descriptors[2] = EmLegacyRxDescriptor {
            length: 64,
            status: E1000_RXD_STAT_DD as u8,
            ..EmLegacyRxDescriptor::default()
        };
        descriptors[3] = EmLegacyRxDescriptor {
            length: 20,
            status: (E1000_RXD_STAT_DD
                | E1000_RXD_STAT_EOP
                | E1000_RXD_STAT_VP
                | E1000_RXD_STAT_IPCS
                | E1000_RXD_STAT_TCPCS) as u8,
            special: 321,
            ..EmLegacyRxDescriptor::default()
        };
        let mut stats = EmRxStats::default();
        let packet = lem_isc_rxd_pkt_get(
            &mut descriptors,
            2,
            EmRxCapabilities {
                checksum: true,
                vlan_tagging: true,
            },
            &mut stats,
        )
        .unwrap();
        assert_eq!(packet.length, 84);
        assert_eq!(packet.fragments.len(), 2);
        assert_eq!(packet.vlan_tag, Some(321));
        assert!(packet.checksum.ip_valid && packet.checksum.data_valid);
        assert_eq!((stats.bytes, stats.packets), (84, 1));
        assert_eq!((descriptors[2].status, descriptors[3].status), (0, 0));

        let mut advanced = alloc::vec![EmAdvancedRxDescriptor::default(); 2];
        advanced[1] = EmAdvancedRxDescriptor {
            status_error: E1000_RXD_STAT_DD
                | E1000_RXD_STAT_EOP
                | E1000_RXD_STAT_VP
                | E1000_RXD_STAT_IPCS,
            length: 88,
            vlan: 77,
            packet_info: 1,
            rss_hash: 0x1234,
            ..EmAdvancedRxDescriptor::default()
        };
        let packet = em_isc_rxd_pkt_get(
            &mut advanced,
            1,
            EmRxCapabilities {
                checksum: true,
                vlan_tagging: true,
            },
            &mut stats,
        )
        .unwrap();
        assert_eq!(packet.vlan_tag, Some(77));
        assert_eq!(packet.flow_id, Some(0x1234));
        assert_eq!(packet.rss_type, EmRssType::TcpIpv4);
        assert_eq!(advanced[1].status_error & 0xff, 0);

        let mut bad = alloc::vec![EmLegacyRxDescriptor::default(); 1];
        bad[0] = EmLegacyRxDescriptor {
            status: E1000_RXD_STAT_DD as u8,
            errors: 1,
            ..EmLegacyRxDescriptor::default()
        };
        assert!(lem_isc_rxd_pkt_get(&mut bad, 0, EmRxCapabilities::default(), &mut stats).is_err());
        assert_eq!(stats.dropped, 1);
        assert_ne!(bad[0].status, 0);
    }
}
