//! The runnable bot: subscribe, price, answer.
//!
//! ```text
//!   SSE /pawn/mm/rfq-stream ─┐
//!                            ├─► dedupe ─► eligibility ─► DAS ─► pricer
//!   poll /pawn/mm/rfqs ──────┘                                     │
//!                                                                  ▼
//!            submit ◄── sign ◄── local check_payload ◄── clamp to the caps
//! ```
//!
//! The stream is the fast path and the poll is the recovery path. They overlap
//! by design: an announcement delivered twice is quoted once, because the API
//! accepts one answer per maker per round and the bot deduplicates anyway.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use solana_sdk::signature::{read_keypair_file, Keypair, Signer};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use pawn_quote_bot::api::{now_unix, Client};
use pawn_quote_bot::chain::fetch_market_maker;
use pawn_quote_bot::config::Config;
use pawn_quote_bot::das::Das;
use pawn_quote_bot::limits::Ledger;
use pawn_quote_bot::pricing::{self, Pricer};
use pawn_quote_bot::quoting::{build_quote, eligible_by_announcement, eligible_by_card, price};
use pawn_quote_bot::rfq::{RfqAnnouncement, Skip};

/// How often the poll fallback runs. Well inside the ten-second window, so a
/// round is still answerable when the stream is down.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// How often the on-chain `MarketMaker` is re-read: exposure, cap and — the one
/// that stops the bot dead — approval status.
const CHAIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Reconnect backoff bounds for the announcement stream.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Rounds remembered for dedupe before the oldest are forgotten. Rounds live
/// ten seconds, so this is generous by orders of magnitude.
const SEEN_CAPACITY: usize = 4_096;

#[derive(Parser)]
#[command(
    name = "pawn-quote-bot",
    about = "Answer Imperial pawn RFQ auctions with signed appraisals"
)]
struct Args {
    #[arg(long, short, default_value = "config.toml", env = "QUOTE_BOT_CONFIG")]
    config: PathBuf,

    /// Price and log without submitting, whatever the config says. Never the
    /// other way round: there is no flag that turns dry run OFF.
    #[arg(long)]
    dry_run: bool,

    /// Check the configuration and the on-chain registration, then exit.
    #[arg(long)]
    check: bool,
}

struct Bot {
    config: Config,
    api: Client,
    das: Das,
    pricer: Box<dyn Pricer>,
    ledger: Mutex<Ledger>,
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let config = Config::load(&args.config)?;
    let quote_key = load_quote_key(&config.maker.quote_keypair)?;

    // The flag can only tighten. A bot that could be talked out of dry run by a
    // command line is a bot that will be, on the wrong host.
    let dry_run = config.maker.dry_run || args.dry_run;

    let registration = preflight(&config, &quote_key).await?;
    if args.check {
        info!("configuration and registration check passed");
        return Ok(());
    }

    let mut ledger = Ledger::new(
        config.limits.max_outstanding_usd,
        config.limits.max_per_collection_usd,
    );
    ledger.observe_chain(
        registration.outstanding_usd,
        registration.max_outstanding_usd,
    );

    let bot = Arc::new(Bot {
        das: Das::new(
            &config.maker.rpc_url,
            config.pricing.face_value_traits.clone(),
            config.eligibility.grading_traits.clone(),
        )?,
        pricer: pricing::build(&config)?,
        api: Client::new(&config.maker.api_base, config.maker.authority, quote_key)?,
        ledger: Mutex::new(ledger),
        config,
        dry_run,
    });

    if bot.dry_run {
        warn!(
            "DRY RUN — every round will be priced and logged, and nothing will be submitted. \
             Set maker.dry_run = false when the numbers below look right."
        );
    }

    let (tx, mut rx) = mpsc::channel::<RfqAnnouncement>(256);
    tokio::spawn(stream_loop(bot.clone(), tx.clone()));
    tokio::spawn(poll_loop(bot.clone(), tx));
    tokio::spawn(chain_loop(bot.clone()));

    let mut seen: HashSet<String> = HashSet::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                return Ok(());
            }
            announcement = rx.recv() => {
                let Some(announcement) = announcement else {
                    bail!("every announcement source stopped");
                };
                if seen.len() >= SEEN_CAPACITY {
                    seen.clear();
                }
                if !seen.insert(announcement.rfq_id.clone()) {
                    continue;
                }
                tokio::spawn(handle(bot.clone(), announcement));
            }
        }
    }
}

