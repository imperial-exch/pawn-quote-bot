//! What a maker is asked to price, and the arithmetic that turns an appraisal
//! into an exposure.
//!
//! # Never derive a field of the announcement yourself
//!
//! Every field below is STAMPED by the protocol when the round opens. The
//! expiry window in particular: `minExpiryTs` and `maxExpiryTs` are the only
//! two instants between which a signed quote can open a loan, and the program's
//! whole replay defense is that bound. Sign outside it and the borrower's
//! `LOAN_OPEN` reverts. The LTV and the principal cap move too, so a bot that
//! recomputed any of them would sign against terms nobody offered.

use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

/// Basis-point denominator. The pool's `openLtvBps` is out of this.
pub const MAX_BPS: u64 = 10_000;

/// One RFQ announcement, exactly as the API serves it on the stream and the
/// poll endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RfqAnnouncement {
    pub rfq_id: String,
    /// The card's mint, base58.
    pub asset: String,
    /// `COLLATERAL_STANDARD_*`: 0 SPL NFT, 1 pNFT, 2 MPL Core.
    pub standard: u8,
    pub opened_at: i64,
    /// When the auction shuts. Roughly ten seconds after `openedAt`.
    pub closes_at: i64,
    /// The earliest `expiryTs` a quote may carry — a quote that dies before the
    /// borrower can claim it is not a quote.
    pub min_expiry_ts: i64,
    /// The latest `expiryTs` a quote may carry. Sits inside the program's own
    /// ceiling on a quote's remaining life, with the difference left as slack
    /// for the chain clock lagging wall-clock. Never derived: a bot that
    /// computed its own would lose honest quotes to that slack.
    pub max_expiry_ts: i64,
    /// The LTV the pool would lend at against this appraisal.
    pub open_ltv_bps: u16,
    /// The largest PRINCIPAL the pool will write right now, micro-USD.
    pub max_principal_usd: u64,
}

impl RfqAnnouncement {
    pub fn asset_pubkey(&self) -> Option<Pubkey> {
        self.asset.parse().ok()
    }

    /// Seconds left to answer. Negative once the window has shut.
    pub fn secs_remaining(&self, now: i64) -> i64 {
        self.closes_at - now
    }

    /// The principal the pool would lend against an appraisal of `amount_usd`.
    ///
    /// THIS, not the appraisal, is what increments your on-chain
    /// `outstanding_usd` and is measured against `max_outstanding_usd`.
    pub fn principal_for(&self, amount_usd: u64) -> u64 {
        principal_for(amount_usd, self.open_ltv_bps)
    }
}

pub fn principal_for(amount_usd: u64, open_ltv_bps: u16) -> u64 {
    u64::try_from((amount_usd as u128) * (open_ltv_bps as u128) / (MAX_BPS as u128))
        .unwrap_or(u64::MAX)
}

/// The largest appraisal whose implied principal still fits under `cap`.
///
/// Floor division on both sides, so the result is exact or one base unit
/// conservative — never a value that rounds back over the cap.
pub fn max_appraisal_for_principal(cap: u64, open_ltv_bps: u16) -> u64 {
    if open_ltv_bps == 0 {
        return u64::MAX;
    }
    u64::try_from((cap as u128) * (MAX_BPS as u128) / (open_ltv_bps as u128)).unwrap_or(u64::MAX)
}

/// Why a card was not quoted. Kept as data rather than a log line so the reason
/// is testable and so a dry run reports the same thing a live run would.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Skip {
    /// The round shut before the bot got to it.
    WindowClosed,
    /// Not a collateral standard, collection or grade this maker quotes.
    Ineligible(String),
    /// The pricing engine declined.
    Priced(String),
    /// A price came back, but no legal appraisal survives the caps.
    BelowMinimum { clamped_to: u64, minimum: u64 },
    /// No headroom left, locally or on chain.
    NoHeadroom { needed: u64, available: u64 },
    /// The card's metadata could not be read or understood.
    Metadata(String),
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Skip::WindowClosed => write!(f, "the auction window had already closed"),
            Skip::Ineligible(why) => write!(f, "ineligible: {why}"),
            Skip::Priced(why) => write!(f, "the pricer declined: {why}"),
            Skip::BelowMinimum {
                clamped_to,
                minimum,
            } => write!(
                f,
                "the caps clamp this appraisal to {} micro-USD, below the configured \
                 minimum of {minimum} micro-USD",
                clamped_to
            ),
            Skip::NoHeadroom { needed, available } => write!(
                f,
                "needs {needed} micro-USD of principal headroom, {available} available"
            ),
            Skip::Metadata(why) => write!(f, "card metadata unusable: {why}"),
        }
    }
}

