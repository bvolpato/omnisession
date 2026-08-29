#!/usr/bin/env bash
set -euo pipefail

project_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
temporary=$(mktemp -d "${TMPDIR:-/tmp}/omni-compatibility-report.XXXXXX")
cleanup() {
    rm -rf -- "$temporary"
}
trap cleanup EXIT HUP INT TERM

for provider in \
    CLAUDE \
    CODEX \
    OPENCODE \
    GROK \
    HERMES \
    ANTIGRAVITY \
    PI \
    CURSOR_AGENT \
    CURSOR_IDE; do
    unset "OMNI_COMPAT_TESTED_${provider}"
    unset "OMNI_COMPAT_TESTED_SOURCE_${provider}"
    unset "OMNI_COMPAT_TESTED_TAG_${provider}"
    unset "OMNI_COMPAT_TESTED_COMMIT_${provider}"
    unset "OMNI_COMPAT_EVIDENCE_${provider}"
    unset "OMNI_COMPAT_INSTALLED_${provider}"
    unset "OMNI_COMPAT_CANARY_${provider}"
done

node - \
    "$project_root/crates/omnis-cli/provider-compatibility.json" \
    "$temporary/npm-prefix/lib/node_modules" <<'NODE'
const fs = require("node:fs");
const path = require("node:path");
const manifest = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
for (const provider of manifest.providers) {
  const packageName = provider.release_tested.package;
  if (!packageName) continue;
  const packageDirectory = path.join(process.argv[3], packageName);
  fs.mkdirSync(packageDirectory, { recursive: true });
  fs.writeFileSync(
    path.join(packageDirectory, "package.json"),
    `${JSON.stringify({ name: packageName, version: provider.release_tested.version })}\n`,
  );
}
NODE
npm_environment=$(
    node "$project_root/scripts/provider-compatibility.mjs" \
        record-npm "$temporary/npm-prefix"
)
node - \
    "$project_root/crates/omnis-cli/provider-compatibility.json" \
    "$npm_environment" <<'NODE'
const fs = require("node:fs");
const manifest = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
const environment = new Map(
  process.argv[3].split("\n").map((line) => line.split("=", 2)),
);
for (const provider of manifest.providers) {
  if (!provider.release_tested.package) continue;
  const key = `OMNI_COMPAT_TESTED_${provider.id.replaceAll("-", "_").toUpperCase()}`;
  if (environment.get(key) !== provider.release_tested.version) {
    throw new Error(`record-npm did not report ${provider.id}`);
  }
}
NODE

export OMNI_COMPAT_EVIDENCE_CLAUDE=source-ci,synthetic-store
export OMNI_COMPAT_INSTALLED_GROK=failed
node "$project_root/scripts/provider-compatibility.mjs" report \
    --status failed \
    --matrix-status passed \
    --adapter-status failed \
    --platform windows \
    --output-dir "$temporary/failed" \
    --generated-at 2026-01-01T00:00:00Z
unset OMNI_COMPAT_EVIDENCE_CLAUDE
unset OMNI_COMPAT_INSTALLED_GROK
node "$project_root/scripts/provider-compatibility.mjs" report \
    --status not_run \
    --matrix-status not_run \
    --adapter-status not_run \
    --platform windows \
    --output-dir "$temporary/not-run" \
    --generated-at 2026-01-01T00:00:00Z
export OMNI_COMPAT_TESTED_HERMES=0.20.5
export OMNI_COMPAT_TESTED_SOURCE_HERMES=python-package-metadata
export OMNI_COMPAT_TESTED_TAG_HERMES=v2099.1.2
export OMNI_COMPAT_TESTED_COMMIT_HERMES=0123456789abcdef0123456789abcdef01234567
export OMNI_COMPAT_EVIDENCE_HERMES=source-ci,installed-token-free
export OMNI_COMPAT_INSTALLED_HERMES=passed
export OMNI_COMPAT_CANARY_OPENCODE=passed
node "$project_root/scripts/provider-compatibility.mjs" report \
    --status passed \
    --matrix-status passed \
    --adapter-status passed \
    --platform linux \
    --output-dir "$temporary/observed" \
    --generated-at 2026-01-01T00:00:00Z

export OMNI_COMPAT_EVIDENCE_CLAUDE=not-run,source-ci
if node "$project_root/scripts/provider-compatibility.mjs" report \
    --status passed \
    --matrix-status passed \
    --adapter-status passed \
    --platform windows \
    --output-dir "$temporary/invalid-evidence" \
    --generated-at 2026-01-01T00:00:00Z >/dev/null 2>&1; then
    printf '%s\n' 'error: report accepted mixed not-run evidence' >&2
    exit 1