fn load_quote_key(path: &std::path::Path) -> Result<Keypair> {
    read_keypair_file(path).map_err(|e| {
        anyhow::anyhow!(
            "cannot read the quoting keypair at {}: {e}. It must be a JSON byte array, \
             the format solana-keygen writes.",
            path.display()
        )
    })
}

/// Refuse to start on a registration that cannot possibly quote, and say which
/// of the three reasons it is. Every one of them otherwise presents as an
/// unexplained wall of 401s.
async fn preflight(
    config: &Config,
    quote_key: &Keypair,
) -> Result<pawn_quote_bot::chain::MarketMaker> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let registration = fetch_market_maker(
        &http,
        &config.maker.rpc_url,
        &config.maker.authority,
        &config.maker.program_id,
    )
    .await
    .context("cannot read the market-maker registration")?;

    let Some(registration) = registration else {
        bail!(
            "no MarketMaker account for authority {} under program {} — this authority is not \
             registered on this cluster. The pool authority registers makers with \
             `pawn_admin set-market-maker`.",
            config.maker.authority,
            config.maker.program_id
        );
    };

    if registration.quote_pubkey != quote_key.pubkey() {
        bail!(
            "the registered quote_pubkey is {} but maker.quote_keypair holds {} — every \
             signature this bot produces would be refused. Point at the right key, or have the \
             pool authority rotate quote_pubkey to this one.",
            registration.quote_pubkey,
            quote_key.pubkey()
        );
    }
    if !registration.is_approved() {
        bail!(
            "this market maker is {} — quoting requires APPROVED",
            registration.status_name()
        );
    }
    if config.limits.max_outstanding_usd.micros() > registration.max_outstanding_usd {
        warn!(
            "limits.max_outstanding_usd ({}) is above the on-chain cap of {} micro-USD — the \
             chain binds, so the local mirror does nothing",
            config.limits.max_outstanding_usd, registration.max_outstanding_usd
        );
    }

    info!(
        authority = %config.maker.authority,
        quote_pubkey = %registration.quote_pubkey,
        status = registration.status_name(),
        outstanding_usd = registration.outstanding_usd,
        max_outstanding_usd = registration.max_outstanding_usd,
        quotes_won = registration.quotes_won,
        puts_honored = registration.puts_honored,
        puts_walked = registration.puts_walked,
        "registration"
    );
    Ok(registration)
}

