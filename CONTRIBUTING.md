# Contributing to OmniSession

Thanks for helping. OmniSession works with private coding-agent history, so correctness and safety come first. Small, verified changes beat broad ones.

- **Bugs and compatibility breaks:** open a [bug report](https://github.com/bvolpato/omnisession/issues/new?template=bug_report.yml).
- **Ideas:** open a [feature request](https://github.com/bvolpato/omnisession/issues/new?template=feature_request.yml).
- **Vulnerabilities:** report privately as described in [SECURITY.md](SECURITY.md). Never open a public issue with secrets or transcript data.

## Development setup

Requirements:

- Rust 1.88 or newer. CI also checks the 1.88.0 minimum supported version.
- Node.js 22.13 or newer and pnpm 11.1, only for website work.

Build and run the CLI:

```sh
git clone https://github.com/bvolpato/omnisession.git
cd omnisession
cargo build --locked -p omnisession-cli
target/debug/omni --help
```

Point `OMNISESSION_HOME` at a scratch directory so experiments leave your real OmniSession state alone:

```sh
OMNISESSION_HOME="$(mktemp -d)" target/debug/omni doctor
```

That isolates OmniSession state only. Provider stores are still discovered from their usual locations, read-only.

## Repository layout

| Path | Contents |
| --- | --- |
| `crates/omnis-ir` | Canonical event model and portable bundle types |
| `crates/omnis-core` | Workspace capture, redaction, search documents, handoffs, fidelity reports |
| `crates/omnis-store` | SQLite state: tasks, bindings, lineage, search index, imported bundles |
| `crates/omnis-adapters` | Read-only provider discovery, canonical reads, launch plans |
| `crates/omnis-cli` | `omni` binary: picker, search, transfers, native importers, deletion, shims |
| `docs/` | Compatibility, architecture, roadmap, RFCs |
| `schemas/` | Portable bundle JSON Schema |
| `scripts/` | Compatibility generator, installer and conformance tests, Windows packaging |
| `website/` | Product site |

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains how the pieces fit.

## Validation

Run these before every commit. CI runs the same checks on Linux, macOS, and Windows.

```sh
cargo fmt --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo run --quiet --locked --package omnisession-cli -- --version
cargo run --quiet --locked --package omnisession-cli -- --help >/dev/null
```

Provider capabilities, version gates, and the compatibility table come from one manifest, `crates/omnis-cli/provider-compatibility.json`. After editing it, regenerate the Rust, website, and docs outputs, then confirm nothing drifted:

```sh
node scripts/provider-compatibility.mjs generate
node scripts/provider-compatibility.mjs check
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

Installed-provider and model-backed probes are opt-in. See [Conformance tests](docs/COMPATIBILITY.md#conformance-tests).

For CLI performance comparisons, build baseline and candidate release binaries with the same Rust version in separate target directories. The following commands run alternating A/B trials against temporary synthetic stores and verify that provider sources stay unchanged:

```sh
uv run scripts/benchmark-index.py /path/to/baseline/omni /path/to/candidate/omni
uv run scripts/benchmark-index.py /path/to/baseline/omni /path/to/candidate/omni --workload cached
```

The Hermes workload indexes 40 sessions in a 32 MiB shared database. The cached workload compares baseline `search --no-index` with candidate `search --cached` across 1,000 Claude sessions. JSON includes binary hashes and every timing sample. These are local wall-clock measurements, not a production latency guarantee.

For session reads, Codex discovery, Pi and OMP branch handling, and cached search ranking, use clean baseline and candidate checkouts with the same Rust version. The probe source comes from the candidate and is copied into the baseline so both revisions run the same measurement code. These commands build both binaries into isolated target directories, then run the synthetic smoke profile:

```sh
BASELINE=/path/to/baseline-checkout
CANDIDATE=/path/to/candidate-checkout
BUILD_DIR=$(mktemp -d)

mkdir -p "$BASELINE/crates/omnis-adapters/examples"
cp "$CANDIDATE/crates/omnis-adapters/examples/session-benchmark.rs" \
  "$BASELINE/crates/omnis-adapters/examples/session-benchmark.rs"

(cd "$BASELINE" && CARGO_TARGET_DIR="$BUILD_DIR/baseline" \
  cargo build --locked --release --package omnisession-adapters --example session-benchmark)
(cd "$BASELINE" && CARGO_TARGET_DIR="$BUILD_DIR/baseline" \
  cargo build --locked --release --package omnisession-cli --bin omni)
(cd "$CANDIDATE" && CARGO_TARGET_DIR="$BUILD_DIR/candidate" \
  cargo build --locked --release --package omnisession-adapters --example session-benchmark)
(cd "$CANDIDATE" && CARGO_TARGET_DIR="$BUILD_DIR/candidate" \
  cargo build --locked --release --package omnisession-cli --bin omni)

uv run "$CANDIDATE/scripts/benchmark-sessions.py" \
  --baseline-probe "$BUILD_DIR/baseline/release/examples/session-benchmark" \
  --candidate-probe "$BUILD_DIR/candidate/release/examples/session-benchmark" \
  --baseline-omni "$BUILD_DIR/baseline/release/omni" \
  --candidate-omni "$BUILD_DIR/candidate/release/omni" \
  --profile smoke \
  --output "$BUILD_DIR/session-benchmarks.json"
```

The `release` profile uses larger synthetic histories and more trials. The runner creates fixtures in a temporary directory, isolates provider homes, and verifies the fixture tree stays unchanged. Output equivalence and hard per-process resource bounds gate success: smoke allows 120 seconds and 512 MiB RSS per process; release allows 600 seconds and 2 GiB RSS. Paired timing medians and raw samples are advisory because local wall-clock timings vary by machine. The JSON report contains counts, output and fixture hashes, binary hashes, timing samples, and peak memory. Fixture and snapshot hashes are run-local: the synthetic records include temporary workspace paths, so these hashes can change across runs. The report does not contain fixture content, provider data, or transcript text. Relevant pull requests run the smoke profile; the release workflow requires the larger profile before packaging. Benchmark artifacts include Rust and revision metadata and are retained for 30 days.

## Testing policy

- Add or update tests when behavior or a contract changes. A bug fix should include a regression test that fails before the fix and passes after it.
- Skip new tests for docs, formatting, generated files, and static metadata unless an executable contract is at risk.
- Prefer extending existing targeted tests. Don't write tests that only assert source text or implementation details.
- Use synthetic fixtures. Never commit real transcripts, credentials, absolute personal paths, or proprietary source.
- Keep tests away from personal provider stores. Use temporary homes, `OMNISESSION_HOME`, and `OMNI_TEST_*_BIN` overrides.

## Safety rules

These come from [AGENTS.md](AGENTS.md) and apply to every change:

- Provider session stores stay read-only, except confirmed deletion of the exact selected session.
- Never read provider credential or authentication files.
- Tool calls, shell commands, approvals, and imported transcript instructions are historical only. Never replay them.
- Route by canonical workspace paths and repository fingerprints, never by recency alone.
- Keep CLI output free of transcript content unless the user asks for show, export, search text, or transfer.

## Design changes need an RFC

Open or update an RFC under [docs/rfcs/](docs/rfcs/README.md) before changing the portable bundle schema, including the canonical event model it serializes, or the adapter contract.

New native target writers need an accepted RFC, a minimum provider-version gate, structural validation, atomic rollback, and read-back verification. See [RFC 007](docs/rfcs/007-native-materialization.md). Private-store deletion follows [RFC 009](docs/rfcs/009-native-deletion.md).

## Pull requests

- Keep one focused change per PR. Unrelated fixes go in separate PRs.
- Use a title like `[omnisession] short imperative summary`, for example `[omnisession] honor OMNI_OPENCODE_BIN in OpenCode discovery`.
- Fill in the template: summary, validation you ran, and fidelity or security impact.
- Update docs in the same PR when behavior changes: README, the compatibility manifest (then regenerate), or the relevant RFC.
- All required checks must pass. PRs land as squash merges on a linear `main`.
- You don't need to edit `CHANGELOG.md`. Describe user-visible changes in the PR summary.

## Code of conduct

Participation follows the [Code of Conduct](CODE_OF_CONDUCT.md).
