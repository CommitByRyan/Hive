//! Pure tool policy: stable error codes, path/network boundaries, registry
//! metadata, and canonical request hashing.
//!
//! This crate performs no I/O beyond `PathPolicy` canonicalization. The
//! canonicalize-then-compare approach has a TOCTOU window: a concurrent
//! rename or reparse-point swap between the check and the later open is not
//! prevented. Callers must not describe this as handle-level sandboxing.

mod error;
mod hash;
mod network;
mod path;
mod registry;

pub use error::{ErrorCode, PolicyError};
pub use hash::{canonical_json, request_hash, HashRequest};
pub use network::{classify_ip, IpClass, NetworkPolicy};
pub use path::{is_within, AuthorizedPath, PathAccess, PathPolicy};
pub use registry::{ApprovalMode, RiskLevel, ToolRegistry, ToolSpec};
