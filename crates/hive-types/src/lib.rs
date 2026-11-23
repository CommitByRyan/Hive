use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use uuid::{Uuid, Variant};

mod cancel;

pub use cancel::CancellationToken;

pub const PROTOCOL_VERSION: &str = "0.1";
pub const EVENT_SCHEMA_VERSION: u32 = 1;
pub const HANDOFF_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_LEASE_SECONDS: u64 = 60;
pub const DEFAULT_RENEWAL_SECONDS: u64 = 20;
pub const DEFAULT_RETRY_BUDGET: u32 = 3;
pub const DEFAULT_APPROVAL_TIMEOUT_SECONDS: u64 = 600;

pub const ID_PREFIXES: [&str; 11] = [
    "m_", "r_", "n_", "a_", "tn_", "e_", "tc_", "ap_", "f_", "prj_", "tr_",
];

#[derive(Debug, Error)]
pub enum HiveError {
    #[error("invalid ID `{value}`: {reason}")]
    InvalidId { value: String, reason: String },
    #[error("invalid transition for {machine}: {from} -> {to}")]
    InvalidTransition {
        machine: &'static str,
        from: String,
        to: String,
    },
    #[error("invalid configuration `{field}`: {reason}")]
    InvalidConfig { field: &'static str, reason: String },
    #[error("invalid state `{value}` for {kind}")]
    InvalidState { kind: &'static str, value: String },
    #[error("invalid attempt number `{value}`: {reason}")]
    InvalidAttempt { value: u32, reason: String },
    #[error("invalid lease token: {reason}")]
    InvalidLeaseToken { reason: String },
    #[error("unsupported command `{command}`")]
    UnsupportedCommand { command: String },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl HiveError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidId { .. } => "invalid_id",
            Self::InvalidTransition { .. } => "invalid_transition",
            Self::InvalidConfig { .. } => "invalid_config",
            Self::InvalidState { .. } => "invalid_state",
            Self::InvalidAttempt { .. } => "invalid_attempt",
            Self::InvalidLeaseToken { .. } => "invalid_lease_token",
            Self::UnsupportedCommand { .. } => "unsupported_command",
            Self::Io(_) => "io_error",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IdPrefix {
    Mission,
    Run,
    Node,
    AgentSession,
    Turn,
    Event,
    ToolCall,
    Approval,
    Artifact,
    Project,
    Trace,
}

impl IdPrefix {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mission => "m_",
            Self::Run => "r_",
            Self::Node => "n_",
            Self::AgentSession => "a_",
            Self::Turn => "tn_",
            Self::Event => "e_",
            Self::ToolCall => "tc_",
            Self::Approval => "ap_",
            Self::Artifact => "f_",
            Self::Project => "prj_",
            Self::Trace => "tr_",
        }
    }

    pub fn parse(value: &str) -> Result<Self, HiveError> {
        match value {
            "m_" => Ok(Self::Mission),
            "r_" => Ok(Self::Run),
            "n_" => Ok(Self::Node),
            "a_" => Ok(Self::AgentSession),
            "tn_" => Ok(Self::Turn),
            "e_" => Ok(Self::Event),
            "tc_" => Ok(Self::ToolCall),
            "ap_" => Ok(Self::Approval),
            "f_" => Ok(Self::Artifact),
            "prj_" => Ok(Self::Project),
            "tr_" => Ok(Self::Trace),
            other => Err(HiveError::InvalidId {
                value: other.to_owned(),
                reason: "unknown ID prefix".to_owned(),
            }),
        }
    }
}

impl Display for IdPrefix {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

macro_rules! define_prefixed_id {
    ($name:ident, $prefix:literal, $prefix_kind:expr) => {
        #[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn parse(value: &str) -> Result<Self, HiveError> {
                let uuid = value
                    .strip_prefix($prefix)
                    .ok_or_else(|| HiveError::InvalidId {
                        value: value.to_owned(),
                        reason: format!("ID prefix must be {}", $prefix),
                    })?;
                let uuid = Uuid::parse_str(uuid).map_err(|_| HiveError::InvalidId {
                    value: value.to_owned(),
                    reason: "invalid UUID".to_owned(),
                })?;
                Self::from_uuid(uuid)
            }

            pub fn from_uuid(uuid: Uuid) -> Result<Self, HiveError> {
                if uuid.get_version_num() != 4 {
                    return Err(HiveError::InvalidId {
                        value: format!("{}{}", $prefix, uuid),
                        reason: "UUID version must be 4".to_owned(),
                    });
                }
                if !matches!(uuid.get_variant(), Variant::RFC4122) {
                    return Err(HiveError::InvalidId {
                        value: format!("{}{}", $prefix, uuid),
                        reason: "UUID variant must be RFC 4122".to_owned(),
                    });
                }
                Ok(Self(uuid))
            }

            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            pub fn prefixed(&self) -> String {
                format!("{}{}", $prefix, self.0)
            }

            pub const fn prefix(&self) -> IdPrefix {
                $prefix_kind
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl Display for $name {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.prefixed())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.prefixed())
            }
        }

