//! What this bot is already on the hook for.
//!
//! Two sources, and they measure different things:
//!
//! * **On chain**, `MarketMaker.outstanding_usd` is principal out on loans that
//!   really opened. It is authoritative and it is the number the API tests a
//!   quote against — but it only moves when a borrower claims a quote and opens
//!   the loan, which is seconds to minutes after the quote was signed.
//! * **Locally**, every quote this bot has signed and submitted is a live bearer
//!   instrument until it expires. Someone can open a loan against it at any
//!   moment inside that window.
//!
//! So the ledger counts BOTH: the confirmed on-chain figure, plus every quote
//! signed since it was last read. Reservations are released when the quote's
//! own `expiry_ts` passes, by which point either the loan opened — and the next
//! on-chain refresh shows it — or the quote is dead. That is deliberately
//! pessimistic in the overlap: a quote that has already been consumed is
//! counted twice until the next refresh. Overstating your exposure costs you a
//! quote; understating it costs you the difference.

use std::collections::HashMap;

use crate::config::Usd;

#[derive(Clone, Debug)]
struct Reservation {
    principal_usd: u64,
    collection: Option<String>,
    expires_at: i64,
}

/// The bot's view of its own exposure.
pub struct Ledger {
    max_outstanding_usd: u64,
    max_per_collection_usd: u64,
    /// Last confirmed `MarketMaker.outstanding_usd`, micro-USD.
    onchain_outstanding_usd: u64,
    /// The on-chain cap, when it has been read. The effective cap is the
    /// smaller of this and the configured one — a local mirror above the real
    /// limit is not a limit.
    onchain_max_outstanding_usd: Option<u64>,
    reservations: HashMap<String, Reservation>,
}

impl Ledger {
    pub fn new(max_outstanding_usd: Usd, max_per_collection_usd: Usd) -> Self {
        Self {
            max_outstanding_usd: max_outstanding_usd.micros(),
            max_per_collection_usd: max_per_collection_usd.micros(),
            onchain_outstanding_usd: 0,
            onchain_max_outstanding_usd: None,
            reservations: HashMap::new(),
        }
    }

    pub fn observe_chain(&mut self, outstanding_usd: u64, max_outstanding_usd: u64) {
        self.onchain_outstanding_usd = outstanding_usd;
        self.onchain_max_outstanding_usd = Some(max_outstanding_usd);
    }

    /// The cap actually in force: the configured mirror, or the on-chain limit
    /// if that is lower.
    pub fn effective_cap_usd(&self) -> u64 {
        match self.onchain_max_outstanding_usd {
            Some(chain) => self.max_outstanding_usd.min(chain),
            None => self.max_outstanding_usd,
        }
    }

    pub fn drop_expired(&mut self, now: i64) {
        self.reservations.retain(|_, r| r.expires_at > now);
    }

    fn reserved_usd(&self) -> u64 {
        self.reservations.values().map(|r| r.principal_usd).sum()
    }

    fn reserved_for_collection(&self, collection: Option<&str>) -> u64 {
        self.reservations
            .values()
            .filter(|r| r.collection.as_deref() == collection)
            .map(|r| r.principal_usd)
            .sum()
    }

    /// Principal this bot could still put at risk.
    pub fn headroom_usd(&self) -> u64 {
        self.effective_cap_usd()
            .saturating_sub(self.onchain_outstanding_usd)
            .saturating_sub(self.reserved_usd())
    }

    /// Principal still available against one collection. `None` — a card in no
    /// verified collection — is its own bucket rather than exempt.
    pub fn collection_headroom_usd(&self, collection: Option<&str>) -> u64 {
        self.max_per_collection_usd
            .saturating_sub(self.reserved_for_collection(collection))
    }

    /// Record a signed quote. Keyed by RFQ id, so a round answered twice — a
    /// stream and a poll delivering the same announcement — reserves once.
    pub fn reserve(
        &mut self,
        rfq_id: &str,
        principal_usd: u64,
        collection: Option<String>,
        expires_at: i64,
    ) {
        self.reservations.insert(
            rfq_id.to_string(),
            Reservation {
                principal_usd,
                collection,
                expires_at,
            },
        );
    }

    /// Give back a reservation whose quote was never submitted — a rejected
    /// POST, a round that closed underneath the bot. Holding it would leak the
    /// cap down for the life of the process.
    pub fn release(&mut self, rfq_id: &str) {
        self.reservations.remove(rfq_id);
    }

