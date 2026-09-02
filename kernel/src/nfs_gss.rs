//! RFC 2203 / RFC 4121 Kerberos RPCSEC_GSS mechanism for NFSv4.1.
//! Imported context keys are split by enctype, direction, and usage; none are
//! printable or reachable through rpc_pipefs.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use aes::{Aes128, Aes256};
use axfs::{GssSequenceWindow, Krb5ImportedContext, NfsError, NfsResult, RpcGssService, RpcsecGss};
use axsync::Mutex;
use cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::{Sha256, Sha384};

const BLOCK: usize = 16;
const TOK_MIC: [u8; 2] = [4, 4];
const TOK_WRAP: [u8; 2] = [5, 4];
const ACCEPTOR: u8 = 1;
const SEALED: u8 = 2;
const ACCEPTOR_SEAL: u32 = 22;
const ACCEPTOR_SIGN: u32 = 23;
const INITIATOR_SEAL: u32 = 24;
const INITIATOR_SIGN: u32 = 25;
#[derive(Clone, Copy)]
enum Hash {
    Sha1,
    Sha256,
    Sha384,
}
#[derive(Clone, Copy)]
enum Enctype {
    Aes128Sha1,
    Aes256Sha1,
    Aes128Sha2,
    Aes256Sha2,
}
impl Enctype {
    fn parse(v: u32) -> NfsResult<Self> {
        match v {
            17 => Ok(Self::Aes128Sha1),
            18 => Ok(Self::Aes256Sha1),
            19 => Ok(Self::Aes128Sha2),
            20 => Ok(Self::Aes256Sha2),
            _ => Err(NfsError::Security),
        }
    }
    fn key_len(self) -> usize {
        match self {
            Self::Aes128Sha1 | Self::Aes128Sha2 => 16,
            _ => 32,
        }
    }
    fn derive_len(self, constant: u8) -> usize {
        if matches!(self, Self::Aes256Sha2) && constant != 0xaa {
            24
        } else {
            self.key_len()
        }
    }
    fn hash(self) -> Hash {
        match self {
            Self::Aes128Sha1 | Self::Aes256Sha1 => Hash::Sha1,
            Self::Aes128Sha2 => Hash::Sha256,
            Self::Aes256Sha2 => Hash::Sha384,
        }
    }
    fn tag_len(self) -> usize {
        match self {
            Self::Aes128Sha1 | Self::Aes256Sha1 => 12,
            Self::Aes256Sha2 => 24,
            _ => 16,
        }
    }
    fn sha2(self) -> bool {
        matches!(self, Self::Aes128Sha2 | Self::Aes256Sha2)
    }
}

