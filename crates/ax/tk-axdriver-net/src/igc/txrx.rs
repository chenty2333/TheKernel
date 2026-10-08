//! FreeBSD IGC advanced-descriptor transmit and receive callbacks.
//!
//! Translated from FreeBSD `sys/dev/igc/igc_txrx.c`, commit
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2016 Matthew Macy <mmacy@mattmacy.io>.
//! Copyright (c) 2021 Rubicon Communications, LLC (Netgate).

use alloc::vec::Vec;

const ADV_DTYPE_CTXT: u32 = 0x0020_0000;
const ADV_DTYPE_DATA: u32 = 0x0030_0000;
const ADV_DCMD_EOP: u32 = 0x0100_0000;
const ADV_DCMD_IFCS: u32 = 0x0200_0000;
const ADV_DCMD_RS: u32 = 0x0800_0000;
const ADV_DCMD_DEXT: u32 = 0x2000_0000;
const ADV_DCMD_VLE: u32 = 0x4000_0000;
const ADV_DCMD_TSE: u32 = 0x8000_0000;
const ADV_TUCMD_IPV4: u32 = 0x400;
const ADV_TUCMD_IPV6: u32 = 0;
const ADV_TUCMD_TCP: u32 = 0x800;
const ADV_TUCMD_UDP: u32 = 0;
const ADV_TUCMD_SCTP: u32 = 0x1000;
const ADVTXD_MACLEN_SHIFT: u32 = 9;
const ADVTXD_VLAN_SHIFT: u32 = 16;
const ADVTXD_L4LEN_SHIFT: u32 = 8;
const ADVTXD_MSS_SHIFT: u32 = 16;
const ADVTXD_PAYLEN_SHIFT: u32 = 14;
const TXD_POPTS_IXSM: u32 = 0x01;
const TXD_POPTS_TXSM: u32 = 0x02;
const TXD_STAT_DD: u32 = 1;
const RXD_STAT_DD: u32 = 1;
const RXD_STAT_EOP: u32 = 2;
const RXD_STAT_IXSM: u32 = 4;
const RXD_STAT_VP: u32 = 8;
const RXD_STAT_UDPCS: u32 = 0x10;
const RXD_STAT_TCPCS: u32 = 0x20;
const RXD_STAT_IPCS: u32 = 0x40;
const RXD_ERR_TCPE: u8 = 0x20;
const RXD_ERR_IPE: u8 = 0x40;
const RXDEXT_STATERR_RXE: u32 = 0x8000_0000;
const PKTTYPE_MASK: u32 = 0x0000_fff0;
const RXDADV_PKTTYPE_SCTP: u32 = 0x400;
const RXDADV_PKTTYPE_ETQF: u32 = 0x8000;
const RSSTYPE_MASK: u16 = 0xf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TxIpType {
    Ipv4,
    Ipv6,
    Other(u16),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TxProtocol {
    Tcp,
    Udp,
    Sctp,
    Other(u8),
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TxChecksum {
    pub ipv4: bool,
    pub ipv6: bool,
    pub tcp: bool,
    pub udp: bool,
    pub sctp: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TxSegment {
    pub address: u64,
    pub length: u32,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TxPacketInfo {
    pub pidx: usize,
    pub segments: Vec<TxSegment>,
    pub len: u32,
    pub ehdrlen: u16,
    pub ip_hlen: u16,
    pub tcp_hlen: u16,
    pub tso_segsz: u16,
    pub tso: bool,
    pub vlan_tag: Option<u16>,
    pub ip_type: TxIpType,
    pub protocol: TxProtocol,
    pub checksum: TxChecksum,
    pub tx_interrupt: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TxRxError {
    Bounds,
    Unsupported,
    NoDescriptor,
    BadMessage,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TxRingState {
    pub desc: Vec<[u32; 4]>,
    pub rs_queue: Vec<usize>,
    pub rs_pidx: usize,
    pub rs_cidx: usize,
    pub cidx_processed: usize,
    pub bytes: u64,
    pub packets: u64,
    pub queue_index: u16,
}
impl TxRingState {
    pub fn new(count: usize) -> Self {
        Self {
            desc: alloc::vec![[0;4];count],
            rs_queue: alloc::vec![0;count],
            rs_pidx: 0,
            rs_cidx: 0,
            cidx_processed: 0,
            bytes: 0,
            packets: 0,
            queue_index: 0,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RxRingState {
    pub desc: Vec<[u32; 4]>,
    pub queue_index: u16,
    pub bytes: u64,
    pub packets: u64,
    pub discarded: u64,
}
impl RxRingState {
    pub fn new(count: usize) -> Self {
        Self {
            desc: alloc::vec![[0;4];count],
            queue_index: 0,
            bytes: 0,
            packets: 0,
            discarded: 0,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RxFragment {
    pub queue_id: u16,
    pub index: usize,
    pub length: u16,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RxHashType {
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RxMetadata {
    pub len: u32,
    pub checksum_flags: u32,
    pub checksum_data: u16,
    pub vlan_tag: u16,
    pub has_vlan: bool,
    pub flow_id: u32,
    pub rss_type: RxHashType,
    pub fragment_count: u16,
}
pub trait TxRxIo {
    fn write_tdt(&mut self, queue: u16, index: usize);
    fn write_rdt(&mut self, queue: u16, index: usize);
    fn aim_publish_tx(&mut self, queue: u16);
    fn aim_publish_rx(&mut self, queue: u16);
    fn note_drop(&mut self);
}

// upstream: igc_txrx.c igc_tso_setup()
pub fn igc_tso_setup(
    tx: &mut TxRingState,
    pi: &TxPacketInfo,
    cmd: &mut u32,
    olinfo: &mut u32,
) -> Result<(), TxRxError> {
    let mut tucmd = 0;
    let mut vlan_lens = 0;
    let mut mss_l4 = 0;
    match pi.ip_type {
        TxIpType::Ipv6 => tucmd |= ADV_TUCMD_IPV6,
        TxIpType::Ipv4 => {
            tucmd |= ADV_TUCMD_IPV4;
            *olinfo |= TXD_POPTS_IXSM << 8
        }
        TxIpType::Other(_) => return Err(TxRxError::Unsupported),
    }
    let paylen = pi.len - u32::from(pi.ehdrlen) - u32::from(pi.ip_hlen) - u32::from(pi.tcp_hlen);
    if let Some(tag) = pi.vlan_tag {
        vlan_lens |= u32::from(tag) << ADVTXD_VLAN_SHIFT
    }
    vlan_lens |= u32::from(pi.ehdrlen) << ADVTXD_MACLEN_SHIFT;
    vlan_lens |= u32::from(pi.ip_hlen);
    tucmd |= ADV_DCMD_DEXT | ADV_DTYPE_CTXT | ADV_TUCMD_TCP;
    mss_l4 |= u32::from(pi.tso_segsz) << ADVTXD_MSS_SHIFT;
    mss_l4 |= u32::from(pi.tcp_hlen) << ADVTXD_L4LEN_SHIFT;
    let d = tx.desc.get_mut(pi.pidx).ok_or(TxRxError::Bounds)?;
    d[0] = vlan_lens;
    d[1] = 0;
    d[2] = tucmd;
    d[3] = mss_l4;
    *cmd |= ADV_DCMD_TSE;
    *olinfo |= TXD_POPTS_TXSM << 8;
    *olinfo |= paylen << ADVTXD_PAYLEN_SHIFT;
    Ok(())
}

// upstream: igc_txrx.c igc_tx_ctx_setup()
pub fn igc_tx_ctx_setup(
    tx: &mut TxRingState,
    pi: &TxPacketInfo,
    cmd: &mut u32,
    olinfo: &mut u32,
) -> Result<bool, TxRxError> {
    if pi.tso {
        return igc_tso_setup(tx, pi, cmd, olinfo).map(|_| true);
    }
    *olinfo |= pi.len << ADVTXD_PAYLEN_SHIFT;
    if pi.vlan_tag.is_none()
        && !pi.checksum.ipv4
        && !pi.checksum.ipv6
        && !pi.checksum.tcp
        && !pi.checksum.udp
        && !pi.checksum.sctp
    {
        return Ok(false);
    }
    let mut vlan_lens = 0;
    let mut tucmd = 0;
    let mss_l4 = 0;
    if let Some(tag) = pi.vlan_tag {
        vlan_lens |= u32::from(tag) << ADVTXD_VLAN_SHIFT
    }
    vlan_lens |= u32::from(pi.ehdrlen) << ADVTXD_MACLEN_SHIFT;
    match pi.ip_type {
        TxIpType::Ipv4 => tucmd |= ADV_TUCMD_IPV4,
        TxIpType::Ipv6 => tucmd |= ADV_TUCMD_IPV6,
        TxIpType::Other(_) => {}
    }
    vlan_lens |= u32::from(pi.ip_hlen);
    tucmd |= ADV_DCMD_DEXT | ADV_DTYPE_CTXT;
    match pi.protocol {
        TxProtocol::Tcp if pi.checksum.tcp => {
            tucmd |= ADV_TUCMD_TCP;
            *olinfo |= TXD_POPTS_TXSM << 8
        }
        TxProtocol::Udp if pi.checksum.udp => {
            tucmd |= ADV_TUCMD_UDP;
            *olinfo |= TXD_POPTS_TXSM << 8
        }
        TxProtocol::Sctp if pi.checksum.sctp => {
            tucmd |= ADV_TUCMD_SCTP;
            *olinfo |= TXD_POPTS_TXSM << 8
        }
        _ => {}
    }
    let d = tx.desc.get_mut(pi.pidx).ok_or(TxRxError::Bounds)?;
    d[0] = vlan_lens;
    d[1] = 0;
    d[2] = tucmd;
    d[3] = mss_l4;
    Ok(true)
}

// upstream: igc_txrx.c igc_isc_txd_encap()
pub fn igc_isc_txd_encap(
    tx: &mut TxRingState,
    pi: &TxPacketInfo,
    ring_count: usize,
) -> Result<usize, TxRxError> {
    if ring_count != tx.desc.len() || ring_count == 0 || pi.segments.is_empty() {
        return Err(TxRxError::Bounds);
    }
    let mut cmd = ADV_DTYPE_DATA | ADV_DCMD_IFCS | ADV_DCMD_DEXT;
    let mut olinfo = 0;
    if pi.vlan_tag.is_some() {
        cmd |= ADV_DCMD_VLE
    }
    let rs = pi.tx_interrupt;
    let mut i = pi.pidx;
    if igc_tx_ctx_setup(tx, pi, &mut cmd, &mut olinfo)? {
        i = (i + 1) % ring_count
    }
    let mut pidx_last = 0;
    for seg in &pi.segments {
        let d = tx.desc.get_mut(i).ok_or(TxRxError::Bounds)?;
        d[0] = seg.address as u32;
        d[1] = (seg.address >> 32) as u32;
        d[2] = ADV_DCMD_IFCS | cmd | seg.length;
        d[3] = olinfo;
        pidx_last = i;
        i = (i + 1) % ring_count;
    }
    if rs {
        tx.rs_queue[tx.rs_pidx] = pidx_last;
        tx.rs_pidx = (tx.rs_pidx + 1) & (ring_count - 1);
        if tx.rs_pidx == tx.rs_cidx {
            return Err(TxRxError::NoDescriptor);
        }
    }
    tx.desc[pidx_last][2] |= ADV_DCMD_EOP | if rs { ADV_DCMD_RS } else { 0 };
    if pi.tso {
        let hdr = u32::from(pi.ehdrlen) + u32::from(pi.ip_hlen) + u32::from(pi.tcp_hlen);
        if pi.len > hdr && pi.tso_segsz != 0 {
            let payload = pi.len - hdr;
            let segs = payload.div_ceil(u32::from(pi.tso_segsz));
            tx.bytes += u64::from(pi.len + (segs - 1) * hdr);
            tx.packets += u64::from(segs);
            return Ok(i);
        }
    }
    tx.bytes += u64::from(pi.len);
    tx.packets += 1;
    Ok(i)
}

// upstream: igc_txrx.c igc_isc_txd_flush()
pub fn igc_isc_txd_flush<I: TxRxIo>(io: &mut I, tx: &TxRingState, pidx: usize) {
    io.write_tdt(tx.queue_index, pidx);
    io.aim_publish_tx(tx.queue_index)
}

// upstream: igc_txrx.c igc_isc_txd_credits_update()
pub fn igc_isc_txd_credits_update(tx: &mut TxRingState, clear: bool) -> usize {
    let n = tx.desc.len();
    if tx.rs_cidx == tx.rs_pidx || n == 0 {
        return 0;
    }
    let mut cur = tx.rs_queue[tx.rs_cidx];
    if tx.desc[cur][3] & TXD_STAT_DD == 0 {
        return 0;
    }
    if !clear {
        return 1;
    }
    let mut processed = 0;
    let mut prev = tx.cidx_processed;
    let mut rs = tx.rs_cidx;
    loop {
        let delta = (cur + n - prev) % n;
        if delta == 0 {
            break;
        }
        processed += delta;
        prev = cur;
        rs = (rs + 1) & (n - 1);
        if rs == tx.rs_pidx {
            break;
        }
        cur = tx.rs_queue[rs];
        if tx.desc[cur][3] & TXD_STAT_DD == 0 {
            break;
        }
    }
    tx.rs_cidx = rs;
    tx.cidx_processed = prev;
    processed
}

// upstream: igc_txrx.c igc_isc_rxd_refill()
pub fn igc_isc_rxd_refill(
    rx: &mut RxRingState,
    start: usize,
    addresses: &[u64],
) -> Result<usize, TxRxError> {
    let n = rx.desc.len();
    if n == 0 || start >= n {
        return Err(TxRxError::Bounds);
    }
    let mut next = start;
    for addr in addresses {
        let d = rx.desc.get_mut(next).ok_or(TxRxError::Bounds)?;
        d[0] = *addr as u32;
        d[1] = (*addr >> 32) as u32;
        next = (next + 1) % n;
    }
    Ok(next)
}

// upstream: igc_txrx.c igc_isc_rxd_flush()
pub fn igc_isc_rxd_flush<I: TxRxIo>(io: &mut I, rx: &RxRingState, pidx: usize) {
    io.write_rdt(rx.queue_index, pidx);
    io.aim_publish_rx(rx.queue_index)
}

// upstream: igc_txrx.c igc_isc_rxd_available()
pub fn igc_isc_rxd_available(rx: &RxRingState, idx: usize, budget: usize) -> usize {
    let n = rx.desc.len();
    if n == 0 {
        return 0;
    }
    let (mut cnt, mut i) = (0, idx % n);
    while cnt < n && cnt <= budget {
        let stat = rx.desc[i][2];
        if stat & RXD_STAT_DD == 0 {
            break;
        }
        i = (i + 1) % n;
        if stat & RXD_STAT_EOP != 0 {
            cnt += 1
        }
    }
    cnt
}

// upstream: igc_txrx.c igc_rx_checksum()
pub fn igc_rx_checksum(staterr: u32, ptype: u32) -> (u32, u16) {
    let status = staterr & 0xffff;
    let errors = (staterr >> 24) as u8;
    if status & RXD_STAT_IXSM != 0 || errors & (RXD_ERR_IPE | RXD_ERR_TCPE) != 0 {
        return (0, 0);
    }
    let mut flags = 0;
    if status & RXD_STAT_IPCS != 0 {
        flags |= CSUM_IP_CHECKED | CSUM_IP_VALID
    }
    if status & (RXD_STAT_TCPCS | RXD_STAT_UDPCS) != 0 {
        if ptype & RXDADV_PKTTYPE_ETQF == 0 && ptype & RXDADV_PKTTYPE_SCTP != 0 {
            flags |= CSUM_SCTP_VALID
        } else {
            flags |= CSUM_DATA_VALID | CSUM_PSEUDO_HDR;
            return (flags, 0xffff);
        }
    }
    (flags, 0)
}
pub const CSUM_IP_CHECKED: u32 = 1;
pub const CSUM_IP_VALID: u32 = 2;
pub const CSUM_DATA_VALID: u32 = 4;
pub const CSUM_PSEUDO_HDR: u32 = 8;
pub const CSUM_SCTP_VALID: u32 = 16;
pub const CSUM_VLAN_TAG: u32 = 32;

// upstream: igc_txrx.c igc_isc_rxd_pkt_get()
pub fn igc_isc_rxd_pkt_get<I: TxRxIo>(
    io: &mut I,
    rx: &mut RxRingState,
    start: usize,
    checksum_enabled: bool,
) -> Result<(RxMetadata, Vec<RxFragment>), TxRxError> {
    let n = rx.desc.len();
    if n == 0 || start >= n {
        return Err(TxRxError::Bounds);
    }
    let (mut i, mut cidx, mut length) = (0usize, start, 0u32);
    let mut staterr;
    let mut pkt_info;
    let mut ptype;
    let mut frags = Vec::new();
    loop {
        let d = rx.desc.get_mut(cidx).ok_or(TxRxError::Bounds)?;
        staterr = d[2];
        pkt_info = d[0] as u16;
        let len = (d[3] & 0xffff) as u16;
        ptype = d[0] & PKTTYPE_MASK;
        length += u32::from(len);
        d[2] = 0;
        let eop = staterr & RXD_STAT_EOP != 0;
        if eop && staterr & RXDEXT_STATERR_RXE != 0 {
            io.note_drop();
            rx.discarded += 1;
            return Err(TxRxError::BadMessage);
        }
        frags.push(RxFragment {
            queue_id: 0,
            index: cidx,
            length: len,
        });
        cidx = (cidx + 1) % n;
        i += 1;
        if eop {
            break;
        }
    }
    rx.bytes += u64::from(length);
    rx.packets += 1;
    let (mut checksum_flags, mut checksum_data) = (0, 0);
    if checksum_enabled {
        (checksum_flags, checksum_data) = igc_rx_checksum(staterr, ptype)
    }
    let vlan = (staterr & RXD_STAT_VP) != 0;
    let tag = (rx.desc[(cidx + n - 1) % n][3] >> 16) as u16;
    if vlan {
        checksum_flags |= CSUM_VLAN_TAG
    }
    let meta = RxMetadata {
        len: length,
        checksum_flags,
        checksum_data,
        vlan_tag: tag,
        has_vlan: vlan,
        flow_id: rx.desc[(cidx + n - 1) % n][1],
        rss_type: igc_determine_rsstype(pkt_info),
        fragment_count: i as u16,
    };
    Ok((meta, frags))
}

// upstream: igc_txrx.c igc_determine_rsstype()
pub fn igc_determine_rsstype(pkt_info: u16) -> RxHashType {
    match pkt_info & RSSTYPE_MASK {
        1 => RxHashType::TcpIpv4,
        2 => RxHashType::Ipv4,
        3 => RxHashType::TcpIpv6,
        4 => RxHashType::Ipv6Ex,
        5 => RxHashType::Ipv6,
        6 => RxHashType::TcpIpv6Ex,
        7 => RxHashType::UdpIpv4,
        8 => RxHashType::UdpIpv6,
        9 => RxHashType::UdpIpv6Ex,
        _ => RxHashType::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        tdt: Vec<(u16, usize)>,
        rdt: Vec<(u16, usize)>,
        tx_publish: usize,
        rx_publish: usize,
        drops: usize,
    }
    impl TxRxIo for Fake {
        fn write_tdt(&mut self, q: u16, i: usize) {
            self.tdt.push((q, i))
        }
        fn write_rdt(&mut self, q: u16, i: usize) {
            self.rdt.push((q, i))
        }
        fn aim_publish_tx(&mut self, _: u16) {
            self.tx_publish += 1
        }
        fn aim_publish_rx(&mut self, _: u16) {
            self.rx_publish += 1
        }
        fn note_drop(&mut self) {
            self.drops += 1
        }
    }
    fn packet() -> TxPacketInfo {
        TxPacketInfo {
            pidx: 0,
            segments: alloc::vec![
                TxSegment {
                    address: 0x1234_5678_9abc,
                    length: 100
                },
                TxSegment {
                    address: 0xfeed_cafe,
                    length: 50
                }
            ],
            len: 150,
            ehdrlen: 14,
            ip_hlen: 20,
            tcp_hlen: 20,
            tso_segsz: 0,
            tso: false,
            vlan_tag: None,
            ip_type: TxIpType::Ipv4,
            protocol: TxProtocol::Tcp,
            checksum: TxChecksum::default(),
            tx_interrupt: true,
        }
    }
    #[test]
    fn tx_context_tso_ring_completion_and_tail_publication_match_igc_words() {
        let mut tx = TxRingState::new(8);
        let pi = packet();
        let next = igc_isc_txd_encap(&mut tx, &pi, 8).unwrap();
        assert_eq!(next, 2);
        assert_eq!(tx.desc[1][2] & ADV_DCMD_EOP, ADV_DCMD_EOP);
        assert_eq!(tx.desc[1][2] & ADV_DCMD_RS, ADV_DCMD_RS);
        assert_eq!(tx.rs_queue[0], 1);
        assert_eq!(tx.packets, 1);
        tx.desc[1][3] |= TXD_STAT_DD;
        assert_eq!(igc_isc_txd_credits_update(&mut tx, false), 1);
        assert_eq!(igc_isc_txd_credits_update(&mut tx, true), 1);
        let mut io = Fake::default();
        igc_isc_txd_flush(&mut io, &tx, next);
        assert_eq!(io.tdt, [(0, next)]);
        assert_eq!(io.tx_publish, 1);
        let mut t = TxRingState::new(8);
        let mut tso = packet();
        tso.tso = true;
        tso.tso_segsz = 64;
        let mut cmd = 0;
        let mut ol = 0;
        assert!(igc_tx_ctx_setup(&mut t, &tso, &mut cmd, &mut ol).unwrap());
        assert_ne!(cmd & ADV_DCMD_TSE, 0);
        assert_ne!(ol & (TXD_POPTS_TXSM << 8), 0);
        assert_eq!(
            t.desc[0][3],
            (64 << ADVTXD_MSS_SHIFT) | (20 << ADVTXD_L4LEN_SHIFT)
        );
    }
    #[test]
    fn rx_refill_availability_packet_metadata_and_checksum_errors() {
        let mut rx = RxRingState::new(4);
        assert_eq!(
            igc_isc_rxd_refill(&mut rx, 2, &[0x1234_0000_0000, 0x5678]).unwrap(),
            0
        );
        assert_eq!(rx.desc[2][0], 0);
        assert_eq!(rx.desc[2][1], 0x1234);
        assert_eq!(rx.desc[3][0], 0x5678);
        rx.desc[0][0] = 1;
        rx.desc[0][1] = 0x1122;
        rx.desc[0][2] = RXD_STAT_DD;
        rx.desc[0][3] = 100;
        rx.desc[1][0] = 1 | ((0x400u32) << 4);
        rx.desc[1][1] = 0x3344;
        rx.desc[1][2] = RXD_STAT_DD | RXD_STAT_EOP | RXD_STAT_IPCS | RXD_STAT_TCPCS | RXD_STAT_VP;
        rx.desc[1][3] = 50 | (0x321 << 16);
        assert_eq!(igc_isc_rxd_available(&rx, 0, 1), 1);
        let (mut io, mut rx) = (Fake::default(), rx);
        let (meta, frags) = igc_isc_rxd_pkt_get(&mut io, &mut rx, 0, true).unwrap();
        assert_eq!(meta.len, 150);
        assert_eq!(meta.vlan_tag, 0x321);
        assert!(meta.has_vlan);
        assert_eq!(meta.rss_type, RxHashType::TcpIpv4);
        assert_eq!(frags.len(), 2);
        assert_eq!(meta.flow_id, 0x3344);
        assert_eq!(meta.checksum_data, 0xffff);
        rx.desc[2][0] = 1;
        rx.desc[2][2] = RXD_STAT_DD | RXD_STAT_EOP | RXDEXT_STATERR_RXE;
        rx.desc[2][3] = 60;
        assert_eq!(
            igc_isc_rxd_pkt_get(&mut io, &mut rx, 2, true),
            Err(TxRxError::BadMessage)
        );
        assert_eq!(io.drops, 1);
        igc_isc_rxd_flush(&mut io, &rx, 3);
        assert_eq!(io.rdt, [(0, 3)]);
        assert_eq!(io.rx_publish, 1);
    }
    #[test]
    fn rss_types_and_checksum_skip_conditions_cover_special_packets() {
        assert_eq!(igc_determine_rsstype(0), RxHashType::None);
        assert_eq!(igc_determine_rsstype(8), RxHashType::UdpIpv6);
        assert_eq!(igc_rx_checksum(RXD_STAT_IXSM, 0), (0, 0));
        assert_eq!(
            igc_rx_checksum(RXD_STAT_IPCS | ((RXD_ERR_IPE as u32) << 24), 0),
            (0, 0)
        );
        assert_eq!(
            igc_rx_checksum(RXD_STAT_TCPCS, RXDADV_PKTTYPE_SCTP),
            (CSUM_SCTP_VALID, 0)
        );
    }
}
