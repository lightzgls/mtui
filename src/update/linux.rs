//! Per-user Linux updates use one archive on every compatible distribution.
//! Stage both executables before shutdown; restore both if installation or restart fails.

use super::*;
use anyhow::bail;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::process::Command;
use std::os::unix::process::CommandExt;
use std::sync::Mutex;

const MAX_UNPACKED: u64 = 128 * 1024 * 1024;
const MANIFEST: &str = "pending-update.json";
const CLEANUP: &str = "--mtui-update-cleanup";
static PENDING: Mutex<Option<PathBuf>> = Mutex::new(None);

pub(super) fn check_install_directory(executable: &Path) -> Result<()> {
    ensure!(std::env::consts::ARCH == "x86_64", "prebuilt Linux updates currently require x86-64");
    let directory = executable.parent().context("missing install directory")?;
    let probe = directory.join(format!(".mtui-write-check-{}-{}", std::process::id(), nonce()));
    let file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&probe)
        .context("This installation is not writable. Run the Linux curl installer once for per-user in-app updates, or update it with your package manager")?;
    drop(file);
    fs::remove_file(&probe)?;
    Ok(())
}

pub(super) fn prepare_restart(package: Package) -> Result<()> {
    ensure!(package.kind == Kind::Linux, "not a Linux update");
    verify(&package)?;
    check_install_directory(&package.executable)?;
    let target = package.executable.parent().context("missing install directory")?;
    ensure!(target.join("mtui-sign-in").is_file(), "The sign-in helper is missing. Run the Linux installer once to restore the complete installation");
    let staging = target.join(format!(".mtui-update-{}-{}", std::process::id(), nonce()));
    fs::DirBuilder::new().mode(0o700).create(&staging)?;
    let result = (|| -> Result<()> {
        extract(&package.payload, &staging)?;
        for binary in ["mtui", "mtui-sign-in"] {
            check_runtime(&staging.join(binary))?;
        }
        let helper = staging.join("update-helper");
        fs::copy(&package.executable, &helper)?;
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))?;
        let mut manifest = OpenOptions::new().write(true).create_new(true).mode(0o600)
            .open(staging.join(MANIFEST))?;
        manifest.write_all(&serde_json::to_vec(&package)?)?;
        manifest.sync_all()?;
        *PENDING.lock().map_err(|_| anyhow::anyhow!("could not prepare the Linux restart"))? = Some(helper);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

pub(super) fn restart_if_pending() -> Result<()> {
    let helper = PENDING.lock().map_err(|_| anyhow::anyhow!("could not read the Linux restart"))?.take();
    if let Some(helper) = helper {
        let manifest = helper.parent().context("missing update directory")?.join(MANIFEST);
        // Keep the foreground terminal and PID. A detached child would lose stdin
        // to the shell when the old process exits.
        return Err(Command::new(helper).arg(HELPER).arg(manifest).exec()).context("could not start the Linux update helper");
    }
    Ok(())
}

fn extract(payload: &Path, destination: &Path) -> Result<()> {
    let name = payload.file_name().and_then(|name| name.to_str()).context("invalid archive name")?;
    let bundle = name.strip_suffix(".tar.gz").context("invalid archive extension")?;
    let number = bundle.strip_prefix("MTUI-").and_then(|s| s.strip_suffix("-linux-x64"))
        .context("invalid Linux archive name")?;
    ensure!(version(number).is_some(), "invalid archive version");
    let decoder = flate2::read::GzDecoder::new(File::open(payload)?).take(MAX_UNPACKED);
    let mut archive = tar::Archive::new(decoder);
    let mut found = [false; 2];
    let mut count = 0;
    let mut total = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        count += 1;
        ensure!(count <= 64, "too many update archive entries");
        let path = entry.path()?.into_owned();
        let expected = [format!("{bundle}/mtui"), format!("{bundle}/mtui-sign-in")];
        let index = expected.iter().position(|candidate| path == Path::new(candidate));
        total = total.checked_add(entry.size()).context("archive size overflow")?;
        ensure!(total <= MAX_UNPACKED - 64 * 512, "update archive expands beyond its size limit");
        let Some(index) = index else { continue; };
        ensure!(!found[index] && entry.header().entry_type().is_file(), "duplicate or non-regular update executable");
        let output = destination.join(if index == 0 { "mtui" } else { "mtui-sign-in" });
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o700).open(&output)?;
        let written = std::io::copy(&mut entry, &mut file)?;
        ensure!(written == entry.size() && written >= 64, "truncated update executable");
        file.sync_all()?;
        drop(file);
        let mut header = [0; 20];
        File::open(&output)?.read_exact(&mut header)?;
        ensure!(
            &header[..4] == b"\x7fELF" && header[4] == 2 && header[5] == 1
                && u16::from_le_bytes([header[18], header[19]]) == 62,
            "the update executable is not Linux x86-64 ELF"
        );
        found[index] = true;
    }
    ensure!(found == [true, true], "the update must include both MTUI and its sign-in helper");
    Ok(())
}

