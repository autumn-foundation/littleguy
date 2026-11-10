//! Tests of the tools against local servers. They need no network.

use autumn_harvest_agent::{RunId, ToolContext, WorkspaceHandle};

mod files;
#[cfg(feature = "web")]
mod web;

fn ctx(workspace: Option<WorkspaceHandle>) -> ToolContext {
    ToolContext {
        run_id: RunId::new("run-1"),
        call_id: String::from("call-1"),
        session_id: None,
        step: 0,
        workspace,
    }
}
