//! Market-maker credentials.
//!
//! Three headers, minted fresh per request:
//!
//! * `x-pawn-mm` — your seed authority pubkey, base58. IDENTITY, not a signing
//!   key.
//! * `x-pawn-message` — the exact message signed, `{prefix}{unix_timestamp}`.
//! * `x-pawn-signature` — base58 ed25519 over that message, by the key
//!   registered as `MarketMaker.quote_pubkey`.
//!
//! # Scope lives entirely in the prefix
//!
//! [`STREAM_PREFIX`] and [`QUOTE_PREFIX`] are not decoration and are NOT
//! interchangeable. A credential minted to subscribe to the announcement stream
//! cannot submit a quote, which is only true because the two messages differ.
//! Mint the one that matches what you are about to do.
//!
//! A credential is accepted for five minutes and there is no replay table, so
//! it is REPLAYABLE inside that window by anyone who captures it. What makes a
//! replay worthless is downstream — the ten-second auction, and one answer per
//! maker per round — not this header. A captured credential still cannot forge
//! an envelope, which is the only thing worth anything. Ship them over TLS and
//! do not log them.

use solana_sdk::signature::Signer;

/// Scope: subscribe to the RFQ announcement stream, and poll the open list.
pub const STREAM_PREFIX: &str = "imperial-pawn-mm-stream:";
/// Scope: submit a quote.
pub const QUOTE_PREFIX: &str = "imperial-pawn-mm-quote:";

pub const MM_AUTHORITY_HEADER: &str = "x-pawn-mm";
pub const MM_MESSAGE_HEADER: &str = "x-pawn-message";
pub const MM_SIGNATURE_HEADER: &str = "x-pawn-signature";

/// How long the API accepts a credential for. Mirrored here only so the bot can
/// refuse to reuse one it has held too long.
pub const MAX_AGE_SECS: i64 = 300;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    pub authority: String,
    pub message: String,
    pub signature: String,
}

pub fn message_for(prefix: &str, unix_timestamp: i64) -> String {
    format!("{prefix}{unix_timestamp}")
}

/// Mint a credential for one scope, at one instant.
pub fn credential(
    authority: &solana_sdk::pubkey::Pubkey,
    quote_key: &dyn Signer,
    prefix: &str,
    unix_timestamp: i64,
) -> Credential {
    let message = message_for(prefix, unix_timestamp);
    let signature = quote_key.sign_message(message.as_bytes());
    Credential {
        authority: authority.to_string(),
        message,
        signature: signature.to_string(),
    }
}

impl Credential {
    pub fn apply(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header(MM_AUTHORITY_HEADER, &self.authority)
            .header(MM_MESSAGE_HEADER, &self.message)
            .header(MM_SIGNATURE_HEADER, &self.signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::{Keypair, Signature};
    use std::str::FromStr;

    /// The scopes are separate messages, so a credential scraped off a
    /// long-lived stream connection cannot price a card. If these ever became
    /// equal the separation would be imaginary.
    #[test]
    fn the_stream_and_quote_scopes_are_different_messages() {
        assert_ne!(STREAM_PREFIX, QUOTE_PREFIX);
        assert_ne!(
            message_for(STREAM_PREFIX, 100),
            message_for(QUOTE_PREFIX, 100)
        );
    }

    #[test]
    fn the_message_is_the_prefix_immediately_followed_by_the_timestamp() {
        assert_eq!(
            message_for(QUOTE_PREFIX, 1_754_870_400),
            "imperial-pawn-mm-quote:1754870400"
        );
    }

    /// The header carries the AUTHORITY while the signature is by the QUOTE
    /// key. Conflating them is the mistake that makes a correctly configured
    /// bot look unregistered.
    #[test]
    fn the_identity_header_is_the_authority_and_the_signature_is_by_the_quote_key() {
        let authority = Pubkey::new_unique();
        let quote_key = Keypair::new();
        let credential = credential(&authority, &quote_key, QUOTE_PREFIX, 1_700_000_000);

        assert_eq!(credential.authority, authority.to_string());
        assert_ne!(credential.authority, quote_key.pubkey().to_string());

        let signature = Signature::from_str(&credential.signature).unwrap();
        assert!(signature.verify(quote_key.pubkey().as_ref(), credential.message.as_bytes()));
        assert!(!signature.verify(authority.as_ref(), credential.message.as_bytes()));
    }
}
