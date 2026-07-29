#!/usr/bin/env sh
# Install the latest OmniSession release for the current platform.
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
            --connect-timeout 15 --proto '=https' --tlsv1.2 \
            --output "$download_output" "$download_url"; then
            return
        fi
        [ "$download_attempt" -eq 5 ] && break
        info "Download interrupted. Retrying ($download_attempt/5)..."
        download_attempt=$((download_attempt + 1))
        sleep 1
    done
    die "could not download $download_url"
}

configure_shim_path() {
    if [ "${OMNI_NO_MODIFY_PATH:-}" = '1' ]; then
        info 'Skipped shell PATH setup because OMNI_NO_MODIFY_PATH=1.'
        return
    fi

    shell_name=${SHELL:-}
    shell_name=${shell_name##*/}
    case "$shell_name" in
        zsh) profile="$HOME/.zshrc" ;;
        bash) profile="$HOME/.bashrc" ;;
        *) profile="$HOME/.profile" ;;
    esac

    path_marker='# >>> omnisession shims >>>'
    if [ -f "$profile" ] && grep -Fqx "$path_marker" "$profile"; then
        info "Shell PATH already configured in $profile"
        return
    fi

    {
        printf '\n%s\n' "$path_marker"
        # shellcheck disable=SC2016
        printf '%s\n' 'if [ -d "${OMNISESSION_HOME:-$HOME/.omnisession}/shims" ]; then'
        # shellcheck disable=SC2016
        printf '%s\n' '    export PATH="${OMNISESSION_HOME:-$HOME/.omnisession}/shims:$PATH"'
        printf '%s\n' 'fi'
        printf '%s\n' '# <<< omnisession shims <<<'
    } >>"$profile"
    info "Added provider shims to PATH in $profile"
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
cleanup() {
    rm -rf "$temp_dir"
}
trap cleanup EXIT HUP INT TERM

archive_name="omni-${platform}-${architecture}.tar.gz"
release_url="https://github.com/${repository}/releases/latest/download"
archive_path="$temp_dir/$archive_name"
checksums_path="$temp_dir/SHA256SUMS"

if [ ! -t 1 ]; then
    info "OmniSession installer: ${platform}/${architecture} -> ${OMNI_INSTALL_DIR}"
fi

info "Downloading OmniSession for ${platform}/${architecture}..."
download "$archive_path" "$release_url/$archive_name"
download "$checksums_path" "$release_url/SHA256SUMS"

checksum_count=$(awk -v filename="$archive_name" '$2 == filename { count += 1 } END { print count + 0 }' "$checksums_path")
[ "$checksum_count" -eq 1 ] || die "SHA256SUMS does not contain exactly one checksum for $archive_name"
expected_checksum=$(awk -v filename="$archive_name" '$2 == filename { print $1; exit }' "$checksums_path")
if ! awk -v checksum="$expected_checksum" 'BEGIN { exit !(checksum ~ /^[[:xdigit:]]{64}$/) }'; then
    die "invalid SHA-256 checksum for $archive_name"
fi

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

mkdir -p "$OMNI_INSTALL_DIR"
temporary_binary="$OMNI_INSTALL_DIR/.omni.$$"
install -m 0755 "$unpack_dir/omni" "$temporary_binary"
mv -f "$temporary_binary" "$OMNI_INSTALL_DIR/omni"

legacy_binary="$OMNI_INSTALL_DIR/omnis"
legacy_owned=0
if [ -f "$legacy_binary" ] && [ ! -L "$legacy_binary" ] && [ -x "$legacy_binary" ]; then
    legacy_version=$("$legacy_binary" --version 2>/dev/null || :)
    case "$legacy_version" in
        'omnis '*) legacy_owned=1 ;;
    esac
fi

if [ "$legacy_owned" -eq 1 ]; then
    info "Migrating provider shims from legacy omnis command..."
    "$legacy_binary" shim uninstall --bin-dir "$OMNI_INSTALL_DIR" || \
        die 'could not remove legacy OmniSession shims'
fi

info "Installing provider shims..."
if ! "$OMNI_INSTALL_DIR/omni" shim install --bin-dir "$OMNI_INSTALL_DIR"; then
    if [ "$legacy_owned" -eq 1 ]; then
        "$legacy_binary" shim install --bin-dir "$OMNI_INSTALL_DIR" >/dev/null 2>&1 || :
    fi
    die 'could not install provider shims'
fi
if [ "$legacy_owned" -eq 1 ]; then
    rm -f "$legacy_binary"
    info "Removed legacy OmniSession command $legacy_binary"
fi
configure_shim_path
info "Installed omni to $OMNI_INSTALL_DIR/omni"