fn check_runtime(executable: &Path) -> Result<()> {
    let output = Command::new("ldd").env("LC_ALL", "C").arg(executable).output()
        .context("ldd is needed to verify Linux runtime compatibility")?;
    let message = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    ensure!(output.status.success() && !message.contains("not found"),
        "The new version needs system libraries that are not installed. Rerun the Linux installer to install them. Your current copy is unchanged: {}", message.trim());
    Ok(())
}

pub(super) fn run_helper() -> Result<()> {
    let directory = std::env::current_exe()?.parent().context("missing helper directory")?.to_path_buf();
    let manifest = std::env::args_os().nth(2).map(PathBuf::from).context("missing update manifest")?;
    finish_update(&directory, &manifest)
}

fn finish_update(directory: &Path, manifest: &Path) -> Result<()> {
    ensure!(manifest == directory.join(MANIFEST), "invalid update manifest location");
    let raw = fs::read(manifest)?;
    ensure!(raw.len() <= 16 * 1024, "invalid update manifest size");
    let package: Package = serde_json::from_slice(&raw)?;
    ensure!(package.kind == Kind::Linux && package.executable.is_absolute()
        && package.executable.parent() == directory.parent()
        && package.parent == std::process::id()
        && directory.file_name().and_then(|s| s.to_str()).is_some_and(|s| s.starts_with(".mtui-update-")),
        "invalid Linux update paths");
    let attempt = (|| -> Result<()> {
        verify(&package)?;
        for binary in ["mtui", "mtui-sign-in"] {
            check_runtime(&directory.join(binary))?;
        }
        install_pair(directory, &package.executable, || {
            Err(Command::new(&package.executable).arg(CLEANUP).arg(directory).exec())
                .context("could not restart MTUI")
        })
    })();
    attempt.or_else(|error| {
        eprintln!("{error:#}");
        Err(Command::new(&package.executable).exec()).context("could not restart the restored version")
    })
}

pub(super) fn cleanup_completed() -> Result<()> {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new(CLEANUP)) { return Ok(()); }
    let directory = std::env::args_os().nth(2).map(PathBuf::from).context("missing cleanup directory")?;
    let executable = std::env::current_exe()?;
    ensure!(directory.is_absolute() && directory.parent() == executable.parent()
        && directory.file_name().and_then(|s| s.to_str()).is_some_and(|s| s.starts_with(".mtui-update-")), "invalid cleanup directory");
    let raw = fs::read(directory.join(MANIFEST))?;
    ensure!(raw.len() <= 16 * 1024, "invalid cleanup manifest size");
    let package: Package = serde_json::from_slice(&raw)?;
    ensure!(package.executable == executable && package.kind == Kind::Linux, "invalid cleanup target");
    let updates = crate::config::dir()?.join("updates");
    if package.payload.parent().and_then(Path::parent) == Some(updates.as_path()) {
        let _ = fs::remove_file(&package.payload);
        if let Some(download_dir) = package.payload.parent() { let _ = fs::remove_dir(download_dir); }
    }
    fs::remove_dir_all(directory)?;
    Ok(())
}

