//! `fleetd` — adopting a box, telling it what to run, and offering it updates.
//!
//! Three jobs, and the third is the one with a security argument behind it.
//!
//! # Enrolment
//!
//! A box arrives holding an **enrolment secret** an installer put on it, and
//! leaves holding a long-lived credential of its own. The secret is single-use:
//! a second attempt with the same one is refused, because an enrolment secret
//! that still works after the box is in the field is a credential sitting in an
//! installer's notes.
//!
//! # Configuration
//!
//! Versioned, and the box says which version it is running. That is the half
//! that is usually missing: a fleet that can only *push* configuration cannot
//! answer "how many of my boxes actually took the change", which is the question
//! asked the morning after a rollout.
//!
//! # Updates, and why the server is not the trust anchor
//!
//! `fleetd` publishes a [`hems_service::Release`] — a manifest and an Ed25519
//! signature over it. The box verifies the signature against a key **it was
//! built with**, then verifies the artefact's digest against the manifest, and
//! only then installs.
//!
//! So `fleetd` never holds the signing key. It holds signatures somebody else
//! produced, and a compromised `fleetd` can serve a manifest that no box will
//! accept. That is the Cyber Resilience Act's integrity requirement
//! (Regulation (EU) 2024/2847, Annex I Part I § 2(c)) implemented rather than
//! asserted — and it is the difference between "we use HTTPS" and "the update
//! is signed".

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::similar_names
)]

pub mod api;
pub mod config;
pub mod mcp_server;
pub mod registry;
pub mod store;

pub use config::Settings;
pub use registry::{Enrolled, EnrolmentError, Registry};
