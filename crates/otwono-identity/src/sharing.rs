//! Sealing a content key to a recipient (ADR-0019).
//!
//! A `SHARED` object is encrypted once with a per-object content key. That key is then
//! sealed separately to each authorized recipient, so the ciphertext can sit on any node
//! while remaining readable only by the nodes on the list.
//!
//! # Why the sender is ephemeral
//!
//! Each seal uses a **fresh X25519 keypair, discarded immediately**. The sending node needs
//! no long-term key of its own, which matters because the daemon that does the sealing —
//! `otwono-stored` — holds exactly one secret, the storage key, and has no identity key at
//! all. Giving it one would be a new trust boundary in exchange for sender authentication
//! that nothing asks for: who shared an object is already recorded in its `owner` field
//! under the node's signature.
//!
//! It also means two seals of the same key to the same recipient are unlinkable, which is a
//! small privacy gain nobody paid for.
//!
//! # What this is not
//!
//! There is **no forward secrecy**. A recipient's sharing key compromised tomorrow opens
//! everything ever sealed to it. Ephemeral senders give sender-side unlinkability, not
//! recipient-side forward secrecy; getting that needs key rotation and re-wrapping, which is
//! OQ-27 and is not built.

use crate::{base64_encode, IdentityError};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey as X25519Public, StaticSecret as X25519Secret};
use zeroize::Zeroizing;

/// Domain separation for the key-derivation step, so a sealed content key can never be
/// confused with any other use of an X25519 shared secret on this node — the Noise
/// handshake's above all.
pub const SEAL_DOMAIN: &[u8] = b"otwono-shared-key-seal-v1";

/// A content key sealed to one recipient.
///
/// The recipient is named so a node with several keys, or an object with several
/// recipients, can find its own copy without trial decryption. That the list of recipients
/// is itself metadata worth protecting is OQ-28 and is not addressed here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedKey {
    /// The recipient this copy is for, as a NodeID text form.
    pub recipient: String,
    /// Base64 X25519 public key of the ephemeral sender. Discarded after sealing.
    pub ephemeral_public_key: String,
    /// Base64 AEAD ciphertext of the 32-byte content key.
    pub sealed: String,
}

/// Seal `content_key` so only the holder of `recipient_public`'s secret can open it.
pub fn seal_to(
    recipient: &str,
    recipient_public: &[u8; 32],
    content_key: &[u8; 32],
) -> Result<SealedKey, IdentityError> {
    let mut seed = Zeroizing::new([0u8; 32]);
    getrandom::getrandom(seed.as_mut()).map_err(|e| IdentityError::Entropy(e.to_string()))?;
    let ephemeral = X25519Secret::from(*seed);
    let ephemeral_public = X25519Public::from(&ephemeral);

    let shared = ephemeral.diffie_hellman(&X25519Public::from(*recipient_public));
    let key = derive(shared.as_bytes(), ephemeral_public.as_bytes(), recipient_public);

    // A fixed nonce is safe here and only here: the key is derived from an ephemeral secret
    // used exactly once, so the (key, nonce) pair cannot repeat. Reusing this construction
    // anywhere with a long-lived key would be a catastrophe, which is why it does not leave
    // this module.
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key.as_ref()));
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&[0u8; 12]),
            Payload {
                msg: content_key.as_ref(),
                aad: recipient.as_bytes(),
            },
        )
        .map_err(|_| IdentityError::MalformedKey("sealing a content key failed".into()))?;

    Ok(SealedKey {
        recipient: recipient.to_string(),
        ephemeral_public_key: base64_encode(ephemeral_public.as_bytes()),
        sealed: base64_encode(&sealed),
    })
}

/// Open a sealed content key with the recipient's sharing secret.
///
/// The recipient name is bound as additional data, so a copy sealed for one node cannot be
/// presented to another as its own — even by someone who holds both secrets.
pub fn open_with(
    sealed: &SealedKey,
    recipient_secret: &X25519Secret,
) -> Result<Zeroizing<[u8; 32]>, IdentityError> {
    let ephemeral_public: [u8; 32] = decode32(&sealed.ephemeral_public_key)?;
    let recipient_public = X25519Public::from(recipient_secret);

    let shared = recipient_secret.diffie_hellman(&X25519Public::from(ephemeral_public));
    let key = derive(shared.as_bytes(), &ephemeral_public, recipient_public.as_bytes());

    let ciphertext = data_encoding::BASE64
        .decode(sealed.sealed.as_bytes())
        .map_err(|e| IdentityError::MalformedKey(format!("sealed key is not base64: {e}")))?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key.as_ref()));
    let opened = cipher
        .decrypt(
            Nonce::from_slice(&[0u8; 12]),
            Payload {
                msg: &ciphertext,
                aad: sealed.recipient.as_bytes(),
            },
        )
        .map_err(|_| IdentityError::MalformedKey("this sealed key is not for this node".into()))?;

    let bytes: [u8; 32] = opened
        .as_slice()
        .try_into()
        .map_err(|_| IdentityError::MalformedKey("a content key is 32 bytes".into()))?;
    Ok(Zeroizing::new(bytes))
}

/// Derive the sealing key from the shared secret and both public keys.
///
/// Both publics go in, so a shared secret computed from a different pairing cannot produce
/// the same sealing key even if an attacker could arrange a collision in one of them.
fn derive(
    shared: &[u8; 32],
    ephemeral_public: &[u8; 32],
    recipient_public: &[u8; 32],
) -> Zeroizing<[u8; 32]> {
    use blake2::digest::{Update, VariableOutput};
    let mut hasher = blake2::Blake2bVar::new(32).expect("32 is a valid blake2b length");
    hasher.update(SEAL_DOMAIN);
    hasher.update(shared);
    hasher.update(ephemeral_public);
    hasher.update(recipient_public);
    let mut out = Zeroizing::new([0u8; 32]);
    hasher
        .finalize_variable(out.as_mut())
        .expect("the output length matches the one requested");
    out
}

