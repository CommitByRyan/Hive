//! Evaluation, fault-injection and benchmark scaffolding for Hive.
//!
//! This crate adds deterministic fakes and a fixed scenario set on top of the
//! production runtime, agent, gateway and tool dispatcher. It never changes
//! production semantics: everything here is an external driver.

pub mod fakes;
pub mod harness;
pub mod scenario;

pub use fakes::{
    ScriptedResponse, ScriptedToolExecutor, ScriptedToolStep, ScriptedTransport, SucceedingExecutor,
};
pub use harness::{RunRecord, TempDir};
pub use scenario::{run_once, Scenario};
