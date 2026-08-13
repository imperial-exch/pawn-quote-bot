//! Appraise at a fixed percentage of the card's stated face value.
//!
//! The simplest engine that is not reckless, and it is a STARTING POINT rather
//! than a strategy: face value is what the issuer attested, not what the card
//! trades at, and the percentage is the only thing standing between the two.
//! A maker running this in production is betting that the discount covers every
//! difference between attestation and market — across grades, sets and a
//! falling market. Replace it with real comparables as soon as you have them.
//!
//! It refuses to quote a card whose face value it could not read. That is the
//! entire safety property of this engine: no metadata, no number, no quote.

use anyhow::Result;
use async_trait::async_trait;

use crate::das::CardMetadata;
use crate::pricing::{Priced, Pricer};
use crate::rfq::RfqAnnouncement;

pub struct FaceValuePct {
    pct: f64,
}

impl FaceValuePct {
    pub fn new(pct: f64) -> Self {
        Self { pct }
    }

    /// Rounds DOWN. The rounding direction is one base unit of a dollar and it
    /// still goes the safe way: a lower appraisal is a lower put strike.
    pub fn appraise(&self, face_value_usd: u64) -> u64 {
        ((face_value_usd as f64) * self.pct / 100.0).floor() as u64
    }
}

#[async_trait]
impl Pricer for FaceValuePct {
    fn name(&self) -> &str {
        "face_value_pct"
    }

    async fn price(&self, _rfq: &RfqAnnouncement, card: &CardMetadata) -> Result<Priced> {
        let Some(face_value_usd) = card.face_value_usd else {
            return Ok(Priced::Skip(
                "no readable face value in the card's metadata".to_string(),
            ));
        };
        if face_value_usd == 0 {
            return Ok(Priced::Skip("the card's face value is zero".to_string()));
        }
        Ok(Priced::Appraisal(self.appraise(face_value_usd)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(face_value_usd: Option<u64>) -> CardMetadata {
        CardMetadata {
            face_value_usd,
            ..Default::default()
        }
    }

    fn rfq() -> RfqAnnouncement {
        RfqAnnouncement {
            rfq_id: "r".into(),
            asset: "2o1f3ekWgw8h3bXG418qiE6Dx6nSuoj8H1W3Q5Bavy3y".into(),
            standard: 2,
            opened_at: 0,
            closes_at: 10,
            min_expiry_ts: 70,
            max_expiry_ts: 600,
            open_ltv_bps: 6_000,
            max_principal_usd: 5_000_000_000,
        }
    }

    #[tokio::test]
    async fn a_card_with_a_face_value_prices_at_the_configured_percentage() {
        let priced = FaceValuePct::new(60.0)
            .price(&rfq(), &card(Some(1_000_000_000)))
            .await
            .unwrap();
        assert_eq!(priced, Priced::Appraisal(600_000_000));
    }

    /// The rule this engine exists to enforce. A default here would write a put
    /// struck at a number the bot invented.
    #[tokio::test]
    async fn a_card_with_no_readable_face_value_is_skipped_and_never_guessed() {
        let priced = FaceValuePct::new(60.0)
            .price(&rfq(), &card(None))
            .await
            .unwrap();
        assert!(matches!(priced, Priced::Skip(_)));

        let zero = FaceValuePct::new(60.0)
            .price(&rfq(), &card(Some(0)))
            .await
            .unwrap();
        assert!(matches!(zero, Priced::Skip(_)));
    }

    #[test]
    fn the_percentage_rounds_down_so_the_strike_never_rounds_up() {
        assert_eq!(FaceValuePct::new(60.0).appraise(1), 0);
        assert_eq!(FaceValuePct::new(33.0).appraise(100), 33);
        assert_eq!(
            FaceValuePct::new(100.0).appraise(1_250_000_000),
            1_250_000_000
        );
    }
}
