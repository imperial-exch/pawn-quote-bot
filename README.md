# pawn-quote-bot

A market-maker bot for the Imperial pawn RFQ auction. It subscribes to the
announcement stream, prices each card, and answers with a signed appraisal
inside the auction's ten-second window.

It ships with two pricing engines and a trait so you can replace them.

---

## Read this part first

**Your appraisal is the strike of a put you have written.**

A borrower escrows a graded card and asks approved makers what it is worth. Your
answer — `amount_usd`, the *appraisal* — does two things:

1. The pool lends the borrower `appraisal × openLtvBps ÷ 10000`. That
   **principal** is what increments your on-chain `MarketMaker.outstanding_usd`
   and is measured against your `max_outstanding_usd`.
2. If the loan defaults, the card is offered to you **at your own appraisal**.
   `PawnLoan.floor_usd` is stamped from the number you signed, and `MM_BUY`
   settles at exactly that. You do not get to reprice it later, and you do not
   get to argue about it.

So you are short a put struck at your own mark, for a premium you were paid in
winning the auction. And the selection is against you: **a borrower defaults
precisely when the card is worth less than what they owe.** The cards that come
back to you are, systematically, the ones you overpriced. Cards you priced well
get repaid and you never see them again.

Two consequences worth internalising:

- **Winning more auctions is not the goal.** The highest appraisal wins the
  round, so an auction you win by a wide margin is one where the rest of the
  market disagreed with you. Being the only bid at a number is information.
- **Your discount to fair value is your entire risk budget.** There is no
  margin, no liquidation, no partial fill. There is your number and what the
  card is actually worth on the day it comes back.

You *can* decline to exercise — the put window lapses, the card goes to a grace
auction, and your on-chain `puts_walked` and `walked_usd` counters increment.
Those counters are append-only for the life of the account and nothing resets
them. Walking is not free, it is just not a transfer.

**Run with `dry_run = true` until you have watched real auctions go by and you
agree with every number this bot printed.**

---

## How the auction works

```
borrower escrows a card
        │
        ▼
  POST /pawn/rfq                        the borrower opens a round
        │
        ├──► SSE  /pawn/mm/rfq-stream   you hear about it  ─┐
        └──► GET  /pawn/mm/rfqs         or you poll for it ─┤
                                                            ▼
                             ~10 seconds to price and sign
                                                            │
  POST /pawn/rfq/{id}/quotes  ◄─────────────────────────────┘
        │
        ▼
  the window shuts, the best appraisal wins, the borrower claims it
        │
        ▼
  LOAN_OPEN — your outstanding_usd increases by the principal
```

If nobody answers, there is no loan. There is no protocol-run fallback quoter.

### The announcement

Every round you are offered carries these fields, and **every one of them is
stamped by the protocol. Never derive any of them yourself.**

| field | meaning |
| --- | --- |
| `rfqId` | the round |
| `asset` | the card's mint |
| `standard` | how it was escrowed: `0` SPL NFT, `1` pNFT, `2` MPL Core |
| `openedAt` / `closesAt` | the auction window |
| `minExpiryTs` | the earliest `expiryTs` a quote may carry |
| `maxExpiryTs` | the latest `expiryTs` a quote may carry |
| `openLtvBps` | the LTV the pool will lend at |
| `maxPrincipalUsd` | the largest principal the pool can write right now |

The expiry pair is the one to be careful about, because it is the program's
**entire** replay defense on a signed quote. The signature covers the payload
bytes and nothing else — not the program id, not the pool, not the borrower —
so what bounds a quote is `LOAN_OPEN`'s check against the chain clock:

```
now < expiryTs <= now + 900
```

Read that carefully: it makes the set of instants at which your envelope can
open a loan a **fifteen-minute window that ends at the expiry you signed**. It
does not matter how far out you put the expiry. Signing a year ahead does not
write a year-long quote; it writes a quote that is dead for a year and then live
for fifteen minutes. There is nothing else — no nonce, no counter, no
single-use flag anywhere on chain.

`maxExpiryTs` is the API's stamped ceiling and it is tighter than the program's,
at 600 seconds past `openedAt`. The 300-second gap is deliberate slack: the
program compares against the *chain* clock and the API against wall-clock, so a
quote signed at the API's ceiling still opens even if the cluster's clock is
running five minutes behind. Sign inside `[minExpiryTs, maxExpiryTs]` and you
never see either failure. This bot clamps to both ends and refuses at startup
any `expiry_secs` above 900, which could never be honoured.

