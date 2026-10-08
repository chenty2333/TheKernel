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
pub const PAIRING_PUBLIC_KEY: u8 = 0x0c;

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

    /// Choose a safe association model for this implementation. Passkey/OOB
    /// and MITM-required associations fail closed rather than silently falling
    /// back to unauthenticated Just Works.
    pub fn negotiate_just_works(self, peer: Self) -> Result<Negotiated, PairingError> {
        if self.oob_data || peer.oob_data || self.auth_req & 0x04 != 0 || peer.auth_req & 0x04 != 0
        {
            return Err(PairingError::AuthenticationUnsupported);
        }
        let secure_connections = self.auth_req & 0x08 != 0 && peer.auth_req & 0x08 != 0;
        Ok(Negotiated {
            secure_connections,
            key_size: self.max_key_size.min(peer.max_key_size),
            initiator_key_distribution: self.initiator_key_distribution
                & peer.responder_key_distribution,
            responder_key_distribution: self.responder_key_distribution
                & peer.initiator_key_distribution,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Negotiated {
    pub secure_connections: bool,
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
    ConfirmFailed,
    InvalidPublicKey,
    DhKeyCheckFailed,
}

/// An LE central's bounded SMP exchange state. `address` values use Core
/// Appendix D octet order: `[address-type, six address octets]`.
///
/// This low-level exchange currently supports Just Works only. The caller is
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
    AwaitDhKeyCheck,
    AwaitEncryption,
    Complete,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Send(Vec<u8>),
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
        let request = local.encode(PAIRING_REQUEST)?;
        if local.auth_req & 0x04 != 0 || local.oob_data {
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
            self.phase = Phase::Failed;
            return Err(PairingError::InvalidState);
        }
        match self.phase {
            Phase::AwaitResponse => {
                let peer = PairingFeatures::parse(pdu)?;
                if pdu[0] != PAIRING_RESPONSE {
                    return Err(PairingError::InvalidPdu);
                }
                let negotiated = self.local.negotiate_just_works(peer)?;
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
                Ok(Action::Send(pdu_with_wire(0x04, &self.local_random)))
            }
            Phase::AwaitRandom if !self.secure_connections => {
                if pdu.len() != 17 || pdu[0] != 0x04 {
                    return Err(PairingError::InvalidPdu);
                }
                let peer_random_wire: [u8; 16] =
                    pdu[1..].try_into().map_err(|_| PairingError::InvalidPdu)?;
                let peer_random = from_wire::<16>(&peer_random_wire)?;
                let expected = legacy_confirm(
                    &[0; 16],
                    &peer_random,
                    &self.request,
                    &self.response.ok_or(PairingError::InvalidState)?,
                    &self.local_address,
                    &self.peer_address,
                );
                if Some(expected) != self.peer_confirm || Some(expected) == self.local_confirm {
                    return Err(PairingError::ConfirmFailed);
                }
                let local_random = from_wire::<16>(&self.local_random)?;
                let key = smp_crypto::s1(
                    &[0; 16],
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
                let (peer_x, peer_y) = self.peer_public.ok_or(PairingError::InvalidState)?;
                let expected =
                    smp_crypto::f4(&peer_x, &local_x, &from_wire::<16>(&peer_random)?, 0);
                if Some(expected) != self.peer_confirm {
                    return Err(PairingError::ConfirmFailed);
                }
                let dh_key = smp_crypto::p256_ecdh(&self.private_key, &peer_x, &peer_y)
                    .ok_or(PairingError::InvalidPublicKey)?;
                let n1 = from_wire::<16>(&self.local_random)?;
                let n2 = from_wire::<16>(&peer_random)?;
                let a1 = self.local_address;
                let a2 = self.peer_address;
                let (mac_key, ltk) = smp_crypto::f5(&dh_key, &n1, &n2, &a1, &a2);
                self.mac_key = Some(mac_key);
                self.ltk = Some(mask_key(ltk, self.key_size));
                self.peer_random = Some(peer_random);
                // Just Works uses all-zero R and IOcap values in the check.
                let check = smp_crypto::f6(
                    &&mac_key,
                    &n1,
                    &n2,
                    &[0; 16],
                    &[self.local.auth_req, 0, self.local.io_capability],
                    &a1,
                    &a2,
                );
                self.phase = Phase::AwaitDhKeyCheck;
                Ok(Action::Send(pdu_with_key(0x0d, &check)))
            }
            Phase::AwaitDhKeyCheck => {
                if pdu.len() != 17 || pdu[0] != 0x0d {
                    return Err(PairingError::InvalidPdu);
                }
                let peer_check = from_wire::<16>(&pdu[1..])?;
                let mac_key = self.mac_key.ok_or(PairingError::InvalidState)?;
                let n1 = from_wire::<16>(&self.local_random)?;
                let n2 = from_wire::<16>(&self.peer_random.ok_or(PairingError::InvalidState)?)?;
                let response = self.response.ok_or(PairingError::InvalidState)?;
                let peer_iocap = [response[3], response[2], response[1]];
                let expected = smp_crypto::f6(
                    &mac_key,
                    &n2,
                    &n1,
                    &[0; 16],
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
            Phase::Complete | Phase::Failed => Err(PairingError::InvalidState),
            _ => Err(PairingError::InvalidState),
        }
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
            local.negotiate_just_works(peer),
            Ok(Negotiated {
                secure_connections: true,
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
            local.negotiate_just_works(features(0x05, 16)),
            Err(PairingError::AuthenticationUnsupported)
        );
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
    }
}
