//! `histd` — the fleet's record of what every box did.
//!
//! Two records with two different owners and two different reasons to exist, and
//! keeping them apart is most of the design:
//!
//! **The evidence record.** `[A1 7.2]` says what a § 14a control event has to be
//! documented with — the ceiling, when it arrived, what was done about it, and a
//! trace of what the connection point actually drew — and `[A1 7.3]` says it is
//! kept for **two years**. `hems-grid::evidence` builds it; until now it lived
//! in memory and died with the process, which is the difference between a record
//! and an intention.
//!
//! **The settlement record.** The quarter-hour meter registers MiSpeL's
//! Abgrenzung and § 42c's allocation are computed from. Those are quantities
//! that end on an invoice, so every one of them is stored as an exact decimal
//! *string* and never as a float (P3): a settlement that went through an `f64`
//! is a settlement nobody can reproduce.
//!
//! # Where this runs, and why it is not on the box
//!
//! D1 makes the edge a **single** daemon: the § 14a failsafe is a sixty-second
//! heartbeat and a two-hour minimum, and an IPC hop inside that path buys
//! nothing. So a gateway runs `hemsd` and nothing else, and the box's own copy
//! of these records belongs in *its* embedded stores (`chronix`, `redb`) behind
//! a store-and-forward outbox — which is the half that is not written yet. This
//! daemon is the **fleet** side: everybody's two years, queryable, which is what
//! a network operator's Nachweis and a Data Act export are asked for at scale.
//!
//! # Why SQLite today, and what it is a prototype of
//!
//! `meterstore` — PostgreSQL for the recent window, Apache Iceberg for history —
//! is where a fleet holding millions of measuring points ends up, and it is what
//! D6 names. SQLite (`bundled`) is what is here now because it needs no server
//! and no system library, so every query in this daemon is exercised against a
//! *real* database in `cargo test` rather than against a mock, and `just ci`
//! stays a clone-and-run. The schema is written in `mako`'s layout so the move
//! is a second migration directory rather than a rewrite.
//!
//! # The schema is a `migrations/` directory, as in `mako`
//!
//! `migrations/NNNN_*.sql`, a new file per revision and never an edit to one
//! already applied (G4). `mako` is PostgreSQL and applies them with
//! `sqlx::migrate!`; this is SQLite, so the files are compiled in and the
//! applied revision lives in SQLite's own `user_version`. A database written by
//! a **newer** build is refused rather than used: two years of § 14a evidence is
//! the last record in this workspace that should be repaired by guesswork.
//!
//! # Retention is a column, not a policy
//!
//! `[A1 7.3]`'s two years live in `control_event.expires_at`. A retention rule
//! that is a `DELETE` somebody remembers to run is a rule nobody can query; one
//! that is a column can be asked "what will you still have in eighteen months",
//! which is the question a network operator's auditor actually asks.

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
pub mod export;
pub mod store;

pub use config::Settings;
pub use store::{Db, Store, StoreError};
