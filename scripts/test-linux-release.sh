#!/usr/bin/env bash
# Run inside a fresh disposable Linux container, not on a user's desktop.
set -euo pipefail
version=${1:?Usage: test-linux-release.sh VERSION PACKAGES_DIR}
packages=${2:?Usage: test-linux-release.sh VERSION PACKAGES_DIR}
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
if ! command -v curl >/dev/null; then
    apt-get update
    apt-get install -y --no-install-recommends curl ca-certificates
fi
# shellcheck source=scripts/install-linux.sh
source "$repo_root/scripts/install-linux.sh"
download() { cp -- "$packages/${1##*/}" "$2"; }
main --version "$version" --yes
root=$(mktemp -d)
trap 'rm -rf -- "$root"' EXIT
for executable in "$HOME/.local/lib/mtui/mtui" "$HOME/.local/lib/mtui/mtui-sign-in"; do
    output=$(ldd "$executable")
    [[ $output != *'not found'* ]]
done
if "$HOME/.local/lib/mtui/mtui-sign-in" --invalid >"$root/helper.log" 2>&1; then exit 1; fi
grep 'unknown sign-in helper argument' "$root/helper.log"
test -f "$HOME/.local/share/applications/mtui.desktop"
test -f "$HOME/.local/share/icons/hicolor/scalable/apps/mtui.svg"
test -x "$HOME/.local/bin/mtui-uninstall"
for compiler in cargo rustc gcc; do
    if command -v "$compiler"; then printf 'Unexpected compiler: %s\n' "$compiler"; exit 1; fi
done

# Exercise the actual updater with /bin/true instead of starting playback or sign-in.
staging="$root/.mtui-update-probe"
bundle=MTUI-0.0.0-linux-x64
mkdir -p "$staging" "$root/$bundle"
cp "$HOME/.local/lib/mtui/mtui" "$root/mtui"
cp "$HOME/.local/lib/mtui/mtui-sign-in" "$root/mtui-sign-in"
printf 'unchanged settings\n' > "$root/settings.json"
cp /bin/true "$root/$bundle/mtui"
cp /bin/true "$root/$bundle/mtui-sign-in"
tar -czf "$root/$bundle.tar.gz" -C "$root" "$bundle"
cp /bin/true "$staging/mtui"
cp /bin/true "$staging/mtui-sign-in"
cp "$HOME/.local/lib/mtui/mtui" "$staging/update-helper"
digest=$(sha256sum "$root/$bundle.tar.gz")
digest=${digest%% *}
size=$(stat -c %s "$root/$bundle.tar.gz")
(
    printf '{"executable":"%s/mtui","payload":"%s/%s.tar.gz","digest":"sha256:%s","size":%s,"kind":"Linux","parent":%s}\n' \
        "$root" "$root" "$bundle" "$digest" "$size" "$BASHPID" > "$staging/pending-update.json"
    exec "$staging/update-helper" --mtui-update-helper "$staging/pending-update.json"
)
expected=$(sha256sum /bin/true)
expected=${expected%% *}
for executable in "$root/mtui" "$root/mtui-sign-in"; do
    actual=$(sha256sum "$executable")
    [[ ${actual%% *} == "$expected" ]]
done
grep '^unchanged settings$' "$root/settings.json"
printf 'Fresh distro install, sign-in loader, and real Linux updater smoke test passed.\n'
