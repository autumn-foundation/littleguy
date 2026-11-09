//! Each Verus proof mirrors a Rust function. This guard fails when a
//! mirrored body drifts from its source, so a proof never covers stale code.
//!
//! A file marks each mirrored body with `// mirror-begin <name>` and
//! `// mirror-end <name>`. A file with one body may leave out the name.

use std::error::Error;
use std::path::{Path, PathBuf};

type TestResult = Result<(), Box<dyn Error>>;

/// Each mirrored body of the file at `path`, in order, with white space
/// removed.
fn mirrored_bodies(path: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let text = std::fs::read_to_string(path)?;
    let mut bodies = Vec::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("// mirror-begin") {
        let after = &rest[start..];
        let end = after
            .find("// mirror-end")
            .ok_or("a mirror-begin has no mirror-end")?;
        let body: String = after[..end]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        bodies.push(body);
        rest = &after[end + "// mirror-end".len()..];
    }
    if bodies.is_empty() {
        return Err(format!("{} has no mirror markers", path.display()).into());
    }
    Ok(bodies)
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Fails unless `source` and `proof` mirror the same bodies.
fn mirrors(source: &str, proof: &str) -> TestResult {
    let source_bodies = mirrored_bodies(&root().join(source))?;
    let proof_bodies = mirrored_bodies(&root().join(proof))?;
    assert_eq!(
        source_bodies, proof_bodies,
        "update {proof} with the new bodies of {source}"
    );
    Ok(())
}

#[test]
fn the_routing_proof_covers_the_shipped_decide() -> TestResult {
    mirrors(
        "crates/littleguy-core/src/route.rs",
        "formal/verus/src/route.rs",
    )
}

#[test]
fn the_pairing_proof_covers_the_shipped_kernel() -> TestResult {
    mirrors(
        "crates/littleguy-core/src/pairing.rs",
        "formal/verus/src/pairing.rs",
    )
}

#[test]
fn the_address_proof_covers_the_shipped_kernel() -> TestResult {
    mirrors(
        "crates/littleguy-core/src/addr.rs",
        "formal/verus/src/addr.rs",
    )
}