        impl FromStr for $name {
            type Err = HiveError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.prefixed())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(&value).map_err(de::Error::custom)
            }
        }
    };
}

define_prefixed_id!(MissionId, "m_", IdPrefix::Mission);
define_prefixed_id!(RunId, "r_", IdPrefix::Run);
define_prefixed_id!(NodeId, "n_", IdPrefix::Node);
define_prefixed_id!(AgentSessionId, "a_", IdPrefix::AgentSession);
define_prefixed_id!(TurnId, "tn_", IdPrefix::Turn);
define_prefixed_id!(EventId, "e_", IdPrefix::Event);
define_prefixed_id!(ToolCallId, "tc_", IdPrefix::ToolCall);
define_prefixed_id!(ApprovalId, "ap_", IdPrefix::Approval);
define_prefixed_id!(ArtifactId, "f_", IdPrefix::Artifact);
define_prefixed_id!(ProjectId, "prj_", IdPrefix::Project);
define_prefixed_id!(TraceId, "tr_", IdPrefix::Trace);

/// Logical execution count for a Node. Starts at 1; zero is invalid.
///
/// This is deliberately a distinct newtype from [`LeaseToken`]: retries
/// increment the attempt number while every new lease gets a fresh token.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AttemptNo(u32);

impl AttemptNo {
    pub const FIRST: Self = Self(1);