`openLtvBps` and `maxPrincipalUsd` move between rounds too. Quote against the
terms you were handed, not against a snapshot you took earlier.

---

## Registration

You cannot quote until the pool authority has registered you on chain. Ask them
to run:

```
pawn_admin set-market-maker \
  --mm-authority   <YOUR_AUTHORITY_PUBKEY> \
  --quote-pubkey   <YOUR_QUOTE_KEY_PUBKEY> \
  --max-outstanding-usd <YOUR_CAP>
```

That writes a `MarketMaker` account at `[b"mm", authority]` under the pawn
program. You need `status == APPROVED` before anything you sign is accepted.

### Use two keys

**`authority`** is your identity and the seed of the account. It is what signs
`MM_BUY` when you exercise a put — real money, real custody. It never signs
anything this bot sends.

**`quote_pubkey`** is the hot key. It signs the appraisal payloads and the API
credentials, and it is expected to live on a server. It is **rotatable**: the
pool authority can point `quote_pubkey` at a new key in one transaction, and
that rotates your API credential in the same transaction.

A headless bot should hold only the quote key. Keep the authority key offline.
If the box is compromised, the attacker can sign appraisals until you rotate —
bad — but they cannot exercise puts, take custody, or move funds.

The bot enforces this at startup: it reads your `MarketMaker` account and
refuses to run if the account is missing, not `APPROVED`, or names a
`quote_pubkey` that is not the key in `maker.quote_keypair`. Each of those
otherwise presents as an unexplained wall of 401s.

### Authentication, once you are registered

Three headers on every request:

| header | value |
| --- | --- |
| `x-pawn-mm` | your authority pubkey, base58 — identity, not a signing key |
| `x-pawn-message` | the exact message signed: `{prefix}{unix_timestamp}` |
| `x-pawn-signature` | base58 ed25519 over that message, by `quote_pubkey` |

The scope is carried **entirely** by the prefix, and the two are not
interchangeable:

- `imperial-pawn-mm-stream:` — subscribe and poll
- `imperial-pawn-mm-quote:` — submit a quote

A credential is accepted for five minutes and there is no replay table, so it is
replayable inside that window by anyone who captures it. What it does *not* let
them do is forge an envelope — that needs your quote key. Ship credentials over
TLS and do not log them.

---

## Running it

```
cp config.example.toml config.toml
$EDITOR config.toml

cargo run --release -- --check      # config + on-chain registration, then exit
cargo run --release                 # dry run, if config says so
```

`--check` makes exactly one RPC call and tells you whether you are registered,
approved, and holding the right key. Run it first.

`--dry-run` on the command line forces dry run on regardless of the config.
There is deliberately no flag that turns it *off*: a bot that could be talked out
of dry run by a command line will be, on the wrong host.

Logging is `tracing`, controlled by `RUST_LOG` (`RUST_LOG=debug` to see the
exposure ledger).

### What a dry run prints

One line per round, with the appraisal it would have signed, the principal that
implies and the expiry. Watch it for a while. Compare its numbers against what
you would have said. Only then set `dry_run = false`.

---

## Configuration

Full annotated example in `config.example.toml`. Unknown keys are an error, not
an ignored line.

**Units.** This file is written in whole **dollars** (`2000` means $2,000;
`2000.50` works). The wire is 6-decimal **micro-USD**, where $2,000 is
`2000000000`. The conversion happens once, at load.

### `[maker]`

| key | meaning |
| --- | --- |
| `program_id` | the pawn program for your cluster. Never hardcoded — mainnet and staging differ, and the wrong one derives a PDA that does not exist |
| `api_base` | API root **including** the version prefix, e.g. `https://host/api/v1` |
| `authority` | your seed authority pubkey |
| `quote_keypair` | path to the hot signing key, a `solana-keygen` JSON byte array |
| `rpc_url` | a **DAS-capable** RPC |
| `dry_run` | price and log, submit nothing. **Defaults to `true`** |

### `[pricing]`

| key | meaning |
| --- | --- |
| `engine` | `"face_value_pct"` or `"command"` |
| `face_value_pct` | percent of face value to appraise at. `face_value_pct` engine only |
| `face_value_traits` | optional; attribute names searched, in order, for the face value |
| `[pricing.command] path` | executable to run |
| `[pricing.command] timeout_ms` | how long it gets, out of the ten-second window |

