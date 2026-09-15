//! Bounded IPv4/IPv6 fragment reassembly for the namespace packet pipeline.
//!
//! Reassembly belongs below the OS packet-policy callback: a hook which asks
//! for IP defragmentation must never receive a partial L4 header.  The router
//! owns one instance per network namespace, so fragment keys cannot cross a
//! namespace boundary and teardown drops all retained packet bytes at once.

use alloc::vec::Vec;

use axerrno::{AxError, AxResult};

/// IPv4's Total Length is the complete packet while IPv6's Payload Length
/// excludes its fixed 40-byte header. Jumbograms are deliberately outside
/// this bounded ordinary-fragment implementation.
const MAX_IPV4_DATAGRAM: usize = u16::MAX as usize;
const MAX_IPV6_DATAGRAM: usize = 40 + u16::MAX as usize;
const MAX_QUEUES: usize = 64;
const MAX_PIECES_PER_QUEUE: usize = 1024;
// 64 fully reassembled IPv4-sized queues plus their bounded header/piece
// metadata remain comfortably below this hard ceiling.
const MAX_QUEUED_BYTES: usize = 8 * 1024 * 1024;
/// A router receive pass advances this clock once.  The bounded lifetime
/// prevents an incomplete remote datagram from retaining a queue forever.
const QUEUE_LIFETIME: u64 = 512;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Key {
    V4 {
        source: [u8; 4],
        destination: [u8; 4],
        id: u16,
        protocol: u8,
    },
    V6 {
        source: [u8; 16],
        destination: [u8; 16],
        id: u32,
        next_header: u8,
    },
}

struct Piece {
    offset: usize,
    bytes: Vec<u8>,
}

struct Queue {
    key: Key,
    /// The IP header through (but excluding) the IPv6 fragment header.  For
    /// IPv4 this is the normal variable-length IPv4 header.
    /// Fragment zero alone authorizes the unfragmentable/header bytes.
    prefix: Option<Vec<u8>>,
    last_seen: u64,
    total_payload: Option<usize>,
    pieces: Vec<Piece>,
}

/// The result of admitting one network-layer packet.
pub(crate) enum ReassemblyOutcome {
    /// Not fragmented; the caller continues with its original packet.
    Pass,
    /// A fragment was retained but the datagram is not complete yet.
    Pending,
    /// A complete datagram with all fragment headers removed.
    Complete(Vec<u8>),
}

/// Per-router, allocation-bounded reassembly provider.
pub(crate) struct FragmentReassembler {
    queues: Vec<Queue>,
    queued_bytes: usize,
    clock: u64,
}

