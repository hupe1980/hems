//! The specialists.
//!
//! Each answers a question about a **population** that nothing else in this
//! workspace is positioned to answer. `obsd` counts breaches and lists them
//! exactly; what it cannot do is say whether one *cause* accounts for most of
//! them, because that is a correlation across many exact answers.
//!
//! Every one of them is a pure function. `agentplane` runs them anyway, because
//! what it provides is not inference: the run, its input, its answer and every
//! effect go into an append-only hash-chained log, and a replay re-executes the
//! logic while reading each effect back rather than performing it again. "Why
//! did the queue say that in March" becomes a replay instead of an argument —
//! and for a pure function the replay is exact.

pub mod compliance;
pub mod provenance;
