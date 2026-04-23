//! Bounded context: agent lifecycle.
//!
//! P0 surface: domain types only. The agent lifecycle service, persistence,
//! and PTY transports land alongside their first real caller — see
//! `docs/architecture.md` "Deferred ideas" for the planned shape.

pub mod domain;
pub mod error;

pub use error::Error;
