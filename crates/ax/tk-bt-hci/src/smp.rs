//! Bluetooth LE Security Manager Protocol feature exchange primitives.
//!
//! Pairing PDUs are carried on L2CAP CID 0x0006; this module deliberately
//! accepts only complete SMP PDUs and leaves ACL reassembly to the transport.

use alloc::vec::Vec;

use crate::smp_crypto;

pub const SMP_CID: u16 = 0x0006;
pub const PAIRING_REQUEST: u8 = 0x01;
pub const PAIRING_RESPONSE: u8 = 0x02;
pub const PAIRING_FAILED: u8 = 0x05;
pub const SECURITY_REQUEST: u8 = 0x0b;
pub const PAIRING_PUBLIC_KEY: u8 = 0x0c;

/// Frame one complete SMP PDU in an HCI ACL packet and its L2CAP header.
pub fn build_acl_packet(handle: u16, pdu: &[u8]) -> Result<Vec<u8>, crate::Error> {
    if handle > 0x0fff || pdu.is_empty() || pdu.len() > u16::MAX as usize - 4 {
        return Err(crate::Error::InvalidLength);
    }
    let l2cap_length = u16::try_from(pdu.len()).map_err(|_| crate::Error::InvalidLength)?;
    let acl_length = l2cap_length
        .checked_add(4)
        .ok_or(crate::Error::InvalidLength)?;
    let mut packet = Vec::new();
    packet
        .try_reserve_exact(4 + usize::from(acl_length))
        .map_err(|_| crate::Error::NoMemory)?;
    packet.extend_from_slice(&(handle | 0x2000).to_le_bytes()); // PB=first non-flushable
    packet.extend_from_slice(&acl_length.to_le_bytes());
    packet.extend_from_slice(&l2cap_length.to_le_bytes());
    packet.extend_from_slice(&SMP_CID.to_le_bytes());
    packet.extend_from_slice(pdu);
    crate::Packet::parse(crate::PacketType::Acl, &packet)?;
    Ok(packet)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PairingFeatures {
    pub io_capability: u8,
    pub oob_data: bool,
    pub auth_req: u8,
    pub max_key_size: u8,
    pub initiator_key_distribution: u8,
    pub responder_key_distribution: u8,
}

impl PairingFeatures {
    pub fn parse(pdu: &[u8]) -> Result<Self, PairingError> {
        if pdu.len() != 7 || !matches!(pdu[0], PAIRING_REQUEST | PAIRING_RESPONSE) {
            return Err(PairingError::InvalidPdu);
        }
        if pdu[1] > 4
            || pdu[2] > 1
            || pdu[3] & !0x3f != 0
            || pdu[3] & 0x03 > 1
            || !(7..=16).contains(&pdu[4])
            || pdu[5] & !0x07 != 0
            || pdu[6] & !0x07 != 0
        {
            return Err(PairingError::InvalidFeatures);
        }
        Ok(Self {
            io_capability: pdu[1],
            oob_data: pdu[2] != 0,
            auth_req: pdu[3],
            max_key_size: pdu[4],
            initiator_key_distribution: pdu[5],
            responder_key_distribution: pdu[6],
        })
    }

    pub fn encode(self, opcode: u8) -> Result<[u8; 7], PairingError> {
        if !matches!(opcode, PAIRING_REQUEST | PAIRING_RESPONSE)
            || self.io_capability > 4
            || self.max_key_size < 7
            || self.max_key_size > 16
            || self.auth_req & !0x3f != 0
            || self.auth_req & 0x03 > 1
            || self.initiator_key_distribution & !0x07 != 0
            || self.responder_key_distribution & !0x07 != 0
        {
            return Err(PairingError::InvalidFeatures);
        }
        Ok([
            opcode,
            self.io_capability,
            u8::from(self.oob_data),
            self.auth_req,
            self.max_key_size,
            self.initiator_key_distribution,
            self.responder_key_distribution,
        ])
    }

    /// Select the LE association model supported by local and peer I/O.
    pub fn negotiate(self, peer: Self) -> Result<Negotiated, PairingError> {
        if peer.initiator_key_distribution & !self.initiator_key_distribution != 0
            || peer.responder_key_distribution & !self.responder_key_distribution != 0
        {
            return Err(PairingError::InvalidFeatures);
        }
        if self.oob_data || peer.oob_data {
            return Err(PairingError::AuthenticationUnsupported);
        }
        let secure_connections = self.auth_req & 0x08 != 0 && peer.auth_req & 0x08 != 0;
        let mitm = self.auth_req & 0x04 != 0 || peer.auth_req & 0x04 != 0;
        let numeric_comparison = mitm
            && secure_connections
            && matches!(self.io_capability, 1 | 4)
            && matches!(peer.io_capability, 1 | 4);
        let local_displays = matches!(self.io_capability, 0 | 1 | 4);
        let local_inputs = matches!(self.io_capability, 2 | 4);
        let peer_displays = matches!(peer.io_capability, 0 | 1 | 4);
        let peer_inputs = matches!(peer.io_capability, 2 | 4);
        let passkey_entry = mitm
            && !numeric_comparison
            && ((local_displays && peer_inputs) || (local_inputs && peer_displays));
        if mitm && !numeric_comparison && !passkey_entry {
            return Err(PairingError::AuthenticationUnsupported);
        }
        Ok(Negotiated {
            secure_connections,
            numeric_comparison,
            passkey_entry,
            bonding: self.auth_req & 0x03 != 0 && peer.auth_req & 0x03 != 0,
            key_size: self.max_key_size.min(peer.max_key_size),
            initiator_key_distribution: self.initiator_key_distribution
                & peer.initiator_key_distribution,
            responder_key_distribution: self.responder_key_distribution
                & peer.responder_key_distribution,
        })
    }
}

/// Parse the fixed-size peripheral Security Request payload and return its
/// authentication flags for the central-side pairing decision.
pub fn parse_security_request(pdu: &[u8]) -> Result<u8, PairingError> {
    if pdu.len() != 2 || pdu[0] != SECURITY_REQUEST || pdu[1] & !0x0f != 0 {
        return Err(PairingError::InvalidPdu);
    }
    Ok(pdu[1])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Negotiated {
    pub secure_connections: bool,
    pub numeric_comparison: bool,
    pub passkey_entry: bool,
    pub bonding: bool,
    pub key_size: u8,
    pub initiator_key_distribution: u8,
    pub responder_key_distribution: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PairingError {
    InvalidPdu,
    InvalidFeatures,
    InvalidState,
    AuthenticationUnsupported,
    UserRejected,
    ConfirmFailed,
    InvalidPublicKey,
    DhKeyCheckFailed,
    PeerFailed,
}

/// An LE central's bounded SMP exchange state. `address` values use Core
/// Appendix D octet order: `[address-type, six address octets]`.
///
/// This low-level exchange supports Just Works and Secure Connections Numeric
/// Comparison. OOB and peripheral-role flows remain unsupported. The caller is
/// responsible for the HCI ACL/L2CAP path, encryption transition and key
/// distribution; no pairing success is implied by merely constructing it.
pub struct Initiator {
    local: PairingFeatures,
    request: [u8; 7],
    response: Option<[u8; 7]>,
    local_address: [u8; 7],
    peer_address: [u8; 7],
    local_random: [u8; 16], // raw SMP wire order
    private_key: [u8; 32],
    passkey_randoms: [[u8; 16]; 20],
    passkey_round: u8,
    passkey: Option<u32>,
    local_public: Option<([u8; 32], [u8; 32])>,
    peer_public: Option<([u8; 32], [u8; 32])>,
    peer_random: Option<[u8; 16]>,
    peer_confirm: Option<[u8; 16]>,
    local_confirm: Option<[u8; 16]>,
    mac_key: Option<[u8; 16]>,
    ltk: Option<[u8; 16]>,
    secure_connections: bool,
    key_size: u8,
    negotiated: Option<Negotiated>,
    phase: Phase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    AwaitResponse,
    AwaitPublicKey,
    AwaitConfirm,
    AwaitRandom,
    AwaitUserConfirmation,
    AwaitPasskey,
    AwaitDhKeyCheck,
    AwaitEncryption,
    DistributingKeys,
    Complete,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Send(Vec<u8>),
    ConfirmNumeric(u32),
    RequestPasskey,
    DisplayPasskey {
        passkey: u32,
        pdu: Vec<u8>,
    },
    StartEncryption([u8; 16]),
    Paired {
        secure_connections: bool,
        ltk: [u8; 16],
    },
    Failed(PairingError),
}

impl Initiator {
    pub fn new(
        local: PairingFeatures,
        local_address: [u8; 7],
        peer_address: [u8; 7],
        local_random: [u8; 16],
        private_key: [u8; 32],
    ) -> Result<Self, PairingError> {
        Self::new_with_passkey_entropy(
            local,
            local_address,
            peer_address,
            local_random,
            private_key,
            [[0; 16]; 20],
        )
    }

    pub fn new_with_passkey_entropy(
        local: PairingFeatures,
        local_address: [u8; 7],
        peer_address: [u8; 7],
        local_random: [u8; 16],
        private_key: [u8; 32],
        passkey_randoms: [[u8; 16]; 20],
    ) -> Result<Self, PairingError> {
        let request = local.encode(PAIRING_REQUEST)?;
        if local.oob_data {
            return Err(PairingError::AuthenticationUnsupported);
        }
        Ok(Self {
            local,
            request,
            response: None,
            local_address,
            peer_address,
            local_random,
            private_key,
            passkey_randoms,
            passkey_round: 0,
            passkey: None,
            local_public: None,
            peer_public: None,
            peer_random: None,
            peer_confirm: None,
            local_confirm: None,
            mac_key: None,
            ltk: None,
            secure_connections: false,
            key_size: local.max_key_size,
            negotiated: None,
            phase: Phase::AwaitResponse,
        })
    }

    pub fn request_pdu(&self) -> [u8; 7] {
        self.request
    }

    pub fn negotiated(&self) -> Option<Negotiated> {
        self.negotiated
    }

    pub fn long_term_key(&self) -> Option<[u8; 16]> {
        self.ltk
    }

    /// Move to phase 3 only after a successful controller encryption event.
    pub fn encryption_complete(&mut self, success: bool) -> Result<(), PairingError> {
        if self.phase != Phase::AwaitEncryption {
            return Err(PairingError::InvalidState);
        }
        if !success {
            self.phase = Phase::Failed;
            return Err(PairingError::InvalidState);
        }
        self.phase = Phase::DistributingKeys;
        Ok(())
    }

    pub fn distributing_keys(&self) -> bool {
        self.phase == Phase::DistributingKeys
    }

    pub fn handle(&mut self, pdu: &[u8]) -> Action {
        match self.handle_inner(pdu) {
            Ok(actions) => actions,
            Err(error) => {
                self.phase = Phase::Failed;
                Action::Failed(error)
            }
        }
    }

    fn handle_inner(&mut self, pdu: &[u8]) -> Result<Action, PairingError> {
        if pdu.first() == Some(&PAIRING_FAILED) {
            if pdu.len() != 2 {
                return Err(PairingError::InvalidPdu);
            }
            self.phase = Phase::Failed;
            return Err(PairingError::PeerFailed);
        }
        match self.phase {
            Phase::AwaitResponse => {
                let peer = PairingFeatures::parse(pdu)?;
                if pdu[0] != PAIRING_RESPONSE {
                    return Err(PairingError::InvalidPdu);
                }
                let negotiated = self.local.negotiate(peer)?;
                self.secure_connections = negotiated.secure_connections;
                self.key_size = negotiated.key_size;
                self.negotiated = Some(negotiated);
                self.response = Some(pdu.try_into().map_err(|_| PairingError::InvalidPdu)?);
                if self.secure_connections {
                    let (x, y) = smp_crypto::p256_public(&self.private_key)
                        .ok_or(PairingError::InvalidPublicKey)?;
                    self.local_public = Some((x, y));
                    self.phase = Phase::AwaitPublicKey;
                    let mut public = Vec::with_capacity(65);
                    public.push(PAIRING_PUBLIC_KEY);
                    public.extend_from_slice(&to_wire(&x));
                    public.extend_from_slice(&to_wire(&y));
                    Ok(Action::Send(public))
                } else {
                    if negotiated.passkey_entry {
                        self.phase = Phase::AwaitPasskey;
                        return Ok(self.passkey_prompt());
                    }
                    let random = from_wire::<16>(&self.local_random)?;
                    let confirm = legacy_confirm(
                        &[0; 16],
                        &random,
                        &self.request,
                        &self.response.ok_or(PairingError::InvalidState)?,
                        &self.local_address,
                        &self.peer_address,
                    );
                    self.local_confirm = Some(confirm);
                    self.phase = Phase::AwaitConfirm;
                    Ok(Action::Send(pdu_with_key(0x03, &confirm)))
                }
            }
            Phase::AwaitPublicKey => {
                if pdu.len() != 65 || pdu[0] != PAIRING_PUBLIC_KEY {
                    return Err(PairingError::InvalidPdu);
                }
                let mut peer_x = [0; 32];
                let mut peer_y = [0; 32];
                peer_x.copy_from_slice(&from_wire::<32>(&pdu[1..33])?);
                peer_y.copy_from_slice(&from_wire::<32>(&pdu[33..65])?);
                if self
                    .local_public
                    .is_some_and(|(local_x, _)| local_x == peer_x)
                {
                    return Err(PairingError::InvalidPublicKey);
                }
                if smp_crypto::p256_ecdh(&self.private_key, &peer_x, &peer_y).is_none() {
                    return Err(PairingError::InvalidPublicKey);
                }
                self.peer_public = Some((peer_x, peer_y));
                if self.negotiated.is_some_and(|n| n.passkey_entry) {
                    self.phase = Phase::AwaitPasskey;
                    return Ok(self.passkey_prompt());
                }
                let local_x = self.local_public.ok_or(PairingError::InvalidState)?.0;
                let nonce = from_wire::<16>(&self.local_random)?;
                let confirm = smp_crypto::f4(&local_x, &peer_x, &nonce, 0);
                self.phase = Phase::AwaitConfirm;
                Ok(Action::Send(pdu_with_key(0x03, &confirm)))
            }
            Phase::AwaitConfirm => {
                if pdu.len() != 17 || pdu[0] != 0x03 {
                    return Err(PairingError::InvalidPdu);
                }
                let confirm = from_wire::<16>(&pdu[1..])?;
                self.peer_confirm = Some(confirm);
                self.phase = Phase::AwaitRandom;
                Ok(Action::Send(pdu_with_wire(
                    0x04,
                    self.current_local_nonce(),
                )))
            }
            Phase::AwaitRandom if !self.secure_connections => {
                if pdu.len() != 17 || pdu[0] != 0x04 {
                    return Err(PairingError::InvalidPdu);
                }
                let peer_random_wire: [u8; 16] =
                    pdu[1..].try_into().map_err(|_| PairingError::InvalidPdu)?;
                let peer_random = from_wire::<16>(&peer_random_wire)?;
                let tk = self.passkey.map(passkey_tk).unwrap_or([0; 16]);
                let expected = legacy_confirm(
                    &tk,
                    &peer_random,
                    &self.request,
                    &self.response.ok_or(PairingError::InvalidState)?,
                    &self.local_address,
                    &self.peer_address,
                );
                if Some(expected) != self.peer_confirm || Some(expected) == self.local_confirm {
                    return Err(PairingError::ConfirmFailed);
                }
                let local_random = from_wire::<16>(self.current_local_nonce())?;
                let key = smp_crypto::s1(
                    &tk,
                    peer_random[8..]
                        .try_into()
                        .map_err(|_| PairingError::InvalidPdu)?,
                    local_random[8..]
                        .try_into()
                        .map_err(|_| PairingError::InvalidPdu)?,
                );
                let key = mask_key(key, self.key_size);
                self.phase = Phase::AwaitEncryption;
                Ok(Action::StartEncryption(key))
            }
            Phase::AwaitRandom if self.secure_connections => {
                if pdu.len() != 17 || pdu[0] != 0x04 {
                    return Err(PairingError::InvalidPdu);
                }
                let peer_random: [u8; 16] =
                    pdu[1..].try_into().map_err(|_| PairingError::InvalidPdu)?;
                let (local_x, _) = self.local_public.ok_or(PairingError::InvalidState)?;
                let (peer_x, _) = self.peer_public.ok_or(PairingError::InvalidState)?;
                let z = if self.negotiated.is_some_and(|n| n.passkey_entry) {
                    0x80 | (self.passkey.unwrap_or(0) >> self.passkey_round & 1) as u8
                } else {
                    0
                };
                let expected =
                    smp_crypto::f4(&peer_x, &local_x, &from_wire::<16>(&peer_random)?, z);
                if Some(expected) != self.peer_confirm {
                    return Err(PairingError::ConfirmFailed);
                }
                self.peer_random = Some(peer_random);
                if self.negotiated.is_some_and(|n| n.passkey_entry) {
                    if self.passkey_round < 19 {
                        self.passkey_round += 1;
                        let nonce = from_wire::<16>(self.current_local_nonce())?;
                        let bit = self.passkey.unwrap_or(0) >> self.passkey_round & 1;
                        let confirm = smp_crypto::f4(&local_x, &peer_x, &nonce, 0x80 | bit as u8);
                        self.local_confirm = Some(confirm);
                        self.phase = Phase::AwaitConfirm;
                        return Ok(Action::Send(pdu_with_key(0x03, &confirm)));
                    }
                    return self.finish_secure_connections();
                }
                if self.negotiated.is_some_and(|n| n.numeric_comparison) {
                    let local_x = self.local_public.ok_or(PairingError::InvalidState)?.0;
                    let nonce = from_wire::<16>(self.current_local_nonce())?;
                    let peer_nonce = from_wire::<16>(&peer_random)?;
                    let value = smp_crypto::g2(&local_x, &peer_x, &nonce, &peer_nonce);
                    self.phase = Phase::AwaitUserConfirmation;
                    Ok(Action::ConfirmNumeric(value))
                } else {
                    self.finish_secure_connections()
                }
            }
            Phase::AwaitDhKeyCheck => {
                if pdu.len() != 17 || pdu[0] != 0x0d {
                    return Err(PairingError::InvalidPdu);
                }
                let peer_check = from_wire::<16>(&pdu[1..])?;
                let mac_key = self.mac_key.ok_or(PairingError::InvalidState)?;
                let n1 = from_wire::<16>(self.current_local_nonce())?;
                let n2 = from_wire::<16>(&self.peer_random.ok_or(PairingError::InvalidState)?)?;
                let response = self.response.ok_or(PairingError::InvalidState)?;
                let peer_iocap = [response[3], response[2], response[1]];
                let expected = smp_crypto::f6(
                    &mac_key,
                    &n2,
                    &n1,
                    &self.passkey.map(passkey_tk).unwrap_or([0; 16]),
                    &peer_iocap,
                    &self.peer_address,
                    &self.local_address,
                );
                if peer_check != expected {
                    return Err(PairingError::DhKeyCheckFailed);
                }
                let ltk = self.ltk.ok_or(PairingError::InvalidState)?;
                self.phase = Phase::AwaitEncryption;
                Ok(Action::StartEncryption(ltk))
            }
            Phase::AwaitEncryption => Err(PairingError::InvalidState),
            Phase::DistributingKeys => Err(PairingError::InvalidState),
            Phase::AwaitUserConfirmation => Err(PairingError::InvalidState),
            Phase::Complete | Phase::Failed => Err(PairingError::InvalidState),
            _ => Err(PairingError::InvalidState),
        }
    }

    pub fn awaiting_user_confirmation(&self) -> bool {
        self.phase == Phase::AwaitUserConfirmation
    }

    pub fn awaiting_passkey(&self) -> bool {
        self.phase == Phase::AwaitPasskey
    }

    /// Supply the passkey requested by the association model. A display-capable
    /// local side gets a generated six-digit value from its secure nonce pool.
    pub fn provide_passkey(&mut self, passkey: u32) -> Action {
        if self.phase != Phase::AwaitPasskey || passkey > 999_999 {
            return Action::Failed(PairingError::InvalidState);
        }
        if self.secure_connections && self.passkey_randoms.iter().all(|nonce| *nonce == [0; 16]) {
            self.phase = Phase::Failed;
            return Action::Failed(PairingError::AuthenticationUnsupported);
        }
        self.passkey = Some(passkey);
        if self.secure_connections {
            let Some((local_x, _)) = self.local_public else {
                return Action::Failed(PairingError::InvalidState);
            };
            let Some((peer_x, _)) = self.peer_public else {
                return Action::Failed(PairingError::InvalidState);
            };
            let nonce = match from_wire::<16>(self.current_local_nonce()) {
                Ok(nonce) => nonce,
                Err(error) => return Action::Failed(error),
            };
            let confirm = smp_crypto::f4(&local_x, &peer_x, &nonce, 0x80 | (passkey as u8 & 1));
            self.local_confirm = Some(confirm);
            self.phase = Phase::AwaitConfirm;
            Action::Send(pdu_with_key(0x03, &confirm))
        } else {
            let Some(response) = self.response else {
                return Action::Failed(PairingError::InvalidState);
            };
            let confirm = legacy_confirm(
                &passkey_tk(passkey),
                &self.local_random,
                &self.request,
                &response,
                &self.local_address,
                &self.peer_address,
            );
            self.local_confirm = Some(confirm);
            self.phase = Phase::AwaitConfirm;
            Action::Send(pdu_with_key(0x03, &confirm))
        }
    }

    fn passkey_prompt(&mut self) -> Action {
        let peer_io = self.response.map(|response| response[1]).unwrap_or(3);
        let local_displays = matches!(self.local.io_capability, 0 | 1 | 4);
        let peer_inputs = matches!(peer_io, 2 | 4);
        if local_displays && peer_inputs {
            let cutoff = ((u64::from(u32::MAX) + 1) / 1_000_000) * 1_000_000;
            let Some(passkey) = self
                .local_random
                .chunks_exact(4)
                .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap_or([0; 4])))
                .find(|value| u64::from(*value) < cutoff)
                .map(|value| value % 1_000_000)
            else {
                self.phase = Phase::Failed;
                return Action::Failed(PairingError::AuthenticationUnsupported);
            };
            match self.provide_passkey(passkey) {
                Action::Send(pdu) => Action::DisplayPasskey { passkey, pdu },
                other => other,
            }
        } else {
            Action::RequestPasskey
        }
    }

    fn current_local_nonce(&self) -> &[u8; 16] {
        if self.secure_connections && self.negotiated.is_some_and(|n| n.passkey_entry) {
            &self.passkey_randoms[usize::from(self.passkey_round)]
        } else {
            &self.local_random
        }
    }

    pub fn user_confirmation(&mut self, accept: bool) -> Action {
        if self.phase != Phase::AwaitUserConfirmation {
            return Action::Failed(PairingError::InvalidState);
        }
        if !accept {
            self.phase = Phase::Failed;
            return Action::Failed(PairingError::UserRejected);
        }
        match self.finish_secure_connections() {
            Ok(action) => action,
            Err(error) => {
                self.phase = Phase::Failed;
                Action::Failed(error)
            }
        }
    }

    fn finish_secure_connections(&mut self) -> Result<Action, PairingError> {
        self.local_public.ok_or(PairingError::InvalidState)?;
        let (peer_x, peer_y) = self.peer_public.ok_or(PairingError::InvalidState)?;
        let dh_key = smp_crypto::p256_ecdh(&self.private_key, &peer_x, &peer_y)
            .ok_or(PairingError::InvalidPublicKey)?;
        let n1 = from_wire::<16>(self.current_local_nonce())?;
        let n2 = from_wire::<16>(&self.peer_random.ok_or(PairingError::InvalidState)?)?;
        let a1 = self.local_address;
        let a2 = self.peer_address;
        let (mac_key, ltk) = smp_crypto::f5(&dh_key, &n1, &n2, &a1, &a2);
        self.mac_key = Some(mac_key);
        self.ltk = Some(mask_key(ltk, self.key_size));
        let check = smp_crypto::f6(
            &mac_key,
            &n1,
            &n2,
            &self.passkey.map(passkey_tk).unwrap_or([0; 16]),
            &[self.local.auth_req, 0, self.local.io_capability],
            &a1,
            &a2,
        );
        self.phase = Phase::AwaitDhKeyCheck;
        Ok(Action::Send(pdu_with_key(0x0d, &check)))
    }
}

fn pdu_with_key(opcode: u8, value: &[u8; 16]) -> Vec<u8> {
    let mut pdu = Vec::with_capacity(17);
    pdu.push(opcode);
    pdu.extend_from_slice(&to_wire(value));
    pdu
}

fn pdu_with_wire(opcode: u8, value: &[u8; 16]) -> Vec<u8> {
    let mut pdu = Vec::with_capacity(17);
    pdu.push(opcode);
    pdu.extend_from_slice(value);
    pdu
}

fn mask_key(mut key: [u8; 16], size: u8) -> [u8; 16] {
    let keep = usize::from(size.clamp(7, 16));
    key[..16 - keep].fill(0);
    key
}

fn passkey_tk(passkey: u32) -> [u8; 16] {
    let mut key = [0; 16];
    key[..4].copy_from_slice(&passkey.to_le_bytes());
    key
}

fn legacy_confirm(
    tk: &[u8; 16],
    random: &[u8; 16],
    request: &[u8; 7],
    response: &[u8; 7],
    initiator: &[u8; 7],
    responder: &[u8; 7],
) -> [u8; 16] {
    let mut p1 = [0u8; 16];
    p1[..7].copy_from_slice(response);
    p1[7..14].copy_from_slice(request);
    p1[14] = responder[0] & 1;
    p1[15] = initiator[0] & 1;
    let mut p2 = [0u8; 16];
    p2[4..10].copy_from_slice(&initiator[1..]);
    p2[10..].copy_from_slice(&responder[1..]);
    smp_crypto::c1(tk, random, &p1, &p2)
}

fn to_wire<const N: usize>(value: &[u8; N]) -> [u8; N] {
    let mut bytes = *value;
    bytes.reverse();
    bytes
}

fn from_wire<const N: usize>(value: &[u8]) -> Result<[u8; N], PairingError> {
    let mut bytes: [u8; N] = value.try_into().map_err(|_| PairingError::InvalidPdu)?;
    bytes.reverse();
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn features(auth_req: u8, max_key_size: u8) -> PairingFeatures {
        PairingFeatures {
            io_capability: 3,
            oob_data: false,
            auth_req,
            max_key_size,
            initiator_key_distribution: 3,
            responder_key_distribution: 3,
        }
    }

    #[test]
    fn feature_exchange_negotiates_secure_connections_and_key_distribution() {
        let local = features(0x09, 16);
        let peer = features(0x09, 12);
        assert_eq!(
            local.negotiate(peer),
            Ok(Negotiated {
                secure_connections: true,
                numeric_comparison: false,
                passkey_entry: false,
                bonding: true,
                key_size: 12,
                initiator_key_distribution: 3,
                responder_key_distribution: 3,
            })
        );
        assert_eq!(
            PairingFeatures::parse(&local.encode(PAIRING_REQUEST).unwrap()),
            Ok(local)
        );
        assert_eq!(
            local.negotiate(features(0x05, 16)),
            Err(PairingError::AuthenticationUnsupported)
        );
        let mut peer_with_extra_distribution = features(0x09, 16);
        peer_with_extra_distribution.initiator_key_distribution = 4;
        assert_eq!(
            local.negotiate(peer_with_extra_distribution),
            Err(PairingError::InvalidFeatures)
        );
        let mut numeric_local = features(0x0d, 16);
        numeric_local.io_capability = 1;
        let mut numeric_peer = numeric_local;
        numeric_peer.io_capability = 4;
        assert!(
            numeric_local
                .negotiate(numeric_peer)
                .unwrap()
                .numeric_comparison
        );
    }

    #[test]
    fn peripheral_security_request_is_two_octets_and_preserves_auth_flags() {
        assert_eq!(parse_security_request(&[SECURITY_REQUEST, 0x0d]), Ok(0x0d));
        assert_eq!(
            parse_security_request(&[SECURITY_REQUEST]),
            Err(PairingError::InvalidPdu)
        );
        assert_eq!(
            parse_security_request(&[SECURITY_REQUEST, 0x80]),
            Err(PairingError::InvalidPdu)
        );
    }

    #[test]
    fn smp_pdu_is_framed_as_one_checked_l2cap_acl_packet() {
        assert_eq!(
            build_acl_packet(0x0123, &[PAIRING_REQUEST, 3, 0, 9, 16, 3, 3]).unwrap(),
            [
                0x23,
                0x21,
                11,
                0,
                7,
                0,
                6,
                0,
                PAIRING_REQUEST,
                3,
                0,
                9,
                16,
                3,
                3
            ]
        );
        assert_eq!(
            build_acl_packet(0x1000, &[1]),
            Err(crate::Error::InvalidLength)
        );
        assert_eq!(build_acl_packet(1, &[]), Err(crate::Error::InvalidLength));
    }

    #[test]
    fn initiator_sends_secure_connections_public_key_and_checks_peer_point() {
        let mut initiator = Initiator::new(
            features(0x09, 16),
            [0, 1, 2, 3, 4, 5, 6],
            [0, 7, 8, 9, 10, 11, 12],
            [1; 16],
            [2; 32],
        )
        .unwrap();
        let response = features(0x09, 16).encode(PAIRING_RESPONSE).unwrap();
        let Action::Send(public_key) = initiator.handle(&response) else {
            panic!("expected public key")
        };
        assert_eq!(public_key[0], PAIRING_PUBLIC_KEY);
        let mut bad = vec![PAIRING_PUBLIC_KEY];
        bad.extend_from_slice(&[0; 64]);
        assert_eq!(
            initiator.handle(&bad),
            Action::Failed(PairingError::InvalidPublicKey)
        );
    }

    #[test]
    fn legacy_just_works_validates_confirm_and_derives_stk() {
        let local_address = [0, 1, 2, 3, 4, 5, 6];
        let peer_address = [1, 7, 8, 9, 10, 11, 12];
        let local_random = [0x11; 16];
        let local = features(0x01, 16);
        let peer = features(0x01, 16);
        let mut session =
            Initiator::new(local, local_address, peer_address, local_random, [2; 32]).unwrap();
        let response = peer.encode(PAIRING_RESPONSE).unwrap();
        let Action::Send(local_confirm_pdu) = session.handle(&response) else {
            panic!("expected local confirm")
        };
        assert_eq!(local_confirm_pdu[0], 0x03);
        let peer_random = [0x22; 16];
        let confirm = legacy_confirm(
            &[0; 16],
            &from_wire::<16>(&peer_random).unwrap(),
            &session.request,
            &response,
            &local_address,
            &peer_address,
        );
        let Action::Send(random_pdu) = session.handle(&pdu_with_key(0x03, &confirm)) else {
            panic!("expected local random")
        };
        assert_eq!(random_pdu, pdu_with_wire(0x04, &local_random));
        let Action::StartEncryption(stk) = session.handle(&pdu_with_wire(0x04, &peer_random))
        else {
            panic!("expected STK")
        };
        let r1 = from_wire::<16>(&peer_random).unwrap();
        let r2 = from_wire::<16>(&local_random).unwrap();
        assert_eq!(
            stk,
            smp_crypto::s1(
                &[0; 16],
                r1[8..].try_into().unwrap(),
                r2[8..].try_into().unwrap()
            )
        );
    }

    #[test]
    fn secure_connections_just_works_validates_dhkey_check_before_encryption() {
        let local_address = [0, 1, 2, 3, 4, 5, 6];
        let peer_address = [1, 7, 8, 9, 10, 11, 12];
        let local_private = [2; 32];
        let peer_private = [3; 32];
        let local_nonce = [0x11; 16];
        let peer_nonce = [0x22; 16];
        let local_features = features(0x09, 16);
        let peer_features = features(0x09, 16);
        let mut session = Initiator::new(
            local_features,
            local_address,
            peer_address,
            local_nonce,
            local_private,
        )
        .unwrap();
        let response = peer_features.encode(PAIRING_RESPONSE).unwrap();
        let Action::Send(local_public_pdu) = session.handle(&response) else {
            panic!("expected public key")
        };
        let (peer_x, peer_y) = smp_crypto::p256_public(&peer_private).unwrap();
        let mut peer_public = vec![PAIRING_PUBLIC_KEY];
        peer_public.extend_from_slice(&to_wire(&peer_x));
        peer_public.extend_from_slice(&to_wire(&peer_y));
        let Action::Send(local_confirm) = session.handle(&peer_public) else {
            panic!("expected confirm")
        };
        let local_x = from_wire::<32>(&local_public_pdu[1..33]).unwrap();
        let local_confirm_expected = smp_crypto::f4(
            &local_x,
            &peer_x,
            &from_wire::<16>(&local_nonce).unwrap(),
            0,
        );
        assert_eq!(local_confirm, pdu_with_key(0x03, &local_confirm_expected));
        let peer_confirm = smp_crypto::f4(&peer_x, &local_x, &from_wire(&peer_nonce).unwrap(), 0);
        let Action::Send(sent_random) = session.handle(&pdu_with_key(0x03, &peer_confirm)) else {
            panic!("expected random")
        };
        assert_eq!(sent_random, pdu_with_wire(0x04, &local_nonce));
        let dh_key = smp_crypto::p256_ecdh(&local_private, &peer_x, &peer_y).unwrap();
        let (mac_key, ltk) = smp_crypto::f5(
            &dh_key,
            &from_wire::<16>(&local_nonce).unwrap(),
            &from_wire(&peer_nonce).unwrap(),
            &local_address,
            &peer_address,
        );
        let peer_check = smp_crypto::f6(
            &mac_key,
            &from_wire(&peer_nonce).unwrap(),
            &from_wire::<16>(&local_nonce).unwrap(),
            &[0; 16],
            &[peer_features.auth_req, 0, peer_features.io_capability],
            &peer_address,
            &local_address,
        );
        assert_eq!(
            session.handle(&pdu_with_wire(0x04, &peer_nonce)),
            Action::Send(pdu_with_key(
                0x0d,
                &smp_crypto::f6(
                    &mac_key,
                    &from_wire::<16>(&local_nonce).unwrap(),
                    &from_wire(&peer_nonce).unwrap(),
                    &[0; 16],
                    &[local_features.auth_req, 0, local_features.io_capability],
                    &local_address,
                    &peer_address,
                )
            ))
        );
        assert_eq!(
            session.handle(&pdu_with_key(0x0d, &peer_check)),
            Action::StartEncryption(ltk)
        );
        assert!(session.encryption_complete(true).is_ok());
        assert!(session.distributing_keys());
    }

    #[test]
    fn secure_connections_numeric_comparison_waits_for_user_decision() {
        let local_address = [0, 1, 2, 3, 4, 5, 6];
        let peer_address = [1, 7, 8, 9, 10, 11, 12];
        let local_private = [2; 32];
        let peer_private = [3; 32];
        let local_nonce = [0x11; 16];
        let peer_nonce = [0x22; 16];
        let mut local_features = features(0x0d, 16);
        local_features.io_capability = 1;
        let mut peer_features = features(0x0d, 16);
        peer_features.io_capability = 4;
        let mut session = Initiator::new(
            local_features,
            local_address,
            peer_address,
            local_nonce,
            local_private,
        )
        .unwrap();
        let response = peer_features.encode(PAIRING_RESPONSE).unwrap();
        let Action::Send(local_public_pdu) = session.handle(&response) else {
            panic!("expected public key")
        };
        let (peer_x, peer_y) = smp_crypto::p256_public(&peer_private).unwrap();
        let mut peer_public = vec![PAIRING_PUBLIC_KEY];
        peer_public.extend_from_slice(&to_wire(&peer_x));
        peer_public.extend_from_slice(&to_wire(&peer_y));
        let Action::Send(_) = session.handle(&peer_public) else {
            panic!("expected local confirm")
        };
        let local_x = from_wire::<32>(&local_public_pdu[1..33]).unwrap();
        let peer_confirm =
            smp_crypto::f4(&peer_x, &local_x, &from_wire::<16>(&peer_nonce).unwrap(), 0);
        assert!(matches!(
            session.handle(&pdu_with_key(0x03, &peer_confirm)),
            Action::Send(_)
        ));
        let Action::ConfirmNumeric(value) = session.handle(&pdu_with_wire(0x04, &peer_nonce))
        else {
            panic!("expected user confirmation")
        };
        assert_eq!(
            value,
            smp_crypto::g2(
                &local_x,
                &peer_x,
                &from_wire::<16>(&local_nonce).unwrap(),
                &from_wire::<16>(&peer_nonce).unwrap()
            )
        );
        assert!(session.awaiting_user_confirmation());
        assert!(matches!(session.user_confirmation(true), Action::Send(pdu) if pdu[0] == 0x0d));
    }

    #[test]
    fn secure_connections_passkey_entry_runs_all_twenty_confirm_rounds() {
        let local = PairingFeatures {
            io_capability: 2, // keyboard-only: local user enters the passkey
            oob_data: false,
            auth_req: 0x0d,
            max_key_size: 16,
            initiator_key_distribution: 0,
            responder_key_distribution: 0,
        };
        let peer = PairingFeatures {
            io_capability: 0, // display-only
            ..local
        };
        let local_private = [1u8; 32];
        let (local_x, _local_y) = smp_crypto::p256_public(&local_private).unwrap();
        let peer_private = [2u8; 32];
        let (peer_x, peer_y) = smp_crypto::p256_public(&peer_private).unwrap();
        let rounds = core::array::from_fn(|round| [round as u8 + 1; 16]);
        let mut initiator = Initiator::new_with_passkey_entropy(
            local,
            [0, 1, 2, 3, 4, 5, 6],
            [0, 6, 5, 4, 3, 2, 1],
            [0x31; 16],
            local_private,
            rounds,
        )
        .unwrap();
        assert!(matches!(
            initiator.handle(&peer.encode(PAIRING_RESPONSE).unwrap()),
            Action::Send(_)
        ));
        let mut public = vec![PAIRING_PUBLIC_KEY];
        public.extend_from_slice(&to_wire(&peer_x));
        public.extend_from_slice(&to_wire(&peer_y));
        assert_eq!(initiator.handle(&public), Action::RequestPasskey);
        let passkey = 123_456u32;
        assert!(matches!(
            initiator.provide_passkey(passkey),
            Action::Send(_)
        ));
        for round in 0..20u8 {
            let peer_nonce = [0xa0 + round; 16];
            let bit = (passkey >> round) & 1;
            let peer_confirm = smp_crypto::f4(&peer_x, &local_x, &peer_nonce, 0x80 | bit as u8);
            let mut confirm_pdu = vec![0x03];
            confirm_pdu.extend_from_slice(&to_wire(&peer_confirm));
            assert!(matches!(initiator.handle(&confirm_pdu), Action::Send(_)));
            let mut random_pdu = vec![0x04];
            random_pdu.extend_from_slice(&peer_nonce);
            let action = initiator.handle(&random_pdu);
            if round < 19 {
                assert!(matches!(action, Action::Send(_)));
            } else {
                assert!(matches!(action, Action::Send(ref pdu) if pdu[0] == 0x0d));
            }
        }
        let local_nonce = from_wire::<16>(&rounds[19]).unwrap();
        let peer_nonce = from_wire::<16>(&[0xb3; 16]).unwrap();
        let dh_key = smp_crypto::p256_ecdh(&local_private, &peer_x, &peer_y).unwrap();
        let (mac_key, _) = smp_crypto::f5(
            &dh_key,
            &local_nonce,
            &peer_nonce,
            &initiator.local_address,
            &initiator.peer_address,
        );
        let peer_check = smp_crypto::f6(
            &mac_key,
            &peer_nonce,
            &local_nonce,
            &passkey_tk(passkey),
            &[peer.auth_req, 0, peer.io_capability],
            &initiator.peer_address,
            &initiator.local_address,
        );
        assert!(matches!(
            initiator.handle(&pdu_with_key(0x0d, &peer_check)),
            Action::StartEncryption(_)
        ));
    }

    #[test]
    fn legacy_passkey_entry_uses_user_passkey_as_tk() {
        let local = PairingFeatures {
            io_capability: 0, // this central displays
            oob_data: false,
            auth_req: 0x05,
            max_key_size: 16,
            initiator_key_distribution: 1,
            responder_key_distribution: 1,
        };
        let peer = PairingFeatures {
            io_capability: 2, // peer enters the displayed passkey
            ..local
        };
        let local_address = [0, 1, 2, 3, 4, 5, 6];
        let peer_address = [0, 6, 5, 4, 3, 2, 1];
        let local_random = [0x11; 16];
        let request = local.encode(PAIRING_REQUEST).unwrap();
        let response = peer.encode(PAIRING_RESPONSE).unwrap();
        let mut initiator =
            Initiator::new(local, local_address, peer_address, local_random, [1; 32]).unwrap();
        let Action::DisplayPasskey { passkey, pdu } = initiator.handle(&response) else {
            panic!("display passkey association expected");
        };
        assert_eq!(pdu[0], 0x03);
        let tk = passkey_tk(passkey);
        let peer_random = [0x22; 16];
        let confirm = legacy_confirm(
            &tk,
            &peer_random,
            &request,
            &response,
            &local_address,
            &peer_address,
        );
        let mut confirm_pdu = vec![0x03];
        confirm_pdu.extend_from_slice(&to_wire(&confirm));
        assert!(matches!(initiator.handle(&confirm_pdu), Action::Send(_)));
        let mut random_pdu = vec![0x04];
        random_pdu.extend_from_slice(&peer_random);
        let Action::StartEncryption(key) = initiator.handle(&random_pdu) else {
            panic!("valid legacy passkey should derive the STK");
        };
        let local_nonce = from_wire::<16>(&local_random).unwrap();
        let peer_nonce = from_wire::<16>(&peer_random).unwrap();
        let expected = smp_crypto::s1(
            &tk,
            peer_nonce[8..].try_into().unwrap(),
            local_nonce[8..].try_into().unwrap(),
        );
        assert_eq!(key, expected);
    }
}
