//! Bearer tokens for room actions.
//!
//! A token says "device `did` may act in room `rc` until `exp`". It carries no
//! role: permissions come from current room state, so a member promoted to host
//! keeps working with the token they already hold.
//!
//! Wire format is `v1.<payload_b64url>.<sig_b64url>`, signed with HMAC-SHA256
//! over `"v1." + payload_b64url`. Base64 is URL-safe and unpadded so the token
//! is header-safe.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::LazyLock;
use subtle::ConstantTimeEq;
use tracing::info;

type HmacSha256 = Hmac<Sha256>;

const VERSION: &str = "v1";
/// Tokens live as long as the room they're scoped to.
pub const TOKEN_TTL_SECONDS: i64 = 24 * 60 * 60;
/// Real tokens are ~150 bytes. Caps the decode work junk input can cause.
const MAX_TOKEN_LEN: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Room code this token is scoped to.
    pub rc: String,
    /// Device id this token represents.
    pub did: String,
    /// Expiry, unix seconds.
    pub exp: i64,
}

#[derive(Debug, Clone)]
pub enum TokenError {
    /// Server config is wrong (missing/short secret). Never surface details.
    Config(String),
    /// Token wasn't supplied at all.
    Missing,
    /// Token was malformed before we even checked the signature.
    Malformed,
    /// Signature didn't match.
    BadSignature,
    /// `exp` has passed.
    Expired,
    /// Token is for a different room than the path.
    WrongRoom,
    /// Failed to encode a token. Should be impossible for our shape.
    Encoding(String),
}

impl TokenError {
    /// One message per failure class. Never says which check failed.
    pub fn client_message(&self) -> &'static str {
        match self {
            TokenError::Config(_) | TokenError::Encoding(_) => {
                "Authentication is temporarily unavailable."
            }
            TokenError::Missing => "This action requires a room session.",
            TokenError::Malformed
            | TokenError::BadSignature
            | TokenError::Expired
            | TokenError::WrongRoom => "Your room session is invalid or expired. Please rejoin.",
        }
    }
}

/// Signing secrets, loaded once per process. We mint with `current`.
/// `previous` is optional and only used when verifying, so tokens signed
/// before a rotation keep working until they expire.
struct CachedSecrets {
    current: Vec<u8>,
    previous: Option<Vec<u8>>,
}

static SECRETS: LazyLock<Result<CachedSecrets, TokenError>> = LazyLock::new(load_secrets_inner);

/// The current signing secret, borrowed so handlers don't allocate per call.
pub fn load_secret() -> Result<&'static [u8], TokenError> {
    match SECRETS.as_ref() {
        Ok(s) => Ok(s.current.as_slice()),
        Err(e) => Err(e.clone()),
    }
}

fn load_secrets_inner() -> Result<CachedSecrets, TokenError> {
    let current = decode_secret_env("ROOM_TOKEN_SECRET")?
        .ok_or_else(|| TokenError::Config("ROOM_TOKEN_SECRET not set".to_string()))?;
    let previous = decode_secret_env("ROOM_TOKEN_SECRET_PREVIOUS")?;
    Ok(CachedSecrets { current, previous })
}

/// Read `name` and base64-decode it. `Ok(None)` when unset or empty, `Err`
/// when set but malformed or shorter than 32 bytes.
fn decode_secret_env(name: &str) -> Result<Option<Vec<u8>>, TokenError> {
    let raw = match std::env::var(name) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => return Ok(None),
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(raw.trim())
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(raw.trim()))
        .map_err(|e| TokenError::Config(format!("{name} not valid base64: {e}")))?;
    if bytes.len() < 32 {
        return Err(TokenError::Config(format!(
            "{name} decoded to {} bytes, need at least 32",
            bytes.len()
        )));
    }
    Ok(Some(bytes))
}

