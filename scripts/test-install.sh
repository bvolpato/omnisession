#!/usr/bin/env sh
# Smoke-test the curl-pipe installer without downloading a GitHub release.
set -eu

project_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/omni-install-test.XXXXXX")
cleanup() {
    rm -rf "$temp_dir"
}
trap cleanup EXIT HUP INT TERM
unset OMNISESSION_HOME OMNI_NO_MODIFY_PATH

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
    'if [ "${1:-}" = --version ]; then printf "%s\n" "omni 0.10.1"; exit 0; fi' \
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
    'if [ -n "${INTERRUPT_STATE:-}" ]; then' \
    '    printf "%s\n" attempt >>"$INTERRUPT_STATE"' \
    '    kill -TERM "$PPID"' \
    '    exit 23' \
    'fi' \
    'output=' \
    'headers=' \
    'url=' \
    'while [ "$#" -gt 0 ]; do' \
    '    case "$1" in' \
    '        --output|-o) shift; output=$1 ;;' \
    '        --dump-header|-D) shift; headers=$1 ;;' \
    '        *) url=$1 ;;' \
    '    esac' \
    '    shift' \
    'done' \
    'release=https://github.com/bvolpato/omnisession/releases/download/v0.10.1' \
    '[ "${NO_RELEASE_TAG:-}" != 1 ] || release=https://github.com/bvolpato/omnisession/releases/latest/download' \
    'case "$url" in' \
    '    https://github.com/bvolpato/omnisession/releases/latest/download/SHA256SUMS)' \
    '        printf "HTTP/1.1 302 Found\r\nLocation: %s/SHA256SUMS\r\n\r\n" "$release" >"$headers"' \
    '        printf "HTTP/1.1 200 OK\r\n\r\n" >>"$headers"' \
    '        cp "$FIXTURE_DIR/$CHECKSUM_FILE" "$output" ;;' \
    '    "$release/omni-linux-x86_64.tar.gz") cp "$FIXTURE_DIR/omni-linux-x86_64.tar.gz" "$output" ;;' \
    '    *) exit 1 ;;' \
    'esac' >"$tool_dir/curl"
chmod 0755 "$tool_dir/curl"

install_dir="$temp_dir/bin"
output_path="$temp_dir/output"
home_dir="$temp_dir/home"
mkdir -p "$home_dir" "$install_dir"
HOME="$home_dir" SHELL='/bin/zsh' OMNI_INSTALL_DIR="$install_dir" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >"$output_path" 2>&1
[ -x "$install_dir/omni" ] || fail 'installer did not install omni'
[ ! -e "$home_dir/.zshrc" ] || fail 'fresh installer created a shell PATH block'
[ ! -e "$home_dir/.omnisession/shims" ] || fail 'fresh installer created provider aliases'
[ "$("$install_dir/omni" --version)" = 'omni 0.10.1' ] || fail 'installed binary has wrong version'
grep -Fx 'Downloading OmniSession 0.10.1 for linux/x86_64...' "$output_path" >/dev/null || \
    fail 'installer did not print the version that it downloads'
grep -Fx "Installed omni 0.10.1 to $install_dir/omni" "$output_path" >/dev/null || \
    fail 'installer did not print the version that it installed'

# Without a release tag in the redirect, the installer uses the latest URL and prints no version.
untagged_output="$temp_dir/untagged-output"
HOME="$home_dir" OMNI_INSTALL_DIR="$temp_dir/untagged-bin" FIXTURE_DIR="$fixture_dir" NO_RELEASE_TAG=1 \
    CHECKSUM_FILE='SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >"$untagged_output" 2>&1 || \
    fail 'installer failed without a release tag'
grep -Fx "Installed omni to $temp_dir/untagged-bin/omni" "$untagged_output" >/dev/null || \
    fail 'installer printed a version without a release tag'

alias_directory="$home_dir/.omnisession/shims"
provider_directory="$temp_dir/providers"
mkdir -p "$alias_directory" "$provider_directory"
# Old native build signatures are inspected statically. These fixtures cannot be executed.
for old_binary in "$install_dir/omni" "$install_dir/omnis"; do
    printf '\177ELF\n%s\n' 'Installed provider shims in `' >"$old_binary"
