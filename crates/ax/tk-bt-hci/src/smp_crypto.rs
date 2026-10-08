//! Bluetooth LE Security Manager cryptographic primitives.
//!
//! Inputs are byte strings in the order shown by Bluetooth Core Vol 3, Part H
//! Appendix D (most-significant octet first). HCI SMP packet fields are little
//! endian and must be converted at the protocol boundary.

use alloc::vec::Vec;

use aes::Aes128;
use cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use p256::{PublicKey, SecretKey, ecdh::diffie_hellman, elliptic_curve::sec1::ToEncodedPoint};

pub type Key = [u8; 16];

fn aes128(key: &Key, block: &mut [u8; 16]) {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    cipher.encrypt_block(GenericArray::from_mut_slice(block));
}

/// AES-CMAC (RFC 4493), used by the Bluetooth f-functions.
pub fn aes_cmac(key: &Key, message: &[u8]) -> Key {
    let mut zero = [0u8; 16];
    aes128(key, &mut zero);
    let k1 = double_block(zero);
    let k2 = double_block(k1);
    let blocks = message.len().div_ceil(16).max(1);
    let complete = !message.is_empty() && message.len() % 16 == 0;
    let mut last = [0u8; 16];
    let tail = &message[(blocks - 1) * 16..];
    last[..tail.len()].copy_from_slice(tail);
    if complete {
        xor_block(&mut last, &k1);
    } else {
        last[tail.len()] = 0x80;
        xor_block(&mut last, &k2);
    }
    let mut state = [0u8; 16];
    for chunk in message[..(blocks - 1) * 16].chunks_exact(16) {
        xor_block(&mut state, chunk.try_into().expect("CMAC block"));
        aes128(key, &mut state);
    }
    xor_block(&mut state, &last);
    aes128(key, &mut state);
    state
}

fn double_block(mut input: Key) -> Key {
    let carry = input[0] >> 7;
    for i in 0..15 {
        input[i] = (input[i] << 1) | (input[i + 1] >> 7);
    }
    input[15] <<= 1;
    if carry != 0 {
        input[15] ^= 0x87;
    }
    input
}

fn xor_block(dst: &mut Key, src: &Key) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d ^= s;
    }
}

/// LE legacy `e(k, plaintext)` primitive (AES-128 encryption).
pub fn e(key: &Key, plaintext: &Key) -> Key {
    let mut out = *plaintext;
    aes128(key, &mut out);
    out
}

/// LE legacy confirm value: `e(k, e(k, r xor p1) xor p2)`.
pub fn c1(k: &Key, r: &Key, p1: &Key, p2: &Key) -> Key {
    let mut intermediate = *r;
    xor_block(&mut intermediate, p1);
    aes128(k, &mut intermediate);
    xor_block(&mut intermediate, p2);
    aes128(k, &mut intermediate);
    intermediate
}

/// LE legacy short-term key function; `r1` and `r2` are the low 64 bits.
pub fn s1(tk: &Key, r1: &[u8; 8], r2: &[u8; 8]) -> Key {
    let mut input = [0u8; 16];
    input[..8].copy_from_slice(r2);
    input[8..].copy_from_slice(r1);
    e(tk, &input)
}

pub fn f4(u: &[u8; 32], v: &[u8; 32], x: &Key, z: u8) -> Key {
    let mut m = Vec::with_capacity(65);
    m.extend_from_slice(u);
    m.extend_from_slice(v);
    m.push(z);
    aes_cmac(x, &m)
}

pub fn f5(w: &[u8; 32], n1: &Key, n2: &Key, a1: &[u8; 7], a2: &[u8; 7]) -> (Key, Key) {
    const SALT: Key = [
        0x6c, 0x88, 0x83, 0x91, 0xaa, 0xf5, 0xa5, 0x38, 0x60, 0x37, 0x0b, 0xdb, 0x5a, 0x60, 0x83,
        0xbe,
    ];
    let t = aes_cmac(&SALT, w);
    let build = |counter| {
        let mut m = Vec::with_capacity(53);
        m.push(counter);
        m.extend_from_slice(b"btle");
        m.extend_from_slice(n1);
        m.extend_from_slice(n2);
        m.extend_from_slice(a1);
        m.extend_from_slice(a2);
        m.extend_from_slice(&[0x01, 0x00]);
        aes_cmac(&t, &m)
    };
    (build(0), build(1)) // MacKey || LTK
}

pub fn f6(
    w: &Key,
    n1: &Key,
    n2: &Key,
    r: &Key,
    iocap: &[u8; 3],
    a1: &[u8; 7],
    a2: &[u8; 7],
) -> Key {
    let mut m = Vec::with_capacity(65);
    m.extend_from_slice(n1);
    m.extend_from_slice(n2);
    m.extend_from_slice(r);
    m.extend_from_slice(iocap);
    m.extend_from_slice(a1);
    m.extend_from_slice(a2);
    aes_cmac(w, &m)
}

