#!/usr/bin/env sh
# Install the latest OmniSession release for the current platform.
# The braces make the shell read the whole script before it runs any command.
{
set -eu

repository='bvolpato/omnisession'

die() {
    printf '%s\n' "error: $*" >&2
    exit 1
}

info() {
    printf '%s\n' "$*"
}

download() {
    download_output=$1
    download_url=$2
    download_attempt=1
    while [ "$download_attempt" -le 5 ]; do
        if curl --http1.1 --fail --location --silent --show-error \
            --connect-timeout 15 --speed-limit 1024 --speed-time 30 --proto '=https' --tlsv1.2 \
            --dump-header "$headers_path" --output "$download_output" "$download_url"; then
            return
        fi
        [ "$download_attempt" -eq 5 ] && break
        info "Download interrupted. Retrying ($download_attempt/5)..."
        download_attempt=$((download_attempt + 1))
        sleep 1
    done
    die "could not download $download_url"
}

is_old_omni() {
    [ -f "$1" ] && [ ! -L "$1" ] || return 1
    # Identify old native builds without executing a provider or relying on its filename.
    binary_magic=$(od -An -tx1 -N4 "$1" | tr -d ' \n')
    case "$binary_magic" in
        7f454c46 | cffaedfe | feedfacf | cafebabe | bebafeca) ;;
        *) return 1 ;;
    esac
    LC_ALL=C grep -aFq 'Installed provider shims in `' "$1"
}

remove_owned_aliases() {
    owned_binary=$1
    owned_directory=$2
    [ -d "$owned_directory" ] && [ ! -L "$owned_directory" ] || return 0
    for provider in agy claude codex cursor-agent grok hermes omp opencode pi; do
        alias_path="$owned_directory/$provider"
        # Linux and macOS shells support this identity comparison for symlink targets.
        # shellcheck disable=SC3013
        if [ -L "$alias_path" ] && [ "$alias_path" -ef "$owned_binary" ]; then
            rm "$alias_path"
            info "Removed OmniSession provider alias $alias_path"
        fi
    done
}

remove_old_path_block() {
    profile=$1
    [ -f "$profile" ] && [ ! -L "$profile" ] || return 0
    # Some awk implementations drop text after a NUL byte.
    [ "$(LC_ALL=C tr -d '\000' <"$profile" | wc -c)" = "$(wc -c <"$profile")" ] || return 0
    final_newline=0
    [ -n "$(tail -c 1 "$profile")" ] || final_newline=1
    # Only the exact generated block is owned. Edited and unpaired markers remain intact.
    if awk -v final_newline="$final_newline" '
        function emit(line, record) {
            printf "%s%s", separator, line
            separator = "\n"
            emitted = 1
            last_emitted_record = record
        }
        BEGIN {
            block[1] = "# >>> omnisession shims >>>"
            block[2] = "if [ -d \"${OMNISESSION_HOME:-$HOME/.omnisession}/shims\" ]; then"
            block[3] = "    export PATH=\"${OMNISESSION_HOME:-$HOME/.omnisession}/shims:$PATH\""
            block[4] = "fi"
            block[5] = "# <<< omnisession shims <<<"
        }
        {
            pending[++count] = $0
            if (count < 5) next
            matches = 1
            for (i = 1; i <= 5; i++) if (pending[i] != block[i]) matches = 0
            if (matches) { count = 0; removed = 1; next }
            emit(pending[1], NR - count + 1)
            for (i = 1; i < count; i++) pending[i] = pending[i + 1]
            count--
        }
        END {
            for (i = 1; i <= count; i++) emit(pending[i], NR - count + i)
            if (emitted && (final_newline || last_emitted_record < NR)) printf "\n"
            if (!removed) exit 3
        }
    ' "$profile" >"$temp_dir/profile-filtered"; then
        profile_stage=$(mktemp "${profile}.omni.XXXXXX") || die "could not stage $profile"
        cat "$temp_dir/profile-filtered" >"$profile_stage"
        profile_mode=$(stat -c '%a' "$profile" 2>/dev/null || stat -f '%Lp' "$profile")
        chmod "$profile_mode" "$profile_stage"
        mv -f "$profile_stage" "$profile"
        profile_stage=''
        info "Removed OmniSession shim PATH block from $profile"
    else
        status=$?
        [ "$status" -eq 3 ] || die "could not update $profile"
    fi
}

: "${HOME:?HOME must be set}"
OMNI_INSTALL_DIR=${OMNI_INSTALL_DIR:-"$HOME/.local/bin"}