done
ln -s "$install_dir/omni" "$alias_directory/codex"
ln -s "$install_dir/omni" "$alias_directory/omp"
ln -s "$install_dir/omnis" "$alias_directory/grok"
ln -s ../../../bin/omni "$alias_directory/cursor-agent"
ln -s "$install_dir/omni" "$install_dir/opencode"
printf '%s\n' '#!/usr/bin/env sh' 'printf "%s\n" "OMP fixture $*"' >"$provider_directory/omp"
chmod 0755 "$provider_directory/omp"
ln -s "$provider_directory/omp" "$alias_directory/claude"
ln -s "$temp_dir/missing-provider" "$alias_directory/hermes"
printf '%s\n' 'foreign provider file' >"$alias_directory/pi"
mkdir "$alias_directory/agy"
printf '%s\n' 'historical data' >"$alias_directory/keep.txt"

path_block="$temp_dir/path-block"
cat >"$path_block" <<'BLOCK'
# >>> omnisession shims >>>
if [ -d "${OMNISESSION_HOME:-$HOME/.omnisession}/shims" ]; then
    export PATH="${OMNISESSION_HOME:-$HOME/.omnisession}/shims:$PATH"
fi
# <<< omnisession shims <<<
BLOCK
printf '%s\n' '# existing setting' >"$home_dir/.zshrc"
cat "$path_block" "$path_block" >>"$home_dir/.zshrc"
printf '%s' 'export CUSTOM=preserved' >>"$home_dir/.zshrc"
printf '%s\n%s' '# existing setting' 'export CUSTOM=preserved' >"$temp_dir/expected-zshrc"
sed 's/export PATH=/export CUSTOM_PATH=/' "$path_block" >"$home_dir/.bashrc"
cp "$home_dir/.bashrc" "$temp_dir/expected-bashrc"
printf '%s\n%s' '# >>> omnisession shims >>>' 'export CUSTOM=unpaired' >"$home_dir/.profile"
cp "$home_dir/.profile" "$temp_dir/expected-profile"

HOME="$home_dir" SHELL='/bin/zsh' OMNI_INSTALL_DIR="$install_dir" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >>"$output_path" 2>&1
for alias_path in "$alias_directory/codex" "$alias_directory/omp" "$alias_directory/grok" \
    "$alias_directory/cursor-agent" "$install_dir/opencode"; do
    [ ! -L "$alias_path" ] || fail "installer retained owned alias $alias_path"
done
[ ! -e "$install_dir/omnis" ] || fail 'installer retained legacy OmniSession binary'
[ -L "$alias_directory/claude" ] || fail 'installer removed unrelated provider symlink'
[ "$(readlink "$alias_directory/claude")" = "$provider_directory/omp" ] || fail 'installer changed unrelated link'
[ -L "$alias_directory/hermes" ] || fail 'installer removed unrelated dangling symlink'
[ -d "$alias_directory/agy" ] || fail 'installer removed unrelated directory'
[ "$(cat "$alias_directory/pi")" = 'foreign provider file' ] || fail 'installer changed unrelated regular file'
[ "$(cat "$alias_directory/keep.txt")" = 'historical data' ] || fail 'installer changed unrelated data'
cmp -s "$home_dir/.zshrc" "$temp_dir/expected-zshrc" || fail 'installer changed unrelated zsh profile content'
cmp -s "$home_dir/.bashrc" "$temp_dir/expected-bashrc" || fail 'installer changed an edited PATH block'
cmp -s "$home_dir/.profile" "$temp_dir/expected-profile" || fail 'installer changed an unpaired PATH marker'
provider_update=$(PATH="$alias_directory:$install_dir:$provider_directory:$PATH" omp update)
[ "$provider_update" = 'OMP fixture update' ] || fail 'provider update still resolves through OmniSession'

# The migration is idempotent and respects a custom state root.
custom_state="$temp_dir/custom-state"
mkdir -p "$custom_state/shims"
ln -s "$install_dir/omni" "$custom_state/shims/omp"
HOME="$home_dir" OMNISESSION_HOME="$custom_state" OMNI_INSTALL_DIR="$install_dir" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >>"$output_path" 2>&1
[ ! -e "$custom_state/shims" ] || fail 'installer did not remove empty owned alias directory'
cmp -s "$home_dir/.zshrc" "$temp_dir/expected-zshrc" || fail 'reinstall changed the shell profile'

eof_home="$temp_dir/eof-home"
mkdir -p "$eof_home"
printf '%s\n' 'export KEEP=1' >"$eof_home/.zshrc"
printf '%s' "$(cat "$path_block")" >>"$eof_home/.zshrc"
printf '%s' "$(cat "$path_block")" >"$eof_home/.bashrc"
cp "$path_block" "$eof_home/.profile"
HOME="$eof_home" OMNI_INSTALL_DIR="$temp_dir/eof-bin" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >>"$output_path" 2>&1
printf '%s\n' 'export KEEP=1' >"$temp_dir/expected-eof-zshrc"
cmp -s "$eof_home/.zshrc" "$temp_dir/expected-eof-zshrc" || fail 'installer removed a retained line terminator'
[ ! -s "$eof_home/.bashrc" ] || fail 'installer left content after removing an unterminated block-only profile'
[ ! -s "$eof_home/.profile" ] || fail 'installer left content after removing a block-only profile'

