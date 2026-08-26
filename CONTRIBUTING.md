# Contributing

Requirements: Rust 1.85 or newer. Website work requires Node.js 22 and pnpm 11.1.

```sh
cargo fmt --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo run --quiet --locked --package omnisession-cli -- --version
cargo run --quiet --locked --package omnisession-cli -- --help >/dev/null
```

Website dependencies use a frozen pnpm lockfile. `website/pnpm-workspace.yaml` rejects packages published less than three days ago, including transitive packages.

```sh
pnpm --dir website install --frozen-lockfile
pnpm --dir website typecheck
NEXT_PUBLIC_BASE_PATH=/omnisession pnpm --dir website build
pnpm --dir website exec playwright install chromium
pnpm --dir website test:smoke
```

Release-facing changes should also run installer and token-free provider conformance checks:

```sh
sh scripts/test-install.sh
scripts/test-provider-conformance.sh
```

Provider fixtures must be synthetic or scrubbed. Never commit real transcripts, credentials, machine paths, or proprietary source code.

Changes to bundle schema or adapter contract require an RFC under `docs/rfcs/`.
