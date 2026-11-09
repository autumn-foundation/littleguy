//! The `run_code` tool, and the gate that holds each call for a person.
//!
//! `[sandbox]` turns the tool on. The engine runs the code on a
//! [`SandboxRuntime`]: a new Firecracker microVM for each call, or a host
//! process for development. The engine records the outcome, so a replay
//! never runs the code again.
//!
//! Each `run_code` call waits for a person. [`ApprovalGate`] asks the engine
//! for that wait. It also records the call with its arguments in
//! `memory.db`, because the engine names the wait but does not show the
//! call. The daemon reads the record and shows it to the approver
//! (`crate::approvals`).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use autumn_harvest::sandbox::{
    NetworkPolicy, SandboxBackend as Backend, SandboxPolicy, SandboxRuntime,
};
use autumn_harvest_agent::sandbox::{DEFAULT_TOOL_NAME, SandboxTool};
use autumn_harvest_agent::{
    AgentHarness, AgentProfile, BoxFuture, Rule, RunInfo, Tool, ToolCall, ToolDecision, ToolPolicy,
    ToolRules,
};
use littleguy_core::{FirecrackerConfig, SandboxBackend, SandboxConfig};
use littleguy_stores::SqliteStores;

/// The name of the tool.
pub const TOOL: &str = DEFAULT_TOOL_NAME;

/// The reason that a `run_code` call waits, for the approver.
pub const ASK_REASON: &str = "the assistant wants to run code";

/// Builds the runtime of `config`.
///
/// # Errors
///
/// Returns an error when the backend does not start, for example a missing
/// Firecracker binary or image, or when the backend cannot meet the policy.
pub fn runtime(config: &SandboxConfig) -> Result<Arc<SandboxRuntime>> {
    let backend: Arc<dyn Backend> = match &config.backend {
        SandboxBackend::Process => {
            tracing::warn!("the sandbox runs code as a host process; this is for development only");
            Arc::new(autumn_harvest::sandbox::process::ProcessBackend::default())
        }
        SandboxBackend::Firecracker(fc) => firecracker(fc)?,
    };
    runtime_on(backend, config)
}

/// The runtime of `config` on `backend`. The policy accepts process
/// isolation only for the process backend.
///
/// # Errors
///
/// Returns an error when the backend cannot meet the policy.
pub fn runtime_on(
    backend: Arc<dyn Backend>,
    config: &SandboxConfig,
) -> Result<Arc<SandboxRuntime>> {
    let policy = match config.backend {
        SandboxBackend::Process => SandboxPolicy::for_development(),
        SandboxBackend::Firecracker(_) => SandboxPolicy::default(),
    };
    let network = if config.network {
        NetworkPolicy::Allow
    } else {
        NetworkPolicy::Deny
    };
    let policy = SandboxPolicy { network, ..policy };
    let runtime = SandboxRuntime::new(backend, policy)
        .map_err(|error| anyhow::anyhow!("the sandbox does not start: {}", error.message()))?;
    Ok(Arc::new(runtime))
}

/// A Firecracker backend behind the jailer.
fn firecracker(config: &FirecrackerConfig) -> Result<Arc<dyn Backend>> {
    use autumn_harvest::sandbox::firecracker::{
        BackendSettings, BootMethod, Confinement, FirecrackerBackend, FirecrackerLauncher,
        GuestImage, JailerConfig, LauncherConfig, MachineSpec,
    };
    let mut jailer = JailerConfig::new(&config.jailer, config.uid, config.gid);
    jailer.netns.clone_from(&config.netns);
    let launcher = FirecrackerLauncher::new(LauncherConfig {
        firecracker_bin: config.firecracker.clone(),
        image: GuestImage::new(&config.kernel, &config.rootfs),
        machine: MachineSpec::default(),
        boot: BootMethod::ConfigFile,
        confinement: Confinement::Jailer(jailer),
        guest_port: autumn_harvest::sandbox::firecracker::config::GUEST_AGENT_PORT,
        api_timeout: autumn_harvest::sandbox::firecracker::api::DEFAULT_API_TIMEOUT,
    })
    .map_err(|error| anyhow::anyhow!("firecracker does not start: {}", error.message()))?;
    let backend = FirecrackerBackend::new(Arc::new(launcher), BackendSettings::default())
        .map_err(|error| anyhow::anyhow!("firecracker does not start: {}", error.message()))
        .context("building the sandbox backend")?;
    Ok(Arc::new(backend))
}

/// The tool, its gate and its wait.
#[derive(Debug, Clone)]
pub struct Sandbox {
    /// The runtime that runs the code.
    pub runtime: Arc<SandboxRuntime>,
    /// The store that keeps each call that waits.
    pub stores: Arc<SqliteStores>,
    /// How long a call waits for a decision.
    pub timeout: Duration,
}

impl Sandbox {
    /// Gives the harness the tool and the gate, and the profile the wait.
    #[must_use]
    pub fn apply(
        self,
        harness: AgentHarness,
        profile: AgentProfile,
    ) -> (AgentHarness, AgentProfile) {
        let tool: Arc<dyn Tool> = SandboxTool::new(self.runtime).shared();
        let gate = ApprovalGate::new(self.stores);
        (
            harness.tool(tool).policy(Arc::new(gate)),
            profile.approval_timeout(self.timeout),
        )
    }
}

/// Asks a person before each `run_code` call, and records the call.
///
/// Every other tool runs without a question, as before. A call that cannot
/// be recorded is denied, so no call waits that the approver cannot read.
#[derive(Debug)]
pub struct ApprovalGate {
    stores: Arc<SqliteStores>,
    rules: ToolRules,
}

impl ApprovalGate {
    /// A gate that records in `stores`.
    #[must_use]
    pub fn new(stores: Arc<SqliteStores>) -> Self {
        Self {
            stores,
            rules: ToolRules::new().tool(TOOL, Rule::Ask),
        }
    }

    /// The decision on `call`. The policy runs inside the model-turn
    /// activity, and replay reads its answer. So a call is recorded once
    /// for each attempt of that activity.
    #[must_use]
    pub fn decide_call(&self, call: &ToolCall, tool: Option<&dyn Tool>) -> ToolDecision {
        match self.rules.decide_now(call, tool) {
            ToolDecision::RequireApproval { .. } => {
                match self
                    .stores
                    .record_approval(&call.id, &call.name, &call.arguments, ASK_REASON)
                {
                    Ok(id) => {
                        tracing::info!(approval = id, tool = %call.name, "a call waits for approval");
                        ToolDecision::RequireApproval {
                            reason: String::from(ASK_REASON),
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "a call that needs approval was not recorded");
                        ToolDecision::Deny {
                            reason: String::from(
                                "the call could not be recorded for approval, so it did not run",
                            ),
                        }
                    }
                }
            }
            other => other,
        }
    }
}

impl ToolPolicy for ApprovalGate {
    fn decide<'a>(
        &'a self,
        call: &'a ToolCall,
        tool: Option<&'a dyn Tool>,
        _info: &'a RunInfo,
    ) -> BoxFuture<'a, ToolDecision> {
        Box::pin(std::future::ready(self.decide_call(call, tool)))
    }
}
