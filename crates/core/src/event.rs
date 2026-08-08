//! Unified event model and event source abstraction.
//!
//! Both eBPF and /proc polling produce the same event stream; the Orchestrator
//! only sees the `EventSource` trait — mode selection, fallback and Hybrid are
//! all policy decisions.

use std::sync::Arc;
use std::time::Instant;

use crate::config::ConfigSnapshot;

/// Event kinds. `CpuMigrate` comes from kernel-side sched_switch sampling (P5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// Process fork (ProcSource emits Fork with tid == pid; thread creation is
    /// expressed separately as ThreadClone)
    Fork,
    /// Process exec (image replacement; thread-name/rule state must reset)
    Exec,
    /// Thread clone (a new thread within the same process)
    ThreadClone,
    /// Thread migrated between CPUs (big↔little drift detection)
    CpuMigrate,
    /// Process/thread exit
    Exit,
}

/// Unified process event.
#[derive(Debug, Clone)]
pub struct ProcessEvent {
    pub pid: i32,
    pub tid: i32,
    pub kind: EventKind,
    /// Target CPU for CpuMigrate events.
    pub cpu: Option<u32>,
    /// Package name if already read by the event source (saves repeated /proc
    /// reads; may be None from the eBPF source).
    pub pkg: Option<String>,
}

impl ProcessEvent {
    pub fn fork(pid: i32, tid: i32) -> Self {
        Self { pid, tid, kind: EventKind::Fork, cpu: None, pkg: None }
    }
    pub fn exec(pid: i32, tid: i32) -> Self {
        Self { pid, tid, kind: EventKind::Exec, cpu: None, pkg: None }
    }
    pub fn thread_clone(pid: i32, tid: i32) -> Self {
        Self { pid, tid, kind: EventKind::ThreadClone, cpu: None, pkg: None }
    }
    pub fn cpu_migrate(pid: i32, tid: i32, cpu: u32) -> Self {
        Self { pid, tid, kind: EventKind::CpuMigrate, cpu: Some(cpu), pkg: None }
    }
    pub fn exit(pid: i32, tid: i32) -> Self {
        Self { pid, tid, kind: EventKind::Exit, cpu: None, pkg: None }
    }
    /// Attach the package name already read by the event source.
    pub fn with_pkg(mut self, pkg: String) -> Self {
        self.pkg = Some(pkg);
        self
    }
}

/// Event source abstraction. All implementations must:
/// - `poll` block until deadline or return a batch of events (empty vec = timeout)
/// - `on_config_changed` rebuild whitelist / reset incremental scan state on config change
/// - `shutdown` trigger cleanup and return promptly
pub trait EventSource: Send {
    fn poll(&mut self, deadline: Instant) -> Vec<ProcessEvent>;

    fn on_config_changed(&mut self, cfg: &Arc<ConfigSnapshot>);

    fn shutdown(&mut self);
}