readonly_home="$temp_dir/readonly-home"
mkdir -p "$readonly_home"
printf '%s\n' 'export KEEP=1' >"$readonly_home/.zshrc"
cp "$readonly_home/.zshrc" "$temp_dir/expected-readonly-zshrc"
printf '%s\n' 'export KEEP=1' >"$readonly_home/.bashrc"
cat "$path_block" >>"$readonly_home/.bashrc"
printf '%s' 'export RETAIN=2' >>"$readonly_home/.bashrc"
printf '%s\n%s' 'export KEEP=1' 'export RETAIN=2' >"$temp_dir/expected-readonly-bashrc"
chmod 0444 "$readonly_home/.zshrc" "$readonly_home/.bashrc"
HOME="$readonly_home" OMNI_INSTALL_DIR="$temp_dir/readonly-bin" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >>"$output_path" 2>&1
cmp -s "$readonly_home/.zshrc" "$temp_dir/expected-readonly-zshrc" || fail 'installer changed unrelated read-only profile'
cmp -s "$readonly_home/.bashrc" "$temp_dir/expected-readonly-bashrc" || fail 'installer failed to migrate owned block in read-only profile'
for readonly_profile in "$readonly_home/.zshrc" "$readonly_home/.bashrc"; do
    readonly_mode=$(stat -c '%a' "$readonly_profile" 2>/dev/null || stat -f '%Lp' "$readonly_profile")
    [ "$readonly_mode" = '444' ] || fail 'installer changed read-only profile permissions'
done

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
no_path_bin="$temp_dir/no-path-bin"
mkdir -p "$no_path_home/.omnisession/shims" "$no_path_bin"
printf '%s\n' '# existing setting' >"$no_path_home/.bashrc"
cat "$path_block" >>"$no_path_home/.bashrc"
cp "$no_path_home/.bashrc" "$temp_dir/expected-no-path-bashrc"
printf '\177ELF\n%s\n' 'Installed provider shims in `' >"$no_path_bin/omni"
ln -s "$no_path_bin/omni" "$no_path_home/.omnisession/shims/omp"
HOME="$no_path_home" SHELL='/bin/bash' OMNI_INSTALL_DIR="$no_path_bin" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' OMNI_NO_MODIFY_PATH=1 PATH="$tool_dir:$PATH" \
    sh "$project_root/install.sh" >>"$output_path" 2>&1
cmp -s "$no_path_home/.bashrc" "$temp_dir/expected-no-path-bashrc" || fail 'installer ignored OMNI_NO_MODIFY_PATH=1'
[ ! -e "$no_path_home/.omnisession/shims" ] || fail 'PATH opt-out prevented owned alias cleanup'

foreign_install_dir="$temp_dir/foreign-bin"
mkdir -p "$foreign_install_dir"
# shellcheck disable=SC2016
printf '%s\n' '#!/usr/bin/env sh' ': >"$FOREIGN_PROVIDER_LAUNCHED"' >"$foreign_install_dir/omnis"
chmod 0755 "$foreign_install_dir/omnis"
cp "$foreign_install_dir/omnis" "$temp_dir/foreign-expected"
ln -s "$foreign_install_dir/omnis" "$foreign_install_dir/omp"
HOME="$home_dir" SHELL='/bin/zsh' OMNI_INSTALL_DIR="$foreign_install_dir" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' OMNI_NO_MODIFY_PATH=1 FOREIGN_PROVIDER_LAUNCHED="$temp_dir/provider-launched" PATH="$tool_dir:$PATH" \
    sh "$project_root/install.sh" >>"$output_path" 2>&1
[ -x "$foreign_install_dir/omni" ] || fail 'installer did not install beside foreign omnis command'
cmp -s "$foreign_install_dir/omnis" "$temp_dir/foreign-expected" || fail 'installer changed foreign omnis command'
[ -L "$foreign_install_dir/omp" ] || fail 'installer removed an alias to a foreign executable'
[ ! -e "$temp_dir/provider-launched" ] || fail 'installer executed an untrusted provider'

