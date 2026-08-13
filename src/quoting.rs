//! One round, from announcement to signed envelope.
//!
//! The order is deliberate and every step is cheap before the expensive one:
//! eligibility (free) before metadata (a network read) before pricing (possibly
//! a subprocess) before the caps (free) before the signature.

use anyhow::Result;
use solana_sdk::signature::Signer;

use crate::config::{Config, Standard};
use crate::das::CardMetadata;
use crate::envelope::{sign_envelope, QuotePayload};
use crate::pricing::{Priced, Pricer};
use crate::rfq::{check_payload, expiry_for, size_quote, Caps, RfqAnnouncement, Sized, Skip};

/// A quote the bot is willing to sign, and what it costs.
#[derive(Clone, Debug)]
pub struct Quote {
    pub payload: QuotePayload,
    pub envelope: Vec<u8>,
    pub principal_usd: u64,
    pub collection: Option<String>,
}

/// Is this a card this maker quotes at all? Free, so it runs before the
/// metadata read that would otherwise be spent on a card that was never
/// eligible.
///
/// The standard comes from the ANNOUNCEMENT, which is the protocol's own record
/// of how the card was escrowed. DAS reports it too and the two are compared
/// once the metadata is in hand — if they disagree, something is wrong enough
/// to walk away from.
pub fn eligible_by_announcement(rfq: &RfqAnnouncement, config: &Config) -> Result<(), Skip> {
    let standard = Standard::from_wire(rfq.standard)
        .ok_or_else(|| Skip::Ineligible(format!("unknown collateral standard {}", rfq.standard)))?;
    if !config.allowed_standards().contains(&standard) {
        return Err(Skip::Ineligible(format!(
            "collateral standard {standard:?} is not in eligibility.standards"
        )));
    }
    Ok(())
}

/// The rest of eligibility, once the card is known.
pub fn eligible_by_card(
    rfq: &RfqAnnouncement,
    card: &CardMetadata,
    config: &Config,
) -> Result<(), Skip> {
    if card.compressed {
        return Err(Skip::Ineligible(
            "the asset is compressed, so nothing can take custody of it".to_string(),
        ));
    }
    if card.frozen {
        return Err(Skip::Ineligible("the asset is frozen".to_string()));
    }
    if card.standard != rfq.standard {
        return Err(Skip::Metadata(format!(
            "the RFQ says standard {} and the asset reads as {} — refusing to price a card \
             whose shape is in dispute",
            rfq.standard, card.standard
        )));
    }
    if !config.eligibility.collections.is_empty() {
        let allowed = card
            .collection
            .as_ref()
            .and_then(|c| c.parse().ok())
            .is_some_and(|c| config.eligibility.collections.contains(&c));
        if !allowed {
            return Err(Skip::Ineligible(format!(
                "collection {:?} is not in eligibility.collections",
                card.collection
            )));
        }
    }
    if !config.eligibility.grading.is_empty() {
        let grading = card.grading_company.as_deref().unwrap_or("");
        if !config
            .eligibility
            .grading
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(grading))
        {
            return Err(Skip::Ineligible(format!(
                "grading {grading:?} is not in eligibility.grading"
            )));
        }
    }
    Ok(())
}

/// Everything after the price: clamp to the caps, build the payload, re-run the
/// API's own checks locally, sign.
///
/// `headroom_usd` and `collection_headroom_usd` come from the ledger and are
/// passed in rather than read, so the whole decision is testable without a
/// cluster or a clock.
#[allow(clippy::too_many_arguments)]
pub fn build_quote(
    rfq: &RfqAnnouncement,
    card: &CardMetadata,
    appraisal_usd: u64,
    config: &Config,
    headroom_usd: u64,
    collection_headroom_usd: u64,
    now: i64,
    quote_key: &dyn Signer,
) -> Result<Quote, Skip> {
    let Sized {
        amount_usd,
        principal_usd,
    } = size_quote(
        appraisal_usd,
        &Caps {
            open_ltv_bps: rfq.open_ltv_bps,
            max_principal_usd: rfq.max_principal_usd,
            headroom_usd,
            collection_headroom_usd,
            min_appraisal_usd: config.limits.min_appraisal_usd.micros(),
            max_appraisal_usd: config.limits.max_appraisal_usd.micros(),
        },
    )?;

    let asset = rfq
        .asset_pubkey()
        .ok_or_else(|| Skip::Metadata(format!("the RFQ's asset {} is not a pubkey", rfq.asset)))?;

    let payload = QuotePayload {
        asset,
        amount_usd,
        // Clamped into the round's announced window at both ends. The window is
        // the program's replay bound and neither end is negotiable.
        expiry_ts: expiry_for(
            now,
            config.quote.expiry_secs,
            rfq.min_expiry_ts,
            rfq.max_expiry_ts,
        ),
    };

    // The API runs these and so does the program. Failing here costs nothing;
    // failing there costs the round, and the window is ten seconds.
    check_payload(&payload, rfq, principal_usd, headroom_usd).map_err(Skip::Metadata)?;

    Ok(Quote {
        envelope: sign_envelope(&payload, quote_key),
        payload,
        principal_usd,
        collection: card.collection.clone(),
    })
}

