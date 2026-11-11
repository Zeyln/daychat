//! Authentication and end-to-end encryption.
//!
//! - **Login:** the client proves it owns an Ed25519 key by signing a
//!   fresh server challenge. Nothing secret crosses the wire, so an
//!   eavesdropper can't replay it to impersonate the user.
//! - **Private conversations:** each side makes a one-off X25519 key when
//!   connecting. The server only relays the public halves. Messages are
//!   sealed with XChaCha20-Poly1305 under a key derived with HKDF-SHA256.
//!   A short safety code lets two people confirm, out of band, that no
//!   server sat in the middle of the exchange.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

const IDENTITY_CONTEXT: &[u8] = b"daychat-identity-v1";
const AUTH_CONTEXT: &[u8] = b"daychat-auth-v1";
const E2E_CONTEXT: &[u8] = b"daychat-e2e-v1";
const SAFETY_CONTEXT: &[u8] = b"daychat-safety-v1";
const NONCE_LEN: usize = 24;

/// Upper bound for a sealed private message on the wire (base64). Fits
/// 280 four-byte chars plus nonce and tag with room to spare.
pub const MAX_SEALED_LEN: usize = 2048;

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

fn decode32(s: &str) -> Option<[u8; 32]> {
    B64.decode(s).ok()?.try_into().ok()
}

// --- Login -----------------------------------------------------------------------

/// A client's signing identity on one server.
pub struct Identity(SigningKey);

impl Identity {
    /// Derive the identity for `server_addr` from the client's local key,
    /// so the same person is unlinkable across servers.
    pub fn for_server(local_key: &str, server_addr: &str) -> Self {
        let mut h = Sha256::new();
        h.update(IDENTITY_CONTEXT);
        h.update(local_key.as_bytes());
        h.update([0u8]);
        h.update(server_addr.to_ascii_lowercase().as_bytes());
        let seed: [u8; 32] = h.finalize().into();
        Self(SigningKey::from_bytes(&seed))
    }

    /// Public key, as sent in `hello`.
    pub fn public(&self) -> String {
        B64.encode(self.0.verifying_key().to_bytes())
    }

    /// Signature over the server's challenge, as sent in `auth`.
    pub fn answer(&self, challenge: &str) -> String {
        B64.encode(self.0.sign(&auth_message(challenge)).to_bytes())
    }
}

fn auth_message(challenge: &str) -> Vec<u8> {
    [AUTH_CONTEXT, challenge.as_bytes()].concat()
}

pub fn new_challenge() -> String {
    B64.encode(random::<32>())
}

/// Check a login public key and return its canonical encoding.
pub fn parse_public(pubkey: &str) -> Option<String> {
    let bytes = decode32(pubkey)?;
    VerifyingKey::from_bytes(&bytes).ok()?;
    Some(B64.encode(bytes))
}

pub fn verify_answer(pubkey: &str, challenge: &str, answer: &str) -> bool {
    let Some(pk) = decode32(pubkey) else { return false };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else { return false };
    let Ok(sig) = B64.decode(answer) else { return false };
    let Ok(sig) = <[u8; 64]>::try_from(sig.as_slice()) else { return false };
    vk.verify_strict(&auth_message(challenge), &Signature::from_bytes(&sig)).is_ok()
}

// --- Private conversations -----------------------------------------------------

/// True if `s` could be an exchange key (the server only checks the shape).
pub fn is_exchange_key(s: &str) -> bool {
    decode32(s).is_some()
}

/// Our half of a key exchange, kept until the peer's half arrives.
pub struct KeyOffer {
    secret: StaticSecret,
    public: [u8; 32],
}

impl Default for KeyOffer {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyOffer {
    pub fn new() -> Self {
        let secret = StaticSecret::from(random::<32>());
        let public = PublicKey::from(&secret).to_bytes();
        Self { secret, public }
    }

    /// Our public half, sent with `connect`.
    pub fn public(&self) -> String {
        B64.encode(self.public)
    }

