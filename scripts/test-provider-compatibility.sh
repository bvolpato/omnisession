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
done

node "$project_root/scripts/provider-compatibility.mjs" report \
    --status failed \
    --output-dir "$temporary/failed" \
    --generated-at 2026-01-01T00:00:00Z
node "$project_root/scripts/provider-compatibility.mjs" report \
    --status not_run \
    --output-dir "$temporary/not-run" \
    --generated-at 2026-01-01T00:00:00Z
export OMNI_COMPAT_TESTED_HERMES=0.20.5
export OMNI_COMPAT_TESTED_SOURCE_HERMES=python-package-metadata
export OMNI_COMPAT_TESTED_TAG_HERMES=v2099.1.2
export OMNI_COMPAT_TESTED_COMMIT_HERMES=0123456789abcdef0123456789abcdef01234567
node "$project_root/scripts/provider-compatibility.mjs" report \
    --status passed \
    --output-dir "$temporary/observed" \
    --generated-at 2026-01-01T00:00:00Z

node - \
    "$temporary/failed/provider-compatibility.json" \
    "$temporary/not-run/provider-compatibility.json" \
    "$temporary/observed/provider-compatibility.json" <<'NODE'
const fs = require("node:fs");
const failed = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
const notRun = JSON.parse(fs.readFileSync(process.argv[3], "utf8"));
const observed = JSON.parse(fs.readFileSync(process.argv[4], "utf8"));

if (failed.status !== "failed" || failed.providers.some((provider) =>
  provider.matrix_conformance !== "failed" ||
  provider.observed.status !== "not recorded" ||
  provider.observed.version !== null ||
  provider.observed.source !== null ||
  provider.observed.tag !== null ||
  provider.observed.commit !== null ||
  !provider.expected.version
)) {
  throw new Error("failed report conflated expected and observed provider versions");
}
if (notRun.status !== "not_run" ||
    notRun.providers.some((provider) => provider.matrix_conformance !== "not_run")) {
  throw new Error("not-run report did not preserve skipped matrix status");
}
const hermes = observed.providers.find((provider) => provider.id === "hermes");
if (hermes.expected.version !== "0.20.0" ||
    hermes.expected.tag !== "v2026.8.3" ||
    hermes.expected.commit !== "3c27eb6234bf91b8ceee9e9071591b31e9b148cb" ||
    hermes.observed.version !== "0.20.5" ||
    hermes.observed.source !== "python-package-metadata" ||
    hermes.observed.tag !== "v2099.1.2" ||
    hermes.observed.commit !== "0123456789abcdef0123456789abcdef01234567") {
  throw new Error("Hermes report conflated expected pin and observed source revision");
}
NODE

grep -F '| not recorded | not recorded | not recorded |' \
    "$temporary/failed/provider-compatibility.md" >/dev/null
grep -F '| 0.20.0 | v2026.8.3 @ 3c27eb6234bf | 0.20.5 | recorded | python-package-metadata | v2099.1.2 @ 0123456789ab |' \
    "$temporary/observed/provider-compatibility.md" >/dev/null
printf '%s\n' 'provider compatibility report contract passed'
