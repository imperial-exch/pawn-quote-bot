//! Your own registration, read straight off the cluster.
//!
//! The `MarketMaker` account at `[b"mm", authority]` is the whole credit
//! system: it is the approval list, the exposure ledger and the public
//! reputation record at once. Nothing here writes to it — registration and cap
//! changes are the pool authority's `SET_MARKET_MAKER`, not a maker's.
//!
//! Reading it at startup catches, in one request, the three misconfigurations
//! that otherwise present as an unexplained wall of 401s:
//!
//! * the account does not exist — this authority was never registered;
//! * `status != APPROVED` — registered but not yet allowed to price;
//! * `quote_pubkey` is not the key in `maker.quote_keypair` — every signature
//!   this bot produces will be refused, and the fix is a key rotation or a
//!   different file.
//!
//! Layout mirrored by hand from the program, as every client of it does.

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use solana_sdk::pubkey::Pubkey;

/// SYNC: `imperial_pawn::state::MARKET_MAKER_SEED`.
pub const MARKET_MAKER_SEED: &[u8] = b"mm";

/// SYNC: `imperial_pawn::state::account_tag::MARKET_MAKER`.
pub const ACCOUNT_TAG_MARKET_MAKER: u8 = 4;

pub const MM_STATUS_PENDING: u8 = 0;
pub const MM_STATUS_APPROVED: u8 = 1;
pub const MM_STATUS_SUSPENDED: u8 = 2;

/// `MarketMaker` field offsets. SYNC: `imperial_pawn::state::MarketMaker`.
///
/// ```text
///   0..1   discriminator      u8 = ACCOUNT_TAG_MARKET_MAKER
///   1..2   bump               u8
///   2..3   status             u8
///   3..8   _padding
///   8..40  authority          Pubkey
///  40..72  quote_pubkey       Pubkey
///  72..80  max_outstanding_usd u64
///  80..88  outstanding_usd    u64
///  88..96  quotes_won         u64
///  96..104 puts_honored       u64
/// 104..112 puts_walked        u64
/// 112..120 walked_usd         u64
/// 120..128 approved_at        i64
/// 128..192 _reserved
/// ```
pub const MARKET_MAKER_LEN: usize = 192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarketMaker {
    pub status: u8,
    pub authority: Pubkey,
    pub quote_pubkey: Pubkey,
    pub max_outstanding_usd: u64,
    pub outstanding_usd: u64,
    pub quotes_won: u64,
    pub puts_honored: u64,
    pub puts_walked: u64,
    pub walked_usd: u64,
}

impl MarketMaker {
    pub fn is_approved(&self) -> bool {
        self.status == MM_STATUS_APPROVED
    }

    pub fn headroom_usd(&self) -> u64 {
        self.max_outstanding_usd
            .saturating_sub(self.outstanding_usd)
    }

    pub fn status_name(&self) -> &'static str {
        match self.status {
            MM_STATUS_PENDING => "PENDING",
            MM_STATUS_APPROVED => "APPROVED",
            MM_STATUS_SUSPENDED => "SUSPENDED",
            _ => "UNKNOWN",
        }
    }
}

/// Decode a `MarketMaker`.
///
/// `None` for a short account or one carrying a different type tag. An account
/// that does not exist must never decode as a zeroed one: status 0 is PENDING,
/// so a zero-filled read would look like a real, unapproved maker rather than
/// like nothing at all.
pub fn parse_market_maker(data: &[u8]) -> Option<MarketMaker> {
    if data.len() < MARKET_MAKER_LEN || data[0] != ACCOUNT_TAG_MARKET_MAKER {
        return None;
    }
    let pubkey = |at: usize| Pubkey::new_from_array(data[at..at + 32].try_into().unwrap());
    let word = |at: usize| u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
    Some(MarketMaker {
        status: data[2],
        authority: pubkey(8),
        quote_pubkey: pubkey(40),
        max_outstanding_usd: word(72),
        outstanding_usd: word(80),
        quotes_won: word(88),
        puts_honored: word(96),
        puts_walked: word(104),
        walked_usd: word(112),
    })
}

pub fn derive_market_maker(authority: &Pubkey, program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[MARKET_MAKER_SEED, authority.as_ref()], program_id).0
}

