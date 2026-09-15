use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng as _;
use rand::distr::Alphanumeric;
use sha2::{Digest, Sha256};

/// RFC 7636 PKCE pair. Spotify requires S256; the `plain` method is rejected.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        // Alphanumeric is a subset of the unreserved set RFC 7636 allows,
        // and 64 chars sits comfortably inside the 43..=128 range.
        let verifier = random_string(64);
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);
        Self { verifier, challenge }
    }
}

pub fn random_string(len: usize) -> String {
    rand::rng().sample_iter(&Alphanumeric).take(len).map(char::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_length_is_in_spec_range() {
        let p = Pkce::generate();
        assert!((43..=128).contains(&p.verifier.len()));
    }

    #[test]
    fn challenge_is_unpadded_base64url_of_sha256() {
        let p = Pkce::generate();
        // 32 raw bytes -> 43 base64url chars with no padding.
        assert_eq!(p.challenge.len(), 43);
        assert!(!p.challenge.contains(['=', '+', '/']));
    }

    #[test]
    fn known_vector_from_rfc7636() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let digest = Sha256::digest(verifier.as_bytes());
        assert_eq!(URL_SAFE_NO_PAD.encode(digest), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn pairs_are_unique() {
        assert_ne!(Pkce::generate().verifier, Pkce::generate().verifier);
    }
}