    /// Combine with the peer's public half. `None` if it is malformed or
    /// degenerate (e.g. a low-order point chosen to force a known key).
    pub fn finish(self, theirs: &str) -> Option<Session> {
        let theirs = decode32(theirs)?;
        if theirs == self.public {
            return None;
        }
        let shared = self.secret.diffie_hellman(&PublicKey::from(theirs));
        if !shared.was_contributory() {
            return None;
        }
        let (lo, hi) = if self.public < theirs { (self.public, theirs) } else { (theirs, self.public) };

        let info = [E2E_CONTEXT, &lo[..], &hi[..]].concat();
        let mut key = [0u8; 32];
        Hkdf::<Sha256>::new(None, shared.as_bytes()).expand(&info, &mut key).ok()?;
        let cipher = XChaCha20Poly1305::new_from_slice(&key).ok()?;

        let code = Sha256::new()
            .chain_update(SAFETY_CONTEXT)
            .chain_update(shared.as_bytes())
            .chain_update(lo)
            .chain_update(hi)
            .finalize();
        let safety_code = code[..10]
            .chunks(2)
            .map(|c| format!("{:02x}{:02x}", c[0], c[1]))
            .collect::<Vec<_>>()
            .join(" ");

        Some(Session { cipher, mine: self.public, theirs, safety_code })
    }
}

/// An established end-to-end encrypted conversation with one peer.
pub struct Session {
    cipher: XChaCha20Poly1305,
    mine: [u8; 32],
    theirs: [u8; 32],
    /// Same on both sides only if nobody intercepted the key exchange.
    pub safety_code: String,
}

impl Session {
    /// Encrypt a message. The sender's key is bound in as associated data,
    /// so a server can't reflect our own messages back at us as theirs.
    pub fn seal(&self, text: &str) -> String {
        let nonce = random::<NONCE_LEN>();
        let ct = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: text.as_bytes(), aad: &self.mine })
            .expect("XChaCha20-Poly1305 encryption cannot fail");
        B64.encode([&nonce[..], &ct[..]].concat())
    }

    /// Decrypt a message from the peer; `None` if it was tampered with,
    /// not from them, or not valid UTF-8.
    pub fn open(&self, sealed: &str) -> Option<String> {
        let raw = B64.decode(sealed).ok()?;
        if raw.len() < NONCE_LEN {
            return None;
        }
        let (nonce, ct) = raw.split_at(NONCE_LEN);
        let pt = self
            .cipher
            .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad: &self.theirs })
            .ok()?;
        String::from_utf8(pt).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_signature() {
        let me = Identity::for_server(&"ab".repeat(32), "host:7878");
        let pk = parse_public(&me.public()).unwrap();
        let ch = new_challenge();
        assert!(verify_answer(&pk, &ch, &me.answer(&ch)));
        assert!(!verify_answer(&pk, &new_challenge(), &me.answer(&ch)), "replayed answer");
        let other = Identity::for_server(&"ab".repeat(32), "other:7878");
        assert_ne!(other.public(), me.public(), "unlinkable across servers");
        assert!(!verify_answer(&pk, &ch, &other.answer(&ch)));
    }

    #[test]
    fn e2e_roundtrip() {
        let (a, b) = (KeyOffer::new(), KeyOffer::new());
        let (pa, pb) = (a.public(), b.public());
        let sa = a.finish(&pb).unwrap();
        let sb = b.finish(&pa).unwrap();
        assert_eq!(sa.safety_code, sb.safety_code);

        let msg = sa.seal("hej 👋");
        assert_eq!(sb.open(&msg).as_deref(), Some("hej 👋"));
        assert!(sa.open(&msg).is_none(), "own message reflected back");

        let mut tampered = B64.decode(&msg).unwrap();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(sb.open(&B64.encode(tampered)).is_none());

        // A man in the middle ends up with different keys and safety codes.
        let (m1, m2) = (KeyOffer::new(), KeyOffer::new());
        let (pm1, pm2) = (m1.public(), m2.public());
        let sa2 = KeyOffer::new().finish(&pm1).unwrap();
        let sb2 = KeyOffer::new().finish(&pm2).unwrap();
        assert_ne!(sa2.safety_code, sb2.safety_code);
        assert!(m1.finish(&pb).is_some());
        assert!(KeyOffer::new().finish(&B64.encode([0u8; 32])).is_none(), "low-order point");
    }
}
