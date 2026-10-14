#!/usr/bin/env bash
set -euo pipefail
umask 022

fail() { printf 'MTUI: %s\n' "$*" >&2; exit 1; }

usage() {
    printf '%s\n' \
        'Install prebuilt MTUI with sign-in for the current Linux user.' \
        'Usage: bash install.sh [--version X.Y.Z] [--bundle DIRECTORY | --build-dir DIRECTORY] [--yes] [--skip-deps]' \
        '  --version    Install a specific stable release (default: latest).' \
        '  --yes        Accept package-manager confirmations (including Arch system updates).' \
        '  --skip-deps  Do not install system libraries; they must already exist.' \
        '  --bundle     Install an already-extracted release archive without downloading it.' \
        '  --build-dir  Install binaries built from this source checkout (any Linux architecture/libc).' \
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

# Shared by the curl installer and both Linux packages. Export these functions
# into a standalone, offline uninstaller, including when installing old archives.
uninstall_usage() {
    printf '%s\n' 'Remove MTUI and ALL its local data, including sign-in sessions and downloaded tools.' \
        'Usage: mtui-uninstall' 'Close MTUI first. Run as your normal user.'
}

remove_system_package() {
    local removal=()
    if command -v dpkg-query >/dev/null && [[ $(dpkg-query -W -f='${db:Status-Status}' mtui 2>/dev/null || true) == installed ]]; then
        removal=(apt-get purge mtui)
    elif command -v pacman >/dev/null && pacman -Q mtui >/dev/null 2>&1; then
        removal=(pacman -R mtui)
    elif command -v rpm >/dev/null && rpm -q mtui >/dev/null 2>&1; then
        if command -v dnf >/dev/null; then removal=(dnf remove mtui)
        elif command -v zypper >/dev/null; then removal=(zypper remove mtui)
        else removal=(rpm -e mtui); fi
    elif command -v apk >/dev/null && apk info --exists mtui >/dev/null 2>&1; then
        removal=(apk del mtui)
    fi
    # Per-user/archive installations use no package manager on any distribution.
    (( ${#removal[@]} )) || return 0
    if (( EUID == 0 )); then "${removal[@]}"
    else sudo "${removal[@]}"; fi
}

remove_path_block() {
    local file=$1 temporary
    [[ -f $file ]] || return 0
    grep -Fxq '# >>> MTUI PATH >>>' "$file" || return 0
    grep -Fxq '# <<< MTUI PATH <<<' "$file" || return 0
    temporary=$(mktemp)
    awk '
        $0 == "# >>> MTUI PATH >>>" { owned=1; next }
        $0 == "# <<< MTUI PATH <<<" { owned=0; next }
        !owned { print }
    ' "$file" > "$temporary"
    cat -- "$temporary" > "$file"
    rm -f -- "$temporary"
}

mtui_data_dirs() {
    local base fallback index
    local bases=("${XDG_CONFIG_HOME:-}" "${XDG_CACHE_HOME:-}" "${XDG_DATA_HOME:-}" "${XDG_STATE_HOME:-}")
    local defaults=("$HOME/.config" "$HOME/.cache" "$HOME/.local/share" "$HOME/.local/state")
    for index in "${!bases[@]}"; do
        base=${bases[index]}; fallback=${defaults[index]}
        [[ $base == /* ]] || base=$fallback
        printf '%s/mtui\n' "${base%/}"
        printf '%s/mtui\n' "$fallback"
    done
}

uninstall_main() {
    if (( $# )); then
        case $1 in --help|-h) uninstall_usage; return ;; *) printf 'Unknown option: %s\n' "$1" >&2; return 1 ;; esac
    fi
    [[ ${HOME:-} == /* && $HOME != / ]] || { printf 'HOME must be an absolute user directory.\n' >&2; return 1; }
    local root="$HOME/.local/lib/mtui" bin="$HOME/.local/bin" data tool file directory
    if command -v pgrep >/dev/null && pgrep -u "$(id -u)" -x 'mtui|mtui-sign-in' >/dev/null; then
        printf 'Close MTUI and its sign-in window before uninstalling.\n' >&2
        return 1
    fi
    # Native packages use the same command. Remove the package before user data
    # so cancelling sudo or APT does not leave a half-uninstalled application.
    remove_system_package
    for tool in mtui mtui-sign-in mtui-uninstall; do
        if [[ -L $bin/$tool && $(readlink -- "$bin/$tool") == "$root/$tool" ]]; then
            rm -f -- "$bin/$tool"
        fi
    done
    if [[ -f $root/.shell-path-files ]]; then
        while IFS= read -r file; do remove_path_block "$file"; done < "$root/.shell-path-files"
    fi
    if [[ -f $root/.created-shell-files ]]; then
        while IFS= read -r file; do
            if [[ -f $file ]] && ! grep -q '[^[:space:]]' "$file"; then rm -f -- "$file"; fi
        done < "$root/.created-shell-files"
    fi
    data=${XDG_DATA_HOME:-$HOME/.local/share}
    [[ $data == /* ]] || data="$HOME/.local/share"
    local desktop_bases="$data"$'\n'"$HOME/.local/share"
    if [[ -f $root/.desktop-data-dirs ]]; then desktop_bases+=$'\n'$(cat -- "$root/.desktop-data-dirs"); fi
    if [[ -f $root/.installed-by-mtui ]]; then
        IFS= read -r data < "$root/.installed-by-mtui"
        desktop_bases+=$'\n'"$data"
    fi
    while IFS= read -r data; do
        [[ $data == /* && $data != / ]] || { printf 'Invalid MTUI installation record.\n' >&2; return 1; }
        rm -f -- "$data/applications/mtui.desktop" "$data/icons/hicolor/scalable/apps/mtui.svg"
    done <<< "$desktop_bases"
    # Use only MTUI directories; never remove a shared XDG base directory or
    # tools installed independently through the system package manager.
    local directories
    directories=$(mtui_data_dirs)
    if [[ -f $root/.data-dirs ]]; then directories+=$'\n'$(cat -- "$root/.data-dirs"); fi
    while IFS= read -r directory; do
        [[ $directory == /*/mtui && $directory != *'/../'* && $directory != *'/./'* ]] || { printf 'Invalid MTUI data path: %s\n' "$directory" >&2; return 1; }
        rm -rf -- "$directory"
    done <<< "$directories"
    rm -rf -- "$root"
    printf 'MTUI uninstalled. All MTUI settings, sessions, history, logs, and downloaded tools were removed.\n'
}

write_uninstaller() {
    {
        printf '#!/usr/bin/env bash\nset -euo pipefail\n'
        declare -f uninstall_usage remove_system_package remove_path_block mtui_data_dirs uninstall_main
        printf '\nuninstall_main "$@"\n'
    } > "$1"
    chmod 755 "$1"
}

configure_shell_path() {
    local file directory
    local zsh_home=${ZDOTDIR:-$HOME} config=${XDG_CONFIG_HOME:-$HOME/.config}
    [[ $zsh_home == /* ]] || zsh_home=$HOME
    [[ $config == /* ]] || config="$HOME/.config"
    mkdir -p -- "$zsh_home"
    local files=("$HOME/.profile" "$HOME/.bashrc" "$zsh_home/.zprofile" "$zsh_home/.zshrc")
    for file in "$HOME/.bash_profile" "$HOME/.bash_login"; do
        [[ ! -f $file ]] || files+=("$file")
    done
    for file in "${files[@]}"; do
        [[ -e $file ]] || printf '%s\n' "$file" >> "$root/.created-shell-files"
        if ! grep -Fxq '# >>> MTUI PATH >>>' "$file" 2>/dev/null; then
            cat >> "$file" <<'EOF'

# >>> MTUI PATH >>>
case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) export PATH="$HOME/.local/bin:$PATH" ;;
esac
# <<< MTUI PATH <<<
EOF
        fi
        printf '%s\n' "$file" >> "$root/.shell-path-files"
    done
    directory="$config/fish/conf.d"
    mkdir -p -- "$directory"
    file="$directory/mtui-path.fish"
    [[ -e $file ]] || printf '%s\n' "$file" >> "$root/.created-shell-files"
    if ! grep -Fxq '# >>> MTUI PATH >>>' "$file" 2>/dev/null; then
        cat >> "$file" <<'EOF'

# >>> MTUI PATH >>>
if not contains -- "$HOME/.local/bin" $PATH
    set -gx PATH "$HOME/.local/bin" $PATH
end
# <<< MTUI PATH <<<
EOF
    fi
    printf '%s\n' "$file" >> "$root/.shell-path-files"
    # Retain old locations on reinstall, so uninstall can also clean those.
    sort -u -o "$root/.shell-path-files" "$root/.shell-path-files"
}

main() {
    local release_version='' bundle_directory='' build_directory='' source_root='' assume_yes=0 skip_deps=0
    while (( $# )); do
        case $1 in
            --version) (( $# >= 2 )) || fail '--version needs X.Y.Z.'; release_version=$2; shift 2 ;;
            --bundle) (( $# >= 2 )) || fail '--bundle needs an extracted release directory.'; bundle_directory=$2; shift 2 ;;
            --build-dir) (( $# >= 2 )) || fail '--build-dir needs a source-build directory.'; build_directory=$2; shift 2 ;;
            --yes) assume_yes=1; shift ;;
            --skip-deps) skip_deps=1; shift ;;
            --help|-h) usage; return ;;
            *) fail "Unknown option: $1" ;;
        esac
    done
    [[ $(uname -s) == Linux ]] || fail 'This installer is for Linux.'
    [[ -z $bundle_directory || -z $build_directory ]] || fail 'Choose either --bundle or --build-dir.'
    # Local builds already target their host; only prebuilt archives need this gate.
    if [[ -z $build_directory ]]; then
        [[ $(uname -m) == x86_64 ]] || fail 'Prebuilt releases currently require x86-64; ARM needs a source build.'
        local libc
        libc=$(getconf GNU_LIBC_VERSION 2>/dev/null) || fail 'This binary requires glibc; Alpine/musl needs a separate build.'
        [[ $libc =~ ^glibc\ ([0-9]+)\.([0-9]+)$ ]] || fail 'Could not identify glibc.'
        (( BASH_REMATCH[1] > 2 || (BASH_REMATCH[1] == 2 && BASH_REMATCH[2] >= 34) )) || fail 'glibc 2.34 or newer is required.'
    fi
    [[ ${HOME:-} == /* && $HOME != *$'\n'* && $HOME != *'='* ]] || fail 'HOME must be an absolute path suitable for a desktop launcher.'
    local tool
    for tool in tar sha256sum ldd install mktemp; do
        command -v "$tool" >/dev/null || fail "Missing $tool; install it with your distribution's package manager first."
    done
    if [[ -n $build_directory ]]; then
        build_directory=$(cd -- "$build_directory" && pwd)
        [[ -n ${BASH_SOURCE[0]:-} ]] || fail '--build-dir must run from scripts/install-linux.sh in a source checkout.'
        source_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
        [[ -f $source_root/Cargo.toml ]] || fail '--build-dir needs the MTUI source checkout.'
        local source_version
        source_version=$(awk -F '"' '/^version = / { print $2; exit }' "$source_root/Cargo.toml")
        [[ -z $release_version || $release_version == "$source_version" ]] || fail '--version does not match the source build.'
        release_version=$source_version
    elif [[ -n $bundle_directory ]]; then
        bundle_directory=$(cd -- "$bundle_directory" && pwd)
        local directory_name=${bundle_directory##*/}
        [[ $directory_name =~ ^MTUI-([0-9]+\.[0-9]+\.[0-9]+)-linux-x64$ ]] || fail 'Expected an extracted MTUI-X.Y.Z-linux-x64 release directory.'
        [[ -z $release_version || $release_version == "${BASH_REMATCH[1]}" ]] || fail '--version does not match the extracted bundle.'
        release_version=${BASH_REMATCH[1]}
    else
        command -v curl >/dev/null || fail 'Missing curl; install it with your distribution package manager first.'
    fi
    local distribution manager
    detect_distribution /etc/os-release
    local root="$HOME/.local/lib/mtui" bin="$HOME/.local/bin"
    for tool in mtui mtui-sign-in mtui-uninstall; do
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
    local extracted
    if [[ -n $build_directory ]]; then
        extracted="$staging/source-build"
        mkdir -- "$extracted"
        for tool in mtui mtui-sign-in; do
            [[ -f $build_directory/$tool && ! -L $build_directory/$tool ]] || fail "Missing regular source-built $tool executable."
            install -m755 "$build_directory/$tool" "$extracted/$tool"
        done
        install -m644 "$source_root/installer/linux/mtui.desktop" "$extracted/mtui.desktop"
        install -m644 "$source_root/assets/mtui-icon.svg" "$extracted/mtui.svg"
        install -m644 "$source_root/LICENSE" "$extracted/LICENSE"
    elif [[ -n $bundle_directory ]]; then
        extracted=$bundle_directory
    else
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
        extracted="$staging/extracted/$bundle"
    fi
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
    write_uninstaller "$root/mtui-uninstall"
    for tool in mtui mtui-sign-in mtui-uninstall; do
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
    printf '%s\n' "$data" > "$root/.installed-by-mtui"
    printf '%s\n' "$data" >> "$root/.desktop-data-dirs"
    sort -u -o "$root/.desktop-data-dirs" "$root/.desktop-data-dirs"
    mtui_data_dirs >> "$root/.data-dirs"
    sort -u -o "$root/.data-dirs" "$root/.data-dirs"
    configure_shell_path
    rm -rf -- "$staging"
    trap - EXIT
    printf 'MTUI installed. Open a new terminal and type: mtui\n'
    if [[ $(command -v mtui || true) != "$bin/mtui" ]]; then
        # shellcheck disable=SC2016
        printf 'To use this terminal immediately, run: export PATH="$HOME/.local/bin:$PATH"\n'
    fi
    printf 'To uninstall later, close MTUI and type: mtui-uninstall\n'
    printf 'Use Settings > Updates for future updates. Restart any running copy first.\n'
}

if [[ -z ${BASH_SOURCE[0]:-} || ${BASH_SOURCE[0]} == "$0" ]]; then
    main "$@"
fi