fn mac(key: &[u8], hash: Hash, data: &[u8], n: usize) -> NfsResult<Vec<u8>> {
    let mut out = match hash {
        Hash::Sha1 => {
            let mut h = <Hmac<Sha1> as Mac>::new_from_slice(key).map_err(|_| NfsError::Security)?;
            h.update(data);
            h.finalize().into_bytes().to_vec()
        }
        Hash::Sha256 => {
            let mut h =
                <Hmac<Sha256> as Mac>::new_from_slice(key).map_err(|_| NfsError::Security)?;
            h.update(data);
            h.finalize().into_bytes().to_vec()
        }
        Hash::Sha384 => {
            let mut h =
                <Hmac<Sha384> as Mac>::new_from_slice(key).map_err(|_| NfsError::Security)?;
            h.update(data);
            h.finalize().into_bytes().to_vec()
        }
    };
    if n > out.len() {
        return Err(NfsError::Security);
    }
    out.truncate(n);
    Ok(out)
}
fn block(key: &[u8], b: &mut [u8; BLOCK], decrypt: bool) -> NfsResult<()> {
    let b = GenericArray::from_mut_slice(b);
    match key.len() {
        16 => {
            let a = Aes128::new_from_slice(key).map_err(|_| NfsError::Security)?;
            if decrypt {
                a.decrypt_block(b)
            } else {
                a.encrypt_block(b)
            }
        }
        32 => {
            let a = Aes256::new_from_slice(key).map_err(|_| NfsError::Security)?;
            if decrypt {
                a.decrypt_block(b)
            } else {
                a.encrypt_block(b)
            }
        }
        _ => return Err(NfsError::Security),
    }
    Ok(())
}
/// RFC 3961 n-fold.  The use here is fixed to a five-octet usage constant
/// and AES's 128-bit block, but expresses the RFC's rotate/repeat and
/// one's-complement addition directly rather than padding the constant.
fn nfold_128(input: &[u8; 5]) -> [u8; BLOCK] {
    let input_bits = 40usize;
    let output_bits = 128usize;
    let lcm = 640usize;
    let mut sum = [0u8; BLOCK];
    for chunk in 0..lcm / output_bits {
        let mut add = [0u8; BLOCK];
        for bit in 0..output_bits {
            let position = chunk * output_bits + bit;
            let repeat = position / input_bits;
            let offset = position % input_bits;
            let rotation = (13 * repeat) % input_bits;
            let source = (offset + input_bits - rotation) % input_bits;
            let value = (input[source / 8] >> (7 - source % 8)) & 1;
            add[bit / 8] |= value << (7 - bit % 8)
        }
        let mut carry = 0u16;
        for index in (0..BLOCK).rev() {
            let value = sum[index] as u16 + add[index] as u16 + carry;
            sum[index] = value as u8;
            carry = value >> 8
        }
        while carry != 0 {
            for index in (0..BLOCK).rev() {
                let value = sum[index] as u16 + carry;
                sum[index] = value as u8;
                carry = value >> 8
            }
        }
    }
    sum
}
/// RFC 3961 DK for enctypes 17/18: n-fold usage||constant then repeatedly
/// encrypt with the initial all-zero cipher state until the key is filled.
fn dk_3961(key: &[u8], usage: u32, c: u8, n: usize) -> NfsResult<Vec<u8>> {
    let mut constant = [0u8; 5];
    constant[..4].copy_from_slice(&usage.to_be_bytes());
    constant[4] = c;
    let mut b = nfold_128(&constant);
    let mut o = Vec::new();
    o.try_reserve_exact(n).map_err(|_| NfsError::Transport)?;
    while o.len() < n {
        block(key, &mut b, false)?;
        o.extend_from_slice(&b)
    }
    o.truncate(n);
    Ok(o)
}
/// RFC 8009 KDF-HMAC-SHA2.  The five-byte `usage || constant` is the KDF
/// label; this derivation has no context field (`counter || label || 0 || L`).
fn dk_8009(key: &[u8], hash: Hash, usage: u32, c: u8, n: usize) -> NfsResult<Vec<u8>> {
    let mut label = [0u8; 5];
    label[..4].copy_from_slice(&usage.to_be_bytes());
    label[4] = c;
    let bits = (n.checked_mul(8).ok_or(NfsError::Length)? as u32).to_be_bytes();
    let mut o = Vec::new();
    o.try_reserve_exact(n).map_err(|_| NfsError::Transport)?;
    for counter in 1u32.. {
        let part = match hash {
            Hash::Sha256 => {
                let mut h =
                    <Hmac<Sha256> as Mac>::new_from_slice(key).map_err(|_| NfsError::Security)?;
                h.update(&counter.to_be_bytes());
                h.update(&label);
                h.update(&[0]);
                h.update(&bits);
                h.finalize().into_bytes().to_vec()
            }
            Hash::Sha384 => {
                let mut h =
                    <Hmac<Sha384> as Mac>::new_from_slice(key).map_err(|_| NfsError::Security)?;
                h.update(&counter.to_be_bytes());
                h.update(&label);
                h.update(&[0]);
                h.update(&bits);
                h.finalize().into_bytes().to_vec()
            }
            Hash::Sha1 => return Err(NfsError::Security),
        };
        o.extend_from_slice(&part);
        if o.len() >= n {
            o.truncate(n);
            return Ok(o);
        }
    }
    unreachable!()
}
fn cbc_enc(key: &[u8], data: &[u8]) -> NfsResult<Vec<u8>> {
    if data.len() % BLOCK != 0 {
        return Err(NfsError::Length);
    }
    let mut chain = [0; BLOCK];
    let mut o = Vec::new();
    o.try_reserve_exact(data.len())
        .map_err(|_| NfsError::Transport)?;
    for p in data.chunks(BLOCK) {
        let mut b = [0; BLOCK];
        for i in 0..BLOCK {
            b[i] = p[i] ^ chain[i]
        }
        block(key, &mut b, false)?;
        chain = b;
        o.extend_from_slice(&b)
    }
    Ok(o)
}
fn cbc_dec(key: &[u8], data: &[u8]) -> NfsResult<Vec<u8>> {
    if data.len() % BLOCK != 0 {
        return Err(NfsError::Length);
    }
    let mut chain = [0; BLOCK];
    let mut o = Vec::new();
    o.try_reserve_exact(data.len())
        .map_err(|_| NfsError::Transport)?;
    for p in data.chunks(BLOCK) {
        let mut b: [u8; BLOCK] = p.try_into().map_err(|_| NfsError::Security)?;
        let keep = b;
        block(key, &mut b, true)?;
        for i in 0..BLOCK {
            b[i] ^= chain[i]
        }
        chain = keep;
        o.extend_from_slice(&b)
    }
    Ok(o)
}
fn cts_enc(key: &[u8], p: &[u8]) -> NfsResult<Vec<u8>> {
    if p.len() < BLOCK {
        return Err(NfsError::Length);
    }
    let r = p.len() % BLOCK;
    if r == 0 {
        return cbc_enc(key, p);
    }
    let full = p.len() / BLOCK;
    let prefix = (full - 1) * BLOCK;
    let mut o = cbc_enc(key, &p[..prefix])?;
    let prev = if prefix == 0 {
        [0; BLOCK]
    } else {
        o[prefix - BLOCK..prefix]
            .try_into()
            .map_err(|_| NfsError::Security)?
    };
    let mut pen = [0; BLOCK];
    for i in 0..BLOCK {
        pen[i] = p[prefix + i] ^ prev[i]
    }
    block(key, &mut pen, false)?;
    let mut last = [0; BLOCK];
    for i in 0..r {
        last[i] = p[prefix + BLOCK + i] ^ pen[i]
    }
    block(key, &mut last, false)?;
    o.extend_from_slice(&last);
    o.extend_from_slice(&pen[..r]);
    Ok(o)
}
fn cts_dec(key: &[u8], c: &[u8]) -> NfsResult<Vec<u8>> {
    if c.len() < BLOCK {
        return Err(NfsError::Length);
    }
    let r = c.len() % BLOCK;
    if r == 0 {
        return cbc_dec(key, c);
    }
    let full = c.len() / BLOCK;
    let prefix = (full - 1) * BLOCK;
    let mut o = cbc_dec(key, &c[..prefix])?;
    let prev = if prefix == 0 {
        [0; BLOCK]
    } else {
        c[prefix - BLOCK..prefix]
            .try_into()
            .map_err(|_| NfsError::Security)?
    };
    let mut last: [u8; BLOCK] = c[prefix..prefix + BLOCK]
        .try_into()
        .map_err(|_| NfsError::Security)?;
    block(key, &mut last, true)?;
    let mut pen = [0; BLOCK];
    pen[..r].copy_from_slice(&c[prefix + BLOCK..]);
    pen[r..].copy_from_slice(&last[r..]);
    let mut pp = pen;
    block(key, &mut pp, true)?;
    for i in 0..BLOCK {
        pp[i] ^= prev[i]
    }
    o.extend_from_slice(&pp);
    for i in 0..r {
        o.push(last[i] ^ pen[i])
    }
    Ok(o)
}
fn hdr(id: [u8; 2], flags: u8, ec: u16, rrc: u16, seq: u64) -> [u8; 16] {
    let mut h = [0u8; 16];
    h[..2].copy_from_slice(&id);
    h[2] = flags;
    if id == TOK_MIC {
        h[3..8].fill(0xff)
    } else {
        h[3] = 0xff;
        h[4..6].copy_from_slice(&ec.to_be_bytes());
        h[6..8].copy_from_slice(&rrc.to_be_bytes())
    }
    h[8..].copy_from_slice(&seq.to_be_bytes());
    h
}
fn token_fields(v: &[u8], id: [u8; 2], required: u8, peer: bool) -> Option<(u16, u16, u64)> {
    if v.len() != 16 || v[..2] != id {
        return None;
    }
    let filler_ok = if id == TOK_MIC {
        v[3..8] == [0xff; 5]
    } else {
        v[3] == 0xff
    };
    if !filler_ok {
        return None;
    }
    let flags = v[2];
    let allowed = ACCEPTOR | 4 | if id == TOK_WRAP { SEALED } else { 0 };
    if flags & !allowed != 0 || peer != (flags & ACCEPTOR != 0) || flags & 4 != required & 4 {
        return None;
    }
    if id == TOK_MIC && flags & SEALED != 0 {
        return None;
    }
    if id == TOK_WRAP && flags & SEALED != required & SEALED {
        return None;
    }
    let ec = if id == TOK_MIC {
        0
    } else {
        u16::from_be_bytes(v[4..6].try_into().ok()?)
    };
    let rrc = if id == TOK_MIC {
        0
    } else {
        u16::from_be_bytes(v[6..8].try_into().ok()?)
    };
    let seq = u64::from_be_bytes(v[8..].try_into().ok()?);
    Some((ec, rrc, seq))
}
fn rotate_left(input: &[u8], count: usize) -> Vec<u8> {
    if input.is_empty() {
        return Vec::new();
    }
    let n = count % input.len();
    let mut out = Vec::with_capacity(input.len());
    out.extend_from_slice(&input[n..]);
    out.extend_from_slice(&input[..n]);
    out
}
fn equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= x ^ y
    }
    d == 0
}
fn opaque(v: &[u8]) -> NfsResult<Vec<u8>> {
    let n = u32::try_from(v.len()).map_err(|_| NfsError::Length)?;
    let mut o = Vec::new();
    let pad = (4 - v.len() % 4) % 4;
    o.try_reserve_exact(4 + v.len() + pad)
        .map_err(|_| NfsError::Transport)?;
    o.extend_from_slice(&n.to_be_bytes());
    o.extend_from_slice(v);
    o.resize(o.len() + pad, 0);
    Ok(o)
}
fn take_opaque_at<'a>(v: &'a [u8], at: &mut usize) -> NfsResult<&'a [u8]> {
    let start = *at;
    let length = u32::from_be_bytes(
        v.get(start..start + 4)
            .ok_or(NfsError::Malformed)?
            .try_into()
            .map_err(|_| NfsError::Malformed)?,
    ) as usize;
    let data = start.checked_add(4).ok_or(NfsError::Length)?;
    let end = data.checked_add(length).ok_or(NfsError::Length)?;
    let padded = (end + 3) & !3;
    if padded > v.len() || v[end..padded].iter().any(|x| *x != 0) {
        return Err(NfsError::Malformed);
    }
    *at = padded;
    v.get(data..end).ok_or(NfsError::Malformed)
}
fn deopaque(v: &[u8]) -> NfsResult<&[u8]> {
    let mut at = 0;
    let result = take_opaque_at(v, &mut at)?;
    if at != v.len() {
        return Err(NfsError::Malformed);
    }
    Ok(result)
}

