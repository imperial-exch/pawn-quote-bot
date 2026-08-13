//! The Imperial pawn maker API.
//!
//! ```text
//! GET  {api_base}/pawn/mm/rfq-stream    SSE announcements   (stream scope)
//! GET  {api_base}/pawn/mm/rfqs          the same list       (stream scope)
//! POST {api_base}/pawn/rfq/{id}/quotes  one answer          (quote scope)
//! ```
//!
//! The stream is the fast path and the poll is the recovery path — they serve
//! the same rounds, and the poll is the complete one. A dropped stream is
//! ordinary, so [`Client::open_rfqs`] is polled on a timer regardless and the
//! caller deduplicates.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use tokio::sync::mpsc;

use crate::auth::{credential, Credential, QUOTE_PREFIX, STREAM_PREFIX};
use crate::rfq::RfqAnnouncement;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubmitQuoteRequest {
    envelope_hex: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitQuoteResponse {
    pub rfq_id: String,
    pub amount_usd: u64,
    pub principal_usd: u64,
    pub quotes_received: i64,
    pub closes_at: i64,
}

pub struct Client {
    http: reqwest::Client,
    base: String,
    authority: Pubkey,
    quote_key: Keypair,
}

impl Client {
    pub fn new(base: &str, authority: Pubkey, quote_key: Keypair) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .context("cannot build the HTTP client")?,
            base: base.trim_end_matches('/').to_string(),
            authority,
            quote_key,
        })
    }

    pub fn quote_pubkey(&self) -> Pubkey {
        use solana_sdk::signature::Signer;
        self.quote_key.pubkey()
    }

    /// The quoting key, for signing an envelope. Kept here so there is one copy
    /// of it in the process rather than one per caller.
    pub fn signer(&self) -> &dyn solana_sdk::signature::Signer {
        &self.quote_key
    }

    fn credential(&self, prefix: &str) -> Credential {
        credential(&self.authority, &self.quote_key, prefix, now_unix())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Every round still inside its window, across every replica. The recovery
    /// path when the stream drops, and the startup backlog.
    pub async fn open_rfqs(&self) -> Result<Vec<RfqAnnouncement>> {
        let request = self.http.get(self.url("/pawn/mm/rfqs"));
        let response = self.credential(STREAM_PREFIX).apply(request).send().await?;
        let response = check(response).await?;
        Ok(response.json().await?)
    }

    /// Answer one round. `envelope` is the raw `PWNQ` bytes; the API takes them
    /// hex-encoded and re-parses them, so what it verifies is what was signed.
    pub async fn submit_quote(&self, rfq_id: &str, envelope: &[u8]) -> Result<SubmitQuoteResponse> {
        let request = self
            .http
            .post(self.url(&format!("/pawn/rfq/{rfq_id}/quotes")))
            .json(&SubmitQuoteRequest {
                envelope_hex: hex::encode(envelope),
            });
        let response = self.credential(QUOTE_PREFIX).apply(request).send().await?;
        let response = check(response).await?;
        Ok(response.json().await?)
    }

    /// Subscribe to the announcement stream, forwarding each round onto `sink`.
    ///
    /// Returns when the stream ends or errors — reconnecting is the caller's
    /// job, because backoff policy is not this function's to decide.
    pub async fn stream_rfqs(&self, sink: &mpsc::Sender<RfqAnnouncement>) -> Result<()> {
        let request = self
            .http
            .get(self.url("/pawn/mm/rfq-stream"))
            // The stream is long-lived; the client-wide timeout would cut it.
            .timeout(Duration::from_secs(60 * 60 * 24))
            .header(reqwest::header::ACCEPT, "text/event-stream");
        let response = self.credential(STREAM_PREFIX).apply(request).send().await?;
        let response = check(response).await?;

        let mut decoder = SseDecoder::default();
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            for frame in decoder.push(&chunk?) {
                let Some(announcement) = frame.announcement() else {
                    continue;
                };
                if sink.send(announcement).await.is_err() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    // The API's error bodies say exactly which rule was broken — which expiry
    // bound it wanted, which cap bound. Losing that to a bare status code is
    // losing the only diagnostic there is.
    let body = response.text().await.unwrap_or_default();
    bail!("{status}: {}", body.trim());
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One decoded server-sent event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

impl SseFrame {
    /// The announcement this frame carries, if it is one. Unknown event names
    /// and unparseable payloads are skipped rather than fatal: the stream is
    /// allowed to grow fields and comment lines without taking the bot down.
    pub fn announcement(&self) -> Option<RfqAnnouncement> {
        if self.event.as_deref() != Some("rfq") {
            return None;
        }
        serde_json::from_str(&self.data).ok()
    }
}

/// A minimal `text/event-stream` decoder: accumulate bytes, emit a frame per
/// blank line. Chunk boundaries fall wherever the network puts them, so the
/// buffer is what makes a field split across two TCP reads survive.
#[derive(Default)]
pub struct SseDecoder {
    buffer: String,
}

impl SseDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut frames = Vec::new();
        while let Some(end) = self.buffer.find("\n\n") {
            let block: String = self.buffer.drain(..end + 2).collect();
            if let Some(frame) = decode_block(&block) {
                frames.push(frame);
            }
        }
        frames
    }
}

fn decode_block(block: &str) -> Option<SseFrame> {
    let mut frame = SseFrame::default();
    let mut data = Vec::new();
    for line in block.lines() {
        // A `:` in column zero is a comment — that is what the keep-alive is.
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => frame.event = Some(value.to_string()),
            "data" => data.push(value.to_string()),
            _ => {}
        }
    }
    if data.is_empty() && frame.event.is_none() {
        return None;
    }
    frame.data = data.join("\n");
    Some(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANNOUNCEMENT: &str = r#"{"rfqId":"7c9e6679-7425-40de-944b-e07fc1f90ae7","asset":"2o1f3ekWgw8h3bXG418qiE6Dx6nSuoj8H1W3Q5Bavy3y","standard":2,"openedAt":1000,"closesAt":1010,"minExpiryTs":1070,"maxExpiryTs":1600,"openLtvBps":6000,"maxPrincipalUsd":5000000000}"#;

    #[test]
    fn a_whole_frame_in_one_chunk_decodes_to_an_announcement() {
        let mut decoder = SseDecoder::default();
        let frames = decoder.push(format!("event: rfq\ndata: {ANNOUNCEMENT}\n\n").as_bytes());
        assert_eq!(frames.len(), 1);
        let announcement = frames[0].announcement().expect("an rfq");
        assert_eq!(announcement.max_expiry_ts, 1_600);
        assert_eq!(announcement.max_principal_usd, 5_000_000_000);
    }

    /// The network splits wherever it likes, including mid-field and mid-JSON.
    /// A decoder that assumed one chunk per frame would lose rounds.
    #[test]
    fn a_frame_split_across_chunks_is_reassembled() {
        let whole = format!("event: rfq\ndata: {ANNOUNCEMENT}\n\n");
        for split in [1, 7, 20, whole.len() - 3] {
            let mut decoder = SseDecoder::default();
            assert!(decoder.push(&whole.as_bytes()[..split]).is_empty());
            let frames = decoder.push(&whole.as_bytes()[split..]);
            assert_eq!(frames.len(), 1, "split at {split}");
            assert!(frames[0].announcement().is_some());
        }
    }

    #[test]
    fn several_frames_in_one_chunk_all_come_out() {
        let mut decoder = SseDecoder::default();
        let chunk =
            format!("event: rfq\ndata: {ANNOUNCEMENT}\n\nevent: rfq\ndata: {ANNOUNCEMENT}\n\n");
        assert_eq!(decoder.push(chunk.as_bytes()).len(), 2);
    }

    /// Keep-alive comments and event types this bot does not know about must
    /// not take the stream down or be mistaken for rounds.
    #[test]
    fn keep_alive_comments_and_unknown_events_yield_no_announcement() {
        let mut decoder = SseDecoder::default();
        assert!(decoder
            .push(b":\n\n")
            .iter()
            .all(|f| f.announcement().is_none()));

        let frames = decoder.push(b"event: something-new\ndata: {}\n\n");
        assert_eq!(frames.len(), 1);
        assert!(frames[0].announcement().is_none());

        let frames = decoder.push(b"event: rfq\ndata: not json\n\n");
        assert!(
            frames[0].announcement().is_none(),
            "a bad payload is skipped, not fatal"
        );
    }

    #[test]
    fn a_partial_trailing_frame_is_held_until_it_completes() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"event: rfq\ndata: {\"rfqId\"").is_empty());
        assert!(decoder.push(b":\"x\"").is_empty());
    }
}
