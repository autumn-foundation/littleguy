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
//! It also checks that code in the sandbox cannot reach the daemon (ADR
//! 0032): a daemon in the container answers `lg` but refuses the same call
//! from inside the namespaces of a run, and the config is read-only to the
//! `lg` user while the skills and the scripts stay writable.
//!
//! Last it makes an old volume: it removes the scripts folder as root, as a
//! volume from before saved scripts lacks it. The `volume` service of
//! `compose.yaml` must make the folder again and give it to `lg`.
//!
//! It runs as its own compose project, so it never touches a running
//! littleguy, and it removes its container and volume at the end.

use std::path::Path;

use smop::prelude::*;

/// The compose project of the check. A running littleguy uses another.
pub const PROJECT: &str = "littleguy-check";

/// A daemon on an empty volume, then `lg` outside and inside the namespaces
/// of a run. The refusal names the sandbox.
const DAEMON_PROBE: &str = "lg serve >/tmp/lg.log 2>&1 & \
     for i in $(seq 100); do [ -S /data/share/littleguy/lg.sock ] && break; sleep 0.1; done; \
     lg pairs >/dev/null \
     && unshare -Urn lg pairs 2>&1 | grep -q 'no request from code in a sandbox'";

/// The config folder is not writable by `lg`, and the skills and scripts
/// folders are.
const FILES_PROBE: &str = "! test -w /data/config/littleguy \
     && test -w /data/config/littleguy/skills \
     && test -w /data/config/littleguy/scripts";

/// The probes. The first two run in the namespaces of one `run_code` call:
/// no network, then the runtimes of the image. The last two check what code
/// in a run can reach.
const PROBES: [&[&str]; 4] = [
    &["unshare", "-Urn", "true"],
    &[
        "unshare",
        "-Urn",
        "sh",
        "-c",
        "python3 -c pass && node -e 0 && bash -c true",
    ],
    &["sh", "-c", DAEMON_PROBE],
    &["sh", "-c", FILES_PROBE],
];

/// An old volume, then the repair of `compose.yaml`: root removes the
/// scripts folder, and the `volume` service makes it again for `lg`.
const OLD_VOLUME: [&[&str]; 2] = [
    &[
        "run",
        "--rm",
        "--no-deps",
        "--user",
        "root",
        "--entrypoint",
        "rm",
        "lg",
        "-rf",
        "/data/config/littleguy/scripts",
    ],
    &["run", "--rm", "volume"],
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
        .try_for_each(|probe| docker(root, probe_args(probe)))
        .and_then(|()| {
            OLD_VOLUME
                .iter()
                .try_for_each(|step| docker(root, compose(step)))
        })
        .and_then(|()| docker(root, probe_args(&["sh", "-c", FILES_PROBE])));
    let cleaned = docker(root, compose(&["down", "--volumes", "--remove-orphans"]));
    probed.context(
        "the container cannot run code or keep its folders: check security_opt and the volume service in compose.yaml",
    )?;
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_probe_runs_in_the_check_project_as_the_lg_service() {
        for probe in &PROBES[..2] {
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

    #[test]
    fn the_old_volume_loses_its_scripts_as_root_and_the_volume_service_repairs_it() {
        let remove = compose(OLD_VOLUME[0]);
        assert!(remove.windows(2).any(|pair| pair == ["--user", "root"]));
        assert_eq!(
            remove.last().map(String::as_str),
            Some("/data/config/littleguy/scripts")
        );
        assert_eq!(
            compose(OLD_VOLUME[1]).last().map(String::as_str),
            Some("volume")
        );
    }

    #[test]
    fn compose_runs_the_volume_service_as_root_before_lg() {
        let compose = include_str!("../../compose.yaml");
        let volume = compose
            .split("\n  volume:\n")
            .nth(1)
            .and_then(|rest| rest.split("\n  lg:\n").next())
            .unwrap();
        assert!(volume.contains("user: root"), "{volume}");
        assert!(volume.contains(
            "chown -R lg:lg /data/config/littleguy/skills /data/config/littleguy/scripts"
        ));
        assert!(!volume.contains("lg.toml"), "the config stays root's");
        assert!(compose.contains("condition: service_completed_successfully"));
    }
}
