//! The `PWNQ` quote envelope — the only bytes a market maker ever signs.
//!
//! ```text
//!   [0..4)     magic u32 = b"PWNQ"
//!   [4..68)    ed25519 signature (64B) — signs the PAYLOAD BYTES ONLY
//!   [68..100)  the maker's quoting public key (32B)
//!   [100..102) payload_len u16, little-endian
//!   [102..)    payload
//! ```
//!
//! The payload, little-endian, exactly 64 bytes:
//!
//! ```text
//!   [0..1)   version    u8     — QUOTE_VERSION; SIGNED, and it selects the layout
//!   [1..16)  _reserved  15B    — MUST be zero
//!   [16..48) asset      Pubkey — the card being appraised
//!   [48..56) amount_usd u64    — the appraisal, 6-decimal micro-USD
//!   [56..64) expiry_ts  i64    — unix seconds
//! ```
//!
//! The signature covers the 64 payload bytes and NOTHING else — not the magic,
//! not the signer key, not the program id, not the borrower. A quote is
//! therefore replayable against anything that adopts this layout, which is why
//! the program bounds `expiry_ts` on BOTH sides rather than only checking that
//! it is in the future: `now < expiry_ts <= now + MAX_QUOTE_LIFETIME_SECS`.
//! There is no nonce. Treat a signed envelope as a bearer instrument: anyone
//! holding it can present it, at any instant in the fifteen minutes before the
//! expiry you signed.
//!
//! The version byte is inside the signed span, so the MAKER declares what these
//! bytes mean; `payload_len` at envelope offset 100 is not, so a relayer
//! cannot. The 15 reserved bytes are the space a future payload version takes,
//! and both this decoder and the program reject a nonzero one.
//!
//! Deliberately implemented here rather than pulled from a shared crate, so
//! this bot has no dependency that would not exist outside the monorepo. The
//! conformance test in `tests/conformance.rs` pins it byte-for-byte against the
//! protocol's own encoder.

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Signature, Signer};

pub const QUOTE_MAGIC: [u8; 4] = *b"PWNQ";

pub const SIGNATURE_OFFSET: usize = 4;
pub const SIGNATURE_LEN: usize = 64;
pub const PUBKEY_OFFSET: usize = 68;
pub const PUBKEY_LEN: usize = 32;
pub const PAYLOAD_LEN_OFFSET: usize = 100;
pub const ENVELOPE_HEADER: usize = 102;

const _: () = assert!(PUBKEY_OFFSET == SIGNATURE_OFFSET + SIGNATURE_LEN);
const _: () = assert!(PAYLOAD_LEN_OFFSET == PUBKEY_OFFSET + PUBKEY_LEN);
const _: () = assert!(ENVELOPE_HEADER == PAYLOAD_LEN_OFFSET + 2);

/// The only payload layout the program accepts.
pub const QUOTE_VERSION: u8 = 1;

/// The widest gap the program will accept between the chain clock and a quote's
/// `expiry_ts`, in seconds. `LOAN_OPEN` requires
/// `now < expiry_ts <= now + MAX_QUOTE_LIFETIME_SECS`, and that is the whole
/// replay defence.
///
/// A maker does not aim at this number. The API stamps a tighter `maxExpiryTs`
/// on every announcement — deliberately inside this bound, so the chain clock
/// can lag wall-clock without an honest open bouncing — and that is what the
/// bot signs against. This constant is here so a configured TTL that could
/// never be honoured is refused at startup rather than clamped every round.
pub const MAX_QUOTE_LIFETIME_SECS: i64 = 900;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuotePayload {
    pub asset: Pubkey,
    /// The APPRAISAL, 6-decimal micro-USD. Not the principal.
    pub amount_usd: u64,
    pub expiry_ts: i64,
}

impl QuotePayload {
    pub const LEN: usize = 64;
    pub const VERSION_OFFSET: usize = 0;
    pub const RESERVED_OFFSET: usize = 1;
    pub const ASSET_OFFSET: usize = 16;
    pub const AMOUNT_USD_OFFSET: usize = 48;
    pub const EXPIRY_TS_OFFSET: usize = 56;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut out = [0u8; Self::LEN];
        out[Self::VERSION_OFFSET] = QUOTE_VERSION;
        out[Self::ASSET_OFFSET..Self::AMOUNT_USD_OFFSET].copy_from_slice(self.asset.as_ref());
        out[Self::AMOUNT_USD_OFFSET..Self::EXPIRY_TS_OFFSET]
            .copy_from_slice(&self.amount_usd.to_le_bytes());
        out[Self::EXPIRY_TS_OFFSET..Self::LEN].copy_from_slice(&self.expiry_ts.to_le_bytes());
        out
    }

    /// `None` for anything that is not exactly [`QuotePayload::LEN`] bytes, or
    /// that does not carry [`QUOTE_VERSION`] with its reserved bytes zeroed. A
    /// short, overlong or differently-versioned payload is a different wire
    /// format, not a quote with defaults, and the program rejects it outright.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::LEN
            || bytes[Self::VERSION_OFFSET] != QUOTE_VERSION
            || bytes[Self::RESERVED_OFFSET..Self::ASSET_OFFSET]
                .iter()
                .any(|b| *b != 0)
        {
            return None;
        }
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        Some(Self {
            asset: Pubkey::new_from_array(
                bytes[Self::ASSET_OFFSET..Self::AMOUNT_USD_OFFSET]
                    .try_into()
                    .unwrap(),
            ),
            amount_usd: word(Self::AMOUNT_USD_OFFSET),
            expiry_ts: word(Self::EXPIRY_TS_OFFSET) as i64,
        })
    }

    /// Would `LOAN_OPEN` accept this expiry against a chain clock reading
    /// `now_ts`? Both halves of the program's bound, in one call. The window is
    /// [`MAX_QUOTE_LIFETIME_SECS`] wide and ENDS at `expiry_ts`, wherever the
    /// maker put it — an expiry far in the future is a DELAYED quote, not a
    /// long-lived one.
    pub fn is_live_at(&self, now_ts: i64) -> bool {
        self.expiry_ts > now_ts && self.expiry_ts <= now_ts.saturating_add(MAX_QUOTE_LIFETIME_SECS)
    }
}

