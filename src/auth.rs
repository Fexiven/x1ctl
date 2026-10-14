//! Text-mode challenge/response used to unlock the CDC control channel.
//!
//! The scheme and the static key material are the same as on the Katana V2/V2X;
//! this module is adapted from the MIT-licensed `v2x` crate (`src/auth.rs`,
//! © Rasmus Moorats). The X1 sends header `1E 04` and PID bytes `86 32`.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};

use crate::error::{Error, Result};

/// Static bytes shipped in Creative's `CTCDC.dll`.
const KEY_DATA: [u8; 28] = [
    0xD3, 0x1A, 0x21, 0x27, 0x9B, 0xE3, 0x46, 0xF0, 0x99, 0x9D, 0x6E, 0xC4, 0xC3, 0xFE, 0xBE, 0x98,
    0x90, 0x18, 0x69, 0xC1, 0x18, 0xFB, 0xB1, 0x25, 0x6E, 0x0C, 0xE0, 0x7B,
];

pub const WHOAREYOU: &[u8] = b"whoareyou";
/// "whoareyou" + header(2) + pid(2) + nonce(32)
pub const CHALLENGE_LEN: usize = 9 + 4 + 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub header: [u8; 2],
    pub pid: [u8; 2],
    pub nonce: [u8; 32],
}

impl Challenge {
    pub fn pid(&self) -> u16 {
        u16::from_le_bytes(self.pid)
    }
}

/// `[header(2)] [static(28)] [pid(2)]`
pub fn build_key(header: [u8; 2], pid: [u8; 2]) -> [u8; 32] {
    let mut key = [0u8; 32];
    key[0..2].copy_from_slice(&header);
    key[2..30].copy_from_slice(&KEY_DATA);
    key[30..32].copy_from_slice(&pid);
    key
}

/// Parse `"whoareyou" h0 h1 p0 p1 nonce[32]` (trailing CRLF optional).
/// The nonce is binary and may itself contain `\r\n`, so this works on length.
pub fn parse_challenge(data: &[u8]) -> Result<Challenge> {
    let start = find(data, WHOAREYOU)
        .ok_or_else(|| Error::Auth("challenge does not contain 'whoareyou'".into()))?;
    let d = &data[start..];
    if d.len() < CHALLENGE_LEN {
        return Err(Error::Auth(format!(
            "challenge too short: {} bytes (need {CHALLENGE_LEN})",
            d.len()
        )));
    }
    let a = &d[WHOAREYOU.len()..];
    let mut c = Challenge {
        header: [a[0], a[1]],
        pid: [a[2], a[3]],
        nonce: [0; 32],
    };
    c.nonce.copy_from_slice(&a[4..36]);
    Ok(c)
}

/// 64-byte response: `[IV(16, first 12 used as GCM nonce)] [ciphertext(32)] [tag(16)]`.
pub fn compute_response(c: &Challenge) -> Result<[u8; 64]> {
    let mut iv = [0u8; 16];
    getrandom::getrandom(&mut iv).map_err(|e| Error::Auth(format!("rng: {e}")))?;
    compute_response_with_iv(c, iv)
}

pub fn compute_response_with_iv(c: &Challenge, iv: [u8; 16]) -> Result<[u8; 64]> {
    let key = build_key(c.header, c.pid);
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|e| Error::Auth(format!("cipher init: {e}")))?;
    let ct = cipher
        .encrypt(Nonce::from_slice(&iv[..12]), c.nonce.as_ref())
        .map_err(|e| Error::Auth(format!("encrypt: {e}")))?;
    let mut out = [0u8; 64];
    out[..16].copy_from_slice(&iv);
    out[16..].copy_from_slice(&ct);
    Ok(out)
}

/// Verify a response (used by tests and the device simulator).
pub fn verify_response(c: &Challenge, resp: &[u8; 64]) -> bool {
    let key = build_key(c.header, c.pid);
    let Ok(cipher) = Aes256Gcm::new_from_slice(&key) else {
        return false;
    };
    cipher
        .decrypt(Nonce::from_slice(&resp[..12]), &resp[16..])
        .map(|p| p == c.nonce)
        .unwrap_or(false)
}

pub(crate) fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Known Katana V2X pair from the v2x test suite: proves the key schedule.
    #[test]
    fn known_pair_decrypts() {
        let key = build_key([0x1E, 0x04], [0x83, 0x32]);
        let nonce = hex("490e1ebfc0d2d397040ea4d5b4d1d3d351895194a4b4b55f51a98624b49786a6");
        let resp = hex(
            "c9c930342e1cf00f935360bfbb6d92e51c0e185b3559828f9c393ae8c99d34b0ab40ff92ab656f5e816374c41f92ff230a585c813880eb3bd971cbd035d9b9c2",
        );
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let pt = cipher
            .decrypt(Nonce::from_slice(&resp[..12]), &resp[16..64])
            .unwrap();
        assert_eq!(pt, nonce);
    }

    #[test]
    fn x1_challenge_roundtrip_with_crlf_in_nonce() {
        let mut raw = b"whoareyou".to_vec();
        raw.extend_from_slice(&[0x1E, 0x04, 0x86, 0x32]);
        let mut nonce = [0x41u8; 32];
        nonce[5] = b'\r';
        nonce[6] = b'\n';
        raw.extend_from_slice(&nonce);
        raw.extend_from_slice(b"\r\n");
        let c = parse_challenge(&raw).unwrap();
        assert_eq!(c.pid(), 0x3286);
        assert_eq!(c.nonce, nonce);
        let r = compute_response(&c).unwrap();
        assert!(verify_response(&c, &r));
    }
}
