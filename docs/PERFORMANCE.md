# Performance profiling

OmniSession has synthetic benchmarks for session discovery, transcript parsing, cached search, indexing, store writes, result ranking, and store startup. Use synthetic fixtures and isolated homes. Do not run performance measurements against provider-owned session stores.

## Build optimized binaries with symbols

Set a different `CARGO_TARGET_DIR` for each source revision so Cargo does not reuse an artifact built from another checkout. Release optimization keeps the operation representative, and release debug information lets profilers resolve Rust symbols.

```sh
env CARGO_TARGET_DIR=/tmp/omni-target-baseline CARGO_PROFILE_RELEASE_DEBUG=1 \
  cargo build --locked --release \
  --package omnisession-cli --bin omni \
  --package omnisession-adapters --example session-benchmark

env CARGO_TARGET_DIR=/tmp/omni-target-candidate CARGO_PROFILE_RELEASE_DEBUG=1 \
  cargo build --locked --release \
  --package omnisession-cli --bin omni \
  --package omnisession-adapters --example session-benchmark
```

## Compare one workload

Select one or more named cases to keep the run focused. Every case creates synthetic provider data, records fixture hashes, compares stable output between binaries, and enforces wall time and memory bounds. It alternates baseline and candidate order. Reports contain raw samples and medians. They report p50 as the median and p95 using the nearest-rank method, along with binary hashes and fixture hashes. Adapter `operation_ns` excludes process startup; wall time includes process startup and CLI work. The search cases index their synthetic corpus before timing cached search.

```sh
uv run scripts/benchmark-sessions.py \
  --baseline-probe /tmp/omni-target-baseline/release/examples/session-benchmark \
  --candidate-probe /tmp/omni-target-candidate/release/examples/session-benchmark \
  --baseline-omni /tmp/omni-target-baseline/release/omni \
  --candidate-omni /tmp/omni-target-candidate/release/omni \
  --profile smoke \
  --case pi-branch-compaction \
  --repeats 7 --warmups 2 \
  --output /tmp/omni-session-benchmark.json
```

Use `--profile release` for larger inputs. It increases transcripts to 10,000–12,000 turns, Codex discovery to 40,000 session files, and search to 10,000 sessions. Each sample launches a new CLI process. “Cold” therefore means a new process and its store startup, not a cold OS page cache; the harness does not flush system caches, and warmups can warm them. A workload called `cached` means the trajectory is already indexed. Cached-search index setup is measured separately in `search_index_setup_samples`; the search sample still includes process and database startup. Warmup runs are excluded from timing samples.

The synthetic search corpus includes `benchmark archive` in each title and user message, and `Synthetic response` in each assistant message. These two cached-search selectors support comparisons of multi-term and mixed-clause ranking. Smoke uses 1,000 sessions; release uses 10,000.

```sh
BASELINE=/tmp/omni-target-baseline
CANDIDATE=/tmp/omni-target-candidate

uv run scripts/benchmark-sessions.py \
  --baseline-probe "$BASELINE/release/examples/session-benchmark" \
  --candidate-probe "$CANDIDATE/release/examples/session-benchmark" \
  --baseline-omni "$BASELINE/release/omni" \
  --candidate-omni "$CANDIDATE/release/omni" \
  --profile smoke \
  --case search-and-1000 \
  --case search-mixed-clause-1000

uv run scripts/benchmark-sessions.py \
  --baseline-probe "$BASELINE/release/examples/session-benchmark" \
  --candidate-probe "$CANDIDATE/release/examples/session-benchmark" \
  --baseline-omni "$BASELINE/release/omni" \
  --candidate-omni "$CANDIDATE/release/omni" \
  --profile release \
  --case search-and-10000 \
  --case search-mixed-clause-10000
```

Run each benchmark on an otherwise quiet machine. p95 from a small number of samples is only a rough tail estimate; preserve the raw samples and increase `--repeats` before comparing small changes. The report records the operating system, kernel, architecture, CPU count, allowed CPU affinity when supported, and Python version, but results can still vary with CPU frequency, other workloads, filesystem cache, and hardware.

On Linux, optionally prefix the benchmark driver with `taskset -c 2` (replace `2` with an allowed CPU) to run the driver and its child probes on the same CPU. This limits their affinity but does not reserve that CPU or keep other workloads off it. When timing swings, Callgrind instruction counts can help separate scheduling noise from a change in executed work.

## Profile a selected candidate workload

Callgrind records instruction counts and call relationships. Massif records heap usage over time. Both run in a separate pass against the candidate after the ordinary baseline/candidate timing comparison. Their wall time and RSS are labeled as instrumented; Valgrind changes execution speed substantially, so use these artifacts to locate expensive code, not to claim native latency improvements. Similar instruction counts with fluctuating wall times can point to scheduling noise. Without a function filter, whole-process Callgrind includes benchmark validation work such as hashing the output; this can dominate the profile even though adapter `operation_ns` stops before that work.

The output directory must be new or empty. Newly created output directories are private to the current user. The instrumented run repeats the candidate workload, checks its output digest and record counts, verifies provider fixture bytes stay unchanged, and enforces per-process timeout and RSS bounds. Pick an explicit case and profiler to avoid unexpectedly instrumenting the entire workload suite.

