//! The tools of the assistant: web fetch, web search and workspace files.
//!
//! `[tools]` turns each one on or off. Web search also needs the Brave
//! Search API key in [`SEARCH_KEY_ENV`]. The key never goes in the config
//! file, and the log never shows it.
//!
//! The workspace lives in [`WORKSPACE_DIR`] beside the database. The engine
//! saves each change of a successful tool call there as a snapshot, so the
//! files survive a restart.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use autumn_harvest::payload_codec::PayloadCodecs;
use autumn_harvest_agent::{AgentHarness, AgentProfile, DirWorkspaceBlobs, Tool, WorkspaceStore};
use littleguy_core::ToolsConfig;
use littleguy_tools::{FetchTool, SearchTool, file_tools};

/// The variable that holds the Brave Search API key.
pub const SEARCH_KEY_ENV: &str = "LG_BRAVE_API_KEY";

/// The folder of the workspace snapshots, beside the database.
pub const WORKSPACE_DIR: &str = "workspace";

/// The workspace folder beside the database `db`.
#[must_use]
pub fn workspace_for(db: &Path) -> PathBuf {
    db.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(WORKSPACE_DIR)
}

/// The search key of the environment, when it is set and not empty.
#[must_use]
pub fn search_key_from_env() -> Option<String> {
    std::env::var(SEARCH_KEY_ENV)
        .ok()
        .filter(|key| !key.trim().is_empty())
}

/// The tools that `config` turns on, and the workspace store they need.
#[derive(Debug, Default)]
pub struct Tools {
    tools: Vec<Arc<dyn Tool>>,
    workspace: Option<PathBuf>,
}

impl Tools {
    /// The tools of `config` for the database `db`. Web search needs
    /// `search_key`.
    #[must_use]
    pub fn new(config: &ToolsConfig, db: &Path, search_key: Option<String>) -> Self {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        if config.web_fetch {
            tools.push(Arc::new(FetchTool::new(config.fetch_max_chars)));
        }
        match (config.web_search, search_key) {
            (true, Some(key)) => tools.push(Arc::new(SearchTool::new(key))),
            (true, None) => {
                tracing::info!("web search is off: set {SEARCH_KEY_ENV} to turn it on");
            }
            (false, _) => {}
        }
        if config.files {
            tools.extend(file_tools());
        }
        Self {
            tools,
            workspace: config.files.then(|| workspace_for(db)),
        }
    }

    /// The names of the tools, in order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }

    /// Gives the harness the tools and the workspace store, and the
    /// profile a workspace.
    #[must_use]
    pub fn apply(
        self,
        harness: AgentHarness,
        profile: AgentProfile,
    ) -> (AgentHarness, AgentProfile) {
        let harness = harness.tools(self.tools);
        match self.workspace {
            Some(root) => {
                let blobs = Arc::new(DirWorkspaceBlobs::new(root));
                (
                    harness.workspace(WorkspaceStore::new(blobs, PayloadCodecs::default())),
                    profile.workspace(),
                )
            }
            None => (harness, profile),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_picks_the_tools() {
        let db = Path::new("/var/lg/lg.db");
        let all = Tools::new(&ToolsConfig::default(), db, Some(String::from("k")));
        assert_eq!(
            all.names(),
            [
                "web_fetch",
                "web_search",
                "read_file",
                "write_file",
                "list_files",
                "delete_file"
            ]
        );
        assert_eq!(
            all.workspace.as_deref(),
            Some(Path::new("/var/lg/workspace"))
        );

        // Without a key there is no search.
        let keyless = Tools::new(&ToolsConfig::default(), db, None);
        assert!(!keyless.names().contains(&"web_search"));

        let none = ToolsConfig {
            web_fetch: false,
            web_search: false,
            files: false,
            ..ToolsConfig::default()
        };
        let empty = Tools::new(&none, db, Some(String::from("k")));
        assert_eq!(empty.names().len(), 0);
        assert_eq!(empty.workspace, None);
    }

    #[test]
    fn a_bare_database_name_keeps_the_workspace_beside_it() {
        assert_eq!(workspace_for(Path::new("lg.db")), Path::new("workspace"));
    }
}
