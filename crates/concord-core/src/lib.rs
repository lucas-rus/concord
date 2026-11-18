//! # concord-core
//!
//! Language-independent domain model of Concord:
//!
//! - [`symbol`]: symbols, contracts, bodies and use sites;
//! - [`change`]: classifying changes as contract or body changes;
//! - [`compat`]: whether a contract change breaks existing call sites;
//! - [`coherence`]: the contract-coherence protocol (directory, leases, notices,
//!   finish gate) as a pure state machine;
//! - [`conflict`]: offline pairwise analysis of concurrent branches.

pub mod change;
pub mod coherence;
pub mod compat;
pub mod conflict;
pub mod symbol;

pub use change::{ChangeKind, SymbolChange, diff_symbols};
pub use coherence::{AgentId, Blocker, CoherenceState, Directory, Gate, Notice, NoticeKind};
pub use compat::{Compat, classify};
pub use conflict::{BranchFootprint, Finding, Summary, analyze, summarize};
pub use symbol::{Contract, Digest, Param, ParamKind, Signature, Span, Symbol, SymbolId, SymbolKind, UseSite};
