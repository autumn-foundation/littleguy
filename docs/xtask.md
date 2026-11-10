# `cargo xtask`

| Command | What it runs |
|---|---|
| `cargo xtask fmt` | `cargo fmt --all --check` |
| `cargo xtask clippy` | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` |
| `cargo xtask test` | `cargo test --workspace --all-features --locked` |
| `cargo xtask features` | For each crate with features: clippy on every target with no features, with each feature alone and with all features, warnings denied. Then its tests with no features. |
| `cargo xtask feature-docs [--write]` | Renders the tables of [`features.md`](features.md) from the manifests, and fails when the file differs. `--write` updates the file. |
| `cargo xtask ci` | `fmt`, then `feature-docs`, then `clippy`, then `test`. Run it before each push. |
| `cargo xtask coverage [--fail-under N]` | `cargo llvm-cov` with all features over every crate except `xtask`. Fails under N% lines, 85 by default. |
| `cargo xtask verus` | Downloads the pinned Verus release, checks its SHA-256, and verifies `formal/verus`. Set `VERUS` to use a local binary. |
