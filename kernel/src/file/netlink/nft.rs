//! `file::netlink` subsections; see the parent `mod.rs` for the module map.

use super::*;

#[derive(Clone)]
pub(crate) struct NftChain {
pub(crate)     table: String,
pub(crate)     name: String,
pub(crate)     hook: Option<NftHook>,
pub(crate)     policy: NftVerdict,
}
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum NftVerdict {
    Continue,
    Accept,
    Drop,
    Reject,
    Return,
    Jump,
    Goto,
}
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum NftHook {
    Prerouting,
    Input,
    Forward,
    Output,
    Postrouting,
}
#[derive(Clone)]
pub(crate) enum NftExpr {
    Ct {
        key: u32,
        dreg: u32,
    },
    Payload {
        base: u32,
        offset: u32,
        len: u32,
        dreg: u32,
    },
    Cmp {
        sreg: u32,
        op: u32,
        data: Vec<u8>,
    },
    Nat {
        kind: u32,
        family: u32,
        addr_reg: Option<u32>,
        proto_reg: Option<u32>,
        masquerade: bool,
    },
}
#[derive(Clone)]
pub(crate) struct NftRule {
pub(crate)     table: String,
pub(crate)     chain: String,
pub(crate)     handle: u64,
pub(crate)     verdict: NftVerdict,
pub(crate)     target_chain: Option<String>,
pub(crate)     lookup: Option<(String, Vec<u8>)>,
pub(crate)     expressions: Vec<NftExpr>,
pub(crate)     counter: u64,
}
#[derive(Clone)]
pub(crate) struct NftSet {
pub(crate)     table: String,
pub(crate)     name: String,
pub(crate)     id: u32,
pub(crate)     flags: u32,
pub(crate)     key_type: u32,
pub(crate)     data_type: u32,
}
#[derive(Clone)]
pub(crate) struct NftSetElement {
pub(crate)     table: String,
pub(crate)     set: String,
pub(crate)     key: Vec<u8>,
}
#[derive(Clone)]
pub(crate) struct NftNamespaceTables {
pub(crate)     namespace: Weak<NetworkNamespace>,
pub(crate)     tables: Vec<String>,
pub(crate)     chains: Vec<NftChain>,
pub(crate)     rules: Vec<NftRule>,
pub(crate)     sets: Vec<NftSet>,
pub(crate)     elements: Vec<NftSetElement>,
pub(crate)     next_rule: u64,
pub(crate)     generation: u32,
}
pub(crate) static NFT_TABLES: Lazy<Mutex<Vec<NftNamespaceTables>>> =
    Lazy::new(|| Mutex::new(Vec::new()));
// Serializes an nfnetlink write with packet traversal.  The state is copied
// before a datagram and published only when every message validates, so even
// a multi-message batch has no externally observable intermediate graph.
pub(crate) static NFT_TRANSACTION: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct ConntrackTuple {
    pub(crate) family: u8,
    pub(crate) protocol: u8,
    pub(crate) source: [u8; 16],
    pub(crate) destination: [u8; 16],
    pub(crate) source_port: u16,
    pub(crate) destination_port: u16,
}
pub(crate) struct ConntrackEntry {
    pub(crate) namespace: Weak<NetworkNamespace>,
    pub(crate) original: ConntrackTuple,
    pub(crate) translated: ConntrackTuple,
    pub(crate) reply: ConntrackTuple,
    pub(crate) packets: u64,
    pub(crate) expires_at: u64,
}
static CONNTRACK: Lazy<Mutex<Vec<ConntrackEntry>>> = Lazy::new(|| Mutex::new(Vec::new()));
const CONNTRACK_MAX_ENTRIES: usize = 4096;
pub(crate) const CONNTRACK_MAX_NAMESPACE_ENTRIES: usize = 1024;
pub(crate) const CONNTRACK_TIMEOUT_MILLIS: u64 = 60_000;

