//! Card metadata over the Digital Asset Standard read API.
//!
//! One `getAsset` per RFQ. It must be a DAS-capable RPC: the graded slabs this
//! product lends against are MPL Core assets, which are not SPL tokens and have
//! no token account, so nothing short of DAS can describe them.
//!
//! # Missing metadata is never a guess
//!
//! [`CardMetadata::face_value_usd`] is `None` when the attribute is absent or
//! unparseable, and every caller must treat that as SKIP THE CARD. A maker who
//! substitutes a default for a face value it could not read has written a put
//! struck at a number it invented.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Everything the bot learned about a card, and everything a `command` pricer
/// is handed. Serialized camelCase, matching the RFQ announcement.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CardMetadata {
    pub asset: String,
    pub name: String,
    /// Verified collection address, or `None`. An unverified group is not a
    /// collection and is deliberately not reported as one.
    pub collection: Option<String>,
    pub collection_name: Option<String>,
    /// `COLLATERAL_STANDARD_*` as derived from the DAS `interface`. Compare it
    /// against the RFQ's own `standard` rather than trusting either alone.
    pub standard: u8,
    /// Lives in a merkle tree, so nothing can take custody of it.
    pub compressed: bool,
    pub frozen: bool,
    /// Every `trait_type` -> `value`, verbatim, so a custom pricer can read
    /// attributes this bot does not know about.
    pub attributes: BTreeMap<String, String>,
    /// The card's face value in 6-decimal MICRO-USD, parsed out of
    /// `attributes`. `None` means it could not be read — skip the card.
    pub face_value_usd: Option<u64>,
    /// The grading company, verbatim from the attribute that carried it.
    pub grading_company: Option<String>,
    pub image: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DasGrouping {
    group_key: Option<String>,
    group_value: Option<String>,
    verified: Option<bool>,
    collection_metadata: Option<DasCollectionMetadata>,
}

