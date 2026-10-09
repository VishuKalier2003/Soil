//! Soil core: the authoritative, deterministic domain model.
//!
//! This crate performs no IO, reads no clock, uses no randomness and holds no global state.
//! Everything that touches the outside world (adapters' transports, clocks, persistence,
//! authority providers) is injected by `soil-runtime` or the control plane.
//!
//! See `docs/DESIGN.md` for the model this crate implements and `docs/INVARIANTS.md` for the
//! runtime invariants it upholds.

pub mod adapter;
pub mod canonical;
pub mod constraints;
pub mod document;
pub mod effect;
pub mod evidence;
pub mod expr;
pub mod flow;
pub mod guarantee;
pub mod model;
pub mod plan;
pub mod policy;
pub mod state;
pub mod template;
pub mod transaction;
pub mod validate;

/// Version of every public schema in this crate (documents, evidence, plans).
pub const API_VERSION: &str = "soil.dev/v1alpha1";

#[cfg(test)]
pub(crate) mod fixtures;
