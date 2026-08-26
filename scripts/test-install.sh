#!/usr/bin/env sh
# Smoke-test the curl-pipe installer without downloading a GitHub release.
set -eu

project_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/omni-install-test.XXXXXX")
cleanup() {
    rm -rf "$temp_dir"
}
trap cleanup EXIT HUP INT TERM

fail() {
    printf '%s\n' "error: $*" >&2
    exit 1
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

fixture_dir="$temp_dir/fixture"
tool_dir="$temp_dir/tools"
package_dir="$fixture_dir/package"
mkdir -p "$package_dir" "$tool_dir"

# shellcheck disable=SC2016
printf '%s\n' '#!/usr/bin/env sh' \
    'set -eu' \
    'if [ "$1" = shim ] && [ "$2" = install ] && [ "$3" = --bin-dir ]; then' \
    '    printf "Add provider shims to PATH: %s/omni-shims\\n" "$4"' \
    '    exit 0' \
    'fi' \
    'exit 1' >"$package_dir/omni"
chmod 0755 "$package_dir/omni"
printf '%s\n' 'MIT License' >"$package_dir/LICENSE"
tar -C "$package_dir" -czf "$fixture_dir/omni-linux-x86_64.tar.gz" omni LICENSE
checksum=$(sha256_file "$fixture_dir/omni-linux-x86_64.tar.gz")
printf '%s  %s\n' "$checksum" 'omni-linux-x86_64.tar.gz' >"$fixture_dir/SHA256SUMS"
printf '%s  %s\n' '0000000000000000000000000000000000000000000000000000000000000000' \
    'omni-linux-x86_64.tar.gz' >"$fixture_dir/BAD-SHA256SUMS"

real_awk=$(command -v awk)
export REAL_AWK="$real_awk"
# Simulate older POSIX awk implementations that reject interval expressions.
# shellcheck disable=SC2016
printf '%s\n' '#!/usr/bin/env sh' \
    'case "$*" in' \
    '    *"{64}"*) exit 2 ;;' \
    'esac' \
    'exec "$REAL_AWK" "$@"' >"$tool_dir/awk"
chmod 0755 "$tool_dir/awk"

# shellcheck disable=SC2016
printf '%s\n' '#!/usr/bin/env sh' \
    'case "${1:-}" in' \
    '    -s) printf "%s\\n" Linux ;;' \
    '    -m) printf "%s\\n" x86_64 ;;' \
    '    *) exit 1 ;;' \
    'esac' >"$tool_dir/uname"
chmod 0755 "$tool_dir/uname"

# shellcheck disable=SC2016
printf '%s\n' '#!/usr/bin/env sh' \
    'set -eu' \
    'if [ "${FAIL_FIRST_DOWNLOAD:-}" = 1 ] && [ ! -e "$CURL_STATE" ]; then' \
    '    : >"$CURL_STATE"' \
    '    exit 35' \
    'fi' \
    'output=' \
    'url=' \
    'while [ "$#" -gt 0 ]; do' \
    '    case "$1" in' \
    '        --output|-o) shift; output=$1 ;;' \
    '        *) url=$1 ;;' \
    '    esac' \
    '    shift' \
    'done' \
    'case "$url" in' \
    '    */SHA256SUMS) cp "$FIXTURE_DIR/$CHECKSUM_FILE" "$output" ;;' \
    '    */omni-linux-x86_64.tar.gz) cp "$FIXTURE_DIR/omni-linux-x86_64.tar.gz" "$output" ;;' \
    '    *) exit 1 ;;' \
    'esac' >"$tool_dir/curl"
chmod 0755 "$tool_dir/curl"

install_dir="$temp_dir/bin"
output_path="$temp_dir/output"
home_dir="$temp_dir/home"
mkdir -p "$home_dir" "$install_dir"
# shellcheck disable=SC2016
printf '%s\n' '#!/usr/bin/env sh' \
    'if [ "${1:-}" = --version ]; then printf "%s\n" "omnis 0.8.25"; exit 0; fi' \
    'if [ "${1:-}" = shim ] && [ "${2:-}" = uninstall ]; then exit 0; fi' \
    'exit 1' >"$install_dir/omnis"
chmod 0755 "$install_dir/omnis"
HOME="$home_dir" SHELL='/bin/zsh' OMNI_INSTALL_DIR="$install_dir" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >"$output_path" 2>&1
[ -x "$install_dir/omni" ] || fail 'installer did not install omni'
[ ! -e "$install_dir/omnis" ] || fail 'installer did not remove legacy OmniSession binary'
grep -F "Add provider shims to PATH: $install_dir/omni-shims" "$output_path" >/dev/null || \
    fail 'installer did not pass through shim PATH guidance'
grep -Fqx '# >>> omnisession shims >>>' "$home_dir/.zshrc" || fail 'installer did not configure zsh PATH'
# shellcheck disable=SC2016
grep -Fqx '    export PATH="${OMNISESSION_HOME:-$HOME/.omnisession}/shims:$PATH"' "$home_dir/.zshrc" || \
    fail 'installer did not add provider shims to PATH'
mkdir -p "$home_dir/.omnisession/shims"
path_first=$(HOME="$home_dir" OMNISESSION_HOME="$home_dir/.omnisession" sh -c \
    '. "$HOME/.zshrc"; printf "%s\\n" "${PATH%%:*}"')
[ "$path_first" = "$home_dir/.omnisession/shims" ] || fail 'generated zsh profile does not prepend shim directory'

HOME="$home_dir" SHELL='/bin/zsh' OMNI_INSTALL_DIR="$install_dir" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >>"$output_path" 2>&1
marker_count=$(grep -Fxc '# >>> omnisession shims >>>' "$home_dir/.zshrc")
[ "$marker_count" -eq 1 ] || fail 'installer added duplicate zsh PATH blocks'

retry_install_dir="$temp_dir/retry-bin"
retry_state="$temp_dir/curl-retry-state"
HOME="$home_dir" SHELL='/bin/zsh' OMNI_INSTALL_DIR="$retry_install_dir" \
    FIXTURE_DIR="$fixture_dir" CHECKSUM_FILE='SHA256SUMS' FAIL_FIRST_DOWNLOAD=1 \
    CURL_STATE="$retry_state" OMNI_NO_MODIFY_PATH=1 PATH="$tool_dir:$PATH" \
    sh "$project_root/install.sh" >>"$output_path" 2>&1
[ -x "$retry_install_dir/omni" ] || fail 'installer did not recover from interrupted download'
grep -F 'Download interrupted. Retrying (1/5)...' "$output_path" >/dev/null || \
    fail 'installer did not report interrupted download retry'

no_path_home="$temp_dir/no-path-home"
mkdir -p "$no_path_home"
printf '%s\n' '# existing setting' >"$no_path_home/.bashrc"
HOME="$no_path_home" SHELL='/bin/bash' OMNI_INSTALL_DIR="$temp_dir/no-path-bin" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' OMNI_NO_MODIFY_PATH=1 PATH="$tool_dir:$PATH" \
    sh "$project_root/install.sh" >>"$output_path" 2>&1
grep -Fqx '# existing setting' "$no_path_home/.bashrc" || fail 'installer overwrote bash profile'
if grep -Fqx '# >>> omnisession shims >>>' "$no_path_home/.bashrc"; then
    fail 'installer ignored OMNI_NO_MODIFY_PATH=1'
fi

foreign_install_dir="$temp_dir/foreign-bin"
mkdir -p "$foreign_install_dir"
printf '%s\n' '#!/usr/bin/env sh' 'printf "%s\n" "different-project 1.0"' >"$foreign_install_dir/omnis"
chmod 0755 "$foreign_install_dir/omnis"
HOME="$home_dir" SHELL='/bin/zsh' OMNI_INSTALL_DIR="$foreign_install_dir" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' OMNI_NO_MODIFY_PATH=1 PATH="$tool_dir:$PATH" \
    sh "$project_root/install.sh" >>"$output_path" 2>&1
[ -x "$foreign_install_dir/omni" ] || fail 'installer did not install beside foreign omnis command'
[ "$("$foreign_install_dir/omnis" --version)" = 'different-project 1.0' ] || \
    fail 'installer changed foreign omnis command'

bad_install_dir="$temp_dir/bad-bin"
if HOME="$home_dir" SHELL='/bin/zsh' OMNI_INSTALL_DIR="$bad_install_dir" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='BAD-SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >/dev/null 2>&1; then
    fail 'installer accepted an invalid checksum'
fi
[ ! -e "$bad_install_dir/omni" ] || fail 'installer wrote a binary before checksum verification'

printf '%s\n' 'install.sh smoke test passed'
