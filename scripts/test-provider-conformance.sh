#!/usr/bin/env bash
set -euo pipefail

project_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
temporary=$(mktemp -d "${TMPDIR:-/tmp}/omni-provider-conformance.XXXXXX")
cleanup() {
    rm -rf -- "$temporary"
}
trap cleanup EXIT HUP INT TERM

require_binary() {
    local variable=$1
    local command_name=$2
    local binary=${!variable:-}
    if [[ -z "$binary" ]]; then
        while IFS= read -r candidate; do
            if [[ -L "$candidate" && $(basename -- "$(readlink "$candidate")") == omni ]]; then
                continue
            fi
            binary=$candidate
            break
        done < <(type -a -p -- "$command_name" 2>/dev/null | awk '!seen[$0]++')
    fi
    if [[ -z "$binary" || ! -x "$binary" ]]; then
        printf 'error: %s requires %s or %s\n' "$command_name conformance" "$variable" "$command_name" >&2
        exit 1
    fi
    declare -gx "$variable=$binary"
}

version_stub() {
    local name=$1
    local output=$2
    local path="$temporary/$name"
    printf '#!/usr/bin/env sh\nprintf "%%s\\n" %q\n' "$output" >"$path"
    chmod 0755 "$path"
    printf '%s\n' "$path"
}

cursor_safe_cargo() {
    if [[ $(uname -s) != Linux ]]; then
        cargo "$@"
        return
    fi
    if command -v unshare >/dev/null 2>&1 && \
        unshare --user --map-root-user --pid --fork --mount-proc true >/dev/null 2>&1; then
        unshare --user --map-root-user --pid --fork --mount-proc cargo "$@"
        return
    fi
    cargo "$@"
}

require_binary OMNI_TEST_CLAUDE_BIN claude
require_binary OMNI_TEST_CODEX_BIN codex
require_binary OMNI_TEST_OPENCODE_BIN opencode
require_binary OMNI_TEST_GROK_BIN grok
require_binary OMNI_TEST_HERMES_BIN hermes

if [[ -z "${OMNI_TEST_CURSOR_BIN:-}" ]]; then
    export OMNI_TEST_CURSOR_BIN
    OMNI_TEST_CURSOR_BIN=$(version_stub cursor-agent '2026.07.23-e383d2b')
fi
if [[ -z "${OMNI_TEST_PI_BIN:-}" ]]; then
    export OMNI_TEST_PI_BIN
    OMNI_TEST_PI_BIN=$(version_stub pi '0.82.0')
fi
if [[ -z "${OMNI_TEST_ANTIGRAVITY_BIN:-}" ]]; then
    export OMNI_TEST_ANTIGRAVITY_BIN
    OMNI_TEST_ANTIGRAVITY_BIN=$(version_stub antigravity '1.1.8')
fi
if [[ -z "${OMNI_TEST_CURSOR_IDE_BIN:-}" ]]; then
    export OMNI_TEST_CURSOR_IDE_BIN="$temporary/Cursor-3.12.17-x86_64.AppImage"
    printf '#!/usr/bin/env sh\nexit 1\n' >"$OMNI_TEST_CURSOR_IDE_BIN"
    chmod 0755 "$OMNI_TEST_CURSOR_IDE_BIN"
fi

for variable in \
    ANTHROPIC_API_KEY \
    OPENAI_API_KEY \
    XAI_API_KEY \
    GEMINI_API_KEY \
    GOOGLE_API_KEY \
    OPENROUTER_API_KEY; do
    unset "$variable"
done

cd "$project_root"

cursor_safe_cargo test --locked --package omnisession-cli --test native_conformance \
    installed_nine_by_nine_cross_provider_matrix -- --ignored --exact --nocapture
cargo test --locked --package omnisession-cli --test native_conformance \
    installed_hermes_round_trips_isolated_synthetic_history \
    -- --ignored --exact --nocapture
cargo test --locked --package omnisession-cli --test grok_conformance \
    installed_grok_round_trips_isolated_synthetic_history -- --ignored --exact --nocapture
cargo test --locked --package omnisession-cli \
    opencode_import::tests::installed_opencode_round_trips_isolated_bounded_history \
    -- --ignored --exact --nocapture
cargo test --locked --package omnisession-cli --test native_conformance \
    installed_antigravity_round_trips_isolated_synthetic_history \
    -- --ignored --exact --nocapture
cursor_safe_cargo test --locked --package omnisession-cli --test native_conformance \
    installed_cursor_ide_round_trips_isolated_synthetic_history \
    -- --ignored --exact --nocapture

printf '%s\n' 'token-free provider conformance passed: 72 matrix cells, Hermes native fork, and large/private-store checks'
