//! Short-lived stream tokens: what a LAN client presents to open a
//! streaming session.  Minted through the loopback local API (behind the
//! install token), so the install token itself never leaves the host.

use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use std::collections::HashMap;

/// Shortest lifetime a token gets (a client must be able to connect).
pub const MIN_TTL: Duration = Duration::seconds(30);
/// Longest lifetime a token gets; the holder refreshes before expiry.
/// Safe range 5..=120 minutes.
pub const MAX_TTL: Duration = Duration::hours(1);

/// A minted token and what it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamGrant {
    pub token: String,
    pub model: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenRejection {
    #[error("unknown stream token")]
    Unknown,
    #[error("stream token expired")]
    Expired,
}

#[derive(Debug, Clone)]
struct Grant {
    model: String,
    expires_at: DateTime<Utc>,
}

/// Live tokens, keyed by the SHA-256 of the token.
#[derive(Debug, Default)]
pub struct StreamTokens {
    grants: Mutex<HashMap<String, Grant>>,
}

impl StreamTokens {
    /// Mint a token for `model`, valid for `ttl` (clamped) from `now`.
    pub fn mint(&self, model: &str, ttl: Duration, now: DateTime<Utc>) -> StreamGrant {
        let ttl = ttl.clamp(MIN_TTL, MAX_TTL);
        let token = crate::secrets::new_secret_hex();
        let expires_at = now + ttl;
        let mut grants = self.grants.lock();
        grants.retain(|_, g| g.expires_at > now);
        grants.insert(
            crate::secrets::sha256_hex(&token),
            Grant {
                model: model.to_string(),
                expires_at,
            },
        );
        StreamGrant {
            token,
            model: model.to_string(),
            expires_at,
        }
    }

    /// The model `token` opens, if it is live at `now`.
    pub fn check(&self, token: &str, now: DateTime<Utc>) -> Result<String, TokenRejection> {
        let grants = self.grants.lock();
        match grants.get(&crate::secrets::sha256_hex(token)) {
            None => Err(TokenRejection::Unknown),
            Some(g) if g.expires_at <= now => Err(TokenRejection::Expired),
            Some(g) => Ok(g.model.clone()),
        }
    }

    /// Tokens held (live or not yet pruned).
    pub fn len(&self) -> usize {
        self.grants.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};

    fn t0() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 26, 12, 0, 0).unwrap()
    }

    #[test]
    fn a_minted_token_opens_its_model_until_it_expires() {
        let tokens = StreamTokens::default();
        let grant = tokens.mint("stt-a", Duration::minutes(10), t0());
        assert_eq!(grant.model, "stt-a");
        assert_eq!(grant.expires_at, t0() + Duration::minutes(10));
        assert_eq!(grant.token.len(), 64, "256 bits, hex");
        assert_eq!(
            tokens.check(&grant.token, t0() + Duration::minutes(9)),
            Ok("stt-a".to_string())
        );
        assert_eq!(
            tokens.check(&grant.token, t0() + Duration::minutes(10)),
            Err(TokenRejection::Expired)
        );
    }

    #[test]
    fn unknown_tokens_are_rejected() {
        let tokens = StreamTokens::default();
        assert_eq!(tokens.check("nope", t0()), Err(TokenRejection::Unknown));
    }

    #[test]
    fn every_mint_is_a_new_token() {
        let tokens = StreamTokens::default();
        let a = tokens.mint("m", Duration::minutes(1), t0());
        let b = tokens.mint("m", Duration::minutes(1), t0());
        assert_ne!(a.token, b.token);
        assert!(
            tokens.check(&a.token, t0()).is_ok(),
            "older tokens stay valid"
        );
    }

    #[test]
    fn ttl_is_clamped_to_the_allowed_range() {
        let tokens = StreamTokens::default();
        let long = tokens.mint("m", Duration::days(3), t0());
        assert_eq!(long.expires_at, t0() + MAX_TTL);
        let short = tokens.mint("m", Duration::seconds(-5), t0());
        assert_eq!(short.expires_at, t0() + MIN_TTL);
    }

    #[test]
    fn expired_tokens_are_pruned_on_mint() {
        let tokens = StreamTokens::default();
        tokens.mint("m", Duration::minutes(1), t0());
        tokens.mint("m", Duration::minutes(1), t0() + Duration::hours(1));
        assert_eq!(tokens.len(), 1);
    }

    #[test]
    fn the_store_never_holds_the_token_itself() {
        let tokens = StreamTokens::default();
        let grant = tokens.mint("m", Duration::minutes(1), t0());
        assert!(!format!("{tokens:?}").contains(&grant.token));
    }

    #[test]
    fn rejections_read_well() {
        assert_eq!(TokenRejection::Unknown.to_string(), "unknown stream token");
        assert_eq!(TokenRejection::Expired.to_string(), "stream token expired");
    }
}
