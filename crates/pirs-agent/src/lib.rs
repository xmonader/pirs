pub mod agent;
pub mod agent_loop;
pub mod audit;
pub mod compaction;
pub mod control_pins;
pub mod delegate;
pub mod events;
pub mod gate;
pub mod jobs;
pub mod memory;
pub mod phase_agent;
pub mod profile;
pub mod steering;
pub mod strategy;
pub mod thrash;
pub mod tool;
pub mod trace;
pub mod usage;
pub mod use_tool;
pub mod validate;

pub use agent::{Agent, AgentError, QueueMode};
pub use audit::{
    audit_enabled, audit_listener, default_audit_path, wrap_emit, AuditLog,
};
pub use control_pins::{
    enforce_tool_result_adjacency, is_reminder_kind, preserve_control_pins, reminder_kind,
    strip_reminder_kind, wrap_reminder, PROTECTED_KINDS,
};
pub use events::{AgentEvent, Emit, Hooks, ToolResultPatch};
pub use gate::GreenEvidence;
pub use strategy::pin_plan_model;
pub use thrash::{
    LoopDetectionTracker, MistakeTracker, ThrashGuard, ToolSignature,
};
pub use tool::{tool_defs, AgentTool, ExecutionMode, ToolExecContext, ToolOutput};
