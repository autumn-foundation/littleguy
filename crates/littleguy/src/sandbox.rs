//! The `run_code` tool, and the gate that holds each call for a person.
//!
//! `[sandbox]` turns the tool on. The engine runs the code on a
//! `SandboxRuntime`: a new Firecracker microVM for each call, or a host
//! process for development. The engine records the outcome, so a replay
//! never runs the code again.
//!
//! Each `run_code` call waits for a person. The approval gate of
//! [`crate::guard`] asks the engine for that wait.
//!
//! Feature `sandbox` builds the tool. Without it, [`Runtime`] has no value,
//! so the compiler drops each path that runs code, and a `[sandbox]`
//! section fails at start.
//!
//! The engine builds both backends only on Linux. On another system, a
//! `[sandbox]` section fails at start and names the backend.

#[cfg(feature = "sandbox")]
use std::sync::Arc;

#[cfg(all(feature = "sandbox", target_os = "linux"))]
use anyhow::Context;
use anyhow::Result;
#[cfg(feature = "sandbox")]
use autumn_harvest::sandbox::{
    NetworkPolicy, SandboxBackend as Backend, SandboxPolicy, SandboxRuntime,
};
use autumn_harvest_agent::AgentHarness;
#[cfg(feature = "sandbox")]
use autumn_harvest_agent::sandbox::SandboxTool;
use littleguy_core::SandboxConfig;
#[cfg(feature = "sandbox")]
use littleguy_core::{FirecrackerConfig, SandboxBackend};

/// The name of the tool. The engine names it the same way, and a test
/// checks that.
pub const TOOL: &str = "run_code";

/// The reason that a `run_code` call waits, for the approver.
pub const ASK_REASON: &str = "the assistant wants to run code";

/// The runtime of the `run_code` tool.
#[cfg(feature = "sandbox")]
pub type Runtime = Arc<SandboxRuntime>;

/// The runtime of the `run_code` tool. This build has no sandbox, so the
/// type has no value.
#[cfg(not(feature = "sandbox"))]
pub type Runtime = std::convert::Infallible;

/// A second handle on `runtime`, if any.
#[cfg(feature = "sandbox")]
#[must_use]
pub fn share(runtime: Option<&Runtime>) -> Option<Runtime> {
    runtime.cloned()
}

/// A second handle on `runtime`. This build has no sandbox, so there is
/// none.
#[cfg(not(feature = "sandbox"))]
#[must_use]
pub const fn share(_runtime: Option<&Runtime>) -> Option<Runtime> {
    None
}

/// Gives `harness` the `run_code` tool on `runtime`, if any.
#[cfg(feature = "sandbox")]
#[must_use]
pub fn give(harness: AgentHarness, runtime: Option<Runtime>) -> AgentHarness {
    match runtime {
        Some(runtime) => harness.tool(SandboxTool::new(runtime).shared()),
        None => harness,
    }
}

/// Gives `harness` the `run_code` tool on `runtime`. This build has no
/// runtime, so `harness` stays as it is.
#[cfg(not(feature = "sandbox"))]
#[must_use]
pub const fn give(harness: AgentHarness, runtime: Option<Runtime>) -> AgentHarness {
    match runtime {
        Some(runtime) => match runtime {},
        None => harness,
    }
}

/// Refuses `[sandbox]`, because this build has no sandbox.
///
/// # Errors
///
/// Always.
#[cfg(not(feature = "sandbox"))]
pub fn runtime(_config: &SandboxConfig) -> Result<Runtime> {
    anyhow::bail!(crate::features::missing("[sandbox]", "sandbox"))
}

#[cfg(feature = "sandbox")]
/// Builds the runtime of `config`.
///
/// # Errors
///
/// Returns an error when the backend does not start, for example a missing
/// Firecracker binary or image, or when the backend cannot meet the policy.
pub fn runtime(config: &SandboxConfig) -> Result<Arc<SandboxRuntime>> {
    let backend: Arc<dyn Backend> = match &config.backend {
        SandboxBackend::Process => process()?,
        SandboxBackend::Firecracker(fc) => firecracker(fc)?,
    };
    runtime_on(backend, config)
}

#[cfg(feature = "sandbox")]
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

#[cfg(all(feature = "sandbox", target_os = "linux"))]
/// A backend that runs code as a host process, for development.
#[expect(
    clippy::unnecessary_wraps,
    reason = "off Linux the same function refuses, so both share one signature"
)]
fn process() -> Result<Arc<dyn Backend>> {
    tracing::warn!("the sandbox runs code as a host process; this is for development only");
    Ok(Arc::new(
        autumn_harvest::sandbox::process::ProcessBackend::default(),
    ))
}

#[cfg(all(feature = "sandbox", not(target_os = "linux")))]
/// Refuses the process backend, because the engine builds it only on Linux.
fn process() -> Result<Arc<dyn Backend>> {
    anyhow::bail!(only_on_linux("process"))
}

#[cfg(all(feature = "sandbox", not(target_os = "linux")))]
/// Refuses the Firecracker backend, because the engine builds it only on
/// Linux.
fn firecracker(_config: &FirecrackerConfig) -> Result<Arc<dyn Backend>> {
    anyhow::bail!(only_on_linux("firecracker"))
}

#[cfg(all(feature = "sandbox", not(target_os = "linux")))]
/// The error for a `backend` that this system does not have.
fn only_on_linux(backend: &str) -> String {
    format!(
        "the {backend} sandbox backend runs only on Linux, and this lg runs on {}",
        std::env::consts::OS
    )
}

#[cfg(all(feature = "sandbox", target_os = "linux"))]
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

#[cfg(all(test, feature = "sandbox"))]
mod tests {
    #[test]
    fn the_tool_name_is_the_engine_name() {
        assert_eq!(
            super::TOOL,
            autumn_harvest_agent::sandbox::DEFAULT_TOOL_NAME
        );
    }
}
