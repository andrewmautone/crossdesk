//! Pairing (SPAKE2 over a 6-digit code) and per-session authenticated encryption.
//!
//! Pairing yields a 32-byte long-term key stored by both peers. Every
//! connection derives fresh directional keys from it with HKDF over both
//! hello nonces, then uses ChaCha20-Poly1305 with an implicit counter nonce,
//! so replayed, reordered or forged frames fail to decrypt.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use rand::{Rng, RngCore};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

pub fn random_nonce() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

pub fn random_id() -> String {
    random_nonce()
}

pub fn pairing_code() -> String {
    format!("{:06}", rand::thread_rng().gen_range(0..1_000_000))
}

pub struct Pake(Spake2<Ed25519Group>);

impl Pake {
    pub fn start(code: &str) -> (Pake, Vec<u8>) {
        let (s, msg) = Spake2::<Ed25519Group>::start_symmetric(
            &Password::new(code.trim().as_bytes()),
            &Identity::new(b"crossdesk-pair-v1"),
        );
        (Pake(s), msg)
    }

    pub fn finish(self, peer_msg: &[u8]) -> Result<[u8; 32], String> {
        let key = self
            .0
            .finish(peer_msg)
            .map_err(|e| format!("pairing failed: {e:?}"))?;
        let mut out = [0u8; 32];
        let hk = Hkdf::<Sha256>::new(None, &key);
        hk.expand(b"crossdesk long-term key", &mut out)
            .map_err(|e| e.to_string())?;
        Ok(out)
    }
}

pub struct SendCipher {
    aead: ChaCha20Poly1305,
    counter: u64,
}

pub struct RecvCipher {
    aead: ChaCha20Poly1305,
    counter: u64,
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    *Nonce::from_slice(&n)
}

impl SendCipher {
    pub fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        let out = self
            .aead
            .encrypt(&nonce(self.counter), plain)
            .expect("encryption cannot fail");
        self.counter += 1;
        out
    }
}

impl RecvCipher {
    pub fn open(&mut self, sealed: &[u8]) -> Result<Vec<u8>, String> {
        let out = self
            .aead
            .decrypt(&nonce(self.counter), sealed)
            .map_err(|_| "decryption failed (wrong key or tampered data)".to_string())?;
        self.counter += 1;
        Ok(out)
    }
}

/// Derive this side's (send, recv) ciphers. `client_nonce`/`server_nonce` are the hello nonces.
pub fn session(
    key: &[u8; 32],
    client_nonce: &str,
    server_nonce: &str,
    is_client: bool,
) -> (SendCipher, RecvCipher) {
    let salt = format!("{client_nonce}{server_nonce}");
    let hk = Hkdf::<Sha256>::new(Some(salt.as_bytes()), key);
    let mut c2s = [0u8; 32];
    let mut s2c = [0u8; 32];
    hk.expand(b"crossdesk c2s", &mut c2s).expect("valid length");
    hk.expand(b"crossdesk s2c", &mut s2c).expect("valid length");
    let (send, recv) = if is_client { (c2s, s2c) } else { (s2c, c2s) };
    (
        SendCipher {
            aead: ChaCha20Poly1305::new(Key::from_slice(&send)),
            counter: 0,
        },
        RecvCipher {
            aead: ChaCha20Poly1305::new(Key::from_slice(&recv)),
            counter: 0,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_code_gives_same_key() {
        let (a, ma) = Pake::start("123456");
        let (b, mb) = Pake::start("123456");
        assert_eq!(a.finish(&mb).unwrap(), b.finish(&ma).unwrap());
    }

    #[test]
    fn wrong_code_cannot_decrypt() {
        let (a, ma) = Pake::start("123456");
        let (b, mb) = Pake::start("654321");
        let ka = a.finish(&mb).unwrap();
        let kb = b.finish(&ma).unwrap();
        let (mut send, _) = session(&ka, "c", "s", true);
        let (_, mut recv) = session(&kb, "c", "s", false);
        assert!(recv.open(&send.seal(b"hi")).is_err());
    }

    #[test]
    fn session_round_trip_and_replay_rejected() {
        let key = [7u8; 32];
        let (mut cs, mut cr) = session(&key, "n1", "n2", true);
        let (mut ss, mut sr) = session(&key, "n1", "n2", false);
        let f = cs.seal(b"hello");
        assert_eq!(sr.open(&f).unwrap(), b"hello");
        assert!(sr.open(&f).is_err());
        assert_eq!(cr.open(&ss.seal(b"back")).unwrap(), b"back");
    }
}