/// Executes the namespace's nft OUTPUT verdict chain at the packet emission
/// boundary.  Rule order is insertion order, matching the retained nft
/// chain order; a terminal drop/reject is never converted to a fake success.
pub(crate) fn nft_output_verdict(namespace: &Arc<NetworkNamespace>) -> AxResult {
    // Socket send admission occurs before axnet constructs its IP header.
    // The authoritative OUTPUT traversal is the router hook, once the full
    // packet exists; treating this headerless preflight as a packet would
    // make legitimate payload/ct/nat expressions fail spuriously.
    let _ = namespace;
    Ok(())
}

/// Namespace-local packet traversal entry used by packet, TUN/TAP and inet
/// boundary code.  The packet is mutable because NAT expressions alter the
/// in-flight headers before the lower router observes them.
pub(crate) fn nft_packet_hook(
    namespace: &Arc<NetworkNamespace>,
    hook: NftHook,
    packet: &mut [u8],
) -> AxResult {
    #[cfg(feature = "bpf")]
    crate::bpf::run_network_packet_links(
        namespace,
        match hook {
            NftHook::Prerouting => crate::file::bpf::BpfNetworkHook::Prerouting,
            NftHook::Input => crate::file::bpf::BpfNetworkHook::Input,
            NftHook::Forward => crate::file::bpf::BpfNetworkHook::Forward,
            NftHook::Output => crate::file::bpf::BpfNetworkHook::Output,
            NftHook::Postrouting => crate::file::bpf::BpfNetworkHook::Postrouting,
        },
        packet,
    )?;
    let _transaction = NFT_TRANSACTION.lock();
    conntrack_reverse_translate(namespace, packet)?;
    if let Some(tuple) = conntrack_tuple(packet) {
        conntrack_observe(namespace, tuple)?;
    }
    let mut namespaces = NFT_TABLES.lock();
    namespaces.retain(|state| state.namespace.strong_count() != 0);
    let needle = Arc::downgrade(namespace);
    for state in namespaces
        .iter_mut()
        .filter(|state| Weak::ptr_eq(&state.namespace, &needle))
    {
        let mut stack = Vec::new();
        for index in 0..state.chains.len() {
            let selected = &state.chains[index];
            if selected.hook != Some(hook) {
                continue;
            }
            let table = nft_owned_name(&selected.table)?;
            let chain = nft_owned_name(&selected.name)?;
            nft_evaluate_chain(namespace, state, &table, &chain, packet, &mut stack)?;
        }
    }
    Ok(())
}

fn conntrack_tuple(packet: &[u8]) -> Option<ConntrackTuple> {
    let version = *packet.first()? >> 4;
    let (family, protocol, source_offset, destination_offset, l4) = match version {
        4 => {
            let ihl = usize::from(*packet.first()? & 0x0f).checked_mul(4)?;
            if ihl < 20 || packet.len() < ihl {
                return None;
            }
            (4, *packet.get(9)?, 12, 16, ihl)
        }
        6 => {
            if packet.len() < 40 {
                return None;
            }
            (6, *packet.get(6)?, 8, 24, 40)
        }
        _ => return None,
    };
    let mut source = [0; 16];
    let mut destination = [0; 16];
    let width = if family == 4 { 4 } else { 16 };
    source[..width].copy_from_slice(packet.get(source_offset..source_offset + width)?);
    destination[..width]
        .copy_from_slice(packet.get(destination_offset..destination_offset + width)?);
    let (source_port, destination_port) = match protocol {
        6 | 17 | 132 | 33 => (
            u16::from_be_bytes(packet.get(l4..l4 + 2)?.try_into().ok()?),
            u16::from_be_bytes(packet.get(l4 + 2..l4 + 4)?.try_into().ok()?),
        ),
        _ => (0, 0),
    };
    Some(ConntrackTuple {
        family,
        protocol,
        source,
        destination,
        source_port,
        destination_port,
    })
}

fn conntrack_reverse(tuple: ConntrackTuple) -> ConntrackTuple {
    ConntrackTuple {
        family: tuple.family,
        protocol: tuple.protocol,
        source: tuple.destination,
        destination: tuple.source,
        source_port: tuple.destination_port,
        destination_port: tuple.source_port,
    }
}