/// The caps an appraisal has to survive, gathered in one place so the clamp is
/// one function rather than a chain of ad-hoc comparisons.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub open_ltv_bps: u16,
    /// The pool's binding principal cap for this round.
    pub max_principal_usd: u64,
    /// Principal headroom left under this maker's own limit.
    pub headroom_usd: u64,
    /// Principal headroom left for this card's collection.
    pub collection_headroom_usd: u64,
    pub min_appraisal_usd: u64,
    pub max_appraisal_usd: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sized {
    pub amount_usd: u64,
    pub principal_usd: u64,
}

/// Bring an appraisal down to the largest value every cap allows, or skip.
///
/// Clamping DOWN is always safe and is why this does not simply refuse an
/// oversized appraisal: the appraisal is the strike of the put you are writing,
/// so quoting a lower number is strictly less exposure. Clamping UP never
/// happens and must never be added — it would raise a strike your pricer did
/// not choose.
pub fn size_quote(appraisal_usd: u64, caps: &Caps) -> Result<Sized, Skip> {
    let by_pool = max_appraisal_for_principal(caps.max_principal_usd, caps.open_ltv_bps);
    let by_maker = max_appraisal_for_principal(caps.headroom_usd, caps.open_ltv_bps);
    let by_collection =
        max_appraisal_for_principal(caps.collection_headroom_usd, caps.open_ltv_bps);

    let amount_usd = appraisal_usd
        .min(caps.max_appraisal_usd)
        .min(by_pool)
        .min(by_maker)
        .min(by_collection);

    if amount_usd < caps.min_appraisal_usd {
        return Err(Skip::BelowMinimum {
            clamped_to: amount_usd,
            minimum: caps.min_appraisal_usd,
        });
    }

    let principal_usd = principal_for(amount_usd, caps.open_ltv_bps);
    if principal_usd == 0 {
        return Err(Skip::BelowMinimum {
            clamped_to: amount_usd,
            minimum: caps.min_appraisal_usd,
        });
    }
    if principal_usd > caps.headroom_usd {
        return Err(Skip::NoHeadroom {
            needed: principal_usd,
            available: caps.headroom_usd,
        });
    }
    Ok(Sized {
        amount_usd,
        principal_usd,
    })
}

/// The expiry to sign: `expiry_secs` from now, clamped into the round's
/// announced window.
///
/// Both ends are hard. Below `min_expiry_ts` the quote dies before the borrower
/// can claim it and the API refuses it; above `max_expiry_ts` it is a quote the
/// program will not open a loan against, because `LOAN_OPEN` bounds how far
/// ahead of the chain clock an expiry may sit. The configured TTL is therefore
/// a preference between those two, never an override of either.
pub fn expiry_for(now: i64, expiry_secs: i64, min_expiry_ts: i64, max_expiry_ts: i64) -> i64 {
    (now + expiry_secs).max(min_expiry_ts).min(max_expiry_ts)
}

