#!/usr/bin/env bash
set -euo pipefail
umask 022

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
version=${1:?Usage: bash scripts/package-linux.sh VERSION [BUILD_DIR] [OUTPUT_DIR]}
build_dir=${2:-"$repo_root/target/release"}
output_dir=${3:-"$repo_root/target/packages"}

if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    printf 'Expected a stable release version, got: %s\n' "$version" >&2
    exit 1
fi
if [[ $(dpkg --print-architecture) != amd64 ]]; then
    printf 'Linux releases currently support x86-64 only.\n' >&2
    exit 1
fi
for binary in mtui mtui-sign-in; do
    if [[ ! -x "$build_dir/$binary" ]]; then
        printf 'Missing executable: %s/%s; build with cargo build --release --locked --bins.\n' "$build_dir" "$binary" >&2
        exit 1
    fi
done

build_dir=$(cd -- "$build_dir" && pwd)
mkdir -p -- "$output_dir"
output_dir=$(cd -- "$output_dir" && pwd)
staging=$(mktemp -d)
trap 'rm -rf -- "$staging"' EXIT
package_root="$staging/debian/mtui"
bundle="MTUI-$version-linux-x64"
portable_root="$staging/$bundle"

install -Dm755 "$build_dir/mtui" "$package_root/usr/bin/mtui"
install -Dm755 "$build_dir/mtui-sign-in" "$package_root/usr/bin/mtui-sign-in"
install -Dm644 "$repo_root/installer/linux/mtui.desktop" "$package_root/usr/share/applications/mtui.desktop"
install -Dm644 "$repo_root/assets/mtui-icon.svg" "$package_root/usr/share/icons/hicolor/scalable/apps/mtui.svg"
install -Dm644 "$repo_root/LICENSE" "$package_root/usr/share/doc/mtui/copyright"
install -Dm644 "$repo_root/README.md" "$package_root/usr/share/doc/mtui/README.md"
install -Dm644 "$repo_root/docs/usage.md" "$package_root/usr/share/doc/mtui/docs/usage.md"

# Scan both binaries so installing the player also installs sign-in's WebKit runtime.
mkdir -p -- "$package_root/DEBIAN"
install -Dm644 "$repo_root/installer/linux/control" "$staging/debian/control"
dependencies=$(cd -- "$staging" && dpkg-shlibdeps -O \
    -edebian/mtui/usr/bin/mtui -edebian/mtui/usr/bin/mtui-sign-in)
dependencies=${dependencies#shlibs:Depends=}
if [[ $dependencies != *libwebkit2gtk-4.1-0* ]]; then
    printf 'The sign-in helper did not declare its WebKitGTK runtime dependency.\n' >&2
    exit 1
fi
installed_size=$(du -sk "$package_root/usr" | cut -f1)
cat > "$package_root/DEBIAN/control" <<EOF
Package: mtui
Version: $version
Architecture: amd64
Section: sound
Priority: optional
Maintainer: Lightzgls <lightzgls@users.noreply.github.com>
Homepage: https://github.com/lightzgls/mtui
Installed-Size: $installed_size
Depends: $dependencies, ca-certificates
Description: Terminal music player for YouTube Music
 Includes native audio playback and the YouTube Music sign-in helper.
EOF
dpkg-deb --root-owner-group -Zxz --build "$package_root" "$output_dir/$bundle.deb"

install -Dm755 "$build_dir/mtui" "$portable_root/mtui"
install -Dm755 "$build_dir/mtui-sign-in" "$portable_root/mtui-sign-in"
install -Dm644 "$repo_root/LICENSE" "$portable_root/LICENSE"
install -Dm644 "$repo_root/README.md" "$portable_root/README.md"
install -Dm644 "$repo_root/docs/usage.md" "$portable_root/docs/usage.md"
tar --owner=0 --group=0 --numeric-owner -czf "$output_dir/$bundle.tar.gz" -C "$staging" "$bundle"

printf 'Runtime dependencies: %s\n' "$dependencies"
stat --printf='%n: %s bytes\n' "$output_dir/$bundle.deb" "$output_dir/$bundle.tar.gz"