async fn stream_loop(bot: Arc<Bot>, tx: mpsc::Sender<RfqAnnouncement>) {
    let mut backoff = BACKOFF_MIN;
    loop {
        match bot.api.stream_rfqs(&tx).await {
            Ok(()) => {
                info!("the announcement stream closed, reconnecting");
                backoff = BACKOFF_MIN;
            }
            Err(e) => {
                // Not an error-level event: the poll fallback covers the gap,
                // so a dropped stream costs latency rather than rounds.
                warn!("the announcement stream failed ({e}), retrying in {backoff:?}");
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

/// The recovery path. It runs unconditionally rather than only when the stream
/// is known to be down: a stream that is connected but silent looks identical
/// to a quiet market, and the difference is only visible here.
async fn poll_loop(bot: Arc<Bot>, tx: mpsc::Sender<RfqAnnouncement>) {
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        match bot.api.open_rfqs().await {
            Ok(open) => {
                for announcement in open {
                    if tx.send(announcement).await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => warn!("polling open RFQs failed: {e}"),
        }
    }
}

/// Keep the exposure figure and the approval status fresh. A maker suspended on
/// chain stops quoting here rather than discovering it one 401 at a time.
async fn chain_loop(bot: Arc<Bot>) {
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(http) => http,
        Err(e) => {
            error!("cannot build the RPC client: {e}");
            return;
        }
    };
    loop {
        tokio::time::sleep(CHAIN_REFRESH_INTERVAL).await;
        match fetch_market_maker(
            &http,
            &bot.config.maker.rpc_url,
            &bot.config.maker.authority,
            &bot.config.maker.program_id,
        )
        .await
        {
            Ok(Some(registration)) => {
                if !registration.is_approved() {
                    error!(
                        "this market maker is now {} on chain — quotes will be refused until \
                         it is APPROVED again",
                        registration.status_name()
                    );
                }
                let mut ledger = bot.ledger.lock().await;
                ledger.observe_chain(
                    registration.outstanding_usd,
                    registration.max_outstanding_usd,
                );
                ledger.drop_expired(now_unix());
                debug!(
                    headroom_usd = ledger.headroom_usd(),
                    live_reservations = ledger.live_reservations(),
                    "exposure refreshed"
                );
            }
            Ok(None) => error!("the market-maker account has disappeared"),
            Err(e) => warn!("cannot refresh the market-maker account: {e}"),
        }
    }
}

async fn handle(bot: Arc<Bot>, rfq: RfqAnnouncement) {
    let rfq_id = rfq.rfq_id.clone();
    if let Err(e) = quote_round(&bot, rfq).await {
        warn!(rfq_id, "could not answer: {e:#}");
        bot.ledger.lock().await.release(&rfq_id);
    }
}

async fn quote_round(bot: &Bot, rfq: RfqAnnouncement) -> Result<()> {
    let now = now_unix();
    let remaining = rfq.secs_remaining(now);
    if remaining <= 0 {
        debug!(rfq_id = rfq.rfq_id, "skipped: {}", Skip::WindowClosed);
        return Ok(());
    }

    if let Err(skip) = eligible_by_announcement(&rfq, &bot.config) {
        info!(rfq_id = rfq.rfq_id, asset = rfq.asset, "skipped: {skip}");
        return Ok(());
    }

    let card = bot
        .das
        .get_asset(&rfq.asset)
        .await
        .with_context(|| format!("cannot read metadata for {}", rfq.asset))?;

    if let Err(skip) = eligible_by_card(&rfq, &card, &bot.config) {
        info!(rfq_id = rfq.rfq_id, card = card.name, "skipped: {skip}");
        return Ok(());
    }

    let appraisal_usd = match price(bot.pricer.as_ref(), &rfq, &card).await? {
        Ok(appraisal_usd) => appraisal_usd,
        Err(skip) => {
            info!(rfq_id = rfq.rfq_id, card = card.name, "skipped: {skip}");
            return Ok(());
        }
    };

    // The headroom is read and the reservation taken under ONE lock, so two
    // rounds arriving together cannot both be sized against the same headroom
    // and jointly overshoot the cap.
    let quote = {
        let mut ledger = bot.ledger.lock().await;
        ledger.drop_expired(now);
        let headroom_usd = ledger.headroom_usd();
        let collection_headroom_usd = ledger.collection_headroom_usd(card.collection.as_deref());

        let quote = match build_quote(
            &rfq,
            &card,
            appraisal_usd,
            &bot.config,
            headroom_usd,
            collection_headroom_usd,
            now,
            bot.api.signer(),
        ) {
            Ok(quote) => quote,
            Err(skip) => {
                info!(rfq_id = rfq.rfq_id, card = card.name, "skipped: {skip}");
                return Ok(());
            }
        };
        ledger.reserve(
            &rfq.rfq_id,
            quote.principal_usd,
            quote.collection.clone(),
            quote.payload.expiry_ts,
        );
        quote
    };

    if bot.dry_run {
        // The reservation is released again: a dry run must not talk itself out
        // of quoting the next card.
        bot.ledger.lock().await.release(&rfq.rfq_id);
        info!(
            rfq_id = rfq.rfq_id,
            card = card.name,
            appraisal_usd = quote.payload.amount_usd,
            principal_usd = quote.principal_usd,
            expiry_ts = quote.payload.expiry_ts,
            secs_remaining = remaining,
            "DRY RUN — would submit"
        );
        return Ok(());
    }

    let accepted = bot
        .api
        .submit_quote(&rfq.rfq_id, &quote.envelope)
        .await
        .context("the API refused the quote")?;
    info!(
        rfq_id = accepted.rfq_id,
        card = card.name,
        appraisal_usd = accepted.amount_usd,
        principal_usd = accepted.principal_usd,
        quotes_received = accepted.quotes_received,
        "quoted"
    );
    Ok(())
}
