//! threadctl-core — domain logic layer.
//!
//! Design principles:
//! - No aya/eBPF dependency: pure Linux syscalls + pure logic, unit-testable
//! - Event source abstraction (`event::EventSource`) decoupled from policy execution (`policy`)
//! - Configuration flows between modules as immutable snapshots (`config::ConfigSnapshot`)

#[cfg(not(any(target_os = "linux", target_os = "android")))]
compile_error!("threadctl-core only supports Linux/Android targets");

#[cfg(target_pointer_width = "32")]
compile_error!("threadctl requires a 64-bit target");

pub mod audit;
pub mod backend;
pub mod capability;
pub mod caps;
pub mod config;
pub mod decision;
pub mod debug;
pub mod engine;
pub mod event;
pub mod foreground;
pub mod i18n;
pub mod kdl_parser;
pub mod merge;
pub mod observe;
pub mod policy;
pub mod proc;
pub mod profile;
pub mod relock;
pub mod ruleset;
pub mod scene;
pub mod store;
pub mod system_context;
pub mod topology;
pub mod tracker;
pub mod tune;

pub use event::{EventKind, ProcessEvent};
pub use topology::{CpuCluster, CpuClusterKind, CpuSet, CpuTopology};