pub fn g2(u: &[u8; 32], v: &[u8; 32], x: &Key, y: &Key) -> u32 {
    let mut m = Vec::with_capacity(80);
    m.extend_from_slice(u);
    m.extend_from_slice(v);
    m.extend_from_slice(y);
    let mac = aes_cmac(x, &m);
    u32::from_be_bytes(mac[12..].try_into().expect("four octets")) % 1_000_000
}

/// LE resolvable private address hash, returning the three hash octets.
pub fn ah(irk: &Key, prand: &[u8; 3]) -> [u8; 3] {
    let mut plaintext = [0u8; 16];
    plaintext[13..].copy_from_slice(prand);
    let encrypted = e(irk, &plaintext);
    encrypted[13..].try_into().expect("three octets")
}

/// Return the P-256 public key as uncompressed x/y coordinates (big endian).
pub fn p256_public(private: &[u8; 32]) -> Option<([u8; 32], [u8; 32])> {
    let secret = SecretKey::from_slice(private).ok()?;
    let point = secret.public_key().to_encoded_point(false);
    Some((
        point.x()?.as_slice().try_into().ok()?,
        point.y()?.as_slice().try_into().ok()?,
    ))
}

/// Compute the P-256 ECDH secret from a private scalar and peer x/y coordinates.
pub fn p256_ecdh(private: &[u8; 32], peer_x: &[u8; 32], peer_y: &[u8; 32]) -> Option<[u8; 32]> {
    let secret = SecretKey::from_slice(private).ok()?;
    let mut encoded = [0u8; 65];
    encoded[0] = 4;
    encoded[1..33].copy_from_slice(peer_x);
    encoded[33..].copy_from_slice(peer_y);
    let peer = PublicKey::from_sec1_bytes(&encoded).ok()?;
    let shared = diffie_hellman(secret.to_nonzero_scalar(), peer.as_affine());
    shared.raw_secret_bytes().as_slice().try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex<const N: usize>(s: &str) -> [u8; N] {
        let mut out = [0; N];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
        }
        out
    }

    #[test]
    fn cmac_rfc4493_and_core_f4_f5_f6_g2_ah_vectors() {
        let key = hex("2b7e151628aed2a6abf7158809cf4f3c");
        assert_eq!(aes_cmac(&key, &[]), hex("bb1d6929e95937287fa37d129b756746"));
        let u = hex("20b003d2f297be2c5e2c83a7e9f9a5b9eff49111acf4fddbcc0301480e359de6");
        let v = hex("55188b3d32f6bb9a900afcfbeed4e72a59cb9ac2f19d7cfb6b4fdd49f47fc5fd");
        let x = hex("d5cb8454d177733effffb2ec712baeab");
        assert_eq!(f4(&u, &v, &x, 0), hex("f2c916f107a9bd1cf1eda1bea974872d"));
        let w = hex("ec0234a357c8ad05341010a60a397d9b99796b13b4f866f1868d34f373bfa698");
        let n1 = x;
        let n2 = hex("a6e8e7cc25a75f6e216583f7ff3dc4cf");
        let a1 = hex("0056123737bfce");
        let a2 = hex("00a713702dcfc1");
        let (mackey, ltk) = f5(&w, &n1, &n2, &a1, &a2);
        assert_eq!(mackey, hex("2965f176a1084a02fd3f6a20ce636e20"));
        assert_eq!(ltk, hex("6986791169d7cd23980522b594750a38"));
        let r = hex("12a3343bb453bb5408da42d20c2d0fc8");
        assert_eq!(
            f6(&mackey, &n1, &n2, &r, &[1, 1, 2], &a1, &a2),
            hex("e3c473989cd0e8c5d26c0b09da958f61")
        );
        // Appendix D's 32-bit value is 0x2f9ed5ba; decimal modulo 10^6 is 938554.
        assert_eq!(g2(&u, &v, &x, &n2), 938554);
        let irk = hex("ec0234a357c8ad05341010a60a397d9b");
        assert_eq!(ah(&irk, &[0x70, 0x81, 0x94]), [0x0d, 0xfb, 0xaa]);
    }

    #[test]
    fn p256_ecdh_is_symmetric_and_rejects_bad_peer_points() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        let (ax, ay) = p256_public(&a).unwrap();
        let (bx, by) = p256_public(&b).unwrap();
        assert_eq!(p256_ecdh(&a, &bx, &by), p256_ecdh(&b, &ax, &ay));
        assert!(p256_ecdh(&a, &[0; 32], &[0; 32]).is_none());
    }
}