fn decode32(text: &str) -> Result<[u8; 32], IdentityError> {
    let raw = data_encoding::BASE64
        .decode(text.as_bytes())
        .map_err(|e| IdentityError::MalformedKey(format!("not base64: {e}")))?;
    raw.as_slice()
        .try_into()
        .map_err(|_| IdentityError::MalformedKey("an X25519 key is 32 bytes".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keypair(seed: u8) -> (X25519Secret, [u8; 32]) {
        let secret = X25519Secret::from([seed; 32]);
        let public = *X25519Public::from(&secret).as_bytes();
        (secret, public)
    }

    #[test]
    fn a_recipient_opens_what_was_sealed_to_it() {
        let (secret, public) = keypair(7);
        let content_key = [0xABu8; 32];
        let sealed = seal_to("otw1:aaaa-bbbb-cccc-dddd", &public, &content_key).unwrap();
        assert_eq!(*open_with(&sealed, &secret).unwrap(), content_key);
    }

    #[test]
    fn nobody_else_opens_it() {
        // The property the whole design rests on: holding the ciphertext is not reading it.
        let (_, alice_public) = keypair(1);
        let (bob_secret, _) = keypair(2);
        let sealed = seal_to("otw1:aaaa-bbbb-cccc-dddd", &alice_public, &[9u8; 32]).unwrap();
        assert!(open_with(&sealed, &bob_secret).is_err());
    }

    #[test]
    fn a_copy_sealed_for_one_node_cannot_be_claimed_by_another() {
        // The recipient name is bound as additional data. Without that, someone holding two
        // secrets could take a copy addressed to one and present it as the other's.
        let (secret, public) = keypair(3);
        let mut sealed = seal_to("otw1:aaaa-bbbb-cccc-dddd", &public, &[4u8; 32]).unwrap();
        sealed.recipient = "otw1:eeee-ffff-gggg-hhhh".to_string();
        assert!(open_with(&sealed, &secret).is_err());
    }

    #[test]
    fn two_seals_of_the_same_key_to_the_same_node_are_unlinkable() {
        // A fresh ephemeral per seal. Identical ciphertexts would tell a holder that two
        // objects share a content key.
        let (secret, public) = keypair(5);
        let content_key = [0x11u8; 32];
        let a = seal_to("otw1:aaaa-bbbb-cccc-dddd", &public, &content_key).unwrap();
        let b = seal_to("otw1:aaaa-bbbb-cccc-dddd", &public, &content_key).unwrap();
        assert_ne!(a.sealed, b.sealed);
        assert_ne!(a.ephemeral_public_key, b.ephemeral_public_key);
        // And both still open to the same key.
        assert_eq!(*open_with(&a, &secret).unwrap(), content_key);
        assert_eq!(*open_with(&b, &secret).unwrap(), content_key);
    }

    #[test]
    fn a_tampered_ciphertext_is_refused_rather_than_returning_rubbish() {
        let (secret, public) = keypair(6);
        let mut sealed = seal_to("otw1:aaaa-bbbb-cccc-dddd", &public, &[2u8; 32]).unwrap();
        let mut raw = data_encoding::BASE64.decode(sealed.sealed.as_bytes()).unwrap();
        raw[0] ^= 1;
        sealed.sealed = base64_encode(&raw);
        assert!(open_with(&sealed, &secret).is_err());
    }

    #[test]
    fn a_swapped_ephemeral_key_is_refused() {
        let (secret, public) = keypair(8);
        let sealed_a = seal_to("otw1:aaaa-bbbb-cccc-dddd", &public, &[1u8; 32]).unwrap();
        let sealed_b = seal_to("otw1:aaaa-bbbb-cccc-dddd", &public, &[1u8; 32]).unwrap();
        let frankenstein = SealedKey {
            ephemeral_public_key: sealed_b.ephemeral_public_key,
            ..sealed_a
        };
        assert!(open_with(&frankenstein, &secret).is_err());
    }

    #[test]
    fn the_sealed_form_never_contains_the_content_key() {
        // Cheap, and it would catch the worst possible bug in this file.
        let (_, public) = keypair(9);
        let content_key = [0x5Au8; 32];
        let sealed = seal_to("otw1:aaaa-bbbb-cccc-dddd", &public, &content_key).unwrap();
        let wire = serde_json::to_string(&sealed).unwrap();
        assert!(!wire.contains(&base64_encode(&content_key)));
        for window in data_encoding::BASE64
            .decode(sealed.sealed.as_bytes())
            .unwrap()
            .windows(32)
        {
            assert_ne!(
                window, content_key,
                "the content key is in the ciphertext verbatim"
            );
        }
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        let (secret, _) = keypair(10);
        for bad in [
            SealedKey {
                recipient: "otw1:a".into(),
                ephemeral_public_key: "not base64!".into(),
                sealed: "AAAA".into(),
            },
            SealedKey {
                recipient: "otw1:a".into(),
                ephemeral_public_key: base64_encode(&[0u8; 16]),
                sealed: "AAAA".into(),
            },
            SealedKey {
                recipient: "otw1:a".into(),
                ephemeral_public_key: base64_encode(&[0u8; 32]),
                sealed: "!!!".into(),
            },
        ] {
            assert!(open_with(&bad, &secret).is_err());
        }
    }
}
