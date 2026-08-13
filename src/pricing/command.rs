//! Delegate pricing to an external program.
//!
//! This is what makes "plug in your own pricing" true without writing Rust. The
//! bot spawns your executable, writes one JSON object to its stdin, and reads
//! one JSON object back from its stdout:
//!
//! ```text
//! stdin   {"rfq": { ...the announcement, verbatim... },
//!          "card": { ...the DAS metadata... }}
//!
//! stdout  {"appraisal_usd": 750000000}      the appraisal, MICRO-USD
//!    or   {"skip": "no comparable sale"}
//! ```
//!
//! # Units, once more
//!
//! `appraisal_usd` is 6-decimal MICRO-USD — the same unit as `maxPrincipalUsd`
//! on the RFQ you were just handed. $750 is `750000000`. A pricer that returns
//! `750` is quoting $0.00075, which `limits.min_appraisal_usd` will reject; that
//! is the intended way to notice this mistake, and it is why that floor should
//! never be set to zero.
//!
//! # Failure is a skip, never a default
//!
//! A pricer that crashes, times out, writes nothing, writes something that is
//! not JSON, or answers with neither key is an ERROR and the round is skipped.
//! Nothing here falls back to an internally derived number: an external pricer
//! that stopped working must stop the quoting, not quietly hand the decision
//! back to a default nobody chose.
//!
//! stderr is not captured into the answer; run your pricer's own logging there.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::das::CardMetadata;
use crate::pricing::{Priced, Pricer};
use crate::rfq::RfqAnnouncement;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PricerAnswer {
    /// The appraisal in 6-decimal micro-USD.
    appraisal_usd: Option<u64>,
    /// Decline, with a reason for the log.
    skip: Option<String>,
}

pub struct CommandPricer {
    path: PathBuf,
    timeout: Duration,
}

impl CommandPricer {
    pub fn new(path: PathBuf, timeout_ms: u64) -> Self {
        Self {
            path,
            timeout: Duration::from_millis(timeout_ms),
        }
    }
}

/// The exact bytes handed to the pricer on stdin.
pub fn request_json(rfq: &RfqAnnouncement, card: &CardMetadata) -> Result<String> {
    Ok(serde_json::to_string(&serde_json::json!({
        "rfq": rfq,
        "card": card,
    }))?)
}

/// Read the pricer's answer.
///
/// Exactly one of the two keys must be present. Both, or neither, is an error
/// rather than a preference for one of them: a pricer that answered ambiguously
/// has a bug, and picking a branch on its behalf is how an appraisal nobody
/// intended gets signed.
pub fn parse_answer(stdout: &str) -> Result<Priced> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        bail!("the pricer wrote nothing to stdout");
    }
    let answer: PricerAnswer = serde_json::from_str(trimmed)
        .with_context(|| format!("the pricer's stdout is not a recognised answer: {trimmed}"))?;
    match (answer.appraisal_usd, answer.skip) {
        (Some(_), Some(_)) => {
            bail!("the pricer returned both `appraisal_usd` and `skip`")
        }
        (Some(appraisal_usd), None) => Ok(Priced::Appraisal(appraisal_usd)),
        (None, Some(reason)) => Ok(Priced::Skip(reason)),
        (None, None) => bail!("the pricer returned neither `appraisal_usd` nor `skip`"),
    }
}

#[async_trait]
impl Pricer for CommandPricer {
    fn name(&self) -> &str {
        "command"
    }

