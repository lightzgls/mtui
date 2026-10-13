#!/usr/bin/env bash
set -euo pipefail
umask 022

fail() { printf 'MTUI: %s\n' "$*" >&2; exit 1; }

usage() {
    printf '%s\n' \
        'Install prebuilt MTUI with sign-in for the current Linux user.' \
        'Usage: bash install.sh [--version X.Y.Z] [--yes] [--skip-deps]' \
        '  --version    Install a specific stable release (default: latest).' \
        '  --yes        Accept package-manager confirmations (including Arch system updates).' \
        '  --skip-deps  Do not install system libraries; they must already exist.' \
        'Run without sudo. Only missing system libraries require administrator access.'
}

detect_distribution() {
    local release_file=${1:-/etc/os-release}
    local ID=unknown ID_LIKE='' PRETTY_NAME=Linux
    if [[ -r $release_file ]]; then
        # os-release is a root-owned file with shell-compatible values.
        # shellcheck disable=SC1090
        source "$release_file"
    fi
    distribution=$PRETTY_NAME
    manager=unknown
    local family
    for family in "$ID" $ID_LIKE; do
        case $family in
            debian|ubuntu) manager=apt-get; break ;;
            fedora) manager=dnf; break ;;
            arch|manjaro) manager=pacman; break ;;
            opensuse*|suse) manager=zypper; break ;;
        esac
    done
}

download() {
    curl --fail --silent --show-error --location --retry 3 \
        --connect-timeout 15 --max-time 180 --proto '=https' --proto-redir '=https' \
        --output "$2" "$1"
}

privileged() {
    local command=("$@")
    if (( EUID != 0 )); then
        command -v sudo >/dev/null || fail 'sudo is needed to install missing system libraries.'
        command=(sudo "${command[@]}")
    fi
    # Never let a package manager consume the script arriving through curl's pipe.
    if [[ -r /dev/tty ]] && ( : </dev/tty ) 2>/dev/null; then
        "${command[@]}" </dev/tty
    elif (( assume_yes )); then
        "${command[@]}" </dev/null
    else
        fail 'No interactive terminal. Rerun with --yes to accept dependency installation.'
    fi
}

install_dependencies() {
    [[ ! -e /run/ostree-booted ]] || fail 'Missing libraries on an immutable desktop. Install inside Distrobox, or provide WebKitGTK 4.1 on the host and rerun.'
    local confirm=()
    (( assume_yes )) && confirm=(-y)
    case $manager in
        apt-get)
            local alsa=libasound2 ssl=libssl3
            privileged apt-get update
            if apt-cache show libasound2t64 >/dev/null 2>&1; then alsa=libasound2t64; fi
            if apt-cache show libssl3t64 >/dev/null 2>&1; then ssl=libssl3t64; fi
            privileged apt-get install "${confirm[@]}" --no-install-recommends "$alsa" "$ssl" libwebkit2gtk-4.1-0 ca-certificates
            ;;
        dnf)
            privileged dnf install "${confirm[@]}" --setopt=install_weak_deps=False alsa-lib openssl-libs gtk3 webkit2gtk4.1 dbus-libs libgcc ca-certificates
            ;;
        pacman)
            confirm=()
            (( assume_yes )) && confirm=(--noconfirm)
            printf 'Arch: missing libraries require a full system update to avoid a partial upgrade. Review the transaction before confirming.\n'
            privileged pacman -Syu --needed "${confirm[@]}" alsa-lib openssl gtk3 webkit2gtk-4.1 dbus ca-certificates
            ;;
        zypper)
            confirm=()
            (( assume_yes )) && confirm=(--non-interactive)
            privileged zypper "${confirm[@]}" install --no-recommends alsa libopenssl3 libgtk-3-0 libwebkit2gtk-4_1-0 libdbus-1-3 libgcc_s1 ca-certificates
            ;;
        *) fail 'Cannot install libraries for this distribution automatically. Provide ALSA, OpenSSL 3, GTK 3, and WebKitGTK 4.1, then rerun.' ;;
    esac
}

runtime_ready() {
    local binary output
    for binary in mtui mtui-sign-in; do
        output=$(LC_ALL=C ldd "$1/$binary" 2>&1) || return 1
        [[ $output != *'not found'* ]] || return 1
    done
}