# The in-app updater replaces an old binary without removing its provider aliases.
updated_home="$temp_dir/updated-home"
updated_bin="$temp_dir/updated-bin"
mkdir -p "$updated_home/.omnisession/shims" "$updated_bin" "$temp_dir/shared-target"
printf '\177ELF\n%s\n' 'newer build without the old marker' >"$updated_bin/omni"
ln -s "$updated_bin/omni" "$updated_home/.omnisession/shims/codex"
ln -s "$updated_bin/omni" "$updated_bin/claude"
ln -s "$provider_directory/omp" "$updated_bin/omp"
HOME="$updated_home" OMNI_INSTALL_DIR="$updated_bin" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >>"$output_path" 2>&1
[ ! -e "$updated_home/.omnisession/shims" ] || fail 'installer retained aliases after an in-app update'
[ ! -L "$updated_bin/claude" ] || fail 'installer retained an install directory alias after an in-app update'
[ -L "$updated_bin/omp" ] || fail 'installer removed an unrelated provider after an in-app update'

# An existing omni link is replaced. A provider that shares its old target is not an alias.
printf '%s\n' 'shared target' >"$temp_dir/shared-target/tool"
rm "$updated_bin/omni"
ln -s "$temp_dir/shared-target/tool" "$updated_bin/omni"
ln -s "$temp_dir/shared-target/tool" "$updated_bin/codex"
HOME="$updated_home" OMNI_INSTALL_DIR="$updated_bin" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >>"$output_path" 2>&1
[ ! -L "$updated_bin/omni" ] || fail 'installer kept a linked omni command'
[ -L "$updated_bin/codex" ] || fail 'installer removed a provider that does not resolve to omni'
[ "$(cat "$temp_dir/shared-target/tool")" = 'shared target' ] || fail 'installer wrote through a linked omni command'

directory_bin="$temp_dir/directory-bin"
mkdir -p "$directory_bin/omni"
if HOME="$home_dir" OMNI_INSTALL_DIR="$directory_bin" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >>"$output_path" 2>&1; then
    fail 'installer reported success when omni is a directory'
fi
rmdir "$directory_bin/omni" || fail 'installer wrote into a directory named omni'

# A piped download that stops early must not run part of the installer.
script_lines=$(wc -l <"$project_root/install.sh")
for script_length in "$((script_lines - 1))" "$((script_lines * 3 / 4))" "$((script_lines / 2))"; do
    if head -n "$script_length" "$project_root/install.sh" | \
        HOME="$home_dir" OMNI_INSTALL_DIR="$temp_dir/truncated-bin" FIXTURE_DIR="$fixture_dir" \
            CHECKSUM_FILE='SHA256SUMS' PATH="$tool_dir:$PATH" sh >>"$output_path" 2>&1; then
        fail 'truncated installer reported success'
    fi
    [ ! -e "$temp_dir/truncated-bin" ] || fail 'truncated installer changed the install directory'
done

interrupt_state="$temp_dir/interrupt-state"
interrupt_status=0
HOME="$home_dir" OMNI_INSTALL_DIR="$temp_dir/interrupt-bin" FIXTURE_DIR="$fixture_dir" INTERRUPT_STATE="$interrupt_state" \
    CHECKSUM_FILE='SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >>"$output_path" 2>&1 || interrupt_status=$?
[ "$interrupt_status" -eq 143 ] || fail "interrupted installer exited with $interrupt_status"
[ "$(wc -l <"$interrupt_state")" -eq 1 ] || fail 'interrupted installer retried the download'

nul_home="$temp_dir/nul-home"
mkdir -p "$nul_home"
printf 'before\000after\n' >"$nul_home/.zshrc"
cat "$path_block" >>"$nul_home/.zshrc"
cp "$nul_home/.zshrc" "$temp_dir/expected-nul-zshrc"
HOME="$nul_home" OMNI_INSTALL_DIR="$temp_dir/nul-bin" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >>"$output_path" 2>&1
cmp -s "$nul_home/.zshrc" "$temp_dir/expected-nul-zshrc" || fail 'installer changed a profile that contains a NUL byte'

bad_install_dir="$temp_dir/bad-bin"
if HOME="$home_dir" SHELL='/bin/zsh' OMNI_INSTALL_DIR="$bad_install_dir" FIXTURE_DIR="$fixture_dir" \
    CHECKSUM_FILE='BAD-SHA256SUMS' PATH="$tool_dir:$PATH" sh "$project_root/install.sh" >/dev/null 2>&1; then
    fail 'installer accepted an invalid checksum'
fi
[ ! -e "$bad_install_dir/omni" ] || fail 'installer wrote a binary before checksum verification'

printf '%s\n' 'install.sh smoke test passed'
