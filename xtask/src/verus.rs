//! `cargo xtask verus`: verify every Verus proof in `formal/verus`.
//!
//! Verus checks the crate root `formal/verus/src/lib.rs`. It must report at
//! least one verified item and 0 errors.
//!
//! `VERUS` names a local verus binary. Without it, the command downloads the
//! pinned release and checks its SHA-256. The pin matches autumn-harvest, so
//! both repositories prove with the same Verus. The release names the Rust
//! toolchain that it links against, and rustup installs that toolchain.

use std::path::{Path, PathBuf};

use regex::Regex;
use smop::prelude::*;

/// The pinned Verus release. Pin a weekly release, because Verus keeps the
/// assets of only the newest rolling release.
const VERUS_RELEASE: &str = "0.2026.10.04.426d8b0";

/// The SHA-256 of the zip of [`VERUS_RELEASE`].
const VERUS_SHA256: &str = "624018662868563a7bec0dd07be5e4dd0c3d61124b4b55105765884bf2d4d1a0";

/// The crate root that Verus checks.
const CRATE_ROOT: &str = "formal/verus/src/lib.rs";

/// The result line of a run that verifies at least one item with no error.
const RESULT_LINE: &str = r"(?m)^verification results:: [1-9][0-9]* verified, 0 errors$";

/// Returns the verus binary: `VERUS`, or the pinned release in `work`.
fn verus_binary(work: &Path) -> Result<PathBuf> {
    if let Some(verus) = std::env::var_os("VERUS").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(verus));
    }
    let zip = work.join("verus.zip");
    sh::cmd("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&zip)
        .arg(format!(
            "https://github.com/verus-lang/verus/releases/download/release%2F{VERUS_RELEASE}/verus-{VERUS_RELEASE}-x86-linux.zip"
        ))
        .echo()
        .run()?;
    let list = work.join("verus.zip.sha256");
    fs::write_string(&list, format!("{VERUS_SHA256}  {}\n", zip.display()))?;
    sh::cmd("sha256sum")
        .args(["--check", "--quiet"])
        .arg(&list)
        .run()
        .context("the Verus download does not match its pinned SHA-256")?;
    sh::cmd("unzip")
        .arg("-q")
        .arg(&zip)
        .arg("-d")
        .arg(work)
        .run()?;
    Ok(work.join("verus-x86-linux/verus"))
}

/// The toolchain that `version.json` names, as `<name> (<note>)`.
fn toolchain(version_json: &str) -> Result<String> {
    let doc: serde_json::Value = serde_json::from_str(version_json)?;
    let text = doc["verus"]["toolchain"]
        .as_str()
        .context("version.json has no verus.toolchain string")?;
    text.split_whitespace()
        .next()
        .map(str::to_string)
        .context("verus.toolchain is empty")
}

/// True when the log reports a run with no error.
fn verified(log: &str) -> Result<bool> {
    Ok(Regex::new(RESULT_LINE)?.is_match(log))
}

/// Downloads Verus when needed, runs it, and checks its result line.
pub fn check(root: &Path) -> Result<()> {
    let work = fs::temp_dir()?;
    let verus = verus_binary(work.path())?;
    let dir = verus
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let toolchain = toolchain(&fs::read_string(dir.join("version.json"))?)?;
    sh::cmd("rustup")
        .args(["toolchain", "install", &toolchain, "--profile", "minimal"])
        .echo()
        .run()?;
    // `cargo xtask` runs under rustup, which sets RUSTUP_TOOLCHAIN. Verus
    // must choose its own toolchain.
    let output = std::process::Command::new(&verus)
        .arg(root.join(CRATE_ROOT))
        .arg("--crate-type=lib")
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .with_context(|| format!("running {}", verus.display()))?;
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{log}");
    ensure!(output.status.success(), "verus exited {}", output.status);
    ensure!(
        verified(&log)?,
        "want: verification results:: <n> verified, 0 errors"
    );
    println!("all Verus proofs verified");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_toolchain_is_the_first_word() {
        let json = r#"{"verus": {"toolchain": "1.88.0-x86_64-unknown-linux-gnu (note)"}}"#;
        assert_eq!(toolchain(json).unwrap(), "1.88.0-x86_64-unknown-linux-gnu");
        assert!(toolchain(r#"{"verus": {}}"#).is_err());
        assert!(toolchain("not json").is_err());
    }

    #[test]
    fn the_result_line_needs_a_verified_item_and_no_error() {
        assert!(verified("x\nverification results:: 12 verified, 0 errors\n").unwrap());
        assert!(!verified("verification results:: 0 verified, 0 errors\n").unwrap());
        assert!(!verified("verification results:: 3 verified, 1 errors\n").unwrap());
    }

    #[test]
    fn the_crate_root_exists() {
        assert!(super::super::repo_root().join(CRATE_ROOT).is_file());
    }
}