    pub fn new(value: u32) -> Result<Self, HiveError> {
        if value == 0 {
            return Err(HiveError::InvalidAttempt {
                value,
                reason: "attempt number starts at 1".to_owned(),
            });
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u32 {
        self.0
    }

    /// Saturating increment for tests and internal callers that already know
    /// the value cannot reach `u32::MAX`. Prefer [`Self::checked_next`] on the
    /// real execution path.
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn is_first(self) -> bool {
        self.0 == 1
    }
}

impl Serialize for AttemptNo {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u32(self.0)
    }
}

impl<'de> Deserialize<'de> for AttemptNo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

impl Display for AttemptNo {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Opaque lease ownership token. Generated randomly (UUID v4) and never
/// interchangeable with [`AttemptNo`].
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LeaseToken(String);

impl LeaseToken {
    pub fn generate() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn parse(value: &str) -> Result<Self, HiveError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(HiveError::InvalidLeaseToken {
                reason: "token must not be empty".to_owned(),
            });
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for LeaseToken {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for LeaseToken {
    type Err = HiveError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

macro_rules! impl_state_display_from_str {
    ($state:ty, $kind:literal) => {
        impl Display for $state {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $state {
            type Err = HiveError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }
    };
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionState {
    Queued,
    Planning,
    Running,
    Paused,
    Done,
    Failed,
    Cancelled,
}

impl MissionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Planning => "planning",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Result<Self, HiveError> {
        match value {
            "queued" => Ok(Self::Queued),
            "planning" => Ok(Self::Planning),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(HiveError::InvalidState {
                kind: "MissionState",
                value: other.to_owned(),
            }),
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Planning)
                | (Self::Planning, Self::Running)
                | (Self::Running, Self::Paused)
                | (Self::Running, Self::Done)
                | (Self::Paused, Self::Running)
                | (
                    Self::Queued | Self::Planning | Self::Running | Self::Paused,
                    Self::Failed | Self::Cancelled
                )
        )
    }

    pub fn transition(self, next: Self) -> Result<Self, HiveError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(HiveError::InvalidTransition {
                machine: "MissionState",
                from: self.to_string(),
                to: next.to_string(),
            })
        }
    }
}

impl_state_display_from_str!(MissionState, "MissionState");

/// Read-only, derived Mission view. `WaitingApproval` is an observation
/// computed from Node state and must never be written to `missions.state`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObservation {
    Queued,
    Planning,
    Running,
    WaitingApproval,
    Paused,
    Done,
    Failed,
    Cancelled,
}

impl MissionObservation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Planning => "planning",
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Result<Self, HiveError> {
        match value {
            "queued" => Ok(Self::Queued),
            "planning" => Ok(Self::Planning),
            "running" => Ok(Self::Running),
            "waiting_approval" => Ok(Self::WaitingApproval),
            "paused" => Ok(Self::Paused),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(HiveError::InvalidState {
                kind: "MissionObservation",
                value: other.to_owned(),
            }),
        }
    }
}

impl_state_display_from_str!(MissionObservation, "MissionObservation");

/// Derive the Mission observation. `WaitingApproval` appears only when the
/// mission is running, no Node is runnable, and at least one Node is waiting
/// for approval. All other cases map one-to-one to the persisted state.
pub fn observe_mission(
    state: MissionState,
    has_runnable_node: bool,
    has_pending_approval: bool,
) -> MissionObservation {
    match state {
        MissionState::Queued => MissionObservation::Queued,
        MissionState::Planning => MissionObservation::Planning,
        MissionState::Running => {
            if !has_runnable_node && has_pending_approval {
                MissionObservation::WaitingApproval
            } else {
                MissionObservation::Running
            }
        }
        MissionState::Paused => MissionObservation::Paused,
        MissionState::Done => MissionObservation::Done,
        MissionState::Failed => MissionObservation::Failed,
        MissionState::Cancelled => MissionObservation::Cancelled,
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Created,
    Planning,
    Running,
    Paused,
    Done,
    Failed,
    Cancelled,
}

impl RunState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Planning => "planning",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Result<Self, HiveError> {
        match value {
            "created" => Ok(Self::Created),
            "planning" => Ok(Self::Planning),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(HiveError::InvalidState {
                kind: "RunState",
                value: other.to_owned(),
            }),
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Created, Self::Planning)
                | (Self::Planning, Self::Running)
                | (Self::Running, Self::Paused)
                | (Self::Running, Self::Done)
                | (Self::Paused, Self::Running)
                | (
                    Self::Created | Self::Planning | Self::Running | Self::Paused,
                    Self::Failed | Self::Cancelled
                )
        )
    }

    pub fn transition(self, next: Self) -> Result<Self, HiveError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(HiveError::InvalidTransition {
                machine: "RunState",
                from: self.to_string(),
                to: next.to_string(),
            })
        }
    }
}

impl_state_display_from_str!(RunState, "RunState");

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Pending,
    Ready,
    Leased,
    Running,
    WaitingApproval,
    NeedsRecovery,
    Succeeded,
    Failed,
    RetryReady,
    FailedFinal,
    Cancelled,
    Skipped,
}

impl NodeState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Leased => "leased",
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::NeedsRecovery => "needs_recovery",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::RetryReady => "retry_ready",
            Self::FailedFinal => "failed_final",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }

    pub fn parse(value: &str) -> Result<Self, HiveError> {
        match value {
            "pending" => Ok(Self::Pending),
            "ready" => Ok(Self::Ready),
            "leased" => Ok(Self::Leased),
            "running" => Ok(Self::Running),
            "waiting_approval" => Ok(Self::WaitingApproval),
            "needs_recovery" => Ok(Self::NeedsRecovery),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "retry_ready" => Ok(Self::RetryReady),
            "failed_final" => Ok(Self::FailedFinal),
            "cancelled" => Ok(Self::Cancelled),
            "skipped" => Ok(Self::Skipped),
            other => Err(HiveError::InvalidState {
                kind: "NodeState",
                value: other.to_owned(),
            }),
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::FailedFinal | Self::Cancelled | Self::Skipped
        )
    }

    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::Ready | Self::Cancelled | Self::Skipped)
                | (Self::Ready, Self::Leased | Self::Cancelled)
                | (Self::Leased, Self::Running | Self::Ready | Self::Cancelled)
                | (
                    Self::Running,
                    Self::WaitingApproval
                        | Self::NeedsRecovery
                        | Self::Succeeded
                        | Self::Failed
                        | Self::Cancelled
                )
                | (
                    Self::WaitingApproval,
                    Self::Running | Self::Failed | Self::Cancelled
                )
                | (
                    Self::NeedsRecovery,
                    Self::Ready | Self::FailedFinal | Self::Cancelled
                )
                | (
                    Self::Failed,
                    Self::RetryReady | Self::FailedFinal | Self::Cancelled
                )
                | (Self::RetryReady, Self::Ready | Self::FailedFinal)
        )
    }

    pub fn transition(self, next: Self) -> Result<Self, HiveError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(HiveError::InvalidTransition {
                machine: "NodeState",
                from: self.to_string(),
                to: next.to_string(),
            })
        }
    }
}