/// Mint a token for `(room_code, device_id)` valid for [`TOKEN_TTL_SECONDS`].
pub fn mint(
    secret: &[u8],
    room_code: &str,
    device_id: &str,
    now_secs: i64,
) -> Result<String, TokenError> {
    let claims = Claims {
        rc: room_code.to_string(),
        did: device_id.to_string(),
        exp: now_secs + TOKEN_TTL_SECONDS,
    };
    encode(secret, &claims)
}

fn encode(secret: &[u8], claims: &Claims) -> Result<String, TokenError> {
    let payload_json = serde_json::to_vec(claims)
        .map_err(|e| TokenError::Encoding(format!("serialize claims: {e}")))?;
    let payload_b64 = URL_SAFE_NO_PAD.encode(&payload_json);
    let signing_input = format!("{VERSION}.{payload_b64}");
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(signing_input.as_bytes());
    let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(format!("{signing_input}.{sig}"))
}

/// Verify a token against `secret` and the expected room code, at the given
/// wall-clock time. Returns the parsed claims on success.
pub fn verify(
    secret: &[u8],
    token: &str,
    expected_room_code: &str,
    now_secs: i64,
) -> Result<Claims, TokenError> {
    verify_inner(secret, token, expected_room_code, now_secs, false)
}

/// Like [`verify`] but ignores `exp`. Only `/rejoin` uses it, so a client with
/// a stale token can recover without being asked for their name again.
pub fn verify_allow_expired(
    secret: &[u8],
    token: &str,
    expected_room_code: &str,
) -> Result<Claims, TokenError> {
    verify_inner(secret, token, expected_room_code, 0, true)
}

fn verify_inner(
    secret: &[u8],
    token: &str,
    expected_room_code: &str,
    now_secs: i64,
    allow_expired: bool,
) -> Result<Claims, TokenError> {
    if token.is_empty() {
        return Err(TokenError::Missing);
    }
    if token.len() > MAX_TOKEN_LEN {
        return Err(TokenError::Malformed);
    }

    let mut parts = token.splitn(3, '.');
    let version = parts.next().ok_or(TokenError::Malformed)?;
    let payload_b64 = parts.next().ok_or(TokenError::Malformed)?;
    let sig_b64 = parts.next().ok_or(TokenError::Malformed)?;
    if version != VERSION {
        return Err(TokenError::Malformed);
    }

    // Sign the original bytes, not a re-encode, so JSON key order can't matter.
    let signing_input = format!("{VERSION}.{payload_b64}");
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(signing_input.as_bytes());
    let expected_sig = mac.finalize().into_bytes();

    let provided_sig = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| TokenError::Malformed)?;
    if expected_sig.as_slice().ct_eq(&provided_sig).unwrap_u8() != 1 {
        return Err(TokenError::BadSignature);
    }

    let payload_json = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| TokenError::Malformed)?;
    let claims: Claims =
        serde_json::from_slice(&payload_json).map_err(|_| TokenError::Malformed)?;

    if !allow_expired && claims.exp <= now_secs {
        return Err(TokenError::Expired);
    }
    if claims.rc != expected_room_code {
        return Err(TokenError::WrongRoom);
    }

    Ok(claims)
}

/// Verify against the current secret, then the previous one if the signature
/// doesn't match. Handlers should call this: it lets a secret be rotated
/// without invalidating live sessions.
pub fn verify_with_rotation(
    token: &str,
    expected_room_code: &str,
    now_secs: i64,
) -> Result<Claims, TokenError> {
    let secrets = SECRETS.as_ref().map_err(|e| e.clone())?;
    verify_with_secrets(
        &secrets.current,
        secrets.previous.as_deref(),
        token,
        expected_room_code,
        now_secs,
    )
}