    async fn price(&self, rfq: &RfqAnnouncement, card: &CardMetadata) -> Result<Priced> {
        let request = request_json(rfq, card)?;

        let mut child = tokio::process::Command::new(&self.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("cannot run the pricer at {}", self.path.display()))?;

        let mut stdin = child.stdin.take().context("the pricer has no stdin")?;
        // Written inside the timeout: a pricer that never reads its stdin would
        // otherwise block this task on a full pipe forever.
        let write = async move {
            stdin.write_all(request.as_bytes()).await?;
            stdin.shutdown().await
        };

        let collected = tokio::time::timeout(self.timeout, async {
            write.await?;
            child.wait_with_output().await
        })
        .await;

        let output = match collected {
            Ok(result) => result.context("the pricer failed while running")?,
            Err(_) => {
                bail!(
                    "the pricer at {} did not answer within {}ms",
                    self.path.display(),
                    self.timeout.as_millis()
                );
            }
        };

        if !output.status.success() {
            bail!(
                "the pricer at {} exited with {}",
                self.path.display(),
                output.status
            );
        }
        parse_answer(&String::from_utf8_lossy(&output.stdout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_appraisal_answer_is_read_as_micro_usd() {
        assert_eq!(
            parse_answer(r#"{"appraisal_usd": 750000000}"#).unwrap(),
            Priced::Appraisal(750_000_000)
        );
        assert_eq!(
            parse_answer("  {\"appraisal_usd\":1}\n").unwrap(),
            Priced::Appraisal(1),
            "whitespace and a trailing newline are ordinary"
        );
    }

    #[test]
    fn a_skip_answer_carries_its_reason_through() {
        assert_eq!(
            parse_answer(r#"{"skip": "no comparable sale"}"#).unwrap(),
            Priced::Skip("no comparable sale".to_string())
        );
    }

    /// Every one of these is a pricer that is not working. None of them may
    /// become a number: a broken pricer must stop the quoting, not hand the
    /// decision to a default.
    #[test]
    fn an_empty_ambiguous_or_unparseable_answer_is_an_error_and_never_a_number() {
        for bad in [
            "",
            "   ",
            "not json",
            "{}",
            r#"{"appraisal_usd": 1, "skip": "why"}"#,
            r#"{"appraisal": 1}"#,
            r#"{"appraisal_usd": -5}"#,
        ] {
            assert!(parse_answer(bad).is_err(), "{bad:?} must not price a card");
        }
    }

    #[test]
    fn the_request_carries_the_whole_announcement_and_the_whole_card() {
        let rfq = RfqAnnouncement {
            rfq_id: "r-1".into(),
            asset: "2o1f3ekWgw8h3bXG418qiE6Dx6nSuoj8H1W3Q5Bavy3y".into(),
            standard: 2,
            opened_at: 0,
            closes_at: 10,
            min_expiry_ts: 70,
            max_expiry_ts: 600,
            open_ltv_bps: 6_000,
            max_principal_usd: 5_000_000_000,
        };
        let card = CardMetadata {
            asset: rfq.asset.clone(),
            name: "1997 #143 Snorlax-Holo PSA 5 Jap".into(),
            face_value_usd: Some(1_250_000_000),
            ..Default::default()
        };

        let request: serde_json::Value =
            serde_json::from_str(&request_json(&rfq, &card).unwrap()).unwrap();
        assert_eq!(request["rfq"]["maxExpiryTs"], 600);
        assert_eq!(request["rfq"]["maxPrincipalUsd"], 5_000_000_000u64);
        assert_eq!(request["card"]["faceValueUsd"], 1_250_000_000u64);
        assert_eq!(request["card"]["name"], "1997 #143 Snorlax-Holo PSA 5 Jap");
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
            max_principal_usd: 1,
        }
    }

    #[cfg(unix)]
    fn script(name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("pawn-quote-bot-{name}.sh"));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_pricer_that_answers_in_time_prices_the_card() {
        let path = script(
            "ok",
            "cat > /dev/null; echo '{\"appraisal_usd\": 750000000}'",
        );
        let pricer = CommandPricer::new(path, 5_000);
        assert_eq!(
            pricer
                .price(&rfq(), &CardMetadata::default())
                .await
                .unwrap(),
            Priced::Appraisal(750_000_000)
        );
    }

    /// The auction window is ten seconds. A wedged pricer has to become a
    /// skipped round, not a stalled bot that misses every round after it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pricer_that_hangs_is_a_timeout_rather_than_a_stalled_bot() {
        let path = script("hang", "sleep 30");
        let pricer = CommandPricer::new(path, 200);
        let err = pricer
            .price(&rfq(), &CardMetadata::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not answer"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_pricer_that_exits_nonzero_prices_nothing() {
        let path = script("boom", "cat > /dev/null; exit 3");
        let pricer = CommandPricer::new(path, 5_000);
        let err = pricer
            .price(&rfq(), &CardMetadata::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exited with"), "{err}");
    }

    #[tokio::test]
    async fn a_pricer_that_does_not_exist_is_an_error_not_a_silent_skip() {
        let pricer = CommandPricer::new(PathBuf::from("/nonexistent/pricer"), 1_000);
        assert!(pricer
            .price(&rfq(), &CardMetadata::default())
            .await
            .is_err());
    }
}