fi
unset OMNI_COMPAT_EVIDENCE_CLAUDE
export OMNI_COMPAT_EVIDENCE_CODEX=source-ci,installed-token-free
if node "$project_root/scripts/provider-compatibility.mjs" report \
    --status passed \
    --matrix-status passed \
    --adapter-status passed \
    --platform windows \
    --output-dir "$temporary/missing-installed-version" \
    --generated-at 2026-01-01T00:00:00Z >/dev/null 2>&1; then
    printf '%s\n' 'error: report accepted installed evidence without observed version' >&2
    exit 1
fi
unset OMNI_COMPAT_EVIDENCE_CODEX
if node "$project_root/scripts/provider-compatibility.mjs" report \
    --status passed \
    --matrix-status passed \
    --platform windows \
    --output-dir "$temporary/missing-adapter-status" \
    --generated-at 2026-01-01T00:00:00Z >/dev/null 2>&1; then
    printf '%s\n' 'error: report accepted missing adapter status' >&2
    exit 1
fi

node - \
    "$temporary/failed/provider-compatibility.json" \
    "$temporary/not-run/provider-compatibility.json" \
    "$temporary/observed/provider-compatibility.json" <<'NODE'
const fs = require("node:fs");
const failed = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
const notRun = JSON.parse(fs.readFileSync(process.argv[3], "utf8"));
const observed = JSON.parse(fs.readFileSync(process.argv[4], "utf8"));

if (failed.schema_version !== 2 || failed.platform !== "windows" ||
    failed.status !== "failed" || failed.matrix_status !== "passed" ||
    failed.adapter_status !== "failed" ||
    failed.providers.some((provider) =>
  provider.matrix_conformance !== "passed" ||
  provider.platform !== "windows" ||
  provider.observed.status !== "not recorded" ||
  provider.observed.version !== null ||
  provider.observed.source !== null ||
  provider.observed.tag !== null ||
  provider.observed.commit !== null ||
  !provider.expected.version
)) {
  throw new Error("failed report conflated expected and observed provider versions");
}
const failedClaude = failed.providers.find((provider) => provider.id === "claude");
const failedGrok = failed.providers.find((provider) => provider.id === "grok");
if (failedClaude.observed_evidence.join(",") !== "source-ci,synthetic-store" ||
    failedClaude.installed_token_free_conformance !== "not_run" ||
    failedGrok.installed_token_free_conformance !== "failed") {
  throw new Error("failed report did not preserve matrix and exact installed outcomes");
}
if (notRun.status !== "not_run" || notRun.adapter_status !== "not_run" ||
    notRun.providers.some((provider) =>
      provider.matrix_conformance !== "not_run" ||
      !provider.declared_evidence.includes("source-ci") ||
      !provider.declared_evidence.includes("synthetic-store")
    )) {
  throw new Error("not-run report did not preserve skipped matrix status");
}
const hermes = observed.providers.find((provider) => provider.id === "hermes");
if (observed.platform !== "linux" || observed.adapter_status !== "passed" ||
    hermes.expected.version !== "0.20.0" ||
    hermes.expected.tag !== "v2026.8.3" ||
    hermes.expected.commit !== "3c27eb6234bf91b8ceee9e9071591b31e9b148cb" ||
    hermes.observed.version !== "0.20.5" ||
    hermes.observed.source !== "python-package-metadata" ||
    hermes.observed.tag !== "v2099.1.2" ||
    hermes.observed.commit !== "0123456789abcdef0123456789abcdef01234567" ||
    hermes.observed_evidence.join(",") !== "source-ci,installed-token-free" ||
    hermes.installed_token_free_conformance !== "passed") {
  throw new Error("Hermes report conflated expected pin and observed source revision");
}
const opencode = observed.providers.find((provider) => provider.id === "opencode");
if (opencode.authenticated_marker_canary !== "passed" ||
    opencode.observed_evidence.join(",") !== "authenticated-canary") {
  throw new Error("authenticated canary evidence was not surfaced");
}
NODE

grep -F '| not recorded | not recorded | not recorded |' \
    "$temporary/failed/provider-compatibility.md" >/dev/null
grep -F '| 0.20.0 | v2026.8.3 @ 3c27eb6234bf | 0.20.5 | recorded | python-package-metadata | v2099.1.2 @ 0123456789ab |' \
    "$temporary/observed/provider-compatibility.md" >/dev/null
grep -F 'Platform: Windows' "$temporary/failed/provider-compatibility.md" >/dev/null
grep -F 'Matrix conformance: passed' "$temporary/failed/provider-compatibility.md" >/dev/null
grep -F 'Adapter/store conformance: failed' "$temporary/failed/provider-compatibility.md" >/dev/null
grep -F '| passed | failed | not_available |' \
    "$temporary/failed/provider-compatibility.md" >/dev/null
grep -F '| installed | source-ci, installed-token-free | source-ci, installed-token-free |' \
    "$temporary/observed/provider-compatibility.md" >/dev/null
printf '%s\n' 'provider compatibility report contract passed'
