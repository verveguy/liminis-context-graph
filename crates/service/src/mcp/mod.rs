//! Native MCP-over-stdio transport (issue #195). New surface only — `lcg_core::handlers`
//! dispatch is untouched; every tool call is translated into an `IpcRequest` and routed
//! through the existing core dispatch, in-process (standalone) or over a Unix socket
//! (attached).

pub mod attached;
pub mod backend;
pub mod scope;
pub mod server;
pub mod tools;
