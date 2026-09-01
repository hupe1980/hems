//! `tariffd` — the fleet's price service.
//!
//! `hems-tariff::source` parses what the five published sources publish, and
//! `hems-tariff::cache` decides what to do when two of them disagree. Both are
//! pure functions and neither of them has ever made a request. This is the
//! process that does.
//!
//! # The three jobs
//!
//! **Fetch.** Ask each configured source on a schedule, and keep asking after a
//! failure with a backoff that does not turn one outage into a denial of
//! service against somebody else's free API.
//!
//! **Reconcile.** Merge what arrives into the cache, under the trust order of
//! `hems-tariff::cache`, so a Tibber curve arriving after an ENTSO-E one does
//! not overwrite it.
//!
//! **Serve.** Answer the box's question — "what do you have for this horizon" —
//! and say honestly how much of it is covered, so a household with no prices
//! plans against a flat default *knowingly* rather than being told nothing is
//! wrong.
//!
//! # Why the fetching is behind a trait
//!
//! [`Upstream`] is the seam. In production it is [`Http`], which is `reqwest`.
//! In every test it is a table of captured responses, so the whole daemon —
//! schedule, backoff, reconciliation, readiness, the HTTP surface — is covered
//! without a network, in milliseconds, on a machine that is offline. That is the
//! same argument `hems-tariff::source` already makes about its parsers, carried
//! one layer out: a test that needs the internet is a test that is skipped.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    // Domain nouns — MiSpeL, SQLite, PostgreSQL, ENTSO-E, ICON-D2 — are
    // capitalised because that is how they are spelled, not because they are
    // identifiers. The same allowance the domain crates carry.
    clippy::doc_markdown,
    clippy::similar_names
)]

pub mod api;
pub mod config;
pub mod poller;
pub mod upstream;

pub use config::{Endpoint, Settings};
pub use poller::{PollOutcome, Poller};
pub use upstream::{Fetched, Http, Upstream, UpstreamError};