### `[limits]`

| key | meaning |
| --- | --- |
| `min_appraisal_usd` | floor on the **appraisal**. Below it, skip |
| `max_appraisal_usd` | ceiling on the **appraisal** |
| `max_outstanding_usd` | local mirror of your on-chain cap, in **principal** |
| `max_per_collection_usd` | concurrent principal against one collection |

`min_appraisal_usd` and `max_appraisal_usd` bound the **appraisal**, not the
principal — the appraisal is what you are on the hook for, so that is the number
worth bounding. Be clear about what the ceiling costs you: an appraisal of
`max_appraisal_usd` consumes `max_appraisal_usd × openLtvBps ÷ 10000` of your
on-chain `max_outstanding_usd`. At 60% LTV, a $2,000 ceiling eats $1,200 of cap
per card.

`max_outstanding_usd` is a **local mirror**. The chain is the real limit. Keep
this under the on-chain figure; the bot warns if you set it higher, and uses the
smaller of the two either way.

When an appraisal exceeds a cap the bot **clamps it down** rather than skipping —
a lower appraisal is a lower strike, so clamping down is always the safe
direction. It never clamps up. If the clamp lands below `min_appraisal_usd`, it
skips instead: a number you would not have bid is not a bargain.

### `[eligibility]`

| key | meaning |
| --- | --- |
| `standards` | collateral standards to quote. **Defaults to `["mpl_core"]`** |
| `collections` | verified collection addresses. **Empty means ANY** |
| `grading` | grading companies, case-insensitive. Empty means any |
| `grading_traits` | optional; attribute names searched for the grading company |

**Why MPL Core only.** This default is a solvency setting, not a preference.

A pNFT's transferability is gated by its `TokenRecord`, and a record in the
`Locked` state cannot be moved by Token Metadata at all — which is common in
these collections. If you win a quote on such a card and it defaults, your
`MM_BUY` reverts *inside Token Metadata* and **you cannot exercise the put you
already paid for by quoting**. You are left holding the exposure with no route
to the card.

Separately, write-off detection is only reliable for MPL Core burns. A card that
leaves custody by a route nothing can observe is a position you cannot mark.

Widen `standards` only if you have independently confirmed that you can take
delivery of the assets in question.

### `[quote]`

| key | meaning |
| --- | --- |
| `expiry_secs` | how long a signed quote stays valid |

A preference inside the round's announced window, not an override of it. The bot
raises it to `minExpiryTs` and cuts it down to `maxExpiryTs`, and refuses at
startup anything above the program's hard 900-second ceiling — a value above
that is not a longer quote, it is a quote no borrower can open a loan against.

Longer is not free: a live quote is a bearer instrument, and anyone holding it
can present it at any instant in the fifteen minutes before the expiry you
signed.

---

## Writing your own pricer

### Without Rust: `engine = "command"`

Point `[pricing.command] path` at any executable. It gets one JSON object on
stdin and answers with one on stdout.

```json
{
  "rfq":  { "rfqId": "...", "asset": "...", "standard": 2,
            "openedAt": 1, "closesAt": 11,
            "minExpiryTs": 71, "maxExpiryTs": 601,
            "openLtvBps": 6000, "maxPrincipalUsd": 5000000000 },
  "card": { "asset": "...", "name": "1997 #143 Snorlax-Holo PSA 5 Jap",
            "collection": "...", "collectionName": "...", "standard": 2,
            "compressed": false, "frozen": false,
            "attributes": { "Grade": "5", "Grading Company": "PSA" },
            "faceValueUsd": 1250000000, "gradingCompany": "PSA",
            "image": "..." }
}
```

Answer with **one** of:

```json
{"appraisal_usd": 750000000}
{"skip": "no comparable sale"}
```

`appraisal_usd` is 6-decimal **micro-USD** — the same unit as `maxPrincipalUsd`
in the object you were just handed. $750 is `750000000`. A pricer that returns
`750` is quoting $0.00075; `limits.min_appraisal_usd` is what catches that, which
is one reason never to set that floor to zero.

A minimal example:

```sh
#!/bin/sh
# 55% of face value, and nothing at all if we cannot read one.
jq -c 'if .card.faceValueUsd then
         {appraisal_usd: (.card.faceValueUsd * 55 / 100 | floor)}
       else
         {skip: "no face value"}
       end'
```

