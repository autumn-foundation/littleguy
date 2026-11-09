//! The `run_code` tool, and the gate that holds each call for a person.
//!
//! `[sandbox]` turns the tool on. The engine runs the code on a
//! [`SandboxRuntime`]: a new Firecracker microVM for each call, or a host
//! process for development. The engine records the outcome, so a replay
//! never runs the code again.
//!
//! Each `run_code` call waits for a person. The approval gate of
//! [`crate::guard`] asks the engine for that wait.

use std::sync::Arc;

use anyhow::{Context, Result};
use autumn_harvest::sandbox::{
    NetworkPolicy, SandboxBackend as Backend, SandboxPolicy, SandboxRuntime,
};
use autumn_harvest_agent::sandbox::DEFAULT_TOOL_NAME;
use littleguy_core::{FirecrackerConfig, SandboxBackend, SandboxConfig};

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
