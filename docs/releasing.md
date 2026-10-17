# Building and Publishing Releases

[Back to the README](../README.md) · [Installation guide](usage.md) · [Testing guide](testing.md)

## Release Contents

| Asset | Contents / purpose |
|---|---|
| `MTUI-<version>-Setup-x64.exe` | Per-user Windows installer with the player and integrated sign-in |
| `MTUI-<version>-windows-x64-portable.exe` | The same Windows player as a single portable executable |
| `MTUI-<version>-linux-x64.deb` | Debian/Ubuntu package with player, sign-in helper, uninstall command, runtime dependencies, launcher, icon, and guides |
| `MTUI-<version>-linux-x64.tar.gz` | Linux x64 archive with both executables, installer, uninstall command, launcher, icon, license, and guides |
| `install.sh` | Per-user Linux installer with checksum validation and distribution-specific runtime setup |
| `SHA256SUMS` | SHA-256 checksums for the five assets above |

Windows releases use MSVC, statically link the Visual C++ runtime, and must not
import `WebView2Loader.dll`, `VCRUNTIME*.dll`, or `MSVCP*.dll`. WebView2 itself
is still supplied by Windows. Linux releases are built on Ubuntu 22.04 and
require x86-64/glibc 2.34+ and compatible shared runtimes. macOS, ARM Linux,
and musl binaries are not currently published.

## Prepare a New Version

1. Update only MTUI's version in `Cargo.toml` and its own package entry in
   `Cargo.lock`. Do not change unrelated dependency versions.
2. Add `docs/releases/v<version>.md`; update the usage guide's pinned example,
   upgrade instructions, and download measurements when they change. Keep older
   release notes as history rather than rewriting their feature claims.
3. Run isolated Full tests and strict Clippy as described in the testing guide.
   Run the installer tests and ShellCheck below. Check workflow syntax with
   actionlint when changing the workflows.
4. Commit and push the reviewed changes. Confirm the branch CI passes, then push
   an annotated `v<version>` tag pointing to that commit.
5. Wait for the Release workflow and verify the published notes, all six assets,
   checksums, and latest-release target. Never describe a queued build as a
   completed release.

The tag-triggered workflow in `.github/workflows/release.yml` checks that the tag
matches Cargo's version, builds Windows and Linux packages, runs four fresh
Linux smoke jobs, and only then publishes. New versions should use new tags;
do not move an existing release tag or silently replace binaries for an
ordinary update. v0.11.3's same-version refresh was an exception; its notes
record the separate build source. v0.11.4 restores tag-to-build provenance.

## Local Packaging

Use current stable Rust. Windows additionally needs Visual Studio's x64 MSVC
build tools and Inno Setup 6.7.1. From PowerShell:

```powershell
$version = '0.11.6'
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --locked
# Use the path to your installed ISCC.exe.
& 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe' "/DMyAppVersion=$version" installer/mtui.iss
Copy-Item target/release/mtui.exe "dist/MTUI-$version-windows-x64-portable.exe"
```

Restore any previous `RUSTFLAGS` after a local build. Inspect the executable's
imports with MSVC `dumpbin /dependents` as the workflow does; compiling alone
does not establish that a fresh machine has all required runtime components.

On an Ubuntu 22.04 build environment with the source-build libraries and
`dpkg-dev` installed:

```sh
cargo build --release --locked --bins
bash scripts/package-linux.sh 0.11.6 target/release dist
```

The package script scans **both** executables with `dpkg-shlibdeps`, so sign-in's
WebKitGTK dependencies are declared even though the player does not load them.
The native Debian version is `<version>-1`. Both formats include both executables;
never package or update the player without its matching helper. Do not build on
a newer host and claim the older glibc baseline without checking compatibility.

After collecting Windows and Linux outputs in one clean release directory:

```sh
cd dist
sha256sum MTUI-* install.sh > SHA256SUMS
sha256sum --check SHA256SUMS
```

Do not mix older release artifacts into that directory. GitHub's asset digest
must match the uploaded file as well as `SHA256SUMS`; the in-app updater uses the
asset digest from release metadata. Hashes establish payload integrity, not an
independent publisher signature. The curl entry point runs a script from GitHub;
the usage guide also documents downloading and inspecting it first.

## Verification Gates

```sh
shellcheck scripts/install-linux.sh scripts/test-linux-installer.sh scripts/test-linux-release.sh scripts/package-linux.sh
bash scripts/test-linux-installer.sh
dbus-run-session -- cargo test --locked media::mpris::tests::desktop_controllers_discover_metadata_commands_and_seek_signals -- --exact --ignored
```

On a Windows desktop, verify the native media session as well:

```powershell
cargo test --locked media::imp::tests::native_media_session_publishes_and_owns_a_persistent_top_level_window -- --exact --ignored
```

The mocked installer tests do not install packages or change the host. The real
release smoke script **does**, and must run only inside disposable containers:

```sh
docker run --rm \
  --mount "type=bind,source=$PWD/scripts,target=/source/scripts,readonly" \
  --mount "type=bind,source=$PWD/dist,target=/packages,readonly" \
  ubuntu:24.04 bash /source/scripts/test-linux-release.sh 0.11.6 /packages
```

Repeat on `fedora:44`, `archlinux:base`, and `opensuse/tumbleweed:latest` as in
the workflow. The probe verifies real runtime dependency installation, binary
loading, launcher/icon presence, the sign-in helper's argument handling, and the
actual updater's replacement path using harmless executable fixtures. No Rust or
compiler should be needed in these runtime containers. Also check per-user
installation and upgrades without root when changing installer permissions.

Before publishing, validate `.deb` upgrades from the previous Debian version,
archive contents, script checksums, the normal-user curl entry point, and update
asset selection. Preserve account/settings/cache data during install and update
tests. Record OS/base-image versions and test logs. Separately test graphical
sign-in, real audio, foreground terminal input after restart, and Windows tray
behavior on desktop systems; headless container tests cannot certify those.

Download-size claims should distinguish app assets from first-launch tools,
system libraries, package metadata, and possible OS upgrades. State the measured
date and base image. Do not present Ubuntu Desktop's total as the total for every
distribution or equate download size with installed disk usage.