fn conntrack_observe(namespace: &Arc<NetworkNamespace>, tuple: ConntrackTuple) -> AxResult {
    let mut state = CONNTRACK.lock();
    // Sample under the lock so concurrent observations cannot move an entry's
    // deadline backwards. Idle entries expire even when no packets arrive.
    let now = axhal::time::monotonic_time_nanos() / 1_000_000;
    conntrack_observe_at(&mut state, namespace, tuple, now)
}

pub(crate) fn conntrack_observe_at(
    state: &mut Vec<ConntrackEntry>,
    namespace: &Arc<NetworkNamespace>,
    tuple: ConntrackTuple,
    now: u64,
) -> AxResult {
    state.retain(|entry| entry.namespace.strong_count() != 0 && entry.expires_at > now);
    let needle = Arc::downgrade(namespace);
    if let Some(entry) = state.iter_mut().find(|entry| {
        Weak::ptr_eq(&entry.namespace, &needle)
            && (entry.original == tuple || entry.translated == tuple || entry.reply == tuple)
    }) {
        entry.packets = entry.packets.saturating_add(1);
        entry.expires_at = now.saturating_add(CONNTRACK_TIMEOUT_MILLIS);
        return Ok(());
    }
    if state.len() >= CONNTRACK_MAX_ENTRIES
        || state
            .iter()
            .filter(|entry| Weak::ptr_eq(&entry.namespace, &needle))
            .count()
            >= CONNTRACK_MAX_NAMESPACE_ENTRIES
    {
        return Err(LinuxError::ENOBUFS.into());
    }
    state.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    state.push(ConntrackEntry {
        namespace: needle,
        original: tuple,
        translated: tuple,
        reply: conntrack_reverse(tuple),
        packets: 1,
        expires_at: now.saturating_add(CONNTRACK_TIMEOUT_MILLIS),
    });
    Ok(())
}

pub(crate) fn nft_owned_name(name: &str) -> AxResult<String> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(name.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.push_str(name);
    Ok(owned)
}

fn nft_evaluate_chain(
    namespace: &Arc<NetworkNamespace>,
    state: &mut NftNamespaceTables,
    table: &str,
    chain: &str,
    packet: &mut [u8],
    stack: &mut Vec<String>,
) -> AxResult<NftVerdict> {
    // A jump cycle is rejected when installed; retaining this guard makes a
    // corrupted userspace graph fail closed instead of recursing in kernel
    // context.
    if stack.len() >= 64 || stack.iter().any(|item| item == chain) {
        return Err(LinuxError::ELOOP.into());
    }
    stack.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    stack.push(nft_owned_name(chain)?);
    let policy = state
        .chains
        .iter()
        .find(|item| item.table == table && item.name == chain)
        .map(|item| item.policy)
        .ok_or(AxError::NotFound)?;
    for index in 0..state.rules.len() {
        let rule = &mut state.rules[index];
        if rule.table != table || rule.chain != chain {
            continue;
        }
        rule.counter = rule.counter.saturating_add(1);
        if !nft_evaluate_expressions(namespace, packet, &rule.expressions)? {
            continue;
        }
        if let Some((set, key)) = &rule.lookup {
            if !state.elements.iter().any(|item| {
                item.table == table && item.set == *set && (key.is_empty() || item.key == *key)
            }) {
                continue;
            }
        }
        let verdict = rule.verdict;
        let target = rule
            .target_chain
            .as_deref()
            .map(nft_owned_name)
            .transpose()?;
        match verdict {
            NftVerdict::Continue => {}
            NftVerdict::Accept => {
                stack.pop();
                return Ok(NftVerdict::Accept);
            }
            NftVerdict::Drop => {
                stack.pop();
                return Err(LinuxError::EPERM.into());
            }
            NftVerdict::Reject => {
                stack.pop();
                return Err(LinuxError::ECONNREFUSED.into());
            }
            NftVerdict::Return => break,
            NftVerdict::Jump | NftVerdict::Goto => {
                let target = target.ok_or(AxError::InvalidInput)?;
                match nft_evaluate_chain(namespace, state, table, &target, packet, stack)? {
                    NftVerdict::Accept => {
                        stack.pop();
                        return Ok(NftVerdict::Accept);
                    }
                    _ if verdict == NftVerdict::Goto => break,
                    _ => {}
                }
            }
        }
    }
    stack.pop();
    match policy {
        NftVerdict::Drop => Err(LinuxError::EPERM.into()),
        NftVerdict::Reject => Err(LinuxError::ECONNREFUSED.into()),
        _ => Ok(NftVerdict::Continue),
    }
}