/// [`verify_with_rotation`] without the expiry check, for `/rejoin`.
pub fn verify_with_rotation_allow_expired(
    token: &str,
    expected_room_code: &str,
) -> Result<Claims, TokenError> {
    let secrets = SECRETS.as_ref().map_err(|e| e.clone())?;
    verify_with_secrets_allow_expired(
        &secrets.current,
        secrets.previous.as_deref(),
        token,
        expected_room_code,
    )
}

/// [`verify_with_rotation`] with explicit secrets, so tests can avoid the
/// process-wide `LazyLock`.
pub fn verify_with_secrets(
    current: &[u8],
    previous: Option<&[u8]>,
    token: &str,
    expected_room_code: &str,
    now_secs: i64,
) -> Result<Claims, TokenError> {
    match verify(current, token, expected_room_code, now_secs) {
        Ok(claims) => Ok(claims),
        Err(TokenError::BadSignature) => match previous {
            Some(prev) => match verify(prev, token, expected_room_code, now_secs) {
                Ok(claims) => {
                    // Watched during rotation: zero of these means the old
                    // secret can be removed.
                    info!(rc = %claims.rc, "auth_token_verified_with_previous_secret");
                    Ok(claims)
                }
                Err(_) => Err(TokenError::BadSignature),
            },
            None => Err(TokenError::BadSignature),
        },
        Err(other) => Err(other),
    }
}

