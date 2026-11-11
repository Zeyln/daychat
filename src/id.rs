//! Daily IDs.
//!
//! A client holds a random local key. For each server it derives a
//! per-server key (so one server can't replay it to another) and sends that
//! in `Hello`. The server computes
//!
//!   id = H("daychat-id-v1" ‖ server_secret ‖ utc_date ‖ client_key)
//!
//! so a client keeps the same ID all day, gets a fresh one at 00:00 UTC, and
//! nobody without the server secret can link IDs across days.

use rand::RngCore;
use sha2::{Digest, Sha256};

/// Crockford base32 alphabet (no i, l, o, u).
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

pub fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// Derive the ID for `client_key` on `day`. Format: `xxxx-xxxx` (40 bits).
pub fn derive_id(server_secret: &[u8; 32], day: &str, client_key: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"daychat-id-v1");
    h.update(server_secret);
    h.update(day.as_bytes());
    h.update(client_key.as_bytes());
    let digest = h.finalize();

    let bits = digest[..5].iter().fold(0u64, |acc, b| (acc << 8) | *b as u64);
    let mut out = String::with_capacity(9);
    for i in 0..8 {
        let idx = (bits >> (35 - 5 * i)) & 31;
        out.push(ALPHABET[idx as usize] as char);
        if i == 3 {
            out.push('-');
        }
    }
    out
}

/// Normalize user-typed IDs: case-insensitive, dash optional, and the
/// usual Crockford look-alikes (i/l → 1, o → 0).
pub fn normalize_id(raw: &str) -> String {
    let chars: Vec<char> = raw
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_lowercase() {
            'i' | 'l' => '1',
            'o' => '0',
            c => c,
        })
        .collect();
    if chars.len() == 8 {
        let (a, b) = chars.split_at(4);
        format!("{}-{}", a.iter().collect::<String>(), b.iter().collect::<String>())
    } else {
        chars.into_iter().collect()
    }
}

/// True for a normalized ID: `xxxx-xxxx` in the Crockford alphabet.
pub fn looks_like_id(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 9 && b.iter().enumerate().all(|(i, c)| if i == 4 { *c == b'-' } else { ALPHABET.contains(c) })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn new_client_key() -> String {
    let mut k = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut k);
    hex(&k)
}

pub fn valid_client_key(k: &str) -> bool {
    k.len() == 64 && k.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Per-server key derived from the client's local key and the server address.
pub fn server_key(local_key: &str, server_addr: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"daychat-server-key-v1");
    h.update(local_key.as_bytes());
    h.update([0u8]);
    h.update(server_addr.to_ascii_lowercase().as_bytes());
    hex(&h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_within_day_and_rotates() {
        let s = [7u8; 32];
        let k = new_client_key();
        let a = derive_id(&s, "2026-11-11", &k);
        assert_eq!(a, derive_id(&s, "2026-11-11", &k));
        assert_ne!(a, derive_id(&s, "2026-11-12", &k));
        assert_eq!(a.len(), 9);
        assert_eq!(normalize_id(&a.to_uppercase().replace('-', "")), a);
        assert!(looks_like_id(&a));
        assert!(looks_like_id(&normalize_id("ABCD 1234")));
        assert!(!looks_like_id("example.com"));
        assert!(!looks_like_id("abcd-123u"));
    }

    #[test]
    fn keys() {
        let k = new_client_key();
        assert!(valid_client_key(&k));
        assert!(valid_client_key(&server_key(&k, "host:1")));
        assert_ne!(server_key(&k, "a:1"), server_key(&k, "b:1"));
        assert!(!valid_client_key("xyz"));
    }
}
