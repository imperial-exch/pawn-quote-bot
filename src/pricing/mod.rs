//! Pricing engines.
//!
//! # Writing your own, in Rust
//!
//! Implement [`Pricer`]. That is the whole contract:
//!
//! ```ignore
//! use async_trait::async_trait;
//! use pawn_quote_bot::das::CardMetadata;
//! use pawn_quote_bot::pricing::{Priced, Pricer};
//! use pawn_quote_bot::rfq::RfqAnnouncement;
//!
//! struct MyComps;
//!
//! #[async_trait]
//! impl Pricer for MyComps {
//!     fn name(&self) -> &str {
//!         "my_comps"
//!     }
//!
//!     async fn price(
//!         &self,
//!         rfq: &RfqAnnouncement,
//!         card: &CardMetadata,
//!     ) -> anyhow::Result<Priced> {
//!         let Some(recent) = self.recent_sale(&card.asset).await? else {
//!             return Ok(Priced::Skip("no comparable sale".into()));
//!         };
//!         Ok(Priced::Appraisal(recent * 70 / 100))
//!     }
//! }
//! ```
//!
//! Then hand it to the bot in place of [`build`]'s result. Everything after the
//! price — the caps, the clamp, the expiry, the signature, the submission — is
//! the bot's and is applied identically whatever engine produced the number.
//!
//! # Three rules the engine must respect
//!
//! 1. **Return MICRO-USD.** $1,250 is `1_250_000_000`. It is the unit of the
//!    signed payload and of `maxPrincipalUsd`.
//! 2. **Return the APPRAISAL, not the principal.** The pool lends
//!    `appraisal * openLtvBps / 10000`; you are exposed for the whole
//!    appraisal.
//! 3. **Skip rather than guess.** [`Priced::Skip`] costs one auction.
//!    A number you invented costs the difference between it and the card.
//!
//! # Latency
//!
//! The auction window is about ten seconds and includes the metadata read, the
//! signature and the round trip. An engine that takes longer has not priced the
//! card, it has missed it.

pub mod command;
pub mod face_value;

use anyhow::Result;
use async_trait::async_trait;

use crate::config::{Config, Engine};
use crate::das::CardMetadata;
use crate::rfq::RfqAnnouncement;

/// What an engine answers with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Priced {
    /// An appraisal in 6-decimal micro-USD.
    Appraisal(u64),
    /// Do not quote this card, and why.
    Skip(String),
}

#[async_trait]
pub trait Pricer: Send + Sync {
    fn name(&self) -> &str;

    async fn price(&self, rfq: &RfqAnnouncement, card: &CardMetadata) -> Result<Priced>;
}

/// The engine named in config. Replace this call to drop in your own.
pub fn build(config: &Config) -> Result<Box<dyn Pricer>> {
    match config.pricing.engine {
        Engine::FaceValuePct => Ok(Box::new(face_value::FaceValuePct::new(
            config.pricing.face_value_pct,
        ))),
        Engine::Command => {
            let settings = config
                .pricing
                .command
                .as_ref()
                .expect("validated at load: the command engine has its table");
            Ok(Box::new(command::CommandPricer::new(
                settings.path.clone(),
                settings.timeout_ms,
            )))
        }
    }
}
