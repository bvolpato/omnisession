#!/usr/bin/env bash
set -euo pipefail

project_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
mode=${1:-matrix}
case "$mode" in
    matrix | adapters | claude | codex | opencode | grok | hermes | pi) ;;
    *)
        printf 'error: unknown provider conformance mode: %s\n' "$mode" >&2
        exit 2
        ;;
esac
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

compatibility_value() {
    node "$project_root/scripts/provider-compatibility.mjs" get "$1" "$2"
}

ensure_version_stub() {
    local variable=$1
    local name=$2
    local provider=$3
    if [[ -n "${!variable:-}" ]]; then
        return
    fi
    local path
    path=$(version_stub "$name" "$(compatibility_value "$provider" minimum_version)")
    declare -gx "$variable=$path"
}

ensure_cursor_ide_stub() {
    if [[ -n "${OMNI_TEST_CURSOR_IDE_BIN:-}" ]]; then
        return
    fi
    local version
    version=$(compatibility_value cursor-ide minimum_version)
    export OMNI_TEST_CURSOR_IDE_BIN="$temporary/Cursor-${version}-x86_64.AppImage"
    printf '#!/usr/bin/env sh\nexit 1\n' >"$OMNI_TEST_CURSOR_IDE_BIN"
    chmod 0755 "$OMNI_TEST_CURSOR_IDE_BIN"
}

cursor_safe_cargo() {
    if [[ $(uname -s) != Linux ]]; then
        cargo "$@"
        return
    fi
    if command -v unshare >/dev/null 2>&1 && \
        unshare --user --map-root-user --pid --fork --mount-proc true >/dev/null 2>&1; then
        unshare --user --map-root-user --pid --fork --kill-child --mount-proc cargo "$@"
        return
    fi
    cargo "$@"
}

case "$mode" in
    matrix)
        require_binary OMNI_TEST_CLAUDE_BIN claude
        require_binary OMNI_TEST_CODEX_BIN codex
        require_binary OMNI_TEST_OPENCODE_BIN opencode
        require_binary OMNI_TEST_GROK_BIN grok
        require_binary OMNI_TEST_HERMES_BIN hermes
        ensure_version_stub OMNI_TEST_CURSOR_BIN cursor-agent cursor-agent
        ensure_version_stub OMNI_TEST_PI_BIN pi pi
        ensure_version_stub OMNI_TEST_ANTIGRAVITY_BIN antigravity antigravity
        ensure_cursor_ide_stub
        ;;
    adapters)
        ensure_version_stub OMNI_TEST_ANTIGRAVITY_BIN antigravity antigravity
        ensure_cursor_ide_stub
        ;;
    claude) require_binary OMNI_TEST_CLAUDE_BIN claude ;;
    codex) require_binary OMNI_TEST_CODEX_BIN codex ;;
    opencode) require_binary OMNI_TEST_OPENCODE_BIN opencode ;;
    grok) require_binary OMNI_TEST_GROK_BIN grok ;;
    hermes) require_binary OMNI_TEST_HERMES_BIN hermes ;;
    pi)
        require_binary OMNI_TEST_PI_BIN pi
        export PI_SKIP_VERSION_CHECK=1
        export PI_TELEMETRY=0
        ;;
esac

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

case "$mode" in
    matrix)
        cursor_safe_cargo test --locked --package omnisession-cli --test native_conformance \
            installed_nine_by_nine_cross_provider_matrix -- --ignored --exact --nocapture
        ;;
    adapters)
        cargo test --locked --package omnisession-adapters --tests
        cargo test --locked --package omnisession-cli --test native_conformance \
            installed_antigravity_round_trips_isolated_synthetic_history \
            -- --ignored --exact --nocapture
        cursor_safe_cargo test --locked --package omnisession-cli --test native_conformance \
            installed_cursor_ide_round_trips_isolated_synthetic_history \
            -- --ignored --exact --nocapture
        ;;
    claude)
        cargo test --locked --package omnisession-cli --test native_conformance \
            installed_claude_round_trips_isolated_synthetic_history \
            -- --ignored --exact --nocapture
        ;;
    codex)
        cargo test --locked --package omnisession-cli --test native_conformance \
            installed_codex_round_trips_isolated_synthetic_history \
            -- --ignored --exact --nocapture
        ;;
    opencode)
        cargo test --locked --package omnisession-cli \
            opencode_import::tests::installed_opencode_round_trips_isolated_bounded_history \
            -- --ignored --exact --nocapture
        ;;
    grok)
        cargo test --locked --package omnisession-cli --test grok_conformance \
            installed_grok_round_trips_isolated_synthetic_history \
            -- --ignored --exact --nocapture
        ;;
    hermes)
        cargo test --locked --package omnisession-cli --test native_conformance \
            installed_hermes_round_trips_isolated_synthetic_history \
            -- --ignored --exact --nocapture
        ;;
    pi)
        cargo test --locked --package omnisession-cli --test native_conformance \
            installed_pi_round_trips_isolated_synthetic_history \
            -- --ignored --exact --nocapture
        ;;
esac

printf 'token-free provider conformance passed: %s\n' "$mode"