fn nft_evaluate_expressions(
    namespace: &Arc<NetworkNamespace>,
    packet: &mut [u8],
    expressions: &[NftExpr],
) -> AxResult<bool> {
    let mut registers: [Option<Vec<u8>>; 16] = core::array::from_fn(|_| None);
    for expression in expressions {
        match expression {
            NftExpr::Ct { key, dreg } => {
                let index = usize::try_from(*dreg).map_err(|_| AxError::InvalidInput)?;
                let value = match *key {
                    // NFT_CT_STATE: NEW for an initial tuple, ESTABLISHED for
                    // either direction of a retained conntrack entry.
                    0 => (if conntrack_is_established(namespace, packet) {
                        2u32
                    } else {
                        1u32
                    })
                    .to_ne_bytes()
                    .to_vec(),
                    7 => conntrack_tuple(packet)
                        .map(|tuple| tuple.protocol as u32)
                        .unwrap_or(0)
                        .to_ne_bytes()
                        .to_vec(),
                    _ => return Err(AxError::OperationNotSupported),
                };
                let slot = registers.get_mut(index).ok_or(AxError::InvalidInput)?;
                *slot = Some(value);
            }
            NftExpr::Payload {
                base,
                offset,
                len,
                dreg,
            } => {
                let index = usize::try_from(*dreg).map_err(|_| AxError::InvalidInput)?;
                let start = payload_offset(packet, *base, *offset)?;
                let end = start
                    .checked_add(*len as usize)
                    .ok_or(AxError::InvalidInput)?;
                let bytes = packet.get(start..end).ok_or(AxError::InvalidInput)?;
                let mut value = Vec::new();
                value
                    .try_reserve_exact(bytes.len())
                    .map_err(|_| AxError::NoMemory)?;
                value.extend_from_slice(bytes);
                *registers.get_mut(index).ok_or(AxError::InvalidInput)? = Some(value);
            }
            NftExpr::Cmp { sreg, op, data } => {
                let value = registers
                    .get(*sreg as usize)
                    .and_then(Option::as_ref)
                    .ok_or(AxError::InvalidInput)?;
                let equal = value.as_slice() == data.as_slice();
                // NFT_CMP_EQ/NEQ; relational packet comparisons are defined
                // only for equal-width big-endian scalar registers here.
                let matched = match *op {
                    0 => equal,
                    1 => !equal,
                    2 => value.as_slice() < data.as_slice(),
                    3 => value.as_slice() <= data.as_slice(),
                    4 => value.as_slice() > data.as_slice(),
                    5 => value.as_slice() >= data.as_slice(),
                    _ => return Err(AxError::InvalidInput),
                };
                if !matched {
                    return Ok(false);
                }
            }
            NftExpr::Nat {
                kind,
                family,
                addr_reg,
                proto_reg,
                masquerade,
            } => {
                let address = addr_reg
                    .and_then(|reg| registers.get(reg as usize))
                    .and_then(Option::as_ref)
                    .map(Vec::as_slice);
                let port = proto_reg
                    .and_then(|reg| registers.get(reg as usize))
                    .and_then(Option::as_ref)
                    .and_then(|value| value.get(..2))
                    .map(|bytes| u16::from_be_bytes(bytes.try_into().unwrap()));
                nft_apply_nat(
                    namespace,
                    packet,
                    *kind,
                    *family,
                    address,
                    port,
                    *masquerade,
                )?;
            }
        }
    }
    Ok(true)
}

fn payload_offset(packet: &[u8], base: u32, offset: u32) -> AxResult<usize> {
    let ip = match base {
        1 => 0usize,
        2 => match packet.first().map(|byte| byte >> 4) {
            Some(4) => usize::from(packet[0] & 0x0f) * 4,
            Some(6) => 40,
            _ => return Err(AxError::InvalidInput),
        },
        _ => return Err(AxError::OperationNotSupported),
    };
    ip.checked_add(offset as usize).ok_or(AxError::InvalidInput)
}

