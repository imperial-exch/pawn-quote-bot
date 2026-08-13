//! The TOML configuration surface.
//!
//! Every table is `deny_unknown_fields`. A misspelled key in a file that
//! governs how much money you can lose must be an error at startup, never a
//! silently ignored line that leaves a default in place.
//!
//! # Units
//!
//! Two units appear in this bot and confusing them is the expensive mistake:
//!
//! * **Config files are written in whole dollars.** `max_appraisal_usd = 2000`
//!   means $2,000. Fractions are allowed (`2000.50`).
//! * **The wire is 6-decimal micro-USD.** `amount_usd` in the signed payload,
//!   and `maxPrincipalUsd` in an RFQ announcement, are integers where
//!   $2,000 is `2_000_000_000`.
//!
//! Every dollar figure here is parsed into [`Usd`], which stores micro-USD, so
//! the conversion happens exactly once and at the edge.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer};
use solana_sdk::pubkey::Pubkey;

use crate::envelope::MAX_QUOTE_LIFETIME_SECS;

/// One dollar in micro-USD.
pub const USD: u64 = 1_000_000;

/// A dollar amount from config, stored as 6-decimal micro-USD.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Usd(pub u64);

impl Usd {
    pub fn micros(self) -> u64 {
        self.0
    }

    pub fn dollars(self) -> f64 {
        self.0 as f64 / USD as f64
    }
}

impl std::fmt::Display for Usd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "${:.2}", self.dollars())
    }
}

/// TOML types integers and floats separately, and a config author writing `25`
/// must not get a type error where `25.0` works.
#[derive(Deserialize)]
#[serde(untagged)]
enum Scalar {
    Int(i64),
    Float(f64),
}

impl Scalar {
    fn as_f64(&self) -> f64 {
        match self {
            Scalar::Int(v) => *v as f64,
            Scalar::Float(v) => *v,
        }
    }
}

impl<'de> Deserialize<'de> for Usd {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let dollars = Scalar::deserialize(d)?.as_f64();
        if !dollars.is_finite() || dollars < 0.0 {
            return Err(serde::de::Error::custom(
                "a USD amount must be finite and non-negative",
            ));
        }
        Ok(Usd((dollars * USD as f64).round() as u64))
    }
}

fn de_f64<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    Ok(Scalar::deserialize(d)?.as_f64())
}

/// `Pubkey`'s own `Deserialize` reads a 32-element byte array, which is not
/// what anyone writes in a config file. Both of these parse base58.
fn de_pubkey<'de, D: Deserializer<'de>>(d: D) -> Result<Pubkey, D::Error> {
    let text = String::deserialize(d)?;
    text.trim()
        .parse()
        .map_err(|_| serde::de::Error::custom(format!("{text:?} is not a base58 pubkey")))
}

