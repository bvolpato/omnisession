# Contributing

Requirements: Rust 1.85 or newer.

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Provider fixtures must be synthetic or scrubbed. Never commit real transcripts, credentials, machine paths, or proprietary source code.

Changes to bundle schema or adapter contract require an RFC under `docs/rfcs/`.