```sh
uv run scripts/benchmark-sessions.py \
  --baseline-probe /tmp/omni-target-baseline/release/examples/session-benchmark \
  --candidate-probe /tmp/omni-target-candidate/release/examples/session-benchmark \
  --baseline-omni /tmp/omni-target-baseline/release/omni \
  --candidate-omni /tmp/omni-target-candidate/release/omni \
  --profile smoke \
  --case pi-many-short \
  --repeats 7 --warmups 2 \
  --profile-tool callgrind \
  --profile-function '<omnis_adapters::pi::PiAdapter as omnis_adapters::ProviderAdapter>::read_session_at' \
  --profiler /usr/bin/valgrind \
  --profile-output-dir /tmp/omni-callgrind-pi
```

`--profile-function` is valid only with Callgrind. It disables collection at startup and passes the pattern as one literal `--toggle-collect` argument, excluding benchmark setup, validation, and hashing. Inspect the actual symbols with `callgrind_annotate` before choosing a pattern because Rust methods can be inlined or renamed across builds. Prefer exact demangled names: Callgrind toggles collection each time a matching function is entered, so a wildcard that also matches a nested callee can toggle collection off again and produce counts for only part of the target function. The exact Pi adapter method shown in the baseline profile is `<omnis_adapters::pi::PiAdapter as omnis_adapters::ProviderAdapter>::read_session_at`. The harness checks the first instruction count in the Callgrind `summary:` or `totals:` record, reports `instruction_count`, and fails with “no instructions collected” if the pattern matches no collected work. Omit the option for the whole-process profile. The report records the pattern.

Use `--profile-tool massif` to capture heap profiles. To inspect a Callgrind profile, pass its output file to `callgrind_annotate`. To inspect a Massif profile, use `ms_print`. When Valgrind is unpacked into a local directory instead of installed system-wide, set `VALGRIND_LIB` to its `libexec/valgrind` directory for that command. The profiler runs with a clean synthetic home and no inherited provider credentials or settings.

Linux `perf record` can capture sampled CPU stacks with less instrumentation overhead, but it requires a matching kernel tool and permission to access performance events. If the tool is unavailable or the kernel denies access, use Valgrind or run the benchmark on a host configured for perf. Do not change machine-wide profiler permissions just for a local measurement.

## Microbenchmark canonical projections

The core projection benchmark isolates six operations over a fixed synthetic `CanonicalSnapshot`: preview, semantic handoff planning, handoff rendering, search-document generation, import-trajectory conversion, and secret redaction. The source contains deterministic message roles, Unicode, secret markers, historical-only tools, plans, provider metadata, and timestamps. Fixture generation and the source hash happen before timing. Each operation's output hash is computed outside the timed interval, then checked again afterward. JSON parsing is not part of these measurements.

The GitHub Actions workflow runs these timings weekly and on manual dispatch. It saves the 1,000, 10,000, and 50,000 event JSON reports with seven samples, plus `rustc -Vv` output and the commit SHA, in a 30-day `projection-benchmarks-<sha>-<attempt>` artifact. This 15-minute job runs only for scheduled and manual events; it is not a pull-request gate.

Build the example in the optimized symbol-enabled release profile:

```sh
CARGO_TARGET_DIR=/tmp/omni-core-profile \
CARGO_PROFILE_RELEASE_DEBUG=1 \
  cargo build --locked --release \
  --package omnisession-core --example projection-benchmark
```

Run all six operations for 1,000 events, then repeat for 10,000 and 50,000 events. The default synthetic message body is 1,024 characters; change its size only when measuring that dimension too.

```sh
/tmp/omni-core-profile/release/examples/projection-benchmark \
  --operation all --events 1000 --samples 11 --iterations 1
/tmp/omni-core-profile/release/examples/projection-benchmark \
  --operation all --events 10000 --samples 11 --iterations 1
/tmp/omni-core-profile/release/examples/projection-benchmark \
  --operation all --events 50000 --samples 11 --iterations 1
```

The JSON output contains event count, fixture hash, unchanged-source result, output hash, and operation timing samples. `median_ns` is the sample median and `p95_ns` uses the nearest-rank estimate. Keep the sample count odd when you want one central sample.

Use exact demangled names for focused core Callgrind profiles. A wildcard can also match a nested callee and toggle collection off, so filtered counts do not always represent the whole target function:

```sh
valgrind --tool=callgrind --collect-atstart=no \
  '--toggle-collect=omnis_core::plan_semantic_handoff' \
  --callgrind-out-file=/tmp/omni-plan.callgrind \
  /tmp/omni-core-profile/release/examples/projection-benchmark \
  --operation handoff-plan --events 10000 --samples 1 --iterations 1

valgrind --tool=callgrind --collect-atstart=no \
  '--toggle-collect=omnis_core::session_preview' \
  --callgrind-out-file=/tmp/omni-preview.callgrind \
  /tmp/omni-core-profile/release/examples/projection-benchmark \
  --operation preview --events 10000 --samples 1 --iterations 1
```

The symbol names must match the implementation at the selected revision. Inspect the profile with `callgrind_annotate`. Instrumented timings and instruction counts identify call paths; use the uninstrumented microbenchmark for latency comparisons.
