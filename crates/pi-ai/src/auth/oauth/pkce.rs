//! PKCE (RFC 7636) verifier/challenge generation.
//!
//! The verifier is 32 random bytes encoded as base64url without padding. The
//! challenge is `BASE64URL-ENCODE(SHA256(ASCII(verifier)))` with the same
//! alphabet and no padding.

use std::fmt;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

/// PKCE code verifier and S256 challenge pair.
#[derive(Clone, Eq, PartialEq)]
pub struct PkceCodes {
    /// High-entropy secret sent only on the token request.
    pub verifier: String,
    /// S256 challenge placed on the authorization request.
    pub challenge: String,
}

impl fmt::Debug for PkceCodes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PkceCodes")
            .field("verifier", &"[redacted]")
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// Generate a fresh PKCE verifier/challenge pair.
///
/// # Errors
///
/// Returns an error when the OS CSPRNG fails.
pub fn generate_pkce() -> Result<PkceCodes, PkceError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| PkceError::Entropy(error.to_string()))?;
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = s256_challenge(&verifier);
    Ok(PkceCodes {
        verifier,
        challenge,
    })
}

/// Generate a URL-safe no-pad base64 state token from `byte_len` random bytes.
///
/// # Errors
///
/// Returns an error when the OS CSPRNG fails or `byte_len` is zero.
pub fn generate_state(byte_len: usize) -> Result<String, PkceError> {
    if byte_len == 0 {
        return Err(PkceError::Entropy(
            "state byte length must be greater than zero".into(),
        ));
    }
    let mut bytes = vec![0_u8; byte_len];
    getrandom::fill(&mut bytes).map_err(|error| PkceError::Entropy(error.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Compute the S256 code challenge for an existing verifier.
#[must_use]
pub fn s256_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Encode bytes as base64url without padding.
#[must_use]
pub fn base64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Failure generating PKCE material.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PkceError {
    /// Operating-system entropy source failed.
    #[error("failed to generate OAuth entropy: {0}")]
    Entropy(String),
}

impl From<PkceError> for super::super::error::AuthError {
    fn from(value: PkceError) -> Self {
        Self::message(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), String>;

    fn err(msg: impl Into<String>) -> String {
        msg.into()
    }

    #[test]
    fn challenge_matches_s256_vector_shape() {
        // RFC 7636 appendix B: challenge is sha256(verifier) base64url no pad.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = s256_challenge(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
        assert!(!challenge.contains('='));
    }

    #[test]
    fn generate_pkce_has_expected_entropy_shape() -> TestResult {
        let codes = generate_pkce().map_err(|e| err(e.to_string()))?;
        // 32 bytes -> 43 base64url chars without padding.
        assert_eq!(codes.verifier.len(), 43);
        assert_eq!(codes.challenge.len(), 43);
        assert_eq!(codes.challenge, s256_challenge(&codes.verifier));
        assert!(
            codes
                .verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert!(!codes.verifier.contains('='));
        assert!(!codes.challenge.contains('='));

        let other = generate_pkce().map_err(|e| err(e.to_string()))?;
        assert_ne!(codes.verifier, other.verifier);

        let debug = format!("{codes:?}");
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains(&codes.verifier));
        Ok(())
    }

    #[test]
    fn generate_state_is_url_safe_no_pad() -> TestResult {
        let state = generate_state(16).map_err(|e| err(e.to_string()))?;
        // 16 bytes -> 22 chars without padding.
        assert_eq!(state.len(), 22);
        assert!(!state.contains('='));
        assert!(!state.contains('+'));
        assert!(!state.contains('/'));
        Ok(())
    }
}
