//! Soil runtime: the deterministic transaction engine.
//!
//! The engine executes a sealed transaction's action graph over an adapter registry. All
//! effects of the outside world are injected: the [`Clock`], the [`AuthorityProvider`] and
//! [`ApprovalProvider`] (AgentScript/Crane integration points), and the [`EventSink`] that
//! receives evidence as it is appended. Given the same inputs and adapter responses, the engine
//! produces the same transitions and the same evidence hashes.

pub mod clock;
pub mod engine;
pub mod providers;
pub mod run;

pub use clock::{Clock, LogicalClock, SystemClock};
pub use engine::{Engine, EngineError, EngineOptions, RunResult};
pub use providers::{
    ApprovalDecision, ApprovalProvider, ApprovalRequest, AuthorityDecision, AuthorityProvider,
    AuthorityRequest, EventSink, NullSink, StaticApprovals, StaticAuthority,
};
pub use run::{
    FailureCause, NodeRun, Resolution, ResolutionOutcome, SkipReason, TransactionRun,
};
