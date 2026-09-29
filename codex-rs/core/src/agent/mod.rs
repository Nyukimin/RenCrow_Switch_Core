// Modified by RenCrow Switch Core, 2026-09-28: register the read-only agent role module.
pub(crate) mod agent_resolver;
pub(crate) mod api;
pub(crate) mod child_config;
pub(crate) mod control;
mod registry;
pub(crate) mod rencrow_read_only;
pub(crate) mod role;
pub(crate) mod status;
pub(crate) mod types;

pub(crate) use codex_protocol::protocol::AgentStatus;
pub(crate) use control::LocalAgentControl;
pub(crate) use registry::exceeds_thread_spawn_depth_limit;
pub(crate) use registry::next_thread_spawn_depth;
pub(crate) use status::agent_status_from_event;