    pub fn onchain_outstanding_usd(&self) -> u64 {
        self.onchain_outstanding_usd
    }

    pub fn live_reservations(&self) -> usize {
        self.reservations.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd(dollars: u64) -> u64 {
        dollars * 1_000_000
    }

    fn ledger() -> Ledger {
        Ledger::new(Usd(usd(800)), Usd(usd(500)))
    }

    #[test]
    fn a_fresh_ledger_offers_the_whole_configured_cap() {
        assert_eq!(ledger().headroom_usd(), usd(800));
        assert_eq!(ledger().collection_headroom_usd(Some("A")), usd(500));
    }

    /// The overlap that matters: a quote is a live bearer instrument the moment
    /// it is signed, long before any loan shows up on chain. Counting only the
    /// on-chain figure would let the bot sign its whole cap several times over
    /// inside one refresh interval.
    #[test]
    fn a_signed_quote_consumes_headroom_before_any_loan_appears_on_chain() {
        let mut ledger = ledger();
        ledger.reserve("rfq-1", usd(300), Some("A".into()), 1_000);
        assert_eq!(ledger.headroom_usd(), usd(500));

        ledger.reserve("rfq-2", usd(300), Some("A".into()), 1_000);
        assert_eq!(ledger.headroom_usd(), usd(200));
        assert_eq!(ledger.collection_headroom_usd(Some("A")), 0);
    }

    #[test]
    fn on_chain_exposure_and_local_reservations_both_count_against_the_cap() {
        let mut ledger = ledger();
        ledger.observe_chain(usd(400), usd(1_000));
        ledger.reserve("rfq-1", usd(100), None, 1_000);
        assert_eq!(ledger.headroom_usd(), usd(300));
    }

    /// A local mirror set above the on-chain limit is not a limit. The smaller
    /// of the two governs, whichever way round they are configured.
    #[test]
    fn the_smaller_of_the_local_mirror_and_the_on_chain_cap_governs() {
        let mut ledger = ledger();
        ledger.observe_chain(0, usd(500));
        assert_eq!(ledger.effective_cap_usd(), usd(500));
        assert_eq!(ledger.headroom_usd(), usd(500));

        ledger.observe_chain(0, usd(10_000));
        assert_eq!(
            ledger.effective_cap_usd(),
            usd(800),
            "the local mirror binds"
        );
    }

    #[test]
    fn a_reservation_is_released_once_its_quote_can_no_longer_open_a_loan() {
        let mut ledger = ledger();
        ledger.reserve("rfq-1", usd(300), Some("A".into()), 1_000);

        ledger.drop_expired(999);
        assert_eq!(ledger.headroom_usd(), usd(500), "still live at 999");

        ledger.drop_expired(1_001);
        assert_eq!(ledger.headroom_usd(), usd(800));
        assert_eq!(ledger.live_reservations(), 0);
    }

    /// The stream and the poll both deliver the same round, by design. Whatever
    /// dedupe happens upstream, reserving twice on one RFQ must not consume the
    /// cap twice.
    #[test]
    fn reserving_the_same_round_twice_consumes_the_cap_once() {
        let mut ledger = ledger();
        ledger.reserve("rfq-1", usd(300), Some("A".into()), 1_000);
        ledger.reserve("rfq-1", usd(300), Some("A".into()), 1_000);
        assert_eq!(ledger.headroom_usd(), usd(500));
        assert_eq!(ledger.live_reservations(), 1);
    }

    #[test]
    fn a_quote_that_was_never_submitted_gives_its_headroom_straight_back() {
        let mut ledger = ledger();
        ledger.reserve("rfq-1", usd(300), None, 1_000);
        ledger.release("rfq-1");
        assert_eq!(ledger.headroom_usd(), usd(800));
    }

    /// Cards in no verified collection share one bucket rather than escaping
    /// the per-collection limit entirely.
    #[test]
    fn cards_with_no_verified_collection_are_their_own_bucket_not_an_exemption() {
        let mut ledger = ledger();
        ledger.reserve("rfq-1", usd(500), None, 1_000);
        assert_eq!(ledger.collection_headroom_usd(None), 0);
        assert_eq!(ledger.collection_headroom_usd(Some("A")), usd(500));
    }
}