impl_state_display_from_str!(NodeState, "NodeState");

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    Approved,
    Rejected,
    Expired,
    Cancelled,
}

impl ApprovalState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Result<Self, HiveError> {
        match value {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "rejected" => Ok(Self::Rejected),
            "expired" => Ok(Self::Expired),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(HiveError::InvalidState {
                kind: "ApprovalState",
                value: other.to_owned(),
            }),
        }
    }

    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }

    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Pending,
                Self::Approved | Self::Rejected | Self::Expired | Self::Cancelled
            )
        )
    }

    pub fn transition(self, next: Self) -> Result<Self, HiveError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(HiveError::InvalidTransition {
                machine: "ApprovalState",
                from: self.to_string(),
                to: next.to_string(),
            })
        }
    }
}

impl_state_display_from_str!(ApprovalState, "ApprovalState");

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    MissionCreated,
    MissionStarted,
    PlanCreated,
    PlanRevised,
    PlanDegraded,
    NodeReady,
    NodeStarted,
    NodeTurn,
    NodeThought,
    NodeToolCall,
    NodeToolStarted,
    NodeToolResult,
    NodeApprovalRequested,
    NodeApprovalDecided,
    NodeSucceeded,
    NodeFailed,
    NodeBudgetExceeded,
    NodeCancelled,
    NodeSkipped,
    MissionSummary,
    MissionDone,
    MissionFailed,
    MissionCancelled,
    MessageUser,
    RoutineCreated,
    RoutineUpdated,
    RoutineDeleted,
    RoutineTriggered,
    RoutineError,
    ArtifactSaved,
    ModelRetry,
    ModelFallback,
}

impl EventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissionCreated => "mission.created",
            Self::MissionStarted => "mission.started",
            Self::PlanCreated => "plan.created",
            Self::PlanRevised => "plan.revised",
            Self::PlanDegraded => "plan.degraded",
            Self::NodeReady => "node.ready",
            Self::NodeStarted => "node.started",
            Self::NodeTurn => "node.turn",
            Self::NodeThought => "node.thought",
            Self::NodeToolCall => "node.tool_call",
            Self::NodeToolStarted => "node.tool_started",
            Self::NodeToolResult => "node.tool_result",
            Self::NodeApprovalRequested => "node.approval_requested",
            Self::NodeApprovalDecided => "node.approval_decided",
            Self::NodeSucceeded => "node.succeeded",
            Self::NodeFailed => "node.failed",
            Self::NodeBudgetExceeded => "node.budget_exceeded",
            Self::NodeCancelled => "node.cancelled",
            Self::NodeSkipped => "node.skipped",
            Self::MissionSummary => "mission.summary",
            Self::MissionDone => "mission.done",
            Self::MissionFailed => "mission.failed",
            Self::MissionCancelled => "mission.cancelled",
            Self::MessageUser => "message.user",
            Self::RoutineCreated => "routine.created",
            Self::RoutineUpdated => "routine.updated",
            Self::RoutineDeleted => "routine.deleted",
            Self::RoutineTriggered => "routine.triggered",
            Self::RoutineError => "routine.error",
            Self::ArtifactSaved => "artifact.saved",
            Self::ModelRetry => "model.retry",
            Self::ModelFallback => "model.fallback",
        }
    }

    pub fn parse(value: &str) -> Result<Self, HiveError> {
        match value {
            "mission.created" => Ok(Self::MissionCreated),
            "mission.started" => Ok(Self::MissionStarted),
            "plan.created" => Ok(Self::PlanCreated),
            "plan.revised" => Ok(Self::PlanRevised),
            "plan.degraded" => Ok(Self::PlanDegraded),
            "node.ready" => Ok(Self::NodeReady),
            "node.started" => Ok(Self::NodeStarted),
            "node.turn" => Ok(Self::NodeTurn),
            "node.thought" => Ok(Self::NodeThought),
            "node.tool_call" => Ok(Self::NodeToolCall),
            "node.tool_started" => Ok(Self::NodeToolStarted),
            "node.tool_result" => Ok(Self::NodeToolResult),
            "node.approval_requested" => Ok(Self::NodeApprovalRequested),
            "node.approval_decided" => Ok(Self::NodeApprovalDecided),
            "node.succeeded" => Ok(Self::NodeSucceeded),
            "node.failed" => Ok(Self::NodeFailed),
            "node.budget_exceeded" => Ok(Self::NodeBudgetExceeded),
            "node.cancelled" => Ok(Self::NodeCancelled),
            "node.skipped" => Ok(Self::NodeSkipped),
            "mission.summary" => Ok(Self::MissionSummary),
            "mission.done" => Ok(Self::MissionDone),
            "mission.failed" => Ok(Self::MissionFailed),
            "mission.cancelled" => Ok(Self::MissionCancelled),
            "message.user" => Ok(Self::MessageUser),
            "routine.created" => Ok(Self::RoutineCreated),
            "routine.updated" => Ok(Self::RoutineUpdated),
            "routine.deleted" => Ok(Self::RoutineDeleted),
            "routine.triggered" => Ok(Self::RoutineTriggered),
            "routine.error" => Ok(Self::RoutineError),
            "artifact.saved" => Ok(Self::ArtifactSaved),
            "model.retry" => Ok(Self::ModelRetry),
            "model.fallback" => Ok(Self::ModelFallback),
            other => Err(HiveError::InvalidState {
                kind: "EventKind",
                value: other.to_owned(),
            }),
        }
    }
}

