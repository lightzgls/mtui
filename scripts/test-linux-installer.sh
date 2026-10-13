#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
installer="$repo_root/scripts/install-linux.sh"

if [[ ${1:-} == worker ]]; then
    scenario=$2
    manager_name=$3
    export HOME="$TEST_ROOT/home $scenario-$manager_name"
    mkdir -p -- "$HOME"
    # shellcheck source=scripts/install-linux.sh
    source "$installer"
    uname() { if [[ $1 == -s ]]; then printf 'Linux\n'; else printf '%s\n' "${TEST_ARCH:-x86_64}"; fi; }
    getconf() { printf '%s\n' "${TEST_LIBC:-glibc 2.34}"; }
    detect_distribution() { distribution="Test $manager_name"; manager=$manager_name; }
    curl() {
        local output='' url=''
        while (( $# )); do
            case $1 in
                --output) output=$2; shift 2 ;;
                https://*) url=$1; shift ;;
                *) shift ;;
            esac
        done
        printf 'curl %s\n' "$url" >> "$HOME/actions"
        if [[ $url == */latest ]]; then
            printf 'https://github.com/lightzgls/mtui/releases/tag/v0.11.3'
        elif [[ $url == */SHA256SUMS ]]; then
            cp -- "$TEST_ROOT/SHA256SUMS" "$output"
            if [[ $scenario == checksum ]]; then printf '%064d  MTUI-0.11.3-linux-x64.tar.gz\n' 0 > "$output"; fi
        else
            [[ $scenario != download-failure ]] || return 22
            cp -- "$TEST_ROOT/MTUI-0.11.3-linux-x64.tar.gz" "$output"
        fi
    }
    ldd() {
        if [[ $scenario == ready || -f $HOME/dependencies ]]; then
            printf 'libc.so.6 => /lib/libc.so.6\n'
        else
            printf 'libwebkit2gtk-4.1.so.0 => not found\n'
        fi
    }
    # These mocks are called indirectly through the installer's command array.
    # shellcheck disable=SC2317
    sudo() { "$@"; }
    # shellcheck disable=SC2317
    record_package_manager() {
        printf '%s\n' "$*" >> "$HOME/actions"
        [[ $scenario != dependency-failure ]] || return 1
        if [[ $* != 'apt-get update' ]]; then touch "$HOME/dependencies"; fi
    }
    # shellcheck disable=SC2317
    apt-get() { record_package_manager apt-get "$@"; }
    apt-cache() { [[ $scenario != old-debian ]]; }
    # shellcheck disable=SC2317
    dnf() { record_package_manager dnf "$@"; }
    # shellcheck disable=SC2317
    pacman() { record_package_manager pacman "$@"; }
    # shellcheck disable=SC2317
    zypper() { record_package_manager zypper "$@"; }
    if [[ $scenario == conflict ]]; then
        mkdir -p "$HOME/.local/bin"
        printf 'existing installation\n' > "$HOME/.local/bin/mtui"
    fi
    options=(--yes)
    [[ $scenario != latest ]] && options+=(--version 0.11.3)
    [[ $scenario != skip ]] || options+=(--skip-deps)
    main "${options[@]}"
    exit
fi

TEST_ROOT=$(mktemp -d)
export TEST_ROOT
trap 'rm -rf -- "$TEST_ROOT"' EXIT
bundle=MTUI-0.11.3-linux-x64
mkdir -p "$TEST_ROOT/$bundle"
printf '#!/bin/sh\nexit 0\n' > "$TEST_ROOT/$bundle/mtui"
cp "$TEST_ROOT/$bundle/mtui" "$TEST_ROOT/$bundle/mtui-sign-in"
cp "$repo_root/installer/linux/mtui.desktop" "$TEST_ROOT/$bundle/mtui.desktop"
cp "$repo_root/assets/mtui-icon.svg" "$TEST_ROOT/$bundle/mtui.svg"
cp "$repo_root/LICENSE" "$TEST_ROOT/$bundle/LICENSE"
tar -czf "$TEST_ROOT/$bundle.tar.gz" -C "$TEST_ROOT" "$bundle"
(cd "$TEST_ROOT" && sha256sum "$bundle.tar.gz" > SHA256SUMS)

