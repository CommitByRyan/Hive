//! Platform primitives for handle-level path authorization and connection-level
//! IP pinning.
//!
//! Every capability is reported explicitly so callers never mistake a fallback
//! (for example `canonicalize` on non-Windows) for a real OS guarantee.

mod error;
mod net;
mod path;

pub use error::PlatformError;
pub use net::{
    connect_pinned, net_capabilities, resolve_all, verify_peer, NetCapabilities, PinnedSocket,
};
pub use path::{
    default_path_platform, path_capabilities, DefaultPathPlatform, DirectoryEntry,
    DirectoryEntryKind, OpenMode, PathCapabilities, PathHandle, PathPlatform, RootHandle,
};