desktop_command() {
    local value=$1
    value=${value//\\/\\\\\\\\}
    value=${value//\"/\\\\\"}
    value=${value//\$/\\\\\$}
    value=${value//\`/\\\\\`}
    value=${value//%/%%}
    printf '"%s"' "$value"
}

main() {
    local release_version='' assume_yes=0 skip_deps=0
    while (( $# )); do
        case $1 in
            --version) (( $# >= 2 )) || fail '--version needs X.Y.Z.'; release_version=$2; shift 2 ;;
            --yes) assume_yes=1; shift ;;
            --skip-deps) skip_deps=1; shift ;;
            --help|-h) usage; return ;;
            *) fail "Unknown option: $1" ;;
        esac
    done
    [[ $(uname -s) == Linux ]] || fail 'This installer is for Linux.'
    [[ $(uname -m) == x86_64 ]] || fail 'Prebuilt releases currently require x86-64; ARM needs a source build.'
    local libc
    libc=$(getconf GNU_LIBC_VERSION 2>/dev/null) || fail 'This binary requires glibc; Alpine/musl needs a separate build.'
    [[ $libc =~ ^glibc\ ([0-9]+)\.([0-9]+)$ ]] || fail 'Could not identify glibc.'
    (( BASH_REMATCH[1] > 2 || (BASH_REMATCH[1] == 2 && BASH_REMATCH[2] >= 34) )) || fail 'glibc 2.34 or newer is required.'
    [[ ${HOME:-} == /* && $HOME != *$'\n'* && $HOME != *'='* ]] || fail 'HOME must be an absolute path suitable for a desktop launcher.'
    local tool
    for tool in curl tar sha256sum ldd install mktemp; do
        command -v "$tool" >/dev/null || fail "Missing $tool; install it with your distribution's package manager first."
    done
    local distribution manager
    detect_distribution /etc/os-release
    local root="$HOME/.local/lib/mtui" bin="$HOME/.local/bin"
    for tool in mtui mtui-sign-in; do
        if [[ -e $bin/$tool || -L $bin/$tool ]]; then
            [[ -L $bin/$tool && $(readlink -- "$bin/$tool") == "$root/$tool" ]] || fail "Existing $bin/$tool belongs to another installation. Move it aside before installing."
        fi
    done
    if [[ -z $release_version ]]; then
        local latest_url
        latest_url=$(curl --fail --silent --show-error --head --location --retry 3 \
            --connect-timeout 15 --max-time 60 --proto '=https' --proto-redir '=https' \
            --output /dev/null --write-out '%{url_effective}' https://github.com/lightzgls/mtui/releases/latest)
        [[ $latest_url == https://github.com/lightzgls/mtui/releases/tag/v* ]] || fail 'Unexpected latest-release address.'
        release_version=${latest_url##*/v}
    fi
    [[ $release_version =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || fail 'Only stable X.Y.Z release versions are supported.'
    printf 'Installing MTUI %s with sign-in on %s.\nLocation: %s\n' "$release_version" "$distribution" "$root"
    local staging bundle url expected='' matches=0 hash filename extra
    staging=$(mktemp -d)
    # An EXIT trap also covers failed downloads and package-manager cancellation.
    trap 'rm -rf -- "$staging"' EXIT
    bundle="MTUI-$release_version-linux-x64"
    url="https://github.com/lightzgls/mtui/releases/download/v$release_version"
    download "$url/SHA256SUMS" "$staging/SHA256SUMS"
    while read -r hash filename extra; do
        if [[ $filename == "$bundle.tar.gz" && -z $extra ]]; then
            expected=$hash
            matches=$((matches + 1))
        fi
    done < "$staging/SHA256SUMS"
    (( matches == 1 )) || fail 'The release has no unique Linux archive checksum.'
    [[ $expected =~ ^[a-fA-F0-9]{64}$ ]] || fail 'Invalid archive checksum.'
    download "$url/$bundle.tar.gz" "$staging/$bundle.tar.gz"
    printf '%s  %s\n' "$expected" "$staging/$bundle.tar.gz" | sha256sum --check --status || fail 'Download checksum mismatch; nothing was installed.'
    mkdir -- "$staging/extracted"
    tar -xzf "$staging/$bundle.tar.gz" --no-same-owner --no-same-permissions -C "$staging/extracted" \
        "$bundle/mtui" "$bundle/mtui-sign-in" "$bundle/mtui.desktop" "$bundle/mtui.svg" "$bundle/LICENSE"
    local extracted="$staging/extracted/$bundle"
    for tool in mtui mtui-sign-in; do
        [[ -f $extracted/$tool && ! -L $extracted/$tool ]] || fail "The archive is missing a regular $tool executable."
    done
    if ! runtime_ready "$extracted"; then
        (( ! skip_deps )) || fail 'Required runtime libraries are missing; rerun without --skip-deps.'
        printf 'Installing missing playback and sign-in runtime libraries with %s.\n' "$manager"
        install_dependencies
    fi
    runtime_ready "$extracted" || fail 'The system libraries cannot run this release. Your existing installation was not changed.'
    mkdir -p -- "$root" "$bin"
    for tool in mtui mtui-sign-in; do
        install -m755 "$extracted/$tool" "$root/.$tool.new"
    done
    # Rename instead of truncating a binary which might still be running.
    mv -f -- "$root/.mtui-sign-in.new" "$root/mtui-sign-in"
    mv -f -- "$root/.mtui.new" "$root/mtui"
    install -m644 "$extracted/LICENSE" "$root/LICENSE"
    for tool in mtui mtui-sign-in; do
        ln -sfn -- "$root/$tool" "$bin/$tool"
    done
    local data="${XDG_DATA_HOME:-$HOME/.local/share}"
    [[ $data == /* ]] || data="$HOME/.local/share"
    install -Dm644 "$extracted/mtui.svg" "$data/icons/hicolor/scalable/apps/mtui.svg"
    mkdir -p -- "$data/applications"
    while IFS= read -r line; do
        if [[ $line == Exec=* ]]; then
            printf 'Exec=%s\n' "$(desktop_command "$root/mtui")"
        else
            printf '%s\n' "$line"
        fi
    done < "$extracted/mtui.desktop" > "$data/applications/mtui.desktop"
    rm -rf -- "$staging"
    trap - EXIT
    printf 'MTUI installed. Open MTUI from the application menu, or run:\n'
    if [[ $(command -v mtui || true) == "$bin/mtui" ]]; then printf '  mtui\n'; else printf '  %q\n' "$bin/mtui"; fi
    printf 'Use Settings > Updates for future updates. Restart any running copy first.\n'
}

if [[ -z ${BASH_SOURCE[0]:-} || ${BASH_SOURCE[0]} == "$0" ]]; then
    main "$@"
fi
