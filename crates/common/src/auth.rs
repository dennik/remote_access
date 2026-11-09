//! Viewer login: an Ed25519 signature over the agent's challenge.
//!
//! The signature covers the certificate fingerprint the viewer actually saw, so
//! a man-in-the-middle cannot replay it to the real agent. Agents store only
//! public keys, so a compromised agent gives no access to the other machines.

use anyhow::{anyhow, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

const CONTEXT: &[u8] = b"rd-auth-v1";

fn transcript(nonce: &[u8; 32], fingerprint: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(CONTEXT.len() + 32 + fingerprint.len());
    m.extend_from_slice(CONTEXT);
    m.extend_from_slice(nonce);
    m.extend_from_slice(fingerprint.as_bytes());
    m
}

pub fn generate_seed() -> Result<[u8; 32]> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow!("system RNG failed: {e}"))?;
    Ok(seed)
}

pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

pub fn sign(seed: &[u8; 32], nonce: &[u8; 32], fingerprint: &str) -> Vec<u8> {
    SigningKey::from_bytes(seed).sign(&transcript(nonce, fingerprint)).to_bytes().to_vec()
}

pub fn verify(public_key: &[u8; 32], nonce: &[u8; 32], fingerprint: &str, signature: &[u8]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(public_key) else { return false };
    let Ok(sig) = Signature::from_slice(signature) else { return false };
    key.verify(&transcript(nonce, fingerprint), &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_exact_key_nonce_and_fingerprint() {
        let seed = generate_seed().unwrap();
        let public = public_key(&seed);
        let nonce = [7u8; 32];
        let sig = sign(&seed, &nonce, "agent-fp");

        assert!(verify(&public, &nonce, "agent-fp", &sig));
        assert!(!verify(&public, &[8u8; 32], "agent-fp", &sig), "replayed nonce");
        assert!(!verify(&public, &nonce, "mitm-fp", &sig), "relayed through another cert");
        let other = public_key(&generate_seed().unwrap());
        assert!(!verify(&other, &nonce, "agent-fp", &sig), "unauthorised key");
        assert!(!verify(&public, &nonce, "agent-fp", &sig[..10]), "malformed signature");
    }
}