impl_state_display_from_str!(EventKind, "EventKind");

/// Input for appending an event. `seq`, `schema_version` and `created_at`
/// are assigned by the single-writer store, so they are not part of this
/// input record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventInput {
    pub event_id: EventId,
    pub kind: EventKind,
    pub mission_id: MissionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<NodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<AgentSessionId>,
    #[serde(default)]
    pub attempt_no: Option<AttemptNo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<TraceId>,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Persisted event envelope. This is the authoritative client-facing record;
/// it deliberately has no `agent` field, which is a projection of
/// `session_id` at the read/SSE layer (PROTOCOL.md section 6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: u32,
    pub event_id: EventId,
    pub seq: u64,
    pub kind: EventKind,
    pub mission_id: MissionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<NodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<AgentSessionId>,
    #[serde(default)]
    pub attempt_no: Option<AttemptNo>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<TraceId>,
    #[serde(default)]
    pub payload: serde_json::Value,
}

impl EventEnvelope {
    pub fn new(input: EventInput, seq: u64, created_at: String) -> Self {
        Self {
            schema_version: EVENT_SCHEMA_VERSION,
            event_id: input.event_id,
            seq,
            kind: input.kind,
            mission_id: input.mission_id,
            run_id: input.run_id,
            node_id: input.node_id,
            session_id: input.session_id,
            attempt_no: input.attempt_no,
            created_at,
            trace_id: input.trace_id,
            payload: input.payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID_V4: &str = "00000000-0000-4000-8000-000000000001";
    const UUID_V1: &str = "00000000-0000-1000-8000-000000000001";

    #[test]
    fn all_id_types_generate_prefixed_values() {
        assert!(MissionId::new().to_string().starts_with("m_"));
        assert!(RunId::new().to_string().starts_with("r_"));
        assert!(NodeId::new().to_string().starts_with("n_"));
        assert!(AgentSessionId::new().to_string().starts_with("a_"));
        assert!(TurnId::new().to_string().starts_with("tn_"));
        assert!(EventId::new().to_string().starts_with("e_"));
        assert!(ToolCallId::new().to_string().starts_with("tc_"));
        assert!(ApprovalId::new().to_string().starts_with("ap_"));
        assert!(ArtifactId::new().to_string().starts_with("f_"));
        assert!(ProjectId::new().to_string().starts_with("prj_"));
        assert!(TraceId::new().to_string().starts_with("tr_"));
    }

    #[test]
    fn prefixed_ids_round_trip_and_reject_wrong_prefix() {
        let value = format!("m_{UUID_V4}");
        let mission = MissionId::parse(&value).expect("valid mission id");
        assert_eq!(mission.to_string(), value);
        assert_eq!(mission.prefix(), IdPrefix::Mission);
        assert_eq!(mission.as_uuid(), &Uuid::parse_str(UUID_V4).expect("uuid"));

        let error = MissionId::parse(&format!("r_{UUID_V4}")).expect_err("wrong prefix");
        assert_eq!(error.code(), "invalid_id");
    }

    #[test]
    fn prefixed_ids_reject_non_v4_uuid() {
        assert!(MissionId::parse(&format!("m_{UUID_V1}")).is_err());
        assert!(MissionId::parse("m_not-a-uuid").is_err());
    }

    #[test]
    fn mission_state_transitions_are_checked() {
        assert_eq!(
            MissionState::Queued
                .transition(MissionState::Planning)
                .expect("valid"),
            MissionState::Planning
        );
        assert_eq!(
            MissionState::Running
                .transition(MissionState::Paused)
                .expect("valid"),
            MissionState::Paused
        );
        assert!(MissionState::Done
            .transition(MissionState::Running)
            .is_err());
        assert!(!MissionState::Running.is_terminal());
        assert!(MissionState::Done.is_terminal());
    }

    #[test]
    fn node_state_transitions_cover_retry_and_recovery() {
        assert_eq!(
            NodeState::Failed
                .transition(NodeState::RetryReady)
                .expect("valid"),
            NodeState::RetryReady
        );
        assert_eq!(
            NodeState::RetryReady
                .transition(NodeState::Ready)
                .expect("valid"),
            NodeState::Ready
        );
        assert_eq!(
            NodeState::NeedsRecovery
                .transition(NodeState::FailedFinal)
                .expect("valid"),
            NodeState::FailedFinal
        );
        assert!(NodeState::Succeeded.transition(NodeState::Running).is_err());
        assert!(NodeState::FailedFinal.is_terminal());
    }

    #[test]
    fn approval_state_is_terminal_after_decision() {
        assert_eq!(
            ApprovalState::Pending
                .transition(ApprovalState::Expired)
                .expect("valid"),
            ApprovalState::Expired
        );
        assert!(ApprovalState::Approved
            .transition(ApprovalState::Rejected)
            .is_err());
        assert_eq!(ApprovalState::Pending.to_string(), "pending");
        assert_eq!(
            ApprovalState::from_str("approved").expect("parse"),
            ApprovalState::Approved
        );
    }

    #[test]
    fn state_names_round_trip() {
        for value in [
            MissionState::Queued,
            MissionState::Planning,
            MissionState::Running,
            MissionState::Paused,
            MissionState::Done,
            MissionState::Failed,
            MissionState::Cancelled,
        ] {
            assert_eq!(
                MissionState::from_str(&value.to_string()).expect("parse"),
                value
            );
        }

        for value in [
            NodeState::Pending,
            NodeState::Ready,
            NodeState::Leased,
            NodeState::Running,
            NodeState::WaitingApproval,
            NodeState::NeedsRecovery,
            NodeState::Succeeded,
            NodeState::Failed,
            NodeState::RetryReady,
            NodeState::FailedFinal,
            NodeState::Cancelled,
            NodeState::Skipped,
        ] {
            assert_eq!(
                NodeState::from_str(&value.to_string()).expect("parse"),
                value
            );
        }
    }

    #[test]
    fn attempt_no_starts_at_one_and_increments() {
        let first = AttemptNo::FIRST;
        assert_eq!(first.get(), 1);
        assert!(first.is_first());
        assert_eq!(first.next().get(), 2);
        assert_eq!(first.next().next().get(), 3);
        assert_eq!(first.checked_next().map(AttemptNo::get), Some(2));
        assert_eq!(
            AttemptNo::new(u32::MAX).expect("valid").checked_next(),
            None
        );
        assert_eq!(
            AttemptNo::new(u32::MAX).expect("valid").next().get(),
            u32::MAX
        );

        assert_eq!(AttemptNo::new(1).expect("valid").get(), 1);
        assert_eq!(AttemptNo::new(7).expect("valid").get(), 7);

        let error = AttemptNo::new(0).expect_err("zero must be rejected");
        assert_eq!(error.code(), "invalid_attempt");
    }

    #[test]
    fn lease_tokens_are_distinct_and_parseable() {
        let first = LeaseToken::generate();
        let second = LeaseToken::generate();
        assert_ne!(first, second);
        assert!(!first.as_str().is_empty());

        let parsed = LeaseToken::from_str(first.as_str()).expect("round trip");
        assert_eq!(parsed, first);

        assert_eq!(
            LeaseToken::parse("").expect_err("empty").code(),
            "invalid_lease_token"
        );
        assert_eq!(
            LeaseToken::parse("   ").expect_err("whitespace").code(),
            "invalid_lease_token"
        );
    }

    #[test]
    fn event_kind_round_trips() {
        assert_eq!(EventKind::NodeSucceeded.as_str(), "node.succeeded");
        assert_eq!(
            EventKind::from_str("node.succeeded").expect("parse"),
            EventKind::NodeSucceeded
        );
        assert_eq!(
            EventKind::from_str("mission.created").expect("parse"),
            EventKind::MissionCreated
        );
        assert!(EventKind::parse("node.unknown").is_err());
    }

    #[test]
    fn event_envelope_round_trips_through_json() {
        let input = EventInput {
            event_id: EventId::new(),
            kind: EventKind::NodeSucceeded,
            mission_id: MissionId::new(),
            run_id: Some(RunId::new()),
            node_id: Some(NodeId::new()),
            session_id: None,
            attempt_no: Some(AttemptNo::FIRST),
            trace_id: Some(TraceId::new()),
            payload: serde_json::json!({ "ok": true }),
        };

        let envelope = EventEnvelope::new(input, 42, "1970-01-01T00:00:00Z".to_owned());
        assert_eq!(envelope.schema_version, EVENT_SCHEMA_VERSION);
        assert_eq!(envelope.seq, 42);
        assert_eq!(envelope.kind, EventKind::NodeSucceeded);

        let json = serde_json::to_string(&envelope).expect("serialize");
        let decoded: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn attempt_no_serializes_as_integer_and_rejects_zero() {
        let attempt = AttemptNo::new(3).expect("valid");
        assert_eq!(serde_json::to_string(&attempt).expect("serialize"), "3");

        let decoded: AttemptNo = serde_json::from_str("3").expect("deserialize");
        assert_eq!(decoded, attempt);
        assert!(serde_json::from_str::<AttemptNo>("0").is_err());
    }

    #[test]
    fn mission_observation_is_derived_from_running_and_approvals() {
        assert_eq!(
            observe_mission(MissionState::Running, false, true),
            MissionObservation::WaitingApproval
        );
        assert_eq!(
            observe_mission(MissionState::Running, true, true),
            MissionObservation::Running
        );
        assert_eq!(
            observe_mission(MissionState::Running, false, false),
            MissionObservation::Running
        );
        assert_eq!(
            observe_mission(MissionState::Planning, false, true),
            MissionObservation::Planning
        );
        assert_eq!(
            observe_mission(MissionState::Done, false, true),
            MissionObservation::Done
        );
        assert_eq!(
            MissionObservation::from_str("waiting_approval").expect("parse"),
            MissionObservation::WaitingApproval
        );
    }

    #[test]
    fn new_event_kinds_round_trip() {
        for (kind, value) in [
            (EventKind::NodeToolStarted, "node.tool_started"),
            (EventKind::NodeBudgetExceeded, "node.budget_exceeded"),
            (EventKind::ModelRetry, "model.retry"),
            (EventKind::ModelFallback, "model.fallback"),
        ] {
            assert_eq!(kind.as_str(), value);
            assert_eq!(EventKind::from_str(value).expect("parse"), kind);
        }
    }

    #[test]
    fn mission_level_event_serializes_attempt_as_null() {
        let input = EventInput {
            event_id: EventId::new(),
            kind: EventKind::MissionCreated,
            mission_id: MissionId::new(),
            run_id: None,
            node_id: None,
            session_id: None,
            attempt_no: None,
            trace_id: None,
            payload: serde_json::json!({}),
        };
        let envelope = EventEnvelope::new(input, 1, "1970-01-01T00:00:00Z".to_owned());
        let json = serde_json::to_value(&envelope).expect("serialize");
        assert!(json.get("attempt_no").expect("present").is_null());
    }
}
