mod error;
mod records;
mod schema;
mod store;
mod time;

/// Highest usable event cursor: 2^53 - 1, the JavaScript safe-integer bound.
pub const MAX_SEQ: u64 = 9_007_199_254_740_991;

/// Version of the snapshot payload/state layout, used to invalidate caches.
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

pub use error::{StoreError, StoreResult};
pub use records::{
    can_retry, classify_recovery, ApprovalDecision, ApprovalInput, ApprovalRecord, LeaseGuard,
    MissionAggregate, MissionRecord, NodeRecord, RecoveryDecision, RunRecord, SideEffectState,
    SnapshotInput, SnapshotRecord,
};
pub use store::Store;
