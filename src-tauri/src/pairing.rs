//! Pairing module — pairing code, key derivation, peer allowlist, encryption.
//!
//! # Pairing flow
//! 1. Peer A generates a random 6-digit code, displays it to the user.
//! 2. User on Peer B enters the same code.
//! 3. Both peers derive a 256-bit symmetric key via HKDF-SHA256:
//!    `key = HKDF(salt="clipsync-pairing-v1", ikm=<pairing code>, info="")`
//! 4. This key encrypts all subsequent WebSocket messages.
//!
//! # Wire encryption format
//! Each WebSocket binary frame contains:
//!   [12-byte ChaCha20-Poly1305 nonce][encrypted payload + 16-byte tag]
//!
//! # Security
//! - The pairing code provides ~20 bits of entropy (6 decimal digits = ~10^6).
//!   This is sufficient against casual LAN eavesdroppers but NOT against
//!   a determined attacker who can brute-force 10^6 attempts.
//! - In a production setting, increase code length or add an ephemeral
//!   Diffie-Hellman exchange on top.

use chacha20poly1305::aead::{Aead, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use hkdf::Hkdf;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::config::PeerInfo;

/// Size of the ChaCha20-Poly1305 key (256 bits).
const KEY_SIZE: usize = 32;
/// Size of the ChaCha20-Poly1305 nonce (96 bits).
const NONCE_SIZE: usize = 12;
/// HKDF salt to domain-separate ClipSync keys.
const HKDF_SALT: &[u8] = b"clipsync-pairing-v1";

/// Generate a random 6-digit pairing code.
pub fn generate_pairing_code() -> String {
    let mut rng = rand::thread_rng();
    format!("{:06}", rng.gen_range(0..1_000_000))
}

/// Derive a 256-bit symmetric key from a 6-digit pairing code using HKDF-SHA256.
pub fn derive_key(pairing_code: &str) -> [u8; KEY_SIZE] {
    let hkdf = Hkdf::<Sha256>::new(Some(HKDF_SALT), pairing_code.as_bytes());
    let mut key = [0u8; KEY_SIZE];
    hkdf.expand(b"", &mut key)
        .expect("HKDF expand: KEY_SIZE is valid for SHA256");
    key
}

/// Cipher wrapper for encrypting/decrypting wire messages.
pub struct Cipher {
    aead: ChaCha20Poly1305,
}

impl Cipher {
    /// Create a new cipher from a pairing code.
    pub fn from_pairing_code(code: &str) -> Self {
        let key = derive_key(code);
        Self::from_key(&key)
    }

    /// Create a new cipher from a raw key.
    pub fn from_key(key: &[u8; KEY_SIZE]) -> Self {
        let aead =
            ChaCha20Poly1305::new_from_slice(key).expect("KEY_SIZE is valid for ChaCha20Poly1305");
        Self { aead }
    }

    /// Encrypt plaintext, returning nonce || ciphertext.
    /// The nonce is randomly generated per message.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let mut nonce_bytes = [0u8; NONCE_SIZE];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .aead
            .encrypt(nonce, plaintext)
            .map_err(|e| format!("Encryption error: {e}"))?;

        // Prepend nonce to ciphertext
        let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
        result.extend_from_slice(&nonce_bytes);
        result.extend_from_slice(&ciphertext);
        Ok(result)
    }

    /// Decrypt a message in format: nonce || ciphertext.
    pub fn decrypt(&self, wire_data: &[u8]) -> Result<Vec<u8>, String> {
        if wire_data.len() < NONCE_SIZE + 16 {
            return Err("Wire data too short for decryption".to_string());
        }

        let (nonce_bytes, ciphertext) = wire_data.split_at(NONCE_SIZE);
        let nonce = Nonce::from_slice(nonce_bytes);

        self.aead
            .decrypt(nonce, ciphertext)
            .map_err(|e| format!("Decryption error: {e}"))
    }
}

/// Wire message envelope for transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireMessage {
    /// Message type discriminator.
    pub msg_type: String,
    /// Serialized payload (ClipboardContent JSON, or other).
    pub payload: Vec<u8>,
}