/// Assemble an envelope from a signature produced anywhere — an HSM, a remote
/// signer, a service that never lets this process near the key.
pub fn envelope_from_parts(
    signer: &Pubkey,
    signature: &[u8; SIGNATURE_LEN],
    payload: &QuotePayload,
) -> Vec<u8> {
    let encoded = payload.encode();
    let mut bytes = Vec::with_capacity(ENVELOPE_HEADER + encoded.len());
    bytes.extend_from_slice(&QUOTE_MAGIC);
    bytes.extend_from_slice(signature);
    bytes.extend_from_slice(signer.as_ref());
    bytes.extend_from_slice(&(encoded.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&encoded);
    bytes
}

/// Sign a payload and wrap it. Takes a [`Signer`] so a hardware or remote
/// signer drops in unchanged.
pub fn sign_envelope(payload: &QuotePayload, signer: &dyn Signer) -> Vec<u8> {
    let signature = signer.sign_message(&payload.encode());
    envelope_from_parts(
        &signer.pubkey(),
        signature.as_ref().try_into().expect("ed25519 signature"),
        payload,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedEnvelope {
    pub signer: Pubkey,
    pub signature: [u8; SIGNATURE_LEN],
    pub payload: QuotePayload,
}

impl ParsedEnvelope {
    /// Does the signature really cover this payload under this key? Run before
    /// submitting: the API and the program both check it, and finding out here
    /// costs nothing.
    pub fn verify(&self) -> bool {
        Signature::from(self.signature).verify(self.signer.as_ref(), &self.payload.encode())
    }
}

pub fn parse_envelope(bytes: &[u8]) -> Option<ParsedEnvelope> {
    if bytes.len() < ENVELOPE_HEADER || bytes[..4] != QUOTE_MAGIC {
        return None;
    }
    let payload_len = u16::from_le_bytes(
        bytes[PAYLOAD_LEN_OFFSET..ENVELOPE_HEADER]
            .try_into()
            .unwrap(),
    ) as usize;
    if bytes.len() != ENVELOPE_HEADER + payload_len {
        return None;
    }
    Some(ParsedEnvelope {
        signer: Pubkey::new_from_array(
            bytes[PUBKEY_OFFSET..PAYLOAD_LEN_OFFSET].try_into().unwrap(),
        ),
        signature: bytes[SIGNATURE_OFFSET..PUBKEY_OFFSET].try_into().unwrap(),
        payload: QuotePayload::decode(&bytes[ENVELOPE_HEADER..])?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::Keypair;

    fn payload() -> QuotePayload {
        QuotePayload {
            asset: Pubkey::new_from_array([7u8; 32]),
            amount_usd: 1_250_000_000,
            expiry_ts: 1_754_870_400,
        }
    }

    #[test]
    fn the_payload_is_version_reserved_asset_amount_expiry_little_endian_in_64_bytes() {
        let encoded = payload().encode();
        assert_eq!(encoded.len(), 64);
        assert_eq!(encoded[0], QUOTE_VERSION);
        assert_eq!(&encoded[1..16], &[0u8; 15]);
        assert_eq!(&encoded[16..48], &[7u8; 32]);
        assert_eq!(&encoded[48..56], &1_250_000_000u64.to_le_bytes());
        assert_eq!(&encoded[56..64], &1_754_870_400i64.to_le_bytes());
        assert_eq!(QuotePayload::decode(&encoded), Some(payload()));
    }

    #[test]
    fn a_payload_of_any_other_length_is_not_a_quote() {
        let encoded = payload().encode();
        assert_eq!(QuotePayload::decode(&encoded[..63]), None);
        let mut long = encoded.to_vec();
        long.push(0);
        assert_eq!(QuotePayload::decode(&long), None);
    }

    /// The version byte and the reserved bytes are what a future payload
    /// version claims. A decoder that shrugged at either would read those bytes
    /// as a v1 quote carrying terms nobody chose.
    #[test]
    fn a_payload_with_the_wrong_version_or_dirty_reserved_bytes_does_not_decode() {
        let encoded = payload().encode();
        for version in [0u8, 2, 255] {
            let mut bytes = encoded;
            bytes[QuotePayload::VERSION_OFFSET] = version;
            assert_eq!(QuotePayload::decode(&bytes), None, "version {version}");
        }
        for at in QuotePayload::RESERVED_OFFSET..QuotePayload::ASSET_OFFSET {
            let mut bytes = encoded;
            bytes[at] = 1;
            assert_eq!(QuotePayload::decode(&bytes), None, "reserved byte {at}");
        }
    }

    /// The program's whole replay defence, mirrored so a maker can see what it
    /// signed. The window is `MAX_QUOTE_LIFETIME_SECS` wide and ENDS at the
    /// expiry — a far-future expiry is a quote that is dead until then, not one
    /// that lives longer.
    #[test]
    fn a_quote_is_live_only_in_the_ceiling_wide_window_that_ends_at_its_expiry() {
        let payload = payload();
        let expiry = payload.expiry_ts;

        assert!(payload.is_live_at(expiry - 1));
        assert!(payload.is_live_at(expiry - MAX_QUOTE_LIFETIME_SECS));
        assert!(!payload.is_live_at(expiry - MAX_QUOTE_LIFETIME_SECS - 1));
        assert!(!payload.is_live_at(expiry), "expiry_ts > now is strict");
        assert!(!payload.is_live_at(expiry + 1));

        let far = QuotePayload {
            expiry_ts: expiry + 365 * 86_400,
            ..payload
        };
        assert!(!far.is_live_at(expiry));
        assert!(far.is_live_at(far.expiry_ts - 1));
    }

    #[test]
    fn the_envelope_is_magic_signature_pubkey_length_then_payload() {
        let signer = Pubkey::new_from_array([3u8; 32]);
        let bytes = envelope_from_parts(&signer, &[0xab; 64], &payload());
        assert_eq!(bytes.len(), ENVELOPE_HEADER + QuotePayload::LEN);
        assert_eq!(&bytes[0..4], b"PWNQ");
        assert_eq!(&bytes[SIGNATURE_OFFSET..PUBKEY_OFFSET], &[0xab; 64]);
        assert_eq!(&bytes[PUBKEY_OFFSET..PAYLOAD_LEN_OFFSET], signer.as_ref());
        assert_eq!(
            &bytes[PAYLOAD_LEN_OFFSET..ENVELOPE_HEADER],
            &64u16.to_le_bytes()
        );
        assert_eq!(&bytes[ENVELOPE_HEADER..], &payload().encode());
    }

    #[test]
    fn a_signed_envelope_round_trips_and_verifies_under_its_own_key() {
        let key = Keypair::new();
        let bytes = sign_envelope(&payload(), &key);
        let parsed = parse_envelope(&bytes).expect("parses");
        assert_eq!(parsed.signer, key.pubkey());
        assert_eq!(parsed.payload, payload());
        assert!(parsed.verify());
    }

    /// The signature covers the payload bytes only, so every field of the
    /// appraisal is protected and a flipped bit anywhere in it is caught before
    /// the quote is ever sent.
    #[test]
    fn a_single_flipped_bit_in_the_payload_fails_verification() {
        let key = Keypair::new();
        let bytes = sign_envelope(&payload(), &key);
        for at in [
            QuotePayload::ASSET_OFFSET,
            QuotePayload::AMOUNT_USD_OFFSET,
            QuotePayload::EXPIRY_TS_OFFSET,
        ] {
            let mut tampered = bytes.clone();
            tampered[ENVELOPE_HEADER + at] ^= 0x01;
            assert!(!parse_envelope(&tampered).unwrap().verify());
        }

        // The version and reserved bytes fail earlier, at the decode, which is
        // the stricter answer rather than a weaker one.
        for at in [QuotePayload::VERSION_OFFSET, QuotePayload::RESERVED_OFFSET] {
            let mut tampered = bytes.clone();
            tampered[ENVELOPE_HEADER + at] ^= 0x01;
            assert!(parse_envelope(&tampered).is_none());
        }
    }

    #[test]
    fn a_wrong_magic_or_a_lying_length_field_does_not_parse() {
        let key = Keypair::new();
        let bytes = sign_envelope(&payload(), &key);

        let mut wrong_magic = bytes.clone();
        wrong_magic[0] = b'X';
        assert_eq!(parse_envelope(&wrong_magic), None);

        let mut lying = bytes.clone();
        lying[PAYLOAD_LEN_OFFSET..ENVELOPE_HEADER].copy_from_slice(&65u16.to_le_bytes());
        assert_eq!(parse_envelope(&lying), None);

        let mut padded = bytes.clone();
        padded.push(0);
        assert_eq!(parse_envelope(&padded), None, "trailing bytes are refused");
    }
}