impl FragmentReassembler {
    pub(crate) fn try_new() -> AxResult<Self> {
        let mut queues = Vec::new();
        queues
            .try_reserve_exact(MAX_QUEUES)
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            queues,
            queued_bytes: 0,
            clock: 0,
        })
    }

    pub(crate) fn ingest(&mut self, packet: &[u8]) -> AxResult<ReassemblyOutcome> {
        self.clock = self.clock.wrapping_add(1);
        self.expire();
        match packet.first().copied().map(|byte| byte >> 4) {
            Some(4) => self.ipv4(packet),
            Some(6) => self.ipv6(packet),
            _ => Ok(ReassemblyOutcome::Pass),
        }
    }

    fn expire(&mut self) {
        let now = self.clock;
        let mut index = 0;
        while index < self.queues.len() {
            if now.wrapping_sub(self.queues[index].last_seen) > QUEUE_LIFETIME {
                self.queued_bytes = self
                    .queued_bytes
                    .saturating_sub(queue_bytes(&self.queues[index]));
                self.queues.swap_remove(index);
            } else {
                index += 1;
            }
        }
    }

    fn ipv4(&mut self, packet: &[u8]) -> AxResult<ReassemblyOutcome> {
        if packet.len() < 20 {
            return Ok(ReassemblyOutcome::Pass);
        }
        let header_len = usize::from(packet[0] & 0x0f) * 4;
        let total_len = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
        if header_len < 20 || total_len < header_len || total_len > packet.len() {
            return Ok(ReassemblyOutcome::Pass);
        }
        let flags_offset = u16::from_be_bytes([packet[6], packet[7]]);
        let offset = usize::from(flags_offset & 0x1fff) * 8;
        let more = flags_offset & 0x2000 != 0;
        if offset == 0 && !more {
            return Ok(ReassemblyOutcome::Pass);
        }
        let payload = &packet[header_len..total_len];
        if more && payload.len() % 8 != 0 {
            return Ok(ReassemblyOutcome::Pending);
        }
        offset
            .checked_add(payload.len())
            .filter(|end| *end <= MAX_IPV4_DATAGRAM - header_len)
            .ok_or(AxError::InvalidInput)?;
        let mut source = [0; 4];
        source.copy_from_slice(&packet[12..16]);
        let mut destination = [0; 4];
        destination.copy_from_slice(&packet[16..20]);
        let key = Key::V4 {
            source,
            destination,
            id: u16::from_be_bytes([packet[4], packet[5]]),
            protocol: packet[9],
        };
        let outcome = self.insert(key, &packet[..header_len], offset, more, payload)?;
        Ok(match outcome {
            Some(mut full) => {
                // IPv4's total length and checksum are header-owned and must
                // describe the reassembled packet, not fragment zero.
                let length = full.len() as u16;
                full[2..4].copy_from_slice(&length.to_be_bytes());
                full[6] = 0;
                full[7] = 0;
                full[10] = 0;
                full[11] = 0;
                let checksum = ipv4_checksum(&full[..header_len]);
                full[10..12].copy_from_slice(&checksum.to_be_bytes());
                ReassemblyOutcome::Complete(full)
            }
            None => ReassemblyOutcome::Pending,
        })
    }

    fn ipv6(&mut self, packet: &[u8]) -> AxResult<ReassemblyOutcome> {
        if packet.len() < 40 {
            return Ok(ReassemblyOutcome::Pass);
        }
        let declared = usize::from(u16::from_be_bytes([packet[4], packet[5]]));
        let end = 40usize
            .checked_add(declared)
            .filter(|end| *end <= packet.len())
            .ok_or(AxError::InvalidInput)?;
        let mut next = packet[6];
        let mut cursor = 40;
        let mut next_field = 6usize;
        while cursor < end {
            if next == 44 {
                if cursor
                    .checked_add(8)
                    .filter(|limit| *limit <= end)
                    .is_none()
                {
                    return Ok(ReassemblyOutcome::Pass);
                }
                let fragment_next = packet[cursor];
                let bits = u16::from_be_bytes([packet[cursor + 2], packet[cursor + 3]]);
                let offset = usize::from(bits & 0xfff8);
                let more = bits & 1 != 0;
                let payload = &packet[cursor + 8..end];
                if more && payload.len() % 8 != 0 {
                    return Ok(ReassemblyOutcome::Pending);
                }
                offset
                    .checked_add(payload.len())
                    .filter(|end| *end <= MAX_IPV6_DATAGRAM - cursor)
                    .ok_or(AxError::InvalidInput)?;
                let mut source = [0; 16];
                source.copy_from_slice(&packet[8..24]);
                let mut destination = [0; 16];
                destination.copy_from_slice(&packet[24..40]);
                let key = Key::V6 {
                    source,
                    destination,
                    id: u32::from_be_bytes(packet[cursor + 4..cursor + 8].try_into().unwrap()),
                    next_header: fragment_next,
                };
                let outcome = self.insert(key, &packet[..cursor], offset, more, payload)?;
                return Ok(match outcome {
                    Some(mut full) => {
                        // The reassembled packet omits the eight-byte Fragment
                        // header and its predecessor now names the real L4/extension header.
                        full[next_field] = fragment_next;
                        let payload_len =
                            u16::try_from(full.len() - 40).map_err(|_| AxError::InvalidInput)?;
                        full[4..6].copy_from_slice(&payload_len.to_be_bytes());
                        ReassemblyOutcome::Complete(full)
                    }
                    None => ReassemblyOutcome::Pending,
                });
            }
            // RFC 8200 extension headers which carry an ordinary next-header
            // byte.  ESP is intentionally opaque and cannot be walked.
            let length = match next {
                0 | 43 | 60 => packet
                    .get(cursor + 1)
                    .map(|value| (usize::from(*value) + 1) * 8),
                51 => packet
                    .get(cursor + 1)
                    .map(|value| (usize::from(*value) + 2) * 4),
                _ => None,
            };
            let Some(length) = length else {
                break;
            };
            if cursor
                .checked_add(length)
                .filter(|limit| *limit <= end)
                .is_none()
            {
                return Ok(ReassemblyOutcome::Pass);
            }
            next_field = cursor;
            next = packet[cursor];
            cursor += length;
        }
        Ok(ReassemblyOutcome::Pass)
    }

    fn insert(
        &mut self,
        key: Key,
        prefix: &[u8],
        offset: usize,
        more: bool,
        payload: &[u8],
    ) -> AxResult<Option<Vec<u8>>> {
        let index = match self.queues.iter().position(|queue| queue.key == key) {
            Some(index) => index,
            None => {
                self.make_room(payload.len().saturating_add(core::mem::size_of::<Piece>()))?;
                let mut pieces = Vec::new();
                pieces.try_reserve(4).map_err(|_| AxError::NoMemory)?;
                self.queues.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                self.queues.push(Queue {
                    key,
                    prefix: None,
                    last_seen: self.clock,
                    total_payload: None,
                    pieces,
                });
                self.queues.len() - 1
            }
        };
        let mut poison = false;
        let complete = {
            let queue = &mut self.queues[index];
            queue.last_seen = self.clock;
            // Fragment zero is authoritative. A contradictory header or any
            // overlap poisons the complete queue (RFC 5722 behaviour), not
            // merely the arriving piece.
            if offset == 0 {
                match &queue.prefix {
                    Some(existing) if existing != prefix => poison = true,
                    Some(_) => {}
                    None => {
                        let mut stored = Vec::new();
                        stored
                            .try_reserve_exact(prefix.len())
                            .map_err(|_| AxError::NoMemory)?;
                        stored.extend_from_slice(prefix);
                        queue.prefix = Some(stored);
                    }
                }
            }
            if poison {
                None
            } else {
                if !more {
                    let total = offset
                        .checked_add(payload.len())
                        .ok_or(AxError::InvalidInput)?;
                    if let Some(existing) = queue.total_payload
                        && existing != total
                    {
                        poison = true;
                    }
                    queue.total_payload = Some(total);
                }
                if !poison {
                    match insert_non_overlapping(&mut queue.pieces, offset, payload) {
                        Ok(true) => {}
                        // An overlap or piece-cap exhaustion must not leave a
                        // partially trusted queue available for later splice.
                        Ok(false) | Err(AxError::ResourceBusy) => poison = true,
                        Err(error) => return Err(error),
                    }
                }
                match queue.total_payload {
                    Some(total)
                        if !poison && queue.prefix.is_some() && complete(&queue.pieces, total) =>
                    {
                        let retained = queue_bytes(queue);
                        let mut full = Vec::new();
                        full.try_reserve_exact(
                            queue
                                .prefix
                                .as_ref()
                                .unwrap()
                                .len()
                                .checked_add(total)
                                .ok_or(AxError::InvalidInput)?,
                        )
                        .map_err(|_| AxError::NoMemory)?;
                        full.extend_from_slice(queue.prefix.as_ref().unwrap());
                        for piece in &queue.pieces {
                            full.extend_from_slice(&piece.bytes);
                        }
                        Some((full, retained))
                    }
                    _ => None,
                }
            }
        };
        self.queued_bytes = self.queues.iter().map(queue_bytes).sum();
        if poison {
            self.queued_bytes = self
                .queued_bytes
                .saturating_sub(queue_bytes(&self.queues[index]));
            self.queues.swap_remove(index);
            return Ok(None);
        }
        let Some(complete) = complete else {
            return Ok(None);
        };
        self.queued_bytes = self.queued_bytes.saturating_sub(complete.1);
        self.queues.swap_remove(index);
        Ok(Some(complete.0))
    }

    fn make_room(&mut self, incoming: usize) -> AxResult<()> {
        if incoming > MAX_IPV6_DATAGRAM {
            return Err(AxError::InvalidInput);
        }
        while self.queues.len() >= MAX_QUEUES
            || self.queued_bytes.saturating_add(incoming) > MAX_QUEUED_BYTES
        {
            let Some((index, _)) = self
                .queues
                .iter()
                .enumerate()
                .min_by_key(|(_, queue)| queue.last_seen)
            else {
                break;
            };
            self.queued_bytes = self
                .queued_bytes
                .saturating_sub(queue_bytes(&self.queues[index]));
            self.queues.swap_remove(index);
        }
        Ok(())
    }
}