#[derive(Debug, Deserialize)]
struct DasCollectionMetadata {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DasAttribute {
    trait_type: Option<String>,
    value: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct DasMetadata {
    name: Option<String>,
    attributes: Option<Vec<DasAttribute>>,
}

#[derive(Debug, Deserialize)]
struct DasLinks {
    image: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DasContent {
    metadata: Option<DasMetadata>,
    links: Option<DasLinks>,
}

#[derive(Debug, Deserialize)]
struct DasOwnership {
    frozen: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct DasCompression {
    compressed: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct DasItem {
    id: String,
    interface: Option<String>,
    compression: Option<DasCompression>,
    ownership: Option<DasOwnership>,
    grouping: Option<Vec<DasGrouping>>,
    content: Option<DasContent>,
}

/// SYNC: `imperial_pawn`'s `COLLATERAL_STANDARD_*`.
fn standard_of(interface: Option<&str>) -> u8 {
    match interface {
        Some("MplCoreAsset") | Some("MplCoreCollection") => 2,
        Some("ProgrammableNFT") => 1,
        _ => 0,
    }
}

/// Read a dollar figure out of an attribute value.
///
/// Accepts a JSON number, and a string in any of the shapes these collections
/// actually carry: `"1234"`, `"1,234.56"`, `"$1,234.56"`, `"1234.56 USD"`.
/// Returns micro-USD. `None` for anything it cannot read with certainty —
/// there is no partial credit here.
pub fn parse_usd(value: &serde_json::Value) -> Option<u64> {
    if let Some(number) = value.as_f64() {
        return micros(number);
    }
    let text = value.as_str()?;
    let cleaned: String = text
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    if cleaned.is_empty() || cleaned.matches('.').count() > 1 {
        return None;
    }
    micros(cleaned.parse::<f64>().ok()?)
}

fn micros(dollars: f64) -> Option<u64> {
    if !dollars.is_finite() || dollars < 0.0 {
        return None;
    }
    Some((dollars * 1_000_000.0).round() as u64)
}

/// The first attribute whose `trait_type` matches any of `names`,
/// case-insensitively, in the order `names` gives.
fn attribute<'a>(attributes: &'a BTreeMap<String, String>, names: &[String]) -> Option<&'a String> {
    names.iter().find_map(|wanted| {
        attributes
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| value)
    })
}

fn to_card(item: DasItem, face_value_traits: &[String], grading_traits: &[String]) -> CardMetadata {
    let content = item.content;
    let metadata = content.as_ref().and_then(|c| c.metadata.as_ref());

    let mut attributes = BTreeMap::new();
    for attribute in metadata
        .and_then(|m| m.attributes.as_ref())
        .into_iter()
        .flatten()
    {
        let (Some(key), Some(value)) = (attribute.trait_type.as_ref(), attribute.value.as_ref())
        else {
            continue;
        };
        let rendered = match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        attributes.insert(key.clone(), rendered);
    }

    let group = item
        .grouping
        .into_iter()
        .flatten()
        .find(|g| g.group_key.as_deref() == Some("collection") && g.verified != Some(false));

    let face_value_usd = attribute(&attributes, face_value_traits)
        .and_then(|raw| parse_usd(&serde_json::Value::String(raw.clone())));
    let grading_company = attribute(&attributes, grading_traits).cloned();

    CardMetadata {
        name: metadata
            .and_then(|m| m.name.clone())
            .unwrap_or_else(|| item.id.chars().take(8).collect()),
        collection: group.as_ref().and_then(|g| g.group_value.clone()),
        collection_name: group
            .as_ref()
            .and_then(|g| g.collection_metadata.as_ref())
            .and_then(|m| m.name.clone()),
        standard: standard_of(item.interface.as_deref()),
        compressed: item.compression.and_then(|c| c.compressed) == Some(true),
        frozen: item.ownership.and_then(|o| o.frozen) == Some(true),
        image: content
            .as_ref()
            .and_then(|c| c.links.as_ref())
            .and_then(|l| l.image.clone()),
        attributes,
        face_value_usd,
        grading_company,
        asset: item.id,
    }
}

pub struct Das {
    http: reqwest::Client,
    rpc_url: String,
    face_value_traits: Vec<String>,
    grading_traits: Vec<String>,
}

impl Das {
    pub fn new(
        rpc_url: &str,
        face_value_traits: Vec<String>,
        grading_traits: Vec<String>,
    ) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                // The auction window is ten seconds. A metadata read that takes
                // longer than this has already cost the round.
                .timeout(Duration::from_secs(5))
                .build()
                .context("cannot build the DAS HTTP client")?,
            rpc_url: rpc_url.to_string(),
            face_value_traits,
            grading_traits,
        })
    }

    pub async fn get_asset(&self, asset: &str) -> Result<CardMetadata> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "pawn-quote-bot",
            "method": "getAsset",
            "params": { "id": asset },
        });
        let response = self.http.post(&self.rpc_url).json(&body).send().await?;
        let status = response.status();
        let payload: serde_json::Value = response.json().await.with_context(|| {
            format!("the RPC answered {status} with something that is not JSON")
        })?;

        if let Some(error) = payload.get("error") {
            // -32601 is "method not found": a plain RPC without the DAS
            // extension. That is a configuration answer, not a card answer, and
            // saying so is the difference between a five-minute fix and an
            // afternoon.
            if error.get("code").and_then(|c| c.as_i64()) == Some(-32601) {
                bail!(
                    "this RPC does not implement DAS `getAsset` — the face_value_pct engine \
                     needs a DAS-capable endpoint"
                );
            }
            bail!("DAS error: {error}");
        }
        let result = payload
            .get("result")
            .cloned()
            .filter(|r| !r.is_null())
            .context("DAS returned no asset")?;
        let item: DasItem =
            serde_json::from_value(result).context("DAS returned an asset in an unknown shape")?;
        Ok(to_card(item, &self.face_value_traits, &self.grading_traits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn traits() -> (Vec<String>, Vec<String>) {
        (
            vec!["Insured Value".to_string(), "Value".to_string()],
            vec!["Grading Company".to_string()],
        )
    }

    fn item(json: serde_json::Value) -> CardMetadata {
        let (face, grading) = traits();
        to_card(serde_json::from_value(json).unwrap(), &face, &grading)
    }

    #[test]
    fn a_das_asset_becomes_a_card_with_its_collection_grade_and_face_value() {
        let card = item(serde_json::json!({
            "id": "2o1f3ekWgw8h3bXG418qiE6Dx6nSuoj8H1W3Q5Bavy3y",
            "interface": "MplCoreAsset",
            "ownership": { "frozen": false },
            "grouping": [{
                "group_key": "collection",
                "group_value": "CoLLxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                "verified": true,
                "collection_metadata": { "name": "Collector Crypt Graded" }
            }],
            "content": {
                "metadata": {
                    "name": "1997 #143 Snorlax-Holo PSA 5 Jap",
                    "attributes": [
                        { "trait_type": "Insured Value", "value": "$1,250.00" },
                        { "trait_type": "Grading Company", "value": "PSA" },
                        { "trait_type": "Grade", "value": 5 }
                    ]
                }
            }
        }));

        assert_eq!(card.standard, 2);
        assert_eq!(card.face_value_usd, Some(1_250_000_000));
        assert_eq!(card.grading_company.as_deref(), Some("PSA"));
        assert_eq!(
            card.collection_name.as_deref(),
            Some("Collector Crypt Graded")
        );
        assert_eq!(card.attributes.get("Grade").map(String::as_str), Some("5"));
        assert!(!card.frozen);
    }

    /// The rule the whole engine rests on: an unreadable face value is `None`,
    /// never a default. Every one of these would otherwise become a put struck
    /// at a number nobody chose.
    #[test]
    fn a_missing_or_unparseable_face_value_is_none_rather_than_a_guess() {
        let missing = item(serde_json::json!({
            "id": "A", "interface": "MplCoreAsset",
            "content": { "metadata": { "attributes": [{ "trait_type": "Grade", "value": "10" }] } }
        }));
        assert_eq!(missing.face_value_usd, None);

        let no_attributes = item(serde_json::json!({
            "id": "A", "interface": "MplCoreAsset", "content": { "metadata": {} }
        }));
        assert_eq!(no_attributes.face_value_usd, None);

        for junk in ["", "n/a", "unknown", "1.2.3", "--"] {
            let card = item(serde_json::json!({
                "id": "A", "interface": "MplCoreAsset",
                "content": { "metadata": { "attributes": [
                    { "trait_type": "Insured Value", "value": junk }
                ] } }
            }));
            assert_eq!(card.face_value_usd, None, "{junk:?} must not parse");
        }
    }

    #[test]
    fn a_face_value_is_read_from_numbers_and_from_the_usual_string_decorations() {
        assert_eq!(parse_usd(&serde_json::json!(1250)), Some(1_250_000_000));
        assert_eq!(parse_usd(&serde_json::json!(12.5)), Some(12_500_000));
        assert_eq!(parse_usd(&serde_json::json!("1250")), Some(1_250_000_000));
        assert_eq!(
            parse_usd(&serde_json::json!("$1,250.00")),
            Some(1_250_000_000)
        );
        assert_eq!(
            parse_usd(&serde_json::json!("1250.00 USD")),
            Some(1_250_000_000)
        );
        assert_eq!(parse_usd(&serde_json::json!("-5")), None);
        assert_eq!(parse_usd(&serde_json::json!(null)), None);
    }

    /// The trait names are configurable because nothing pins them, and they are
    /// tried in order so a maker can prefer their own attested attribute over a
    /// generic one.
    #[test]
    fn the_configured_trait_names_are_matched_case_insensitively_and_in_order() {
        let json = serde_json::json!({
            "id": "A", "interface": "MplCoreAsset",
            "content": { "metadata": { "attributes": [
                { "trait_type": "value", "value": "10" },
                { "trait_type": "INSURED VALUE", "value": "99" }
            ] } }
        });
        let (face, grading) = traits();
        let card = to_card(serde_json::from_value(json).unwrap(), &face, &grading);
        assert_eq!(
            card.face_value_usd,
            Some(99_000_000),
            "`Insured Value` is named first, so it wins over `Value`"
        );
    }

    #[test]
    fn the_das_interface_maps_onto_the_protocols_standard_byte() {
        assert_eq!(standard_of(Some("MplCoreAsset")), 2);
        assert_eq!(standard_of(Some("ProgrammableNFT")), 1);
        assert_eq!(standard_of(Some("V1_NFT")), 0);
        assert_eq!(standard_of(None), 0);
    }

    #[test]
    fn an_unverified_group_is_not_reported_as_a_collection() {
        let card = item(serde_json::json!({
            "id": "A", "interface": "MplCoreAsset",
            "grouping": [{
                "group_key": "collection",
                "group_value": "CoLLxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                "verified": false
            }]
        }));
        assert_eq!(card.collection, None);
    }
}
