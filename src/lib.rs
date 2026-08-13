//! A market-maker quoting bot for the Imperial pawn RFQ auction.
//!
//! # What you are actually doing
//!
//! A borrower escrows a graded card and asks approved makers what it is worth.
//! You have about ten seconds to answer with a signed appraisal. The best
//! appraisal wins and the pool lends the borrower `appraisal * openLtvBps /
//! 10000` against it.
//!
//! **Your appraisal is the strike of a put you have written.** If the loan
//! defaults, the card is offered to you at your own number. The borrower walks
//! away from the card precisely when it is worth less than what they owe, so the
//! cards that come back to you are selected against you. Quote accordingly, and
//! read the README before you turn `dry_run` off.
//!
//! The library is public so a third party can build on the pieces —
//! [`pricing::Pricer`] in particular — rather than fork the binary.

pub mod api;
pub mod auth;
pub mod chain;
pub mod config;
pub mod das;
pub mod envelope;
pub mod limits;
pub mod pricing;
pub mod quoting;
pub mod rfq;
