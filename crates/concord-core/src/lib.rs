//! # concord-core
//!
//! Language-independent domain model of Concord:
//!
//! - [`symbol`]: symbols, contracts, bodies and use sites;
//! - [`change`]: classifying changes as contract or body changes;
//! - [`coherence`]: the contract-coherence protocol (directory, leases, notices,
//!   finish gate) as a pure state machine;
//! - [`conflict`]: offline pairwise analysis of concurrent branches.

pub mod change;
pub mod coherence;
pub mod conflict;
pub mod symbol;

pub use change::{ChangeKind, SymbolChange, diff_symbols};
pub use coherence::{AgentId, Blocker, CoherenceState, Directory, Gate, Notice, NoticeKind};
pub use conflict::{BranchFootprint, Finding, Summary, analyze, summarize};
pub use symbol::{Contract, Digest, Span, Symbol, SymbolId, SymbolKind, UseSite};
