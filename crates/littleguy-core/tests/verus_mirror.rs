//! Each Verus proof mirrors a Rust function. This guard fails when a
//! mirrored body drifts from its source, so a proof never covers stale code.

use std::error::Error;
use std::path::Path;

/// The text between the mirror markers, with white space removed.
fn mirrored_body(path: &Path) -> Result<String, Box<dyn Error>> {
    let text = std::fs::read_to_string(path)?;
    let start = text
        .find("// mirror-begin")
        .ok_or("no mirror-begin marker")?;
    let end = text.find("// mirror-end").ok_or("no mirror-end marker")?;
    Ok(text[start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect())
}

#[test]
fn the_routing_proof_covers_the_shipped_decide() -> Result<(), Box<dyn Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = mirrored_body(&root.join("crates/littleguy-core/src/route.rs"))?;
    let proof = mirrored_body(&root.join("formal/verus/src/route.rs"))?;
    assert_eq!(
        source, proof,
        "update formal/verus/src/route.rs with the new decide"
    );
    Ok(())
}