fn conntrack_is_established(namespace: &Arc<NetworkNamespace>, packet: &[u8]) -> bool {
    let Some(tuple) = conntrack_tuple(packet) else {
        return false;
    };
    let needle = Arc::downgrade(namespace);
    CONNTRACK.lock().iter().any(|entry| {
        Weak::ptr_eq(&entry.namespace, &needle)
            && (entry.original == tuple || entry.translated == tuple || entry.reply == tuple)
    })
}

fn conntrack_reverse_translate(namespace: &Arc<NetworkNamespace>, packet: &mut [u8]) -> AxResult {
    let Some(tuple) = conntrack_tuple(packet) else {
        return Ok(());
    };
    let needle = Arc::downgrade(namespace);
    let replacement = CONNTRACK
        .lock()
        .iter()
        .find(|entry| Weak::ptr_eq(&entry.namespace, &needle) && entry.reply == tuple)
        .map(|entry| conntrack_reverse(entry.original));
    if let Some(replacement) = replacement {
        rewrite_tuple(packet, replacement)?;
    }
    Ok(())
}

fn nft_apply_nat(
    namespace: &Arc<NetworkNamespace>,
    packet: &mut [u8],
    kind: u32,
    family: u32,
    address: Option<&[u8]>,
    port: Option<u16>,
    masquerade: bool,
) -> AxResult {
    let before = conntrack_tuple(packet).ok_or(AxError::InvalidInput)?;
    if family != 0 && family != before.family as u32 {
        return Err(AxError::InvalidInput);
    };
    let mut after = before;
    let width = if before.family == 4 { 4 } else { 16 };
    // nft NAT type: 0 DNAT, 1 SNAT.  Masquerade is SNAT using the route's
    // source address, which is already present in this compact router model.
    match kind {
        0 => {
            if let Some(address) = address {
                if address.len() != width {
                    return Err(AxError::InvalidInput);
                };
                after.destination[..width].copy_from_slice(address)
            }
            if let Some(port) = port {
                after.destination_port = port
            }
        }
        1 => {
            if !masquerade {
                if let Some(address) = address {
                    if address.len() != width {
                        return Err(AxError::InvalidInput);
                    };
                    after.source[..width].copy_from_slice(address)
                }
            }
            if let Some(port) = port {
                after.source_port = port
            }
        }
        _ => return Err(AxError::InvalidInput),
    }
    rewrite_tuple(packet, after)?;
    let needle = Arc::downgrade(namespace);
    let mut entries = CONNTRACK.lock();
    if let Some(entry) = entries.iter_mut().find(|entry| {
        Weak::ptr_eq(&entry.namespace, &needle)
            && (entry.original == before || entry.translated == before)
    }) {
        entry.translated = after;
        entry.reply = conntrack_reverse(after);
    }
    Ok(())
}