impl WireMessage {
    pub fn new(msg_type: &str, payload: Vec<u8>) -> Self {
        Self {
            msg_type: msg_type.to_string(),
            payload,
        }
    }

    /// Serialize to MessagePack and encrypt.
    pub fn to_encrypted(&self, cipher: &Cipher) -> Result<Vec<u8>, String> {
        let serialized =
            rmp_serde::to_vec(self).map_err(|e| format!("MessagePack serialize error: {e}"))?;
        cipher.encrypt(&serialized)
    }

    /// Decrypt and deserialize from wire format.
    pub fn from_encrypted(data: &[u8], cipher: &Cipher) -> Result<Self, String> {
        let decrypted = cipher.decrypt(data)?;
        rmp_serde::from_slice(&decrypted).map_err(|e| format!("MessagePack deserialize error: {e}"))
    }
}

/// Verify a pairing code format (6 digits).
pub fn is_valid_pairing_code(code: &str) -> bool {
    code.len() == 6 && code.chars().all(|c| c.is_ascii_digit())
}

/// Check if a peer is in the allowlist.
pub fn is_peer_allowed(peers: &[PeerInfo], peer_id: &str) -> bool {
    peers.iter().any(|p| p.id == peer_id)
}

/// Find a peer by ID (mutable reference helper).
pub fn find_peer_mut<'a>(peers: &'a mut [PeerInfo], peer_id: &str) -> Option<&'a mut PeerInfo> {
    peers.iter_mut().find(|p| p.id == peer_id)
}

/// Find a peer by ID.
pub fn find_peer<'a>(peers: &'a [PeerInfo], peer_id: &str) -> Option<&'a PeerInfo> {
    peers.iter().find(|p| p.id == peer_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pairing_code_format() {
        let code = generate_pairing_code();
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        assert!(is_valid_pairing_code(&code));
    }

    #[test]
    fn test_invalid_pairing_code() {
        assert!(!is_valid_pairing_code("12345"));
        assert!(!is_valid_pairing_code("abcdef"));
        assert!(!is_valid_pairing_code(""));
    }

    #[test]
    fn test_key_derivation_deterministic() {
        let key1 = derive_key("123456");
        let key2 = derive_key("123456");
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_key_derivation_different() {
        let key1 = derive_key("123456");
        let key2 = derive_key("654321");
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let cipher = Cipher::from_pairing_code("123456");
        let plaintext = b"Hello, ClipSync!";
        let encrypted = cipher.encrypt(plaintext).unwrap();
        let decrypted = cipher.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_produces_different_ciphertexts() {
        let cipher = Cipher::from_pairing_code("123456");
        let plaintext = b"test";
        let ct1 = cipher.encrypt(plaintext).unwrap();
        let ct2 = cipher.encrypt(plaintext).unwrap();
        // Nonces should differ, so ciphertexts differ
        assert_ne!(ct1, ct2);
    }

    #[test]
    fn test_decrypt_with_wrong_key_fails() {
        let cipher_a = Cipher::from_pairing_code("111111");
        let cipher_b = Cipher::from_pairing_code("222222");
        let encrypted = cipher_a.encrypt(b"secret").unwrap();
        assert!(cipher_b.decrypt(&encrypted).is_err());
    }

    #[test]
    fn test_wire_message_roundtrip() {
        let cipher = Cipher::from_pairing_code("999999");
        let msg = WireMessage::new("clipboard", b"payload data".to_vec());
        let encrypted = msg.to_encrypted(&cipher).unwrap();
        let decrypted = WireMessage::from_encrypted(&encrypted, &cipher).unwrap();
        assert_eq!(decrypted.msg_type, "clipboard");
        assert_eq!(decrypted.payload, b"payload data");
    }

    #[test]
    fn test_wire_message_tampering_detected() {
        let cipher = Cipher::from_pairing_code("999999");
        let msg = WireMessage::new("clipboard", b"payload".to_vec());
        let mut encrypted = msg.to_encrypted(&cipher).unwrap();
        // Flip a bit in the ciphertext
        if encrypted.len() > 20 {
            encrypted[20] ^= 1;
        }
        assert!(WireMessage::from_encrypted(&encrypted, &cipher).is_err());
    }
}
