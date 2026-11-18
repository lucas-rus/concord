//! Program symbols, their contracts and bodies.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Fully qualified symbol name, e.g. `notify.email.send_email` or `shop.cart.Cart.add`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SymbolId(pub String);

impl SymbolId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The last path segment (`send_email` for `notify.email.send_email`).
    pub fn name(&self) -> &str {
        self.0.rsplit('.').next().unwrap_or(&self.0)
    }

    /// The enclosing qualified name (`notify.email` for `notify.email.send_email`).
    pub fn parent(&self) -> Option<&str> {
        self.0.rsplit_once('.').map(|(p, _)| p)
    }

    pub fn child(&self, name: &str) -> SymbolId {
        SymbolId(format!("{}.{}", self.0, name))
    }
}

impl fmt::Display for SymbolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SymbolId {
    fn from(s: &str) -> Self {
        SymbolId(s.to_owned())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Variable,
}

/// A content digest (BLAKE3, hex-encoded) of a canonical token stream.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(pub String);

impl Digest {
    pub fn of(canonical: &str) -> Self {
        Digest(blake3::hash(canonical.as_bytes()).to_hex().to_string())
    }

    /// First 12 hex characters, for display.
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(12)]
    }
}

/// The externally visible "shape" of a symbol: what other code depends on.
///
/// `display` is a human-readable one-liner used in notices; `digest` is computed
/// over a canonical token stream, so formatting and comments do not matter.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Contract {
    pub display: String,
    pub digest: Digest,
    /// Structured call signature, when the front-end can extract one. Used to
    /// tell breaking from compatible contract changes ([`crate::compat`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
}

impl Contract {
    pub fn new(display: impl Into<String>, canonical: &str) -> Self {
        Contract {
            display: display.into(),
            digest: Digest::of(canonical),
            signature: None,
        }
    }

    pub fn with_signature(mut self, signature: Signature) -> Self {
        self.signature = Some(signature);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    /// Before `/`.
    PositionalOnly,
    PositionalOrKeyword,
    /// `*args`.
    VarPositional,
    /// After `*` or `*args`.
    KeywordOnly,
    /// `**kwargs`.
    VarKeyword,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub kind: ParamKind,
    /// Canonical annotation text, if annotated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<String>,
    /// Canonical default value text, if the parameter has a default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

impl Param {
    pub fn has_default(&self) -> bool {
        self.default.is_some()
    }
}

/// How a symbol is called: parameters, return annotation, decorators, and for
/// classes the constructor parameters and bases.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Signature {
    pub params: Vec<Param>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decorators: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_async: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bases: Vec<String>,
}

/// 1-based, inclusive line range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    pub start_line: u32,
    pub end_line: u32,
}

impl Span {
    pub fn overlaps(&self, start: u32, end: u32) -> bool {
        self.start_line <= end && start <= self.end_line
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub id: SymbolId,
    pub kind: SymbolKind,
    /// Repository-relative path of the defining file.
    pub path: String,
    pub span: Span,
    pub contract: Contract,
    /// Digest of the canonical body (implementation) token stream.
    pub body: Digest,
}

/// A place in an agent's code that depends on a symbol (call, reference, import).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UseSite {
    pub path: String,
    pub line: u32,
    /// The enclosing symbol of the use (or the module for top-level code).
    pub from: SymbolId,
    /// Source text of the use, e.g. `send_email(user.email, reset_msg(token))`.
    pub text: String,
    /// Argument shape, if the use is a call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<CallArgs>,
}

/// Shape of the arguments at a call site, enough to check it against a signature.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CallArgs {
    pub positional: u32,
    pub keywords: Vec<String>,
    /// A `*iterable` splat is present (positional count is a lower bound).
    pub star: bool,
    /// A `**mapping` splat is present (keywords are not all known).
    pub double_star: bool,
}