struct Keys {
    kc: Vec<u8>,
    ke: Vec<u8>,
    ki: Vec<u8>,
}
struct SecretWire(Vec<u8>);
impl Drop for SecretWire {
    fn drop(&mut self) {
        self.0.fill(0)
    }
}
impl Drop for Keys {
    fn drop(&mut self) {
        self.kc.fill(0);
        self.ke.fill(0);
        self.ki.fill(0)
    }
}
impl Keys {
    fn derive(e: Enctype, base: &[u8], usage: u32) -> NfsResult<Self> {
        let f = |c| {
            if e.sha2() {
                dk_8009(base, e.hash(), usage, c, e.derive_len(c))
            } else {
                dk_3961(base, usage, c, e.key_len())
            }
        };
        Ok(Self {
            kc: f(0x99)?,
            ke: f(0xaa)?,
            ki: f(0x55)?,
        })
    }
}
struct Gss64Receive {
    highest: u64,
    seen: u64,
    initialized: bool,
}
/// RFC 4121 sequence state is mechanism-local and 64-bit.  Replay is only
/// committed after a token's checksum, encryption, and inner header verify.
struct Gss64Window {
    send: AtomicU64,
    receive: Mutex<Gss64Receive>,
    width: u32,
}
impl Gss64Window {
    fn new(initial: u64, width: u32) -> NfsResult<Self> {
        if width == 0 || width > 64 {
            return Err(NfsError::Security);
        }
        Ok(Self {
            send: AtomicU64::new(initial.max(1)),
            receive: Mutex::new(Gss64Receive {
                highest: 0,
                seen: 0,
                initialized: false,
            }),
            width,
        })
    }
    fn next(&self) -> NfsResult<u64> {
        self.send
            .try_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_add(1))
            .map_err(|_| NfsError::Security)
    }
    fn admit(&self, seq: u64) -> NfsResult<bool> {
        let mut r = self.receive.lock();
        if !r.initialized {
            r.highest = seq;
            r.seen = 1;
            r.initialized = true;
            return Ok(false);
        }
        if seq > r.highest {
            let delta = seq - r.highest;
            r.seen = if delta >= 64 {
                1
            } else {
                (r.seen << delta) | 1
            };
            r.highest = seq;
            return Ok(false);
        }
        let delta = r.highest - seq;
        if delta >= self.width as u64 || delta >= 64 {
            return Err(NfsError::Security);
        }
        if r.seen & (1 << delta) != 0 {
            return Ok(true);
        }
        r.seen |= 1 << delta;
        Ok(false)
    }
    fn accept(&self, seq: u64) -> NfsResult<()> {
        if self.admit(seq)? {
            return Err(NfsError::Security);
        }
        Ok(())
    }
}
pub(crate) struct Krb5Gss {
    service: RpcGssService,
    wire: Vec<u8>,
    enctype: Enctype,
    send_sign: Keys,
    recv_sign: Keys,
    send_seal: Keys,
    recv_seal: Keys,
    send_window: GssSequenceWindow,
    fore_reply_window: GssSequenceWindow,
    mechanism: Gss64Window,
    acceptor_subkey: bool,
    expiry: u32,
}
impl Drop for Krb5Gss {
    fn drop(&mut self) {
        for s in [
            &mut self.wire,
            &mut self.send_sign.kc,
            &mut self.send_sign.ke,
            &mut self.send_sign.ki,
            &mut self.recv_sign.kc,
            &mut self.recv_sign.ke,
            &mut self.recv_sign.ki,
            &mut self.send_seal.kc,
            &mut self.send_seal.ke,
            &mut self.send_seal.ki,
            &mut self.recv_seal.kc,
            &mut self.recv_seal.ke,
            &mut self.recv_seal.ki,
        ] {
            s.fill(0)
        }
    }
}
impl Krb5Gss {
    pub(crate) fn import(
        c: Krb5ImportedContext,
        wire: Vec<u8>,
        service: RpcGssService,
        timeout_seconds: u32,
        window_size: u32,
    ) -> NfsResult<Self> {
        let mut wire = SecretWire(wire);
        let flags = c.mechanism_flags();
        if flags & 1 == 0 || timeout_seconds == 0 {
            return Err(NfsError::Security);
        }
        let e = Enctype::parse(c.enctype())?;
        let now = crate::time::wall_time().as_secs();
        let expiry = c.expiry().min(
            now.saturating_add(timeout_seconds as u64)
                .min(u32::MAX as u64) as u32,
        );
        if now >= expiry as u64 {
            return Err(NfsError::Security);
        }
        let mechanism_seq = c.initial_mechanism_sequence();
        let mut key = c.into_session_key();
        let result = Self::from_key(
            e,
            &mut wire,
            service,
            mechanism_seq,
            expiry,
            flags & 4 != 0,
            window_size,
            &key,
        );
        key.fill(0);
        result
    }
    fn from_key(
        e: Enctype,
        wire: &mut SecretWire,
        service: RpcGssService,
        mechanism_seq: u64,
        expiry: u32,
        acceptor_subkey: bool,
        window_size: u32,
        key: &[u8],
    ) -> NfsResult<Self> {
        let send_sign = Keys::derive(e, key, INITIATOR_SIGN)?;
        let recv_sign = Keys::derive(e, key, ACCEPTOR_SIGN)?;
        let send_seal = Keys::derive(e, key, INITIATOR_SEAL)?;
        let recv_seal = Keys::derive(e, key, ACCEPTOR_SEAL)?;
        let send_window = GssSequenceWindow::new(1, window_size)?;
        let fore_reply_window = GssSequenceWindow::new(1, window_size)?;
        let mechanism = Gss64Window::new(mechanism_seq, window_size)?;
        Ok(Self {
            service,
            wire: core::mem::take(&mut wire.0),
            enctype: e,
            send_sign,
            recv_sign,
            send_seal,
            recv_seal,
            send_window,
            fore_reply_window,
            mechanism,
            acceptor_subkey,
            expiry,
        })
    }
    fn live(&self) -> NfsResult<()> {
        if crate::time::wall_time().as_secs() >= self.expiry as u64 {
            Err(NfsError::Security)
        } else {
            Ok(())
        }
    }
    fn sum(&self, k: &Keys, data: &[u8], encrypted: bool) -> NfsResult<Vec<u8>> {
        if encrypted && self.enctype.sha2() {
            let mut state_and_cipher = Vec::new();
            state_and_cipher
                .try_reserve_exact(BLOCK + data.len())
                .map_err(|_| NfsError::Transport)?;
            state_and_cipher.resize(BLOCK, 0);
            state_and_cipher.extend_from_slice(data);
            mac(
                &k.ki,
                self.enctype.hash(),
                &state_and_cipher,
                self.enctype.tag_len(),
            )
        } else if encrypted {
            mac(&k.ki, self.enctype.hash(), data, self.enctype.tag_len())
        } else {
            mac(&k.kc, self.enctype.hash(), data, self.enctype.tag_len())
        }
    }
    fn flags(&self, acceptor: bool, sealed: bool) -> u8 {
        (if acceptor { ACCEPTOR } else { 0 })
            | (if sealed { SEALED } else { 0 })
            | (if self.acceptor_subkey { 4 } else { 0 })
    }
    fn mic(&self, k: &Keys, acceptor: bool, data: &[u8]) -> NfsResult<Vec<u8>> {
        let seq = self.mechanism.next()?;
        let h = hdr(TOK_MIC, self.flags(acceptor, false), 0, 0, seq);
        let mut signed = Vec::new();
        signed
            .try_reserve_exact(data.len() + 16)
            .map_err(|_| NfsError::Transport)?;
        signed.extend_from_slice(data);
        signed.extend_from_slice(&h);
        let tag = self.sum(k, &signed, false)?;
        signed.clear();
        signed.extend_from_slice(&h);
        signed.extend_from_slice(&tag);
        Ok(signed)
    }
    fn verify_mic_admit(&self, k: &Keys, data: &[u8], token: &[u8]) -> NfsResult<bool> {
        if token.len() != 16 + self.enctype.tag_len() {
            return Err(NfsError::Security);
        }
        let required = if self.acceptor_subkey { 4 } else { 0 };
        let Some((_ec, _rrc, mechanism_seq)) = token_fields(&token[..16], TOK_MIC, required, true)
        else {
            return Err(NfsError::Security);
        };
        let mut signed = Vec::new();
        signed
            .try_reserve_exact(data.len() + 16)
            .map_err(|_| NfsError::Transport)?;
        signed.extend_from_slice(data);
        signed.extend_from_slice(&token[..16]);
        let tag = self.sum(k, &signed, false)?;
        if !equal(&tag, &token[16..]) {
            return Err(NfsError::Security);
        }
        self.mechanism.admit(mechanism_seq)
    }
    fn verify_mic_untracked(&self, k: &Keys, data: &[u8], token: &[u8]) -> NfsResult<()> {
        if token.len() != 16 + self.enctype.tag_len() {
            return Err(NfsError::Security);
        }
        let required = if self.acceptor_subkey { 4 } else { 0 };
        let Some((_ec, _rrc, _mechanism_seq)) = token_fields(&token[..16], TOK_MIC, required, true)
        else {
            return Err(NfsError::Security);
        };
        let mut signed = Vec::new();
        signed
            .try_reserve_exact(data.len() + 16)
            .map_err(|_| NfsError::Transport)?;
        signed.extend_from_slice(data);
        signed.extend_from_slice(&token[..16]);
        let tag = self.sum(k, &signed, false)?;
        if !equal(&tag, &token[16..]) {
            return Err(NfsError::Security);
        }
        Ok(())
    }
    fn verify_mic(&self, k: &Keys, data: &[u8], token: &[u8]) -> NfsResult<()> {
        if self.verify_mic_admit(k, data, token)? {
            return Err(NfsError::Security);
        }
        Ok(())
    }
}
impl RpcsecGss for Krb5Gss {
    fn sequence(&self) -> NfsResult<u32> {
        self.live()?;
        self.send_window.allocate()
    }
    fn credential(&self, _: u32, seq: u32) -> NfsResult<Vec<u8>> {
        self.live()?;
        let mut v = Vec::new();
        v.try_reserve_exact(20 + self.wire.len())
            .map_err(|_| NfsError::Transport)?;
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(&seq.to_be_bytes());
        v.extend_from_slice(
            &(match self.service {
                RpcGssService::None => 1,
                RpcGssService::Integrity => 2,
                RpcGssService::Privacy => 3,
            } as u32)
                .to_be_bytes(),
        );
        v.extend_from_slice(&(self.wire.len() as u32).to_be_bytes());
        v.extend_from_slice(&self.wire);
        Ok(v)
    }
    fn verifier(&self, call_through_credential: &[u8]) -> NfsResult<Vec<u8>> {
        self.live()?;
        self.mic(&self.send_sign, false, call_through_credential)
    }
    fn verify_reply(&self, seq: u32, v: &[u8]) -> NfsResult<()> {
        self.live()?;
        self.verify_mic(&self.recv_sign, &seq.to_be_bytes(), v)?;
        self.fore_reply_window.accept(seq)
    }
    fn wrap(&self, seq: u32, data: &[u8]) -> NfsResult<Vec<u8>> {
        self.live()?;
        match self.service {
            RpcGssService::None => Ok(data.to_vec()),
            RpcGssService::Integrity => {
                let token = self.mic(&self.send_sign, false, data)?;
                let body = opaque(data)?;
                let checksum = opaque(&token)?;
                let mut out = Vec::new();
                out.try_reserve_exact(4 + body.len() + checksum.len())
                    .map_err(|_| NfsError::Transport)?;
                out.extend_from_slice(&seq.to_be_bytes());
                out.extend_from_slice(&body);
                out.extend_from_slice(&checksum);
                Ok(out)
            }
            RpcGssService::Privacy => {
                let mechanism_seq = self.mechanism.next()?;
                let h = hdr(TOK_WRAP, self.flags(false, true), 0, 0, mechanism_seq);
                let mut p = Vec::new();
                p.try_reserve_exact(BLOCK + 4 + data.len() + 16 + self.enctype.tag_len())
                    .map_err(|_| NfsError::Transport)?;
                let mut conf = [0; BLOCK];
                crate::random::fill_secure(&mut conf).map_err(|_| NfsError::Security)?;
                p.extend_from_slice(&conf);
                p.extend_from_slice(&seq.to_be_bytes());
                p.extend_from_slice(data);
                p.extend_from_slice(&h);
                if self.enctype.sha2() {
                    let crypt = cts_enc(&self.send_seal.ke, &p)?;
                    let tag = self.sum(&self.send_seal, &crypt, true)?;
                    let mut token = Vec::new();
                    token
                        .try_reserve_exact(16 + crypt.len() + tag.len())
                        .map_err(|_| NfsError::Transport)?;
                    token.extend_from_slice(&h);
                    token.extend_from_slice(&crypt);
                    token.extend_from_slice(&tag);
                    opaque(&token)
                } else {
                    let tag = self.sum(&self.send_seal, &p, true)?;
                    p.extend_from_slice(&tag);
                    let crypt = cts_enc(&self.send_seal.ke, &p)?;
                    let mut token = h.to_vec();
                    token.extend_from_slice(&crypt);
                    opaque(&token)
                }
            }
        }
    }
    fn unwrap(&self, seq: u32, wire: &[u8]) -> NfsResult<Vec<u8>> {
        self.live()?;
        match self.service {
            RpcGssService::None => Ok(wire.to_vec()),
            RpcGssService::Integrity => {
                let rpc_seq = u32::from_be_bytes(
                    wire.get(..4)
                        .ok_or(NfsError::Malformed)?
                        .try_into()
                        .map_err(|_| NfsError::Malformed)?,
                );
                let mut at = 4;
                let data = take_opaque_at(wire, &mut at)?;
                let checksum = take_opaque_at(wire, &mut at)?;
                if at != wire.len() || rpc_seq != seq {
                    return Err(NfsError::Security);
                }
                self.verify_mic(&self.recv_sign, data, checksum)?;
                Ok(data.to_vec())
            }
            RpcGssService::Privacy => {
                let t = deopaque(wire)?;
                let min = 16 + BLOCK + 4 + 16 + self.enctype.tag_len();
                let required = SEALED | (if self.acceptor_subkey { 4 } else { 0 });
                if t.len() < min {
                    return Err(NfsError::Security);
                }
                let Some((ec, rrc, mechanism_seq)) =
                    token_fields(&t[..16], TOK_WRAP, required, true)
                else {
                    return Err(NfsError::Security);
                };
                let protected = rotate_left(&t[16..], rrc as usize);
                let (p, hat) = if self.enctype.sha2() {
                    let at = protected
                        .len()
                        .checked_sub(self.enctype.tag_len())
                        .ok_or(NfsError::Security)?;
                    if !equal(
                        &self.sum(&self.recv_seal, &protected[..at], true)?,
                        &protected[at..],
                    ) {
                        return Err(NfsError::Security);
                    }
                    let p = cts_dec(&self.recv_seal.ke, &protected[..at])?;
                    let hat = p.len().checked_sub(16).ok_or(NfsError::Security)?;
                    (p, hat)
                } else {
                    let p = cts_dec(&self.recv_seal.ke, &protected)?;
                    let tag_at = p
                        .len()
                        .checked_sub(self.enctype.tag_len())
                        .ok_or(NfsError::Security)?;
                    if !equal(
                        &self.sum(&self.recv_seal, &p[..tag_at], true)?,
                        &p[tag_at..],
                    ) {
                        return Err(NfsError::Security);
                    }
                    let hat = tag_at.checked_sub(16).ok_or(NfsError::Security)?;
                    (p, hat)
                };
                let padding = ec as usize;
                if padding > hat.saturating_sub(BLOCK + 4) {
                    return Err(NfsError::Security);
                }
                let data_end = hat - padding;
                let mut inner = [0u8; 16];
                inner.copy_from_slice(&t[..16]);
                inner[6..8].fill(0);
                if p.len() < BLOCK + 4 + 16
                    || &p[hat..hat + 16] != inner
                    || p[BLOCK..BLOCK + 4] != seq.to_be_bytes()
                {
                    return Err(NfsError::Security);
                }
                self.mechanism.accept(mechanism_seq)?;
                Ok(p[BLOCK + 4..data_end].to_vec())
            }
        }
    }
    fn service(&self) -> RpcGssService {
        self.service
    }
    fn verify_callback(&self, _seq: u32, call: &[u8], verifier: &[u8]) -> NfsResult<()> {
        self.live()?;
        self.verify_mic_untracked(&self.recv_sign, call, verifier)
    }
    fn unwrap_callback(&self, seq: u32, wire: &[u8]) -> NfsResult<Vec<u8>> {
        self.live()?;
        match self.service {
            RpcGssService::None => Ok(wire.to_vec()),
            RpcGssService::Integrity => {
                let rpc_seq = u32::from_be_bytes(
                    wire.get(..4)
                        .ok_or(NfsError::Malformed)?
                        .try_into()
                        .map_err(|_| NfsError::Malformed)?,
                );
                let mut at = 4;
                let data = take_opaque_at(wire, &mut at)?;
                let checksum = take_opaque_at(wire, &mut at)?;
                if at != wire.len() || rpc_seq != seq {
                    return Err(NfsError::Security);
                }
                self.verify_mic_untracked(&self.recv_sign, data, checksum)?;
                Ok(data.to_vec())
            }
            RpcGssService::Privacy => {
                let t = deopaque(wire)?;
                let min = 16 + BLOCK + 4 + 16 + self.enctype.tag_len();
                let required = SEALED | (if self.acceptor_subkey { 4 } else { 0 });
                if t.len() < min {
                    return Err(NfsError::Security);
                }
                let Some((ec, rrc, _mechanism_seq)) =
                    token_fields(&t[..16], TOK_WRAP, required, true)
                else {
                    return Err(NfsError::Security);
                };
                let protected = rotate_left(&t[16..], rrc as usize);
                let (p, hat) = if self.enctype.sha2() {
                    let at = protected
                        .len()
                        .checked_sub(self.enctype.tag_len())
                        .ok_or(NfsError::Security)?;
                    if !equal(
                        &self.sum(&self.recv_seal, &protected[..at], true)?,
                        &protected[at..],
                    ) {
                        return Err(NfsError::Security);
                    }
                    let p = cts_dec(&self.recv_seal.ke, &protected[..at])?;
                    let hat = p.len().checked_sub(16).ok_or(NfsError::Security)?;
                    (p, hat)
                } else {
                    let p = cts_dec(&self.recv_seal.ke, &protected)?;
                    let tag_at = p
                        .len()
                        .checked_sub(self.enctype.tag_len())
                        .ok_or(NfsError::Security)?;
                    if !equal(
                        &self.sum(&self.recv_seal, &p[..tag_at], true)?,
                        &p[tag_at..],
                    ) {
                        return Err(NfsError::Security);
                    }
                    let hat = tag_at.checked_sub(16).ok_or(NfsError::Security)?;
                    (p, hat)
                };
                let padding = ec as usize;
                if padding > hat.saturating_sub(BLOCK + 4) {
                    return Err(NfsError::Security);
                }
                let data_end = hat - padding;
                let mut inner = [0u8; 16];
                inner.copy_from_slice(&t[..16]);
                inner[6..8].fill(0);
                if p.len() < BLOCK + 4 + 16
                    || &p[hat..hat + 16] != inner
                    || p[BLOCK..BLOCK + 4] != seq.to_be_bytes()
                {
                    return Err(NfsError::Security);
                }
                Ok(p[BLOCK + 4..data_end].to_vec())
            }
        }
    }
    fn wrap_callback_reply(&self, seq: u32, bytes: &[u8]) -> NfsResult<Vec<u8>> {
        self.wrap(seq, bytes)
    }
    fn callback_verifier(&self, seq: u32, _reply: &[u8]) -> NfsResult<Vec<u8>> {
        self.live()?;
        self.mic(&self.send_sign, false, &seq.to_be_bytes())
    }
    fn context_handle(&self) -> &[u8] {
        &self.wire
    }
}