/// Price one card. Separated from [`build_quote`] so an engine failure is an
/// error the caller logs, while a decline is a [`Skip`] like any other.
pub async fn price(
    pricer: &dyn Pricer,
    rfq: &RfqAnnouncement,
    card: &CardMetadata,
) -> Result<Result<u64, Skip>> {
    Ok(match pricer.price(rfq, card).await? {
        Priced::Appraisal(amount_usd) => Ok(amount_usd),
        Priced::Skip(reason) => Err(Skip::Priced(reason)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{parse_envelope, MAX_QUOTE_LIFETIME_SECS};
    use solana_sdk::signature::Keypair;

    fn usd(dollars: u64) -> u64 {
        dollars * 1_000_000
    }

    const ASSET: &str = "2o1f3ekWgw8h3bXG418qiE6Dx6nSuoj8H1W3Q5Bavy3y";

    fn config(extra: &str) -> Config {
        config_with_expiry_secs(120, extra)
    }

    fn config_with_expiry_secs(expiry_secs: i64, extra: &str) -> Config {
        let text = format!(
            r#"
[maker]
program_id = "PawnAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
api_base = "https://example.test/api/v1"
authority = "11111111111111111111111111111111"
quote_keypair = "./key.json"
rpc_url = "https://rpc.example.test"

[pricing]
engine = "face_value_pct"
face_value_pct = 60

[limits]
min_appraisal_usd = 25
max_appraisal_usd = 2000
max_outstanding_usd = 800
max_per_collection_usd = 500

[quote]
expiry_secs = {expiry_secs}
{extra}
"#
        );
        toml::from_str(&text).expect("test config parses")
    }

    fn rfq() -> RfqAnnouncement {
        RfqAnnouncement {
            rfq_id: "rfq-1".into(),
            asset: ASSET.into(),
            standard: 2,
            opened_at: 1_000,
            closes_at: 1_010,
            min_expiry_ts: 1_070,
            max_expiry_ts: 1_600,
            open_ltv_bps: 6_000,
            max_principal_usd: usd(5_000),
        }
    }

    fn card() -> CardMetadata {
        CardMetadata {
            asset: ASSET.into(),
            name: "1997 #143 Snorlax-Holo PSA 5 Jap".into(),
            collection: Some("11111111111111111111111111111111".into()),
            standard: 2,
            grading_company: Some("PSA".into()),
            face_value_usd: Some(usd(1_000)),
            ..Default::default()
        }
    }

    /// The default is MPL Core alone, and a pNFT is refused before any network
    /// call. Winning a quote on a `Locked` pNFT means an `MM_BUY` that reverts
    /// inside Token Metadata — a put that cannot be exercised.
    #[test]
    fn a_pnft_is_refused_by_the_default_standards_before_any_metadata_is_fetched() {
        let defaults = config("");
        let mut pnft = rfq();
        pnft.standard = 1;
        assert!(matches!(
            eligible_by_announcement(&pnft, &defaults),
            Err(Skip::Ineligible(_))
        ));
        assert!(eligible_by_announcement(&rfq(), &defaults).is_ok());

        let widened = config("\n[eligibility]\nstandards = [\"mpl_core\", \"pnft\"]\n");
        assert!(eligible_by_announcement(&pnft, &widened).is_ok());
    }

    #[test]
    fn an_empty_collections_list_means_any_and_a_populated_one_means_only_those() {
        let any = config("");
        assert!(eligible_by_card(&rfq(), &card(), &any).is_ok());

        let named = config(
            "\n[eligibility]\ncollections = [\"So11111111111111111111111111111111111111112\"]\n",
        );
        assert!(matches!(
            eligible_by_card(&rfq(), &card(), &named),
            Err(Skip::Ineligible(_))
        ));

        let mut matching = card();
        matching.collection = Some("So11111111111111111111111111111111111111112".into());
        assert!(eligible_by_card(&rfq(), &matching, &named).is_ok());
    }

    #[test]
    fn grading_is_matched_case_insensitively_and_an_ungraded_card_is_refused() {
        let config = config("\n[eligibility]\ngrading = [\"PSA\", \"BGS\"]\n");

        let mut lowercase = card();
        lowercase.grading_company = Some("psa".into());
        assert!(eligible_by_card(&rfq(), &lowercase, &config).is_ok());

        let mut other = card();
        other.grading_company = Some("CGC".into());
        assert!(eligible_by_card(&rfq(), &other, &config).is_err());

        let mut none = card();
        none.grading_company = None;
        assert!(eligible_by_card(&rfq(), &none, &config).is_err());
    }

    #[test]
    fn a_compressed_or_frozen_asset_is_refused() {
        let config = config("");
        let mut compressed = card();
        compressed.compressed = true;
        assert!(eligible_by_card(&rfq(), &compressed, &config).is_err());

        let mut frozen = card();
        frozen.frozen = true;
        assert!(eligible_by_card(&rfq(), &frozen, &config).is_err());
    }

    /// The RFQ and DAS describe the same card two ways. If they disagree about
    /// its shape, the bot does not pick one — the custody path it would be
    /// underwriting is not the one it thinks it is.
    #[test]
    fn a_card_whose_standard_disagrees_with_the_rfq_is_refused_rather_than_reconciled() {
        let config = config("");
        let mut mismatched = card();
        mismatched.standard = 1;
        assert!(matches!(
            eligible_by_card(&rfq(), &mismatched, &config),
            Err(Skip::Metadata(_))
        ));
    }

    #[test]
    fn a_signed_quote_expires_inside_the_announced_window_and_verifies_under_the_quote_key() {
        let key = Keypair::new();
        let quote = build_quote(
            &rfq(),
            &card(),
            usd(600),
            &config(""),
            usd(800),
            usd(500),
            1_000,
            &key,
        )
        .unwrap();

        assert!(quote.payload.expiry_ts >= rfq().min_expiry_ts);
        assert!(quote.payload.expiry_ts <= rfq().max_expiry_ts);
        assert_eq!(quote.payload.amount_usd, usd(600));
        assert_eq!(quote.principal_usd, usd(360));
        assert_eq!(quote.payload.expiry_ts, 1_120);

        let parsed = parse_envelope(&quote.envelope).unwrap();
        assert_eq!(parsed.signer, key.pubkey());
        assert!(parsed.verify());
        assert_eq!(parsed.payload, quote.payload);
    }

    /// The expiry the bot would have chosen is earlier than the round's floor,
    /// so it is raised. A quote that dies before the borrower can claim it is
    /// refused by the API and wastes the round.
    #[test]
    fn a_configured_expiry_shorter_than_the_rounds_floor_is_raised_to_meet_it() {
        let key = Keypair::new();
        let mut late = rfq();
        late.min_expiry_ts = 9_999;
        late.max_expiry_ts = 10_500;
        let quote = build_quote(
            &late,
            &card(),
            usd(600),
            &config(""),
            usd(800),
            usd(500),
            1_000,
            &key,
        )
        .unwrap();
        assert_eq!(quote.payload.expiry_ts, 9_999);
    }

    /// The other end of the same window, and the one that costs money to get
    /// wrong: an envelope signed past `maxExpiryTs` is one the program refuses
    /// to open a loan against, so the configured life is cut down rather than
    /// honoured.
    #[test]
    fn a_configured_expiry_past_the_rounds_ceiling_is_cut_down_to_it() {
        let key = Keypair::new();
        let quote = build_quote(
            &rfq(),
            &card(),
            usd(600),
            &config_with_expiry_secs(MAX_QUOTE_LIFETIME_SECS, ""),
            usd(800),
            usd(500),
            1_000,
            &key,
        )
        .unwrap();
        assert_eq!(quote.payload.expiry_ts, rfq().max_expiry_ts);
    }

    #[test]
    fn an_appraisal_over_the_caps_is_clamped_and_the_clamped_number_is_what_gets_signed() {
        let key = Keypair::new();
        let quote = build_quote(
            &rfq(),
            &card(),
            usd(100_000),
            &config(""),
            usd(800),
            usd(500),
            1_000,
            &key,
        )
        .unwrap();
        assert!(quote.payload.amount_usd <= usd(2_000));
        assert!(quote.principal_usd <= usd(500));
        assert_eq!(
            parse_envelope(&quote.envelope).unwrap().payload.amount_usd,
            quote.payload.amount_usd
        );
    }

    #[test]
    fn no_headroom_left_produces_a_skip_rather_than_an_unsigned_panic() {
        let key = Keypair::new();
        let skip = build_quote(
            &rfq(),
            &card(),
            usd(600),
            &config(""),
            0,
            usd(500),
            1_000,
            &key,
        )
        .unwrap_err();
        assert!(matches!(
            skip,
            Skip::BelowMinimum { .. } | Skip::NoHeadroom { .. }
        ));
    }
}
