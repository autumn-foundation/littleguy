//! A sandbox backend for the tests of the saved scripts.

use std::sync::{Mutex, PoisonError};

use autumn_harvest::sandbox::{
    IsolationLevel, RunStatus, SandboxBackend, SandboxError, SandboxJob, SandboxOutcome,
};
use tokio_util::sync::CancellationToken;

/// A backend that runs nothing. It keeps each job and prints the argv
/// after the code file, one entry a line, then the stdin.
#[derive(Debug, Default)]
pub struct EchoBackend {
    /// Each job that it ran.
    pub jobs: Mutex<Vec<SandboxJob>>,
}

impl SandboxBackend for EchoBackend {
    fn isolation(&self) -> IsolationLevel {
        IsolationLevel::Process
    }

    fn supports_network(&self) -> bool {
        true
    }

    fn execute(
        &self,
        job: SandboxJob,
        _cancel: CancellationToken,
    ) -> autumn_harvest::sandbox::runtime::BoxFuture<'_, Result<SandboxOutcome, SandboxError>> {
        let mut stdout = job.plan.argv[2..]
            .iter()
            .fold(String::new(), |mut out, arg| {
                out.push_str(arg);
                out.push('\n');
                out
            });
        stdout.push_str(&String::from_utf8_lossy(&job.plan.stdin));
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(job);
        Box::pin(std::future::ready(Ok(SandboxOutcome {
            status: RunStatus::Exited { code: 0 },
            stdout_bytes: stdout.len() as u64,
            stdout,
            stderr: String::new(),
            stderr_bytes: 0,
            stdout_truncated: false,
            stderr_truncated: false,
            outputs: Vec::new(),
            skipped_outputs: Vec::new(),
            duration_ms: 3,
            isolation: IsolationLevel::Process,
            image: None,
        })))
    }
}

impl EchoBackend {
    /// How many jobs it ran.
    pub fn runs(&self) -> usize {
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}