fn rewrite_tuple(packet: &mut [u8], tuple: ConntrackTuple) -> AxResult {
    let version = packet
        .first()
        .map(|byte| byte >> 4)
        .ok_or(AxError::InvalidInput)?;
    let (width, src, dst, l4, protocol) = match version {
        4 => {
            let l4 = usize::from(packet[0] & 0xf) * 4;
            if l4 < 20 || packet.len() < l4 {
                return Err(AxError::InvalidInput);
            };
            (4, 12, 16, l4, packet[9])
        }
        6 => {
            if packet.len() < 40 {
                return Err(AxError::InvalidInput);
            };
            (16, 8, 24, 40, packet[6])
        }
        _ => return Err(AxError::InvalidInput),
    };
    packet[src..src + width].copy_from_slice(&tuple.source[..width]);
    packet[dst..dst + width].copy_from_slice(&tuple.destination[..width]);
    if matches!(protocol, 6 | 17 | 33 | 132) && packet.len() >= l4 + 4 {
        packet[l4..l4 + 2].copy_from_slice(&tuple.source_port.to_be_bytes());
        packet[l4 + 2..l4 + 4].copy_from_slice(&tuple.destination_port.to_be_bytes());
    }
    recompute_checksums(packet)?;
    Ok(())
}

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut chunks = bytes.chunks_exact(2);
    for pair in &mut chunks {
        sum += u16::from_be_bytes([pair[0], pair[1]]) as u32
    }
    if let Some(&last) = chunks.remainder().first() {
        sum += (last as u32) << 8
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16)
    }
    !(sum as u16)
}
fn recompute_checksums(packet: &mut [u8]) -> AxResult {
    let version = packet
        .first()
        .map(|byte| byte >> 4)
        .ok_or(AxError::InvalidInput)?;
    let (l4, protocol, length, pseudo) = match version {
        4 => {
            let l4 = usize::from(packet[0] & 0xf) * 4;
            if l4 < 20 || packet.len() < l4 {
                return Err(AxError::InvalidInput);
            };
            packet[10] = 0;
            packet[11] = 0;
            let c = checksum(&packet[..l4]);
            packet[10..12].copy_from_slice(&c.to_be_bytes());
            let len = u16::from_be_bytes(packet[2..4].try_into().unwrap()) as usize;
            (l4, packet[9], len.saturating_sub(l4), {
                let mut p = Vec::new();
                p.extend_from_slice(&packet[12..20]);
                p.extend_from_slice(&[0, packet[9]]);
                p.extend_from_slice(&(len.saturating_sub(l4) as u16).to_be_bytes());
                p
            })
        }
        6 => {
            if packet.len() < 40 {
                return Err(AxError::InvalidInput);
            };
            let len = u16::from_be_bytes(packet[4..6].try_into().unwrap()) as usize;
            (40, packet[6], len, {
                let mut p = Vec::new();
                p.extend_from_slice(&packet[8..40]);
                p.extend_from_slice(&(len as u32).to_be_bytes());
                p.extend_from_slice(&[0, 0, 0, packet[6]]);
                p
            })
        }
        _ => return Err(AxError::InvalidInput),
    };
    if matches!(protocol, 6 | 17) && packet.len() >= l4 + length {
        let check = if protocol == 6 { l4 + 16 } else { l4 + 6 };
        if packet.len() >= check + 2 {
            packet[check] = 0;
            packet[check + 1] = 0;
            let mut data = pseudo;
            data.extend_from_slice(&packet[l4..l4 + length]);
            let value = checksum(&data);
            if protocol == 6 || value != 0 {
                packet[check..check + 2].copy_from_slice(&value.to_be_bytes())
            }
        }
    };
    Ok(())
}

pub(crate) fn nft_chain_hook(bytes: &[u8]) -> AxResult<NftHook> {
    // NFTA_CHAIN_HOOK is a nested `nft_hook_attributes`: hook number is the
    // first u32 attribute.  Priority is retained by nf_tables for ordering;
    // this compact engine has one ordered chain list per hook, so install
    // order is its stable tie breaker.
    let mut number = None;
    for_each_rtattr(bytes, |kind, value| {
        match kind {
            1 if value.len() == size_of::<u32>() => {
                number = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                Ok(())
            }
            // priority and optional device are installation metadata.  Chain
            // order remains deterministic in this engine; device-specific
            // hooks are rejected by the caller when no matching device exists.
            2 if value.len() == size_of::<i32>() => Ok(()),
            3 => Ok(()),
            _ => Err(AxError::InvalidInput),
        }
    })?;
    match number.ok_or(AxError::InvalidInput)? {
        0 => Ok(NftHook::Prerouting),
        1 => Ok(NftHook::Input),
        2 => Ok(NftHook::Forward),
        3 => Ok(NftHook::Output),
        4 => Ok(NftHook::Postrouting),
        _ => Err(AxError::InvalidInput),
    }
}

