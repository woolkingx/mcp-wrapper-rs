//! Unified wrapper tools window.
//!
//! This module owns the backend-visible `mcp.wrapper` tool descriptor, action
//! schema, invocation validation, object-method dispatch, and result envelope.
//! Backend lifecycle and MCP cache state stay owned by their manager modules.

pub mod backend;
pub mod schema;
#[cfg(test)]
mod tests;
pub mod wrapper;

pub use schema::InvocationContext;
pub use wrapper::{invoke, is_wrapper_tool_call, merge_initialize_result, merge_tools_list};