/// [`verify_with_secrets`] without the expiry check.
pub fn verify_with_secrets_allow_expired(
    current: &[u8],
    previous: Option<&[u8]>,
    token: &str,
    expected_room_code: &str,
) -> Result<Claims, TokenError> {
    match verify_allow_expired(current, token, expected_room_code) {
        Ok(claims) => Ok(claims),
        Err(TokenError::BadSignature) => match previous {
            Some(prev) => match verify_allow_expired(prev, token, expected_room_code) {
                Ok(claims) => {
                    info!(rc = %claims.rc, "auth_token_verified_with_previous_secret");
                    Ok(claims)
                }
                Err(_) => Err(TokenError::BadSignature),
            },
            None => Err(TokenError::BadSignature),
        },
        Err(other) => Err(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> Vec<u8> {
        // 32 bytes of "test secret".
        b"0123456789abcdef0123456789abcdef".to_vec()
    }

    #[test]
    fn mint_and_verify_round_trip() {
        let token = mint(&secret(), "ABCDEF", "device-1", 1_700_000_000).expect("mint");
        let claims = verify(&secret(), &token, "ABCDEF", 1_700_000_000).expect("verify");
        assert_eq!(claims.rc, "ABCDEF");
        assert_eq!(claims.did, "device-1");
        assert_eq!(claims.exp, 1_700_000_000 + TOKEN_TTL_SECONDS);
    }

    #[test]
    fn rejects_wrong_room() {
        let token = mint(&secret(), "ABCDEF", "device-1", 1_700_000_000).expect("mint");
        let err = verify(&secret(), &token, "ZZZZZZ", 1_700_000_000).unwrap_err();
        assert!(matches!(err, TokenError::WrongRoom));
    }

    #[test]
    fn rejects_expired() {
        let token = mint(&secret(), "ABCDEF", "device-1", 1_700_000_000).expect("mint");
        let err = verify(
            &secret(),
            &token,
            "ABCDEF",
            1_700_000_000 + TOKEN_TTL_SECONDS + 1,
        )
        .unwrap_err();
        assert!(matches!(err, TokenError::Expired));
    }

    #[test]
    fn rejects_tampered_payload() {
        let token = mint(&secret(), "ABCDEF", "device-1", 1_700_000_000).expect("mint");
        // Replace device id by editing the payload portion in place.
        let mut parts: Vec<&str> = token.split('.').collect();
        let evil = Claims {
            rc: "ABCDEF".to_string(),
            did: "device-evil".to_string(),
            exp: 1_700_000_000 + TOKEN_TTL_SECONDS,
        };
        let evil_payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&evil).unwrap());
        parts[1] = &evil_payload;
        let tampered = parts.join(".");
        let err = verify(&secret(), &tampered, "ABCDEF", 1_700_000_000).unwrap_err();
        assert!(matches!(err, TokenError::BadSignature));
    }

    #[test]
    fn rejects_wrong_secret() {
        let token = mint(&secret(), "ABCDEF", "device-1", 1_700_000_000).expect("mint");
        let other_secret = b"ffffffffffffffffffffffffffffffff";
        let err = verify(other_secret, &token, "ABCDEF", 1_700_000_000).unwrap_err();
        assert!(matches!(err, TokenError::BadSignature));
    }

    #[test]
    fn rejects_malformed() {
        let err = verify(&secret(), "", "ABCDEF", 1_700_000_000).unwrap_err();
        assert!(matches!(err, TokenError::Missing));

        for bad in ["junk", "v1.junk", "v2.aGk.aGk", "v1..", &"x".repeat(2000)] {
            let err = verify(&secret(), bad, "ABCDEF", 1_700_000_000).unwrap_err();
            assert!(
                matches!(err, TokenError::Malformed | TokenError::BadSignature),
                "expected malformed/bad-sig for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn token_is_url_safe_and_compact() {
        let token = mint(&secret(), "ABCDEF", "device-1", 1_700_000_000).expect("mint");
        // base64url alphabet only, so no `+`, `/`, or `=`.
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
            "non url-safe chars in token: {token}"
        );
        // Comfortably under typical header size limits.
        assert!(token.len() < 512, "token too long: {} bytes", token.len());
    }

    #[test]
    fn client_message_does_not_leak_internals() {
        // Specifically: Config errors should never expose the env var name.
        let msg = TokenError::Config("ROOM_TOKEN_SECRET not set".to_string()).client_message();
        assert!(!msg.contains("ROOM_TOKEN_SECRET"));
        assert!(!msg.contains("env"));
    }

    // --- rotation ---

    fn old_secret() -> Vec<u8> {
        b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_vec()
    }

    fn new_secret() -> Vec<u8> {
        b"BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".to_vec()
    }

    #[test]
    fn rotation_accepts_tokens_signed_by_current_secret() {
        let token = mint(&new_secret(), "ABCDEF", "device-1", 1_700_000_000).unwrap();
        let claims = verify_with_secrets(
            &new_secret(),
            Some(&old_secret()),
            &token,
            "ABCDEF",
            1_700_000_000,
        )
        .expect("current secret should verify");
        assert_eq!(claims.did, "device-1");
    }

    #[test]
    fn rotation_accepts_tokens_signed_by_previous_secret() {
        // Token was minted before rotation under the old secret; after
        // rotation the new secret is current and the old one is "previous".
        let token = mint(&old_secret(), "ABCDEF", "device-1", 1_700_000_000).unwrap();
        let claims = verify_with_secrets(
            &new_secret(),
            Some(&old_secret()),
            &token,
            "ABCDEF",
            1_700_000_000,
        )
        .expect("previous secret should still verify");
        assert_eq!(claims.did, "device-1");
    }

    #[test]
    fn rotation_rejects_tokens_signed_by_unknown_secret() {
        let unknown = b"CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";
        let token = mint(unknown, "ABCDEF", "device-1", 1_700_000_000).unwrap();
        let err = verify_with_secrets(
            &new_secret(),
            Some(&old_secret()),
            &token,
            "ABCDEF",
            1_700_000_000,
        )
        .unwrap_err();
        assert!(matches!(err, TokenError::BadSignature));
    }

    #[test]
    fn rotation_without_previous_behaves_like_single_secret() {
        let token = mint(&old_secret(), "ABCDEF", "device-1", 1_700_000_000).unwrap();
        // No previous configured: old-secret tokens look like BadSignature.
        let err =
            verify_with_secrets(&new_secret(), None, &token, "ABCDEF", 1_700_000_000).unwrap_err();
        assert!(matches!(err, TokenError::BadSignature));
    }

    #[test]
    fn rotation_does_not_retry_on_non_signature_errors() {
        // Expiry fails independently of the secret, so retrying would waste
        // CPU and could turn an Expired into a BadSignature, confusing the
        // error mapping.
        let token = mint(&new_secret(), "ABCDEF", "device-1", 1_700_000_000).unwrap();
        let err = verify_with_secrets(
            &new_secret(),
            Some(&old_secret()),
            &token,
            "ABCDEF",
            1_700_000_000 + TOKEN_TTL_SECONDS + 1,
        )
        .unwrap_err();
        assert!(matches!(err, TokenError::Expired));

        // Same for wrong room.
        let err = verify_with_secrets(
            &new_secret(),
            Some(&old_secret()),
            &token,
            "ZZZZZZ",
            1_700_000_000,
        )
        .unwrap_err();
        assert!(matches!(err, TokenError::WrongRoom));
    }

    #[test]
    fn allow_expired_accepts_token_past_exp() {
        // Token minted at t=0, exp at t=TTL. We verify at t = TTL + 100.
        let token = mint(&secret(), "ABCDEF", "device-1", 1_700_000_000).unwrap();
        // Sanity: expired under the regular check.
        let err = verify(
            &secret(),
            &token,
            "ABCDEF",
            1_700_000_000 + TOKEN_TTL_SECONDS + 100,
        )
        .unwrap_err();
        assert!(matches!(err, TokenError::Expired));

        // Allow-expired path returns the claims.
        let claims = verify_allow_expired(&secret(), &token, "ABCDEF").expect("allow_expired");
        assert_eq!(claims.did, "device-1");
        assert_eq!(claims.rc, "ABCDEF");
    }

    #[test]
    fn allow_expired_still_rejects_bad_signature() {
        let token = mint(&secret(), "ABCDEF", "device-1", 1_700_000_000).unwrap();
        let other = b"ffffffffffffffffffffffffffffffff";
        let err = verify_allow_expired(other, &token, "ABCDEF").unwrap_err();
        assert!(matches!(err, TokenError::BadSignature));
    }

    #[test]
    fn allow_expired_still_rejects_wrong_room() {
        let token = mint(&secret(), "ABCDEF", "device-1", 1_700_000_000).unwrap();
        let err = verify_allow_expired(&secret(), &token, "ZZZZZZ").unwrap_err();
        assert!(matches!(err, TokenError::WrongRoom));
    }

    #[test]
    fn allow_expired_honours_rotation_fallback() {
        // Token minted under the old secret; verifier has the new secret as
        // current and the old one as previous. Even though the token is also
        // expired, allow_expired_with_rotation should fall back to previous
        // and succeed.
        let token = mint(&old_secret(), "ABCDEF", "device-1", 1_700_000_000).unwrap();
        let claims =
            verify_with_secrets_allow_expired(&new_secret(), Some(&old_secret()), &token, "ABCDEF")
                .expect("previous secret should still verify even when expired");
        assert_eq!(claims.did, "device-1");
    }

    #[test]
    fn rotation_bad_signature_under_both_keys_is_indistinguishable() {
        // From the client's point of view, "wrong sig under current" and
        // "wrong sig under both keys" must both surface as BadSignature.
        let unknown = b"DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD";
        let token = mint(unknown, "ABCDEF", "device-1", 1_700_000_000).unwrap();

        let err_with_prev = verify_with_secrets(
            &new_secret(),
            Some(&old_secret()),
            &token,
            "ABCDEF",
            1_700_000_000,
        )
        .unwrap_err();
        let err_without_prev =
            verify_with_secrets(&new_secret(), None, &token, "ABCDEF", 1_700_000_000).unwrap_err();
        assert!(matches!(err_with_prev, TokenError::BadSignature));
        assert!(matches!(err_without_prev, TokenError::BadSignature));
    }
}