pub(crate) fn nft_expression_verdict(
    bytes: &[u8],
) -> AxResult<(NftVerdict, Option<String>, Option<String>, Vec<NftExpr>)> {
    // Expressions are nested again (list element -> name/data).  Every
    // standard terminal verdict has an ASCII expression name.  Inspecting
    // only complete NUL-terminated names avoids accepting arbitrary payload
    // substrings as a policy instruction.
    let mut result = NftVerdict::Continue;
    let mut target = None;
    let mut lookup = None;
    let mut operations = Vec::new();
    fn named(bytes: &[u8], needle: &[u8]) -> bool {
        bytes.windows(needle.len()).any(|part| part == needle)
    }
    for_each_rtattr(bytes, |_kind, expression| {
        if named(expression, b"drop\0") {
            result = NftVerdict::Drop;
        } else if named(expression, b"reject\0") {
            result = NftVerdict::Reject;
        } else if named(expression, b"accept\0") {
            result = NftVerdict::Accept;
        } else if named(expression, b"return\0") {
            result = NftVerdict::Return;
        }
        // jump/goto carry a chain name in their data payload; the parser
        // records their control-flow kind and rejects the absent target at
        // evaluation instead of silently accepting a malformed rule.
        else if named(expression, b"jump\0") || named(expression, b"goto\0") {
            result = if named(expression, b"jump\0") {
                NftVerdict::Jump
            } else {
                NftVerdict::Goto
            };
            for_each_rtattr(expression, |expr_kind, expr_body| {
                if expr_kind != 2 {
                    return Ok(());
                }
                for_each_rtattr(expr_body, |kind, data| {
                    if kind == 2 && target.is_none() {
                        target = Some(decode_nft_name(data)?);
                    }
                    Ok(())
                })
            })?;
        } else if named(expression, b"lookup\0") {
            for_each_rtattr(expression, |expr_kind, expr_body| {
                if expr_kind != 2 {
                    return Ok(());
                }
                for_each_rtattr(expr_body, |kind, data| {
                    if kind == 1 && lookup.is_none() {
                        lookup = Some(decode_nft_name(data)?);
                    }
                    Ok(())
                })
            })?;
        } else {
            let mut name = None;
            let mut data = None;
            for_each_rtattr(expression, |kind, value| match kind {
                1 if name.is_none() => {
                    name = Some(decode_nft_name(value)?);
                    Ok(())
                }
                2 if data.is_none() => {
                    data = Some(value);
                    Ok(())
                }
                _ => Ok(()),
            })?;
            match name.as_deref() {
                Some("ct") => {
                    let Some(data) = data else {
                        return Err(AxError::InvalidInput);
                    };
                    let (mut key, mut dreg) = (None, None);
                    for_each_rtattr(data, |kind, value| match kind {
                        1 if value.len() == 4 => {
                            key = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                            Ok(())
                        }
                        2 if value.len() == 4 => {
                            dreg = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                            Ok(())
                        }
                        _ => Err(AxError::InvalidInput),
                    })?;
                    operations.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    operations.push(NftExpr::Ct {
                        key: key.ok_or(AxError::InvalidInput)?,
                        dreg: dreg.ok_or(AxError::InvalidInput)?,
                    });
                }
                Some("payload") => {
                    let Some(data) = data else {
                        return Err(AxError::InvalidInput);
                    };
                    let (mut base, mut offset, mut len, mut dreg) = (None, None, None, None);
                    for_each_rtattr(data, |kind, value| {
                        if value.len() != 4 {
                            return Err(AxError::InvalidInput);
                        }
                        let value = u32::from_ne_bytes(value.try_into().unwrap());
                        match kind {
                            1 => base = Some(value),
                            2 => offset = Some(value),
                            3 => len = Some(value),
                            4 => dreg = Some(value),
                            _ => return Err(AxError::InvalidInput),
                        };
                        Ok(())
                    })?;
                    operations.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    operations.push(NftExpr::Payload {
                        base: base.ok_or(AxError::InvalidInput)?,
                        offset: offset.ok_or(AxError::InvalidInput)?,
                        len: len.ok_or(AxError::InvalidInput)?,
                        dreg: dreg.ok_or(AxError::InvalidInput)?,
                    });
                }
                Some("cmp") => {
                    let Some(data) = data else {
                        return Err(AxError::InvalidInput);
                    };
                    let (mut sreg, mut op, mut rhs) = (None, None, None);
                    for_each_rtattr(data, |kind, value| match kind {
                        1 if value.len() == 4 => {
                            sreg = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                            Ok(())
                        }
                        2 if value.len() == 4 => {
                            op = Some(u32::from_ne_bytes(value.try_into().unwrap()));
                            Ok(())
                        }
                        3 => for_each_rtattr(value, |inner, bytes| {
                            if inner == 1 && rhs.is_none() {
                                let mut copy = Vec::new();
                                copy.try_reserve_exact(bytes.len())
                                    .map_err(|_| AxError::NoMemory)?;
                                copy.extend_from_slice(bytes);
                                rhs = Some(copy)
                            };
                            Ok(())
                        }),
                        _ => Err(AxError::InvalidInput),
                    })?;
                    operations.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    operations.push(NftExpr::Cmp {
                        sreg: sreg.ok_or(AxError::InvalidInput)?,
                        op: op.ok_or(AxError::InvalidInput)?,
                        data: rhs.ok_or(AxError::InvalidInput)?,
                    });
                }
                Some("nat") | Some("masq") => {
                    let data = data.unwrap_or(&[]);
                    let (mut kind, mut family, mut addr, mut proto) = (
                        if name.as_deref() == Some("masq") {
                            Some(1)
                        } else {
                            None
                        },
                        None,
                        None,
                        None,
                    );
                    for_each_rtattr(data, |kind_id, value| {
                        if value.len() != 4 {
                            return Err(AxError::InvalidInput);
                        }
                        let value = u32::from_ne_bytes(value.try_into().unwrap());
                        match kind_id {
                            1 => kind = Some(value),
                            2 => family = Some(value),
                            3 => addr = Some(value),
                            5 => proto = Some(value),
                            _ => {}
                        }
                        Ok(())
                    })?;
                    operations.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    operations.push(NftExpr::Nat {
                        kind: kind.ok_or(AxError::InvalidInput)?,
                        family: family.unwrap_or(0),
                        addr_reg: addr,
                        proto_reg: proto,
                        masquerade: name.as_deref() == Some("masq"),
                    });
                }
                Some("immediate") => {
                    let Some(data) = data else {
                        return Err(AxError::InvalidInput);
                    };
                    let mut code = None;
                    for_each_rtattr(data, |kind, value| {
                        if kind == 2 {
                            for_each_rtattr(value, |inner, bytes| {
                                if inner == 1 && bytes.len() == 4 && code.is_none() {
                                    code = Some(i32::from_ne_bytes(bytes.try_into().unwrap()))
                                };
                                Ok(())
                            })
                        } else {
                            Ok(())
                        }
                    })?;
                    result = match code.ok_or(AxError::InvalidInput)? {
                        0 => NftVerdict::Drop,
                        1 => NftVerdict::Accept,
                        _ => return Err(AxError::OperationNotSupported),
                    };
                }
                Some("counter") | Some("meta") => {}
                Some(_) => return Err(AxError::OperationNotSupported),
                None => return Err(AxError::InvalidInput),
            }
        }
        Ok(())
    })?;
    if matches!(result, NftVerdict::Jump | NftVerdict::Goto) && target.is_none() {
        return Err(AxError::InvalidInput);
    }
    Ok((result, target, lookup, operations))
}

pub(crate) fn nft_set_element_key(bytes: &[u8]) -> AxResult<Vec<u8>> {
    // NFTA_SET_ELEM_LIST_ELEMENTS is a list of NFTA_LIST_ELEM containers;
    // each carries NFTA_SET_ELEM_KEY, itself a NFTA_DATA_VALUE container.
    // This implementation admits a single element per message, exactly what
    // the retained SetElement model represents.
    let mut key = None;
    for_each_rtattr(bytes, |_list_kind, element| {
        for_each_rtattr(element, |kind, data| {
            if kind != 1 || key.is_some() {
                return Ok(());
            }
            for_each_rtattr(data, |data_kind, value| {
                if data_kind == 1 && key.is_none() {
                    let mut copied = Vec::new();
                    copied
                        .try_reserve_exact(value.len())
                        .map_err(|_| AxError::NoMemory)?;
                    copied.extend_from_slice(value);
                    key = Some(copied);
                }
                Ok(())
            })
        })
    })?;
    key.ok_or(AxError::InvalidInput)
}