fn de_pubkeys<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Pubkey>, D::Error> {
    Vec::<String>::deserialize(d)?
        .into_iter()
        .map(|text| {
            text.trim()
                .parse()
                .map_err(|_| serde::de::Error::custom(format!("{text:?} is not a base58 pubkey")))
        })
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub maker: Maker,
    pub pricing: Pricing,
    pub limits: Limits,
    #[serde(default)]
    pub eligibility: Eligibility,
    pub quote: Quote,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Maker {
    /// The `imperial_pawn` program id for the cluster you are quoting on. NEVER
    /// hardcoded anywhere in this bot: mainnet and staging run different
    /// deployments, and a bot that assumes one derives the wrong
    /// `MarketMaker` PDA on the other and reports itself unregistered.
    #[serde(deserialize_with = "de_pubkey")]
    pub program_id: Pubkey,
    /// API root INCLUDING the version prefix, e.g.
    /// `https://api.example.com/api/v1`. Paths are appended to it verbatim.
    pub api_base: String,
    /// Your seed authority pubkey — identity, not a signing key. It is the
    /// `x-pawn-mm` header and the seed of your `[b"mm", authority]` account.
    #[serde(deserialize_with = "de_pubkey")]
    pub authority: Pubkey,
    /// Path to the ed25519 keypair whose public key is registered as
    /// `MarketMaker.quote_pubkey`. A JSON byte array, the format
    /// `solana-keygen` writes.
    ///
    /// This should NOT be your authority key. See the README: the authority
    /// signs `MM_BUY` and owns the registration, the quote key only signs
    /// appraisals and is expected to be hot on a server.
    pub quote_keypair: PathBuf,
    /// A DAS-capable Solana RPC. Used to read your own `MarketMaker` account at
    /// startup and, for the `face_value_pct` engine, to `getAsset` each card.
    pub rpc_url: String,
    /// Price and log, never submit. DEFAULT TRUE — a bot that quotes on its
    /// first run before anyone has read its output is writing puts nobody
    /// reviewed.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    pub engine: Engine,
    /// Percent of the card's face value to appraise at. Only read by the
    /// `face_value_pct` engine.
    #[serde(default = "default_face_value_pct", deserialize_with = "de_f64")]
    pub face_value_pct: f64,
    /// Metadata attribute names searched, in order, for the card's face value.
    /// Matched case-insensitively against `trait_type`.
    #[serde(default = "default_face_value_traits")]
    pub face_value_traits: Vec<String>,
    #[serde(default)]
    pub command: Option<CommandPricing>,
}

fn default_face_value_pct() -> f64 {
    60.0
}

fn default_face_value_traits() -> Vec<String> {
    vec![
        "Insured Value".to_string(),
        "insuredValue".to_string(),
        "Face Value".to_string(),
        "Value".to_string(),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    FaceValuePct,
    Command,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandPricing {
    /// An executable. The RFQ and the card's metadata arrive as one JSON object
    /// on stdin; it answers with `{"appraisal_usd": N}` in MICRO-USD, or
    /// `{"skip": "reason"}`.
    pub path: PathBuf,
    #[serde(default = "default_command_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_command_timeout_ms() -> u64 {
    3_000
}

/// Risk limits, all of them local. Nothing here is enforced by the protocol
/// except by way of the quotes this bot declines to sign.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Bounds on the APPRAISAL, not the principal. Below this the bot skips
    /// rather than quotes: a card too cheap to bother with is not a card to
    /// write a put on.
    pub min_appraisal_usd: Usd,
    /// The largest APPRAISAL this bot will sign. Remember what it costs you:
    /// an appraisal of `max_appraisal_usd` consumes
    /// `max_appraisal_usd * openLtvBps / 10000` of your on-chain
    /// `max_outstanding_usd`, and puts you on the hook for the full
    /// `max_appraisal_usd` if the card is put to you.
    pub max_appraisal_usd: Usd,
    /// A LOCAL MIRROR of the on-chain `MarketMaker.max_outstanding_usd`, in
    /// PRINCIPAL. Keep it under the on-chain cap: the chain is the real limit
    /// and this one exists so the bot stops before the API starts rejecting it.
    pub max_outstanding_usd: Usd,
    /// Concurrent principal this bot will have in flight against any one
    /// collection. Session-scoped — the chain has no per-collection notion.
    pub max_per_collection_usd: Usd,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Eligibility {
    /// Collateral standards to quote on. Defaults to MPL Core ONLY, and you
    /// should think hard before widening it — see [`Standard`] and the README.
    #[serde(default = "default_standards")]
    pub standards: Vec<Standard>,
    /// Verified collection addresses. EMPTY MEANS ANY, which is the permissive
    /// setting; name the collections you have actually priced.
    #[serde(default, deserialize_with = "de_pubkeys")]
    pub collections: Vec<Pubkey>,
    /// Grading companies to accept, matched case-insensitively against the
    /// card's grading attribute. Empty means any.
    #[serde(default)]
    pub grading: Vec<String>,
    /// Metadata attribute names searched, in order, for the grading company.
    #[serde(default = "default_grading_traits")]
    pub grading_traits: Vec<String>,
}

/// Written out rather than derived: `#[serde(default)]` on the `[eligibility]`
/// TABLE calls THIS, not the per-field defaults, so a derived `Default` would
/// give an omitted table empty `standards` — which makes every card ineligible
/// and the bot silently quote nothing.
impl Default for Eligibility {
    fn default() -> Self {
        Self {
            standards: default_standards(),
            collections: Vec::new(),
            grading: Vec::new(),
            grading_traits: default_grading_traits(),
        }
    }
}

fn default_standards() -> Vec<Standard> {
    vec![Standard::MplCore]
}

fn default_grading_traits() -> Vec<String> {
    vec![
        "Grading Company".to_string(),
        "gradingCompany".to_string(),
        "Grader".to_string(),
    ]
}

/// `COLLATERAL_STANDARD_*`, as the RFQ announcement's `standard` byte.
///
/// The default is MPL Core alone, and that is a solvency setting rather than a
/// preference:
///
/// * A pNFT whose `TokenRecord` is `Locked` — common in the graded card
///   collections this product lends against — cannot be transferred by Token
///   Metadata at all. If you win a quote on one and it defaults, your `MM_BUY`
///   reverts inside Token Metadata and you CANNOT exercise the put you paid for
///   with your appraisal. You are left holding the exposure with no way to take
///   the card.
/// * Write-off detection is only reliable for MPL Core burns. A card that
///   leaves custody by a route the protocol cannot observe is a position you
///   cannot mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Standard {
    SplNft,
    Pnft,
    MplCore,
}

impl Standard {
    pub fn from_wire(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Standard::SplNft),
            1 => Some(Standard::Pnft),
            2 => Some(Standard::MplCore),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quote {
    /// How long a signed quote stays valid, from the moment it is signed. A
    /// PREFERENCE inside the round's announced `[minExpiryTs, maxExpiryTs]`
    /// window, not an override of either end: the bot raises it to the floor
    /// and cuts it to the ceiling.
    ///
    /// The ceiling is the protocol's, not ours. `LOAN_OPEN` refuses an envelope
    /// whose expiry sits more than [`MAX_QUOTE_LIFETIME_SECS`] ahead of the
    /// chain clock, so a value above that could never be honoured and is
    /// refused at startup rather than silently cut down every round.
    ///
    /// Longer is not free: a live quote is a bearer instrument someone can
    /// still open a loan against.
    pub expiry_secs: i64,
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Refuse a configuration that cannot produce a quote, or that quietly
    /// means something other than it reads.
    fn validate(&self) -> Result<()> {
        if self.limits.min_appraisal_usd > self.limits.max_appraisal_usd {
            bail!(
                "limits.min_appraisal_usd ({}) is above limits.max_appraisal_usd ({}) — \
                 no card can ever be quoted",
                self.limits.min_appraisal_usd,
                self.limits.max_appraisal_usd
            );
        }
        if self.limits.max_outstanding_usd.micros() == 0 {
            bail!("limits.max_outstanding_usd is zero — nothing could ever be quoted");
        }
        if self.eligibility.standards.is_empty() {
            bail!("eligibility.standards is empty — no card could ever be eligible");
        }
        if self.quote.expiry_secs <= 0 {
            bail!("quote.expiry_secs must be positive");
        }
        if self.quote.expiry_secs > MAX_QUOTE_LIFETIME_SECS {
            bail!(
                "quote.expiry_secs ({}) is above the program's ceiling of {MAX_QUOTE_LIFETIME_SECS} \
                 seconds — LOAN_OPEN refuses an envelope whose expiry sits further than that ahead \
                 of the chain clock, so no borrower could open a loan against it",
                self.quote.expiry_secs
            );
        }
        if self.maker.api_base.trim().is_empty() {
            bail!("maker.api_base is empty");
        }
        match self.pricing.engine {
            Engine::FaceValuePct => {
                if !(self.pricing.face_value_pct > 0.0 && self.pricing.face_value_pct <= 100.0) {
                    bail!(
                        "pricing.face_value_pct must be in (0, 100]; got {}",
                        self.pricing.face_value_pct
                    );
                }
                if self.pricing.face_value_traits.is_empty() {
                    bail!("pricing.face_value_traits is empty — no face value could be found");
                }
            }
            Engine::Command => {
                let command = self.pricing.command.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "pricing.engine is \"command\" but there is no [pricing.command] table"
                    )
                })?;
                if command.timeout_ms == 0 {
                    bail!("pricing.command.timeout_ms must be positive");
                }
            }
        }
        Ok(())
    }

    pub fn allowed_standards(&self) -> BTreeSet<Standard> {
        self.eligibility.standards.iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
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
expiry_secs = 120
"#;

    fn parse(text: &str) -> Result<Config> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn dry_run_defaults_to_true_when_the_key_is_absent() {
        let config = parse(MINIMAL).unwrap();
        assert!(
            config.maker.dry_run,
            "a bot that quotes before anyone reads its output is the failure this default prevents"
        );
    }

    /// An omitted `[eligibility]` table must land on the SAME defaults a
    /// present-but-empty one does. `#[serde(default)]` on a table calls the
    /// type's `Default`, not the per-field defaults, so a derived `Default`
    /// here would leave `standards` empty and the bot would quote nothing at
    /// all while looking perfectly configured.
    #[test]
    fn an_omitted_eligibility_table_still_defaults_to_mpl_core_only() {
        assert_eq!(
            parse(MINIMAL).unwrap().eligibility.standards,
            vec![Standard::MplCore]
        );
        assert_eq!(
            parse(&format!("{MINIMAL}\n[eligibility]\n"))
                .unwrap()
                .eligibility
                .standards,
            vec![Standard::MplCore]
        );
        assert!(!Eligibility::default().grading_traits.is_empty());
    }

    /// Config is dollars, the wire is micro-USD, and the whole conversion lives
    /// in one place. A regression here silently moves every limit by 10^6.
    #[test]
    fn dollar_amounts_become_micro_usd_and_accept_both_integers_and_floats() {
        let config = parse(MINIMAL).unwrap();
        assert_eq!(config.limits.max_appraisal_usd.micros(), 2_000_000_000);
        assert_eq!(config.limits.min_appraisal_usd.micros(), 25_000_000);

        let fractional =
            parse(&MINIMAL.replace("min_appraisal_usd = 25", "min_appraisal_usd = 25.5")).unwrap();
        assert_eq!(fractional.limits.min_appraisal_usd.micros(), 25_500_000);
    }

    #[test]
    fn an_unknown_key_is_an_error_rather_than_a_silently_ignored_line() {
        let typo = MINIMAL.replace("max_outstanding_usd", "max_outstandng_usd");
        assert!(parse(&typo).is_err());

        let stray = format!("{MINIMAL}\n[quote]\nnot_a_key = 1\n");
        assert!(parse(&stray).is_err());
    }

    #[test]
    fn a_min_appraisal_above_the_max_is_refused_rather_than_quoting_nothing_forever() {
        let inverted = MINIMAL.replace("min_appraisal_usd = 25", "min_appraisal_usd = 5000");
        let err = parse(&inverted).unwrap_err().to_string();
        assert!(err.contains("min_appraisal_usd"), "{err}");
    }

    /// The protocol bounds how far ahead of the chain clock a signed expiry may
    /// sit. A configured life above it is not a long quote, it is a quote no
    /// borrower can open a loan against, so it stops the bot at startup.
    #[test]
    fn a_quote_life_past_the_programs_ceiling_is_refused_at_startup() {
        let ok = MINIMAL.replace(
            "expiry_secs = 120",
            &format!("expiry_secs = {MAX_QUOTE_LIFETIME_SECS}"),
        );
        assert!(parse(&ok).is_ok());

        let over = MINIMAL.replace(
            "expiry_secs = 120",
            &format!("expiry_secs = {}", MAX_QUOTE_LIFETIME_SECS + 1),
        );
        let err = parse(&over).unwrap_err().to_string();
        assert!(err.contains("expiry_secs"), "{err}");
    }

    #[test]
    fn the_command_engine_without_its_table_is_refused_at_startup() {
        let command = MINIMAL.replace("engine = \"face_value_pct\"", "engine = \"command\"");
        assert!(parse(&command).is_err());

        let with_table = format!("{command}\n[pricing.command]\npath = \"./pricer\"\n");
        let config = parse(&with_table).unwrap();
        assert_eq!(config.pricing.command.unwrap().timeout_ms, 3_000);
    }

    #[test]
    fn a_face_value_pct_outside_zero_to_one_hundred_is_refused() {
        for bad in ["0", "-10", "150"] {
            let text = MINIMAL.replace("face_value_pct = 60", &format!("face_value_pct = {bad}"));
            assert!(parse(&text).is_err(), "{bad} should be refused");
        }
        assert!(parse(&MINIMAL.replace("face_value_pct = 60", "face_value_pct = 100")).is_ok());
    }

    /// The shipped example is the first thing a new maker edits, and
    /// `deny_unknown_fields` means a key renamed here without renaming it there
    /// makes their very first run fail. Parsing it with the placeholders filled
    /// in catches that drift.
    #[test]
    fn the_shipped_example_config_parses_once_its_placeholders_are_filled_in() {
        let example = include_str!("../config.example.toml")
            .replace(
                "REPLACE_WITH_THE_PAWN_PROGRAM_ID",
                "So11111111111111111111111111111111111111112",
            )
            .replace(
                "REPLACE_WITH_YOUR_AUTHORITY_PUBKEY",
                "11111111111111111111111111111111",
            )
            .replace(
                "https://REPLACE_WITH_THE_API_HOST/api/v1",
                "https://x.test/api/v1",
            )
            .replace("https://REPLACE_WITH_A_DAS_CAPABLE_RPC", "https://rpc.test");

        let config = parse(&example).expect("the example is a valid configuration");
        assert!(config.maker.dry_run, "the example must ship in dry run");
        assert_eq!(config.eligibility.standards, vec![Standard::MplCore]);
        assert_eq!(config.pricing.engine, Engine::FaceValuePct);
    }

    #[test]
    fn the_wire_standard_byte_maps_to_the_configured_names() {
        assert_eq!(Standard::from_wire(0), Some(Standard::SplNft));
        assert_eq!(Standard::from_wire(1), Some(Standard::Pnft));
        assert_eq!(Standard::from_wire(2), Some(Standard::MplCore));
        assert_eq!(Standard::from_wire(3), None);
    }
}
