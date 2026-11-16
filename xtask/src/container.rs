//! `cargo xtask container`: checks that the container of `compose.yaml` can
//! run code.
//!
//! The `process` sandbox backend runs each call in a new user namespace,
//! and in a network namespace when the code gets no network. Docker blocks
//! that step under its default seccomp profile, so `compose.yaml` sets
//! `security_opt`. This command builds the image and runs the same step
//! with the settings of the `lg` service: the user, the `security_opt` and
//! the runtimes that `run_code` uses.
//!
//! It runs as its own compose project, so it never touches a running
//! littleguy, and it removes its container and volume at the end.

use std::path::Path;

use smop::prelude::*;

/// The compose project of the check. A running littleguy uses another.
pub const PROJECT: &str = "littleguy-check";

/// The probes, each in the namespaces of one `run_code` call: no network,
/// then the runtimes of the image.
const PROBES: [&[&str]; 2] = [
    &["unshare", "-Urn", "true"],
    &[
        "unshare",
        "-Urn",
        "sh",
        "-c",
        "python3 -c pass && node -e 0 && bash -c true",
    ],
];

/// The arguments of `docker compose` for the check project.
fn compose(rest: &[&str]) -> Vec<String> {
    ["compose", "-p", PROJECT]
        .iter()
        .chain(rest)
        .map(ToString::to_string)
        .collect()
}

/// The arguments that run `probe` in a one-off `lg` container.
fn probe_args(probe: &[&str]) -> Vec<String> {
    let mut rest = vec!["run", "--rm", "--no-deps", "--entrypoint", probe[0], "lg"];
    rest.extend(&probe[1..]);
    compose(&rest)
}

fn docker(root: &Path, args: Vec<String>) -> Result<()> {
    sh::cmd("docker").args(args).dir(root).echo().run()
}

/// Builds the image and runs each probe, then removes what the check made.
///
/// # Errors
///
/// Returns an error when docker is missing, the image does not build, or a
/// probe fails, which means `run_code` would fail in the container.
pub fn check(root: &Path) -> Result<()> {
    path::which("docker").context("the container check needs docker")?;
    docker(root, compose(&["build", "lg"]))?;
    let probed = PROBES
        .iter()
        .try_for_each(|probe| docker(root, probe_args(probe)));
    let cleaned = docker(root, compose(&["down", "--volumes", "--remove-orphans"]));
    probed.context(
        "the container cannot make the namespaces of run_code: check security_opt in compose.yaml",
    )?;
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_probe_runs_in_the_check_project_as_the_lg_service() {
        for probe in PROBES {
            let args = probe_args(probe);
            assert_eq!(&args[..3], ["compose", "-p", PROJECT]);
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--entrypoint", "unshare"])
            );
            assert!(
                args.iter().any(|arg| arg == "-Urn"),
                "no network, as by default"
            );
        }
    }
}