A pricer that crashes, times out, prints nothing, prints something that is not
JSON, or returns both keys is an **error**, and the round is skipped. Nothing
falls back to an internal default: a pricer that stopped working must stop the
quoting, not quietly hand the decision to a number nobody chose.

Your pricer's own logging goes to stderr, which is inherited.

### In Rust: implement `Pricer`

```rust
use async_trait::async_trait;
use pawn_quote_bot::das::CardMetadata;
use pawn_quote_bot::pricing::{Priced, Pricer};
use pawn_quote_bot::rfq::RfqAnnouncement;

struct MyComps;

#[async_trait]
impl Pricer for MyComps {
    fn name(&self) -> &str { "my_comps" }

    async fn price(
        &self,
        rfq: &RfqAnnouncement,
        card: &CardMetadata,
    ) -> anyhow::Result<Priced> {
        let Some(recent) = self.recent_sale(&card.asset).await? else {
            return Ok(Priced::Skip("no comparable sale".into()));
        };
        Ok(Priced::Appraisal(recent * 70 / 100))
    }
}
```

Then return it from `pricing::build` instead of the shipped engines. Everything
after the number — the caps, the clamp, the expiry, the signature, the
submission — is the bot's, and is applied identically whatever produced it.

Three rules, whichever route you take:

1. **Return micro-USD.**
2. **Return the appraisal, not the principal.** The pool lends a fraction of it;
   you are exposed for all of it.
3. **Skip rather than guess.** A skip costs one auction. An invented number
   costs the difference between it and the card.

Budget: the window is about ten seconds, and it includes the metadata read, the
signature and the round trip. An engine that takes longer has not priced the
card, it has missed it.

### The shipped `face_value_pct` engine

It reads the card's face value from Collector Crypt metadata via DAS `getAsset`,
looks for the first attribute matching `face_value_traits`, and appraises at
`face_value_pct` of it, rounded down.

**If the face value is missing or unparseable, it skips the card.** It never
substitutes a default. A maker that guesses a face value has written a put
struck at a number it invented.

Treat this engine as a starting point rather than a strategy. Face value is what
the issuer attested, not what the card trades at, and the percentage is the only
thing standing between the two — across grades, sets and a falling market.
Replace it with real comparables as soon as you have them.

The attribute names are configurable because nothing pins them. Check what the
collection you are quoting actually carries, and set `face_value_traits` to
match.

---

## Risk accounting

The bot counts exposure from two sources, because they measure different things:

- **On chain**, `MarketMaker.outstanding_usd` is principal out on loans that
  really opened. Authoritative, and it is what the API tests your quote against.
  But it only moves when a borrower claims a quote and opens the loan — seconds
  to minutes after you signed it.
- **Locally**, every quote you have signed is a live bearer instrument until it
  expires. Someone can open a loan against it at any moment inside that window.

So the ledger counts both: the confirmed on-chain figure (refreshed every 30
seconds, which is also how a mid-session suspension is noticed) plus every quote
signed since. Reservations are released when the quote's own `expiryTs` passes —
by then either the loan opened and the next refresh shows it, or the quote is
dead.

This double-counts in the overlap, on purpose. Overstating your exposure costs
you a quote. Understating it costs you the difference.

`max_per_collection_usd` is session-scoped concurrency only: the protocol has no
per-collection notion, so nothing on chain backs it.

---

## Building

```
cargo build --release
cargo test
cargo clippy --all-targets
```

Rust 1.86 or newer. No workspace inheritance and no path dependencies at
runtime — the crate builds standalone.

### A note for anyone reading this inside the Imperial monorepo

This directory is mirrored to a public repository, so it deliberately breaks two
of the monorepo's conventions:

- **No `workspace = true` and no `[lints] workspace = true`.** Dependency
  versions are literal, because nothing inherited from the workspace root exists
  once the crate is lifted out. It stays a workspace member so monorepo CI still
  compiles it.
- **No runtime dependency on `pawn_client`.** The `PWNQ` envelope encoder is
  reimplemented in `src/envelope.rs`, and `pawn_client` is a **dev-dependency
  only**, used by `tests/conformance.rs` to assert the two encoders agree byte
  for byte. A wire change fails there rather than in a maker's production quote.

  The publish job drops `[dev-dependencies]` and that test on the way out, so the
  public crate carries neither.