/// Everything the API's `check_payload` will test, run locally first.
///
/// A quote that fails here could never have opened a loan, and the auction
/// window is ten seconds — burning it on a round trip that was always going to
/// 400 is the thing this exists to prevent.
pub fn check_payload(
    payload: &crate::envelope::QuotePayload,
    rfq: &RfqAnnouncement,
    principal_usd: u64,
    headroom_usd: u64,
) -> Result<(), String> {
    let asset = rfq
        .asset_pubkey()
        .ok_or_else(|| format!("the RFQ's asset {} is not a pubkey", rfq.asset))?;
    if payload.asset != asset {
        return Err("payload asset does not match the RFQ".to_string());
    }
    if payload.expiry_ts < rfq.min_expiry_ts {
        return Err(format!(
            "payload expiry {} is before the round's minimum {}",
            payload.expiry_ts, rfq.min_expiry_ts
        ));
    }
    if payload.expiry_ts > rfq.max_expiry_ts {
        return Err(format!(
            "payload expiry {} is past the round's maximum {} — the program refuses an \
             envelope whose expiry sits too far ahead of the chain clock",
            payload.expiry_ts, rfq.max_expiry_ts
        ));
    }
    if payload.amount_usd == 0 {
        return Err("payload amount is zero".to_string());
    }
    if principal_usd == 0 {
        return Err("the appraisal is too small to lend against at this LTV".to_string());
    }
    if principal_usd > rfq.max_principal_usd {
        return Err(format!(
            "implied principal {principal_usd} is above the pool's cap of {}",
            rfq.max_principal_usd
        ));
    }
    if principal_usd > headroom_usd {
        return Err(format!(
            "implied principal {principal_usd} is above the remaining headroom of {headroom_usd}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::QuotePayload;

    fn usd(dollars: u64) -> u64 {
        dollars * 1_000_000
    }

    fn announcement() -> RfqAnnouncement {
        RfqAnnouncement {
            rfq_id: "7c9e6679-7425-40de-944b-e07fc1f90ae7".to_string(),
            asset: "2o1f3ekWgw8h3bXG418qiE6Dx6nSuoj8H1W3Q5Bavy3y".to_string(),
            standard: 2,
            opened_at: 1_000,
            closes_at: 1_010,
            min_expiry_ts: 1_070,
            max_expiry_ts: 1_600,
            open_ltv_bps: 6_000,
            max_principal_usd: usd(5_000),
        }
    }

    fn caps() -> Caps {
        Caps {
            open_ltv_bps: 6_000,
            max_principal_usd: usd(5_000),
            headroom_usd: usd(800),
            collection_headroom_usd: usd(500),
            min_appraisal_usd: usd(25),
            max_appraisal_usd: usd(2_000),
        }
    }

    /// The single most consequential number in the bot. The appraisal is what
    /// you are on the hook for; the principal is what it costs your cap. A
    /// $1,000 appraisal at 60% LTV lends $600.
    #[test]
    fn the_principal_is_the_appraisal_times_the_ltv_not_the_appraisal() {
        assert_eq!(principal_for(usd(1_000), 6_000), usd(600));
        assert_eq!(principal_for(usd(1_000), 10_000), usd(1_000));
        assert_eq!(principal_for(usd(1_000), 0), 0);
    }

    #[test]
    fn the_inverse_never_returns_an_appraisal_whose_principal_exceeds_the_cap() {
        for bps in [1u16, 137, 6_000, 9_999, 10_000] {
            for cap in [1u64, 999, usd(1), usd(777), usd(5_000)] {
                let appraisal = max_appraisal_for_principal(cap, bps);
                assert!(
                    principal_for(appraisal, bps) <= cap,
                    "bps {bps} cap {cap} produced appraisal {appraisal}"
                );
            }
        }
    }

    /// Clamping DOWN is the safe direction, so an oversized appraisal is sized
    /// to fit rather than thrown away — but the strike that ends up signed is
    /// always at or below what the pricer said.
    #[test]
    fn an_oversized_appraisal_is_clamped_down_to_the_binding_cap() {
        // The collection cap ($500 principal at 60% LTV) binds first.
        let sized = size_quote(usd(10_000), &caps()).unwrap();
        assert_eq!(sized.amount_usd, usd(833) + 333_333);
        assert!(sized.principal_usd <= usd(500));
        assert!(sized.amount_usd <= usd(2_000), "never above max_appraisal");

        let roomy = Caps {
            collection_headroom_usd: usd(100_000),
            headroom_usd: usd(100_000),
            ..caps()
        };
        assert_eq!(
            size_quote(usd(10_000), &roomy).unwrap().amount_usd,
            usd(2_000)
        );
    }

    #[test]
    fn an_appraisal_inside_every_cap_is_signed_unchanged() {
        let sized = size_quote(usd(400), &caps()).unwrap();
        assert_eq!(sized.amount_usd, usd(400));
        assert_eq!(sized.principal_usd, usd(240));
    }

    /// Clamping to something you would not have bid on is not a bargain, it is
    /// an accident. Below the configured floor the bot walks away.
    #[test]
    fn a_clamp_that_lands_below_the_configured_minimum_skips_rather_than_quotes() {
        let squeezed = Caps {
            collection_headroom_usd: usd(1),
            ..caps()
        };
        assert!(matches!(
            size_quote(usd(1_000), &squeezed),
            Err(Skip::BelowMinimum { .. })
        ));

        let exhausted = Caps {
            headroom_usd: 0,
            ..caps()
        };
        assert!(size_quote(usd(1_000), &exhausted).is_err());
    }

    #[test]
    fn the_expiry_is_raised_to_the_rounds_floor_when_the_configured_life_is_shorter() {
        assert_eq!(expiry_for(1_000, 120, 1_070, 1_600), 1_120);
        assert_eq!(
            expiry_for(1_000, 30, 1_070, 1_600),
            1_070,
            "a quote that dies before the borrower can claim it is refused by the API"
        );
    }

    /// The ceiling is the program's, not a preference. A configured TTL past it
    /// is cut down rather than signed: the alternative is an envelope no
    /// borrower can open a loan against.
    #[test]
    fn a_configured_life_past_the_rounds_ceiling_is_cut_down_to_it() {
        assert_eq!(expiry_for(1_000, 5_000, 1_070, 1_600), 1_600);
        assert_eq!(expiry_for(1_000, 600, 1_070, 1_600), 1_600);
        assert_eq!(expiry_for(1_000, 599, 1_070, 1_600), 1_599);
    }

    fn payload(rfq: &RfqAnnouncement, amount_usd: u64) -> QuotePayload {
        QuotePayload {
            asset: rfq.asset_pubkey().unwrap(),
            amount_usd,
            expiry_ts: rfq.min_expiry_ts,
        }
    }

    /// The announced window is the program's replay bound, projected. An expiry
    /// on either side of it could never have opened a loan, so it fails here
    /// rather than at the borrower's transaction minutes later.
    #[test]
    fn a_payload_expiring_outside_the_announced_window_is_refused_locally() {
        let rfq = announcement();
        assert!(check_payload(&payload(&rfq, usd(400)), &rfq, usd(240), usd(800)).is_ok());

        for edge in [rfq.min_expiry_ts, rfq.max_expiry_ts] {
            let mut at_edge = payload(&rfq, usd(400));
            at_edge.expiry_ts = edge;
            assert!(check_payload(&at_edge, &rfq, usd(240), usd(800)).is_ok());
        }

        for outside in [rfq.min_expiry_ts - 1, rfq.max_expiry_ts + 1] {
            let mut bad = payload(&rfq, usd(400));
            bad.expiry_ts = outside;
            assert!(check_payload(&bad, &rfq, usd(240), usd(800)).is_err());
        }
    }

    #[test]
    fn a_payload_for_another_card_is_refused_locally() {
        let rfq = announcement();
        let mut other_card = payload(&rfq, usd(400));
        other_card.asset = Pubkey::new_unique();
        assert!(check_payload(&other_card, &rfq, usd(240), usd(800)).is_err());
    }

    #[test]
    fn a_principal_over_the_pool_cap_or_over_the_makers_headroom_is_refused_locally() {
        let rfq = announcement();
        let p = payload(&rfq, usd(20_000));
        assert!(check_payload(&p, &rfq, usd(12_000), usd(100_000)).is_err());
        assert!(check_payload(&p, &rfq, usd(1_000), usd(500)).is_err());
    }

    #[test]
    fn the_announcement_deserializes_from_the_apis_camel_case_json() {
        let json = r#"{
            "rfqId": "7c9e6679-7425-40de-944b-e07fc1f90ae7",
            "asset": "2o1f3ekWgw8h3bXG418qiE6Dx6nSuoj8H1W3Q5Bavy3y",
            "standard": 2,
            "openedAt": 1000,
            "closesAt": 1010,
            "minExpiryTs": 1070,
            "maxExpiryTs": 1600,
            "openLtvBps": 6000,
            "maxPrincipalUsd": 5000000000
        }"#;
        let parsed: RfqAnnouncement = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, announcement());
        assert_eq!(parsed.secs_remaining(1_005), 5);
        assert_eq!(parsed.principal_for(usd(1_000)), usd(600));
    }
}