/// Returns false for any overlap. Keeping first-arrival bytes is observable
/// and lets family-dependent overlap policies evade filters.
fn insert_non_overlapping(pieces: &mut Vec<Piece>, offset: usize, bytes: &[u8]) -> AxResult<bool> {
    let end = offset
        .checked_add(bytes.len())
        .ok_or(AxError::InvalidInput)?;
    for piece in pieces.iter() {
        let piece_end = piece.offset + piece.bytes.len();
        if piece_end <= offset {
            continue;
        }
        if piece.offset >= end {
            break;
        }
        return Ok(false);
    }
    if pieces.len() >= MAX_PIECES_PER_QUEUE {
        return Err(AxError::ResourceBusy);
    }
    push_piece(pieces, offset, bytes)?;
    pieces.sort_by_key(|piece| piece.offset);
    Ok(true)
}

fn push_piece(pieces: &mut Vec<Piece>, offset: usize, bytes: &[u8]) -> AxResult<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let mut stored = Vec::new();
    stored
        .try_reserve_exact(bytes.len())
        .map_err(|_| AxError::NoMemory)?;
    stored.extend_from_slice(bytes);
    pieces.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    pieces.push(Piece {
        offset,
        bytes: stored,
    });
    Ok(())
}

fn complete(pieces: &[Piece], total: usize) -> bool {
    let mut cursor = 0;
    for piece in pieces {
        if piece.offset != cursor {
            return false;
        }
        cursor = match cursor.checked_add(piece.bytes.len()) {
            Some(value) => value,
            None => return false,
        };
    }
    cursor == total
}

fn queue_bytes(queue: &Queue) -> usize {
    queue.prefix.as_ref().map_or(0, |prefix| prefix.len())
        + queue
            .pieces
            .iter()
            .map(|piece| {
                piece
                    .bytes
                    .len()
                    .saturating_add(core::mem::size_of::<Piece>())
            })
            .sum::<usize>()
}

fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum = 0u32;
    for chunk in header.chunks_exact(2) {
        sum = sum.wrapping_add(u32::from(u16::from_be_bytes([chunk[0], chunk[1]])));
    }
    if let Some(&byte) = header.chunks_exact(2).remainder().first() {
        sum = sum.wrapping_add(u32::from(byte) << 8);
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}
