# `cargo xtask`

| Command | What it runs |
|---|---|
| `cargo xtask fmt` | `cargo fmt --all --check` |
| `cargo xtask clippy` | `cargo clippy --workspace --all-targets --locked -- -D warnings` |
| `cargo xtask test` | `cargo test --workspace --locked` |
| `cargo xtask ci` | `fmt`, then `clippy`, then `test`. Run it before each push. |
| `cargo xtask coverage [--fail-under N]` | `cargo llvm-cov` over every crate except `xtask`. Fails under N% lines, 85 by default. |
| `cargo xtask verus` | Downloads the pinned Verus release, checks its SHA-256, and verifies `formal/verus`. Set `VERUS` to use a local binary. |
