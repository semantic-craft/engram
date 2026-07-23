//! MCP server for engram.
//!
//! Hosts an [`EngramServer`] that wraps the storage layer and exposes a
//! deliberately narrow tool surface to coding agents. Read-only tools live
//! here; write tools (M3+) and consolidation tools (M7+) follow.
//!
//! Pin the MCP protocol version explicitly so we never fall into the
//! agentmemory #510 / #553 "negotiated-down to a version the client
//! discards tools for" failure mode.

pub mod actor;
pub mod admin;
mod server;

pub use actor::actor_from_headers;
pub use admin::{AdminState, admin_router};
pub use server::{EngramServer, MEMORY_INSTRUCTIONS};