case "$OMNI_INSTALL_DIR" in
    /) die 'OMNI_INSTALL_DIR must not be /' ;;
    /*) ;;
    *) die 'OMNI_INSTALL_DIR must be an absolute path' ;;
esac

case "$(uname -s)" in
    Linux) platform='linux' ;;
    Darwin) platform='darwin' ;;
    *) die "unsupported operating system: $(uname -s) (supported: Linux, macOS)" ;;
esac

case "$(uname -m)" in
    x86_64 | amd64) architecture='x86_64' ;;
    aarch64 | arm64) architecture='aarch64' ;;
    *) die "unsupported architecture: $(uname -m) (supported: x86_64, aarch64)" ;;
esac

if ! command -v curl >/dev/null 2>&1; then
    die 'curl is required to install OmniSession'
fi
if ! command -v tar >/dev/null 2>&1; then
    die 'tar is required to install OmniSession'
fi
if ! command -v install >/dev/null 2>&1; then
    die 'install is required to install OmniSession'
fi

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die 'sha256sum or shasum is required to verify OmniSession'
    fi
}

temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/omni-install.XXXXXX") || die 'could not create temporary directory'
profile_stage=''
temporary_binary=''
cleanup() {
    [ -z "$profile_stage" ] || rm -f "$profile_stage"
    [ -z "$temporary_binary" ] || rm -f "$temporary_binary"
    rm -rf "$temp_dir"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

archive_name="omni-${platform}-${architecture}.tar.gz"
releases_url="https://github.com/${repository}/releases"
release_url="$releases_url/latest/download"
archive_path="$temp_dir/$archive_name"
checksums_path="$temp_dir/SHA256SUMS"
headers_path="$temp_dir/headers"

if [ ! -t 1 ]; then
    info "OmniSession installer: ${platform}/${architecture} -> ${OMNI_INSTALL_DIR}"
fi

download "$checksums_path" "$release_url/SHA256SUMS"
# GitHub redirects the latest release to its tag. Download the archive from the same release.
version=$(LC_ALL=C sed -n 's|^[Ll]ocation: .*/releases/download/v\([0-9][0-9A-Za-z.-]*\)/.*|\1|p' "$headers_path" | head -n 1)
if [ -n "$version" ]; then
    release_url="$releases_url/download/v$version"
fi

info "Downloading OmniSession${version:+ $version} for ${platform}/${architecture}..."
download "$archive_path" "$release_url/$archive_name"

checksum_count=$(awk -v filename="$archive_name" '$2 == filename { count += 1 } END { print count + 0 }' "$checksums_path")
[ "$checksum_count" -eq 1 ] || die "SHA256SUMS does not contain exactly one checksum for $archive_name"
expected_checksum=$(awk -v filename="$archive_name" '$2 == filename { print $1; exit }' "$checksums_path")
case "$expected_checksum" in
    '' | *[!0123456789abcdefABCDEF]*) die "invalid SHA-256 checksum for $archive_name" ;;
esac
[ "${#expected_checksum}" -eq 64 ] || die "invalid SHA-256 checksum for $archive_name"

actual_checksum=$(sha256_file "$archive_path")
if [ "$actual_checksum" != "$expected_checksum" ]; then
    die "SHA-256 verification failed for $archive_name"
fi

entries_path="$temp_dir/archive-entries"
tar -tzf "$archive_path" >"$entries_path" || die "could not read $archive_name"
if ! awk '
    $0 == "omni" { omni += 1; next }
    $0 == "LICENSE" { license += 1; next }
    { unexpected += 1 }
    END { exit !(omni == 1 && license == 1 && unexpected == 0 && NR == 2) }
' "$entries_path"; then
    die "$archive_name has an unexpected layout"
fi

unpack_dir="$temp_dir/unpack"
mkdir -p "$unpack_dir"
tar -xzf "$archive_path" -C "$unpack_dir" || die "could not unpack $archive_name"
[ -f "$unpack_dir/omni" ] || die "$archive_name does not contain omni"
[ -f "$unpack_dir/LICENSE" ] || die "$archive_name does not contain LICENSE"
[ ! -L "$unpack_dir/omni" ] || die "$archive_name contains a symlinked omni"
[ ! -L "$unpack_dir/LICENSE" ] || die "$archive_name contains a symlinked LICENSE"

[ ! -d "$OMNI_INSTALL_DIR/omni" ] || die "$OMNI_INSTALL_DIR/omni is a directory"
mkdir -p "$OMNI_INSTALL_DIR"
temporary_binary=$(mktemp "$OMNI_INSTALL_DIR/.omni.XXXXXX") || die 'could not stage OmniSession binary'
install -m 0755 "$unpack_dir/omni" "$temporary_binary"

alias_directory="${OMNISESSION_HOME:-$HOME/.omnisession}/shims"
legacy_binary="$OMNI_INSTALL_DIR/omnis"
if is_old_omni "$legacy_binary" || cmp -s "$legacy_binary" "$temporary_binary"; then
    remove_owned_aliases "$legacy_binary" "$alias_directory"
    remove_owned_aliases "$legacy_binary" "$OMNI_INSTALL_DIR"
fi
mv -f "$temporary_binary" "$OMNI_INSTALL_DIR/omni"
temporary_binary=''
# A provider alias that resolves to the installed binary runs OmniSession. The in-app updater
# leaves these aliases in place, so do not require the previous binary to be an old build.
remove_owned_aliases "$OMNI_INSTALL_DIR/omni" "$alias_directory"
remove_owned_aliases "$OMNI_INSTALL_DIR/omni" "$OMNI_INSTALL_DIR"
if [ ! -L "$alias_directory" ]; then
    rmdir "$alias_directory" 2>/dev/null || :
fi
if is_old_omni "$legacy_binary"; then
    rm "$legacy_binary"
fi
if [ "${OMNI_NO_MODIFY_PATH:-}" != '1' ]; then
    for profile in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.profile"; do
        remove_old_path_block "$profile"
    done
fi
info "Installed omni${version:+ $version} to $OMNI_INSTALL_DIR/omni"
}