fn install_pair(staging: &Path, executable: &Path, restart: impl FnOnce() -> Result<()>) -> Result<()> {
    let target = executable.parent().context("missing install directory")?;
    let helper = target.join("mtui-sign-in");
    let previous = staging.join("previous-mtui");
    let previous_helper = staging.join("previous-sign-in");
    ensure!(executable.is_file() && helper.is_file(), "the existing player or sign-in helper is missing");
    fs::rename(executable, &previous)?;
    if let Err(error) = fs::rename(&helper, &previous_helper) {
        fs::rename(&previous, executable).context("could not restore MTUI")?;
        return Err(error.into());
    }
    let result = (|| -> Result<()> {
        fs::rename(staging.join("mtui-sign-in"), &helper)?;
        fs::rename(staging.join("mtui"), executable)?;
        restart()
    })();
    if let Err(error) = result {
        fs::rename(&previous, executable).context("could not restore the previous player")?;
        fs::rename(&previous_helper, &helper).context("could not restore the previous sign-in helper")?;
        bail!("Linux update failed; the previous version was restored: {error:#}");
    }
    let _ = fs::remove_file(previous);
    let _ = fs::remove_file(previous_helper);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};

    fn directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!("mtui-linux-update-test-{}-{}", std::process::id(), nonce()));
        fs::create_dir(&path).unwrap();
        path
    }

    fn elf() -> Vec<u8> {
        let mut bytes = vec![0; 64];
        bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
        bytes[18] = 62;
        bytes
    }

    fn archive(path: &Path, entries: &[(&str, tar::EntryType, &[u8])]) {
        let mut builder = tar::Builder::new(GzEncoder::new(File::create(path).unwrap(), Compression::fast()));
        for (name, kind, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_entry_type(*kind);
            if kind.is_symlink() { header.set_link_name("/tmp/outside-mtui").unwrap(); }
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn extraction_accepts_only_one_regular_elf_player_and_sign_in_helper() {
        let bytes = elf();
        let player = "MTUI-0.12.0-linux-x64/mtui";
        let helper = "MTUI-0.12.0-linux-x64/mtui-sign-in";
        for (entries, accepted) in [
            (vec![(player, tar::EntryType::Regular, bytes.as_slice()), (helper, tar::EntryType::Regular, bytes.as_slice())], true),
            (vec![(player, tar::EntryType::Regular, bytes.as_slice())], false),
            (vec![(player, tar::EntryType::Regular, bytes.as_slice()), (player, tar::EntryType::Regular, bytes.as_slice())], false),
            (vec![(player, tar::EntryType::Symlink, b"".as_slice()), (helper, tar::EntryType::Regular, bytes.as_slice())], false),
            (vec![(player, tar::EntryType::Regular, b"not ELF".as_slice()), (helper, tar::EntryType::Regular, bytes.as_slice())], false),
        ] {
            let root = directory();
            let payload = root.join("MTUI-0.12.0-linux-x64.tar.gz");
            let output = root.join("output");
            fs::create_dir(&output).unwrap();
            archive(&payload, &entries);
            assert_eq!(extract(&payload, &output).is_ok(), accepted);
            if accepted { assert_eq!(fs::metadata(output.join("mtui")).unwrap().permissions().mode() & 0o777, 0o700); }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn installation_swaps_both_files_and_rolls_back_both_on_failure() {
        for failure in [0, 1, 2] {
            let root = directory();
            let staging = root.join("staging");
            fs::create_dir(&staging).unwrap();
            let target = root.join("mtui");
            fs::write(&target, b"old player").unwrap();
            fs::write(root.join("mtui-sign-in"), b"old helper").unwrap();
            if failure != 1 { fs::write(staging.join("mtui"), b"new player").unwrap(); }
            fs::write(staging.join("mtui-sign-in"), b"new helper").unwrap();
            let result = install_pair(&staging, &target, || {
                if failure == 2 { bail!("restart failed"); }
                assert_eq!(fs::read(&target).unwrap(), b"new player");
                assert_eq!(fs::read(root.join("mtui-sign-in")).unwrap(), b"new helper");
                Ok(())
            });
            assert_eq!(result.is_ok(), failure == 0);
            assert_eq!(fs::read(&target).unwrap(), if failure == 0 { b"new player".as_slice() } else { b"old player".as_slice() });
            assert_eq!(fs::read(root.join("mtui-sign-in")).unwrap(), if failure == 0 { b"new helper".as_slice() } else { b"old helper".as_slice() });
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn corrupt_or_replaced_archives_fail_verification() {
        let root = directory();
        let payload = root.join("MTUI-0.12.0-linux-x64.tar.gz");
        fs::write(&payload, [0x1f, 0x8b, 1, 2]).unwrap();
        let package = Package { executable: root.join("mtui"), payload: payload.clone(),
            digest: format!("sha256:{:x}", Sha256::digest([0x1f, 0x8b, 1, 2])), size: 4, kind: Kind::Linux, parent: 0 };
        verify(&package).unwrap();
        fs::write(payload, [0x1f, 0x8b, 9, 9]).unwrap();
        assert!(verify(&package).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn linux_helper_installs_both_verified_binaries_and_execs_the_replacement() {
        let root = directory();
        fs::write(root.join("settings.json"), b"unchanged settings").unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "update::linux::tests::exec_helper_probe", "--nocapture"])
            .env("MTUI_TEST_UPDATE_ROOT", &root).status().unwrap();
        assert!(status.success());
        assert_eq!(fs::read(root.join("mtui")).unwrap(), fs::read("/bin/true").unwrap());
        assert_eq!(fs::read(root.join("mtui-sign-in")).unwrap(), fs::read("/bin/true").unwrap());
        assert_eq!(fs::read(root.join("settings.json")).unwrap(), b"unchanged settings");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exec_helper_probe() {
        let Some(root) = std::env::var_os("MTUI_TEST_UPDATE_ROOT").map(PathBuf::from) else { return; };
        let staging = root.join(".mtui-update-probe");
        fs::create_dir(&staging).unwrap();
        fs::write(root.join("mtui"), b"old player").unwrap();
        fs::write(root.join("mtui-sign-in"), b"old helper").unwrap();
        let bytes = fs::read("/bin/true").unwrap();
        let payload = root.join("MTUI-0.12.0-linux-x64.tar.gz");
        archive(&payload, &[("MTUI-0.12.0-linux-x64/mtui", tar::EntryType::Regular, &bytes),
            ("MTUI-0.12.0-linux-x64/mtui-sign-in", tar::EntryType::Regular, &bytes)]);
        extract(&payload, &staging).unwrap();
        let package = Package { executable: root.join("mtui"), payload: payload.clone(),
            digest: format!("sha256:{:x}", Sha256::digest(fs::read(&payload).unwrap())),
            size: fs::metadata(payload).unwrap().len(), kind: Kind::Linux, parent: std::process::id() };
        let manifest = staging.join(MANIFEST);
        fs::write(&manifest, serde_json::to_vec(&package).unwrap()).unwrap();
        finish_update(&staging, &manifest).unwrap();
        panic!("a successful exec must replace the test process");
    }
}