passed=0
run_success() {
    local scenario=$1 manager=$2
    bash "$0" worker "$scenario" "$manager" > "$TEST_ROOT/output" 2>&1 || { cat "$TEST_ROOT/output"; exit 1; }
    local home="$TEST_ROOT/home $scenario-$manager"
    cmp "$TEST_ROOT/$bundle/mtui" "$home/.local/lib/mtui/mtui"
    cmp "$TEST_ROOT/$bundle/mtui-sign-in" "$home/.local/lib/mtui/mtui-sign-in"
    [[ -x $home/.local/bin/mtui && -x $home/.local/bin/mtui-sign-in ]]
    grep -F "Exec=\"$home/.local/lib/mtui/mtui\"" "$home/.local/share/applications/mtui.desktop" >/dev/null
    if [[ $scenario == ready ]]; then
        if grep -E '^(apt-get|dnf|pacman|zypper) ' "$home/actions"; then exit 1; fi
    else
        grep -E "^$manager " "$home/actions" >/dev/null
    fi
    case $manager in
        apt-get)
            if [[ $scenario == old-debian ]]; then grep 'libasound2 libssl3 ' "$home/actions" >/dev/null
            else grep 'libasound2t64 libssl3t64 ' "$home/actions" >/dev/null; fi ;;
        pacman)
            grep '^pacman -Syu --needed --noconfirm ' "$home/actions" >/dev/null
            if grep '^pacman -Sy ' "$home/actions"; then exit 1; fi ;;
    esac
    passed=$((passed + 1))
}

run_failure() {
    local scenario=$1 manager=$2
    if bash "$0" worker "$scenario" "$manager" > "$TEST_ROOT/output" 2>&1; then
        printf 'Expected failure for %s/%s\n' "$scenario" "$manager" >&2
        exit 1
    fi
    local home="$TEST_ROOT/home $scenario-$manager"
    [[ ! -e $home/.local/lib/mtui ]]
    if [[ $scenario == conflict ]]; then grep 'existing installation' "$home/.local/bin/mtui" >/dev/null; fi
    passed=$((passed + 1))
}

for manager in apt-get dnf pacman zypper; do run_success install "$manager"; done
run_success old-debian apt-get
run_success latest dnf
run_success ready unknown
run_failure skip dnf
run_failure checksum dnf
run_failure download-failure dnf
run_failure dependency-failure dnf
run_failure conflict dnf
run_failure install unknown
TEST_ARCH=aarch64 run_failure architecture dnf
TEST_LIBC='musl 1.2.5' run_failure libc dnf

# Distribution families and exact IDs are checked without touching the host.
# shellcheck source=scripts/install-linux.sh
source "$installer"
for pair in 'ubuntu:apt-get' 'debian:apt-get' 'fedora:dnf' 'arch:pacman' 'manjaro:pacman' 'opensuse-tumbleweed:zypper'; do
    printf 'ID=%s\nPRETTY_NAME="Test distro"\n' "${pair%%:*}" > "$TEST_ROOT/os-release"
    detect_distribution "$TEST_ROOT/os-release"
    [[ $manager == "${pair##*:}" ]]
done
printf 'ID=linuxmint\nID_LIKE="ubuntu debian"\n' > "$TEST_ROOT/os-release"
detect_distribution "$TEST_ROOT/os-release"
[[ $manager == apt-get ]]
printf 'ID=custom\nID_LIKE="arch"\n' > "$TEST_ROOT/os-release"
detect_distribution "$TEST_ROOT/os-release"
[[ $manager == pacman ]]
# shellcheck disable=SC2016
[[ $(desktop_command '/home/a $b%/mtui') == '"/home/a \\$b%%/mtui"' ]]

# Deliberately exercise stdin as used by curl | bash.
# shellcheck disable=SC2002
cat "$installer" | bash -s -- --help > "$TEST_ROOT/help"
grep 'Install prebuilt MTUI with sign-in' "$TEST_ROOT/help" >/dev/null
printf 'Linux installer: %s installation/failure scenarios and distribution/pipe checks passed.\n' "$passed"