/// Fetch and decode the maker's registration. `Ok(None)` means the account does
/// not exist — this authority has never been registered.
pub async fn fetch_market_maker(
    http: &reqwest::Client,
    rpc_url: &str,
    authority: &Pubkey,
    program_id: &Pubkey,
) -> Result<Option<MarketMaker>> {
    let pda = derive_market_maker(authority, program_id);
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "pawn-quote-bot",
        "method": "getAccountInfo",
        "params": [pda.to_string(), { "encoding": "base64", "commitment": "confirmed" }],
    });
    let payload: serde_json::Value = http
        .post(rpc_url)
        .json(&body)
        .send()
        .await?
        .json()
        .await
        .context("the RPC answered getAccountInfo with something that is not JSON")?;

    if let Some(error) = payload.get("error") {
        bail!("getAccountInfo failed: {error}");
    }
    let value = &payload["result"]["value"];
    if value.is_null() {
        return Ok(None);
    }
    let encoded = value["data"][0]
        .as_str()
        .context("getAccountInfo returned an account without base64 data")?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("the account's data is not base64")?;
    // Present but undecodable is NOT "not registered": it is a program id
    // pointing at a different deployment, or a layout that moved.
    parse_market_maker(&data).map(Some).context(
        "the account at the market-maker PDA is not a MarketMaker — check maker.program_id",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(status: u8, quote_pubkey: Pubkey) -> Vec<u8> {
        let mut data = vec![0u8; MARKET_MAKER_LEN];
        data[0] = ACCOUNT_TAG_MARKET_MAKER;
        data[1] = 255;
        data[2] = status;
        data[8..40].copy_from_slice(Pubkey::new_from_array([1u8; 32]).as_ref());
        data[40..72].copy_from_slice(quote_pubkey.as_ref());
        data[72..80].copy_from_slice(&1_000_000_000u64.to_le_bytes());
        data[80..88].copy_from_slice(&400_000_000u64.to_le_bytes());
        data[104..112].copy_from_slice(&2u64.to_le_bytes());
        data
    }

    #[test]
    fn an_approved_registration_decodes_every_field_the_bot_gates_on() {
        let quote_pubkey = Pubkey::new_unique();
        let mm = parse_market_maker(&account(MM_STATUS_APPROVED, quote_pubkey)).unwrap();
        assert!(mm.is_approved());
        assert_eq!(mm.quote_pubkey, quote_pubkey);
        assert_eq!(mm.max_outstanding_usd, 1_000_000_000);
        assert_eq!(mm.outstanding_usd, 400_000_000);
        assert_eq!(mm.headroom_usd(), 600_000_000);
        assert_eq!(mm.puts_walked, 2);
    }

    /// An absent account and a zero-filled one are different answers. Status 0
    /// is PENDING, so decoding zeros would report an unregistered authority as
    /// a real maker awaiting approval.
    #[test]
    fn a_zeroed_or_mistagged_account_does_not_decode_as_a_pending_maker() {
        assert_eq!(parse_market_maker(&[0u8; MARKET_MAKER_LEN]), None);

        let mut wrong_tag = account(MM_STATUS_APPROVED, Pubkey::new_unique());
        wrong_tag[0] = 5;
        assert_eq!(parse_market_maker(&wrong_tag), None);

        let short = account(MM_STATUS_APPROVED, Pubkey::new_unique());
        assert_eq!(parse_market_maker(&short[..MARKET_MAKER_LEN - 1]), None);
    }

    #[test]
    fn pending_and_suspended_are_named_and_neither_is_approved() {
        for (status, name) in [
            (MM_STATUS_PENDING, "PENDING"),
            (MM_STATUS_SUSPENDED, "SUSPENDED"),
        ] {
            let mm = parse_market_maker(&account(status, Pubkey::new_unique())).unwrap();
            assert!(!mm.is_approved());
            assert_eq!(mm.status_name(), name);
        }
    }

    /// The PDA is seeded on the AUTHORITY and derived under the configured
    /// program id, so mainnet and staging resolve to different accounts for the
    /// same maker. That is exactly why the program id is never hardcoded.
    #[test]
    fn the_pda_is_seeded_on_the_authority_and_moves_with_the_program_id() {
        let authority = Pubkey::new_unique();
        let mainnet = Pubkey::new_unique();
        let staging = Pubkey::new_unique();

        assert_eq!(
            derive_market_maker(&authority, &mainnet),
            derive_market_maker(&authority, &mainnet)
        );
        assert_ne!(
            derive_market_maker(&authority, &mainnet),
            derive_market_maker(&authority, &staging)
        );
        assert_ne!(
            derive_market_maker(&authority, &mainnet),
            derive_market_maker(&Pubkey::new_unique(), &mainnet)
        );
    }
}
