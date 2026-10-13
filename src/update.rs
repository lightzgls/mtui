//! Explicit, asynchronous updates from the project's stable GitHub releases.
use std::fs::{self, OpenOptions};
#[cfg(any(windows, target_os = "linux", test))]
use std::fs::File;
use std::io::Write;
#[cfg(any(windows, target_os = "linux", test))]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(not(any(windows, target_os = "linux")))]
use anyhow::bail;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const RELEASES: &str = "https://api.github.com/repos/lightzgls/mtui/releases/latest";
const DOWNLOADS: &str = "https://github.com/lightzgls/mtui/releases/download/";
const MAX_PACKAGE: u64 = 64 * 1024 * 1024;
const MAX_METADATA: usize = 1024 * 1024;
const HELPER: &str = "--mtui-update-helper";

#[cfg(target_os = "linux")]
mod linux;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum Kind {
    Installed,
    Portable,
    Linux,
}

#[derive(Debug, Clone)]
struct Release {
    version: String,
    asset: Asset,
}

#[derive(Debug, Clone, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
    state: String,
}

#[derive(Deserialize)]
struct Metadata {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Default)]
enum Status {
    #[default]
    Idle,
    Checking,
    Current,
    Available(Release),
    Downloading {
        version: String,
        percent: u8,
    },
    Installing,
    Failed(String),
}

enum Event {
    Checked(Result<Option<Release>, String>),
    Progress(u8),
    Downloaded(Result<Package, String>),
}

#[derive(Default)]
pub struct State {
    status: Status,
    receiver: Option<Receiver<Event>>,
}

#[derive(Serialize, Deserialize)]
pub struct Package {
    executable: PathBuf,
    payload: PathBuf,
    digest: String,
    size: u64,
    kind: Kind,
    parent: u32,
}

impl State {
    pub fn label(&self) -> String {
        match &self.status {
            Status::Idle => format!("v{} · Check", env!("CARGO_PKG_VERSION")),
            Status::Checking => "Checking...".into(),
            Status::Current => "Up to date · Check again".into(),
            Status::Available(release) => format!("Update to v{}", release.version),
            Status::Downloading { percent, .. } => format!("Downloading {percent}%"),
            Status::Installing => "Restarting...".into(),
            Status::Failed(_) => "Try again".into(),
        }
    }

    pub fn detail(&self) -> String {
        match &self.status {
            Status::Idle => "Check for new versions and install them here.".into(),
            Status::Checking => "Checking the latest stable release...".into(),
            Status::Current => format!("MTUI {} is up to date.", env!("CARGO_PKG_VERSION")),
            Status::Available(release) => format!(
                "Install v{} and restart MTUI. Playback will stop when the download finishes.",
                release.version
            ),
            Status::Downloading { version, percent } => {
                format!("Downloading v{version}: {percent}%. MTUI will restart when it is ready.")
            }
            Status::Installing => "Installing the update and restarting MTUI...".into(),
            Status::Failed(reason) => reason.clone(),
        }
    }

    pub fn failed(&self) -> bool {
        matches!(self.status, Status::Failed(_))
    }

    pub fn fail(&mut self, reason: String) {
        self.status = Status::Failed(reason);
        self.receiver = None;
    }

    pub fn activate(&mut self) {
        if matches!(
            self.status,
            Status::Checking | Status::Downloading { .. } | Status::Installing
        ) {
            return;
        }
        let release = match &self.status {
            Status::Available(release) => Some(release.clone()),
            _ => None,
        };
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.status = release
            .as_ref()
            .map_or(Status::Checking, |release| Status::Downloading {
                version: release.version.clone(),
                percent: 0,
            });
        if let Err(error) = std::thread::Builder::new()
            .name("mtui-update".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let executable = std::env::current_exe()?;
                    let kind = package_kind(&executable);
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    let http = client()?;
                    if let Some(release) = release {
                        #[cfg(target_os = "linux")]
                        linux::check_install_directory(&executable)?;
                        let result = runtime
                            .block_on(download(&http, release, executable, kind, &sender))
                            .map_err(|error| format!("Could not download the update: {error:#}"));
                        let _ = sender.send(Event::Downloaded(result));
                    } else {
                        let result = runtime
                            .block_on(latest(&http, env!("CARGO_PKG_VERSION"), kind))
                            .map_err(|error| format!("Could not check for updates: {error:#}"));
                        let _ = sender.send(Event::Checked(result));
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = sender.send(Event::Checked(Err(format!(
                        "Could not start the updater: {error:#}"
                    ))));
                }
            })
        {
            self.fail(format!("Could not start the updater: {error}"));
        }
    }

    /// Downloads finish independently of whether Settings remains open.
    pub fn poll(&mut self) -> Option<Package> {
        loop {
            let event = match self.receiver.as_ref()?.try_recv() {
                Ok(event) => event,
                Err(mpsc::TryRecvError::Empty) => return None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if matches!(self.status, Status::Checking | Status::Downloading { .. }) {
                        self.fail("The update worker stopped. Please try again.".into());
                    }
                    self.receiver = None;
                    return None;
                }
            };
            match event {
                Event::Checked(Ok(Some(release))) => self.status = Status::Available(release),
                Event::Checked(Ok(None)) => self.status = Status::Current,
                Event::Checked(Err(reason)) | Event::Downloaded(Err(reason)) => self.fail(reason),
                Event::Progress(value) => {
                    if let Status::Downloading { percent, .. } = &mut self.status {
                        *percent = value;
                    }
                }
                Event::Downloaded(Ok(package)) => {
                    self.status = Status::Installing;
                    self.receiver = None;
                    return Some(package);
                }
            }
        }
    }
}

fn package_kind(executable: &Path) -> Kind {
    if cfg!(target_os = "linux") {
        Kind::Linux
    } else if executable
        .parent()
        .is_some_and(|dir| dir.join("unins000.exe").is_file())
    {
        Kind::Installed
    } else {
        Kind::Portable
    }
}

fn version(text: &str) -> Option<[u64; 3]> {
    let mut parts = text.strip_prefix('v').unwrap_or(text).split('.');
    let mut result = [0; 3];
    for part in &mut result {
        let text = parts.next()?;
        if text.is_empty()
            || !text.bytes().all(|byte| byte.is_ascii_digit())
            || (text.len() > 1 && text.starts_with('0'))
        {
            return None;
        }
        *part = text.parse().ok()?;
    }
    parts.next().is_none().then_some(result)
}

fn select_release(raw: &[u8], current: &str, kind: Kind) -> Result<Option<Release>> {
    let metadata: Metadata = serde_json::from_slice(raw).context("invalid release response")?;
    ensure!(
        !metadata.draft && !metadata.prerelease,
        "this is not a stable release"
    );
    let remote = version(&metadata.tag_name).context("invalid release version")?;
    if remote <= version(current).context("invalid installed version")? {
        return Ok(None);
    }
    let release_version = metadata
        .tag_name
        .strip_prefix('v')
        .context("invalid release tag")?;
    let name = match kind {
        Kind::Installed => format!("MTUI-{release_version}-Setup-x64.exe"),
        Kind::Portable => format!("MTUI-{release_version}-windows-x64-portable.exe"),
        Kind::Linux => format!("MTUI-{release_version}-linux-x64.tar.gz"),
    };
    let asset = metadata
        .assets
        .into_iter()
        .find(|asset| asset.name == name)
        .context("the update for this platform is not available yet; try again shortly")?;
    ensure!(
        asset.state == "uploaded" && asset.size > 0 && asset.size <= MAX_PACKAGE,
        "invalid update size or upload state"
    );
    ensure!(
        asset.browser_download_url == format!("{DOWNLOADS}{}/{name}", metadata.tag_name),
        "unexpected update download address"
    );
    checksum(
        asset
            .digest
            .as_deref()
            .context("the release has no checksum")?,
    )?;
    Ok(Some(Release {
        version: release_version.into(),
        asset,
    }))
}

fn checksum(digest: &str) -> Result<&str> {
    let value = digest
        .strip_prefix("sha256:")
        .context("unsupported update checksum")?;
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid update checksum"
    );
    Ok(value)
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .user_agent(concat!("MTUI/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("too many download redirects");
            }
            if attempt.url().scheme() == "https"
                && matches!(
                    attempt.url().host_str(),
                    Some(
                        "github.com"
                            | "release-assets.githubusercontent.com"
                            | "objects.githubusercontent.com"
                    )
                )
            {
                attempt.follow()
            } else {
                attempt.error("unexpected update redirect")
            }
        }))
        .build()?)
}

async fn latest(http: &reqwest::Client, current: &str, kind: Kind) -> Result<Option<Release>> {
    let mut response = http
        .get(RELEASES)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len() + chunk.len() <= MAX_METADATA,
            "release response is too large"
        );
        body.extend_from_slice(&chunk);
    }
    select_release(&body, current, kind)
}

async fn download(
    http: &reqwest::Client,
    release: Release,
    executable: PathBuf,
    kind: Kind,
    sender: &Sender<Event>,
) -> Result<Package> {
    let directory = crate::config::dir()?.join("updates").join(format!(
        "{}-{}-{}",
        release.version,
        std::process::id(),
        nonce()
    ));
    fs::create_dir_all(&directory)?;
    let payload = directory.join(&release.asset.name);
    let result = async {
        let mut response = http
            .get(&release.asset.browser_download_url)
            .send()
            .await?
            .error_for_status()?;
        if let Some(length) = response.content_length() {
            ensure!(
                length == release.asset.size,
                "update download size does not match the release"
            );
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&payload)?;
        let mut hash = Sha256::new();
        let mut written = 0;
        let mut progress = 0;
        while let Some(chunk) = response.chunk().await? {
            written += chunk.len() as u64;
            ensure!(
                written <= release.asset.size,
                "update download exceeds its expected size"
            );
            file.write_all(&chunk)?;
            hash.update(&chunk);
            let percent = (written * 100 / release.asset.size) as u8;
            if percent != progress {
                progress = percent;
                let _ = sender.send(Event::Progress(percent));
            }
        }
        file.sync_all()?;
        let digest = release.asset.digest.context("missing update checksum")?;
        ensure!(
            written == release.asset.size
                && format!("{:x}", hash.finalize()).eq_ignore_ascii_case(checksum(&digest)?),
            "update verification failed; the download was discarded"
        );
        Ok(Package {
            executable,
            payload: payload.clone(),
            digest,
            size: written,
            kind,
            parent: std::process::id(),
        })
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&payload);
        let _ = fs::remove_dir(&directory);
    }
    result
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(any(windows, target_os = "linux", test))]
fn verify(package: &Package) -> Result<()> {
    let mut file = File::open(&package.payload)?;
    ensure!(
        file.metadata()?.len() == package.size && package.size <= MAX_PACKAGE,
        "update size changed"
    );
    let mut magic = [0; 2];
    file.read_exact(&mut magic)?;
    if package.kind == Kind::Linux {
        ensure!(magic == [0x1f, 0x8b], "the update is not a gzip archive");
    } else {
        ensure!(&magic == b"MZ", "the update is not a Windows executable");
    }
    let mut hash = Sha256::new();
    hash.update(magic);
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    ensure!(
        format!("{:x}", hash.finalize()).eq_ignore_ascii_case(checksum(&package.digest)?),
        "update checksum changed"
    );
    Ok(())
}

#[cfg(windows)]
pub fn prepare_restart(package: Package) -> Result<()> {
    use std::os::windows::process::CommandExt;
    verify(&package)?;
    let directory = package
        .payload
        .parent()
        .context("missing update directory")?;
    let helper = directory.join("update-helper.exe");
    let manifest = directory.join("pending-update.json");
    fs::copy(&package.executable, &helper).context("could not prepare the update helper")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&manifest)?;
    file.write_all(&serde_json::to_vec(&package)?)?;
    file.sync_all()?;
    std::process::Command::new(helper)
        .arg(HELPER)
        .arg(manifest)
        .creation_flags(0x0800_0000)
        .spawn()
        .context("could not launch the updater")?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn prepare_restart(package: Package) -> Result<()> {
    linux::prepare_restart(package)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn prepare_restart(_: Package) -> Result<()> {
    bail!("in-app installation is available on Windows and Linux")
}

pub fn helper_requested() -> bool {
    std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new(HELPER))
}

#[cfg(windows)]
pub fn run_helper() -> Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::CommandExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn WaitForSingleObject(handle: isize, timeout: u32) -> u32;
        fn GetLastError() -> u32;
    }
    let manifest = std::env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .context("missing update manifest")?;
    let directory = std::env::current_exe()?
        .parent()
        .context("missing helper directory")?
        .to_path_buf();
    ensure!(
        manifest == directory.join("pending-update.json"),
        "invalid update manifest location"
    );
    let raw = fs::read(&manifest)?;
    ensure!(raw.len() <= 16 * 1024, "invalid update manifest size");
    let package: Package = serde_json::from_slice(&raw)?;
    ensure!(
        package.payload.parent() == Some(directory.as_path())
            && package.executable.is_absolute()
            && package_kind(&package.executable) == package.kind,
        "invalid update paths"
    );
    verify(&package)?;
    let process = unsafe {
        OpenProcess(0x0010_0000 /* SYNCHRONIZE */, 0, package.parent)
    };
    if process != 0 {
        let handle = unsafe { OwnedHandle::from_raw_handle(process as *mut c_void) };
        ensure!(
            unsafe { WaitForSingleObject(handle.as_raw_handle() as isize, 30_000) } == 0,
            "MTUI did not close; the update was not installed"
        );
    } else {
        ensure!(
            unsafe { GetLastError() } == 87,
            "could not wait for MTUI to close"
        );
    }
    match package.kind {
        Kind::Installed => {
            let target = package
                .executable
                .parent()
                .context("missing install directory")?;
            let status = std::process::Command::new(&package.payload)
                .args([
                    "/VERYSILENT",
                    "/SUPPRESSMSGBOXES",
                    "/NORESTART",
                    "/NOCLOSEAPPLICATIONS",
                ])
                .arg(format!("/DIR={}", target.display()))
                .creation_flags(0x0800_0000)
                .status()?;
            ensure!(
                status.success(),
                "the installer failed; MTUI was not updated"
            );
        }
        Kind::Portable => replace_portable(&package.payload, &package.executable)?,
        Kind::Linux => anyhow::bail!("a Linux update cannot be installed on Windows"),
    }
    std::process::Command::new(&package.executable)
        .creation_flags(0x0800_0000)
        .spawn()
        .context("the update installed, but MTUI could not restart")?;
    let _ = fs::remove_file(&manifest);
    let _ = fs::remove_file(&package.payload);
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn run_helper() -> Result<()> {
    linux::run_helper()
}

#[cfg(target_os = "linux")]
pub fn restart_if_pending() -> Result<()> {
    linux::restart_if_pending()
}

#[cfg(target_os = "linux")]
pub fn cleanup_completed() {
    if let Err(error) = linux::cleanup_completed() {
        eprintln!("Could not clean up the completed MTUI update: {error:#}");
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn run_helper() -> Result<()> {
    bail!("the updater cannot run on this platform")
}

#[cfg(any(windows, test))]
fn replace_portable(payload: &Path, target: &Path) -> Result<()> {
    let directory = target.parent().context("missing portable directory")?;
    let staged = directory.join(format!(".mtui-update-{}.exe", nonce()));
    let backup = directory.join(format!(".mtui-previous-{}.exe", nonce()));
    // Stage on the target volume so rename remains atomic for portable copies
    // on another drive. A failure before the swap leaves the old app intact.
    let result = (|| -> Result<()> {
        let mut source = File::open(payload)?;
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        std::io::copy(&mut source, &mut destination)?;
        destination.sync_all()?;
        drop(destination);
        fs::rename(target, &backup)?;
        if let Err(error) = fs::rename(&staged, target) {
            fs::rename(&backup, target).context("could not restore the previous executable")?;
            return Err(error.into());
        }
        let _ = fs::remove_file(&backup);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(tag: &str, kind: Kind, bytes: &[u8]) -> serde_json::Value {
        let number = tag.trim_start_matches('v');
        let name = match kind {
            Kind::Installed => format!("MTUI-{number}-Setup-x64.exe"),
            Kind::Portable => format!("MTUI-{number}-windows-x64-portable.exe"),
            Kind::Linux => format!("MTUI-{number}-linux-x64.tar.gz"),
        };
        serde_json::json!({"tag_name": tag, "draft": false, "prerelease": false, "assets": [{"name": name, "state": "uploaded", "size": bytes.len(), "digest": format!("sha256:{:x}", Sha256::digest(bytes)), "browser_download_url": format!("{DOWNLOADS}{tag}/{name}")}]})
    }

    #[test]
    fn releases_compare_numerically_and_select_the_matching_windows_package() {
        for kind in [Kind::Installed, Kind::Portable] {
            let raw = serde_json::to_vec(&metadata("v0.10.0", kind, b"MZpayload")).unwrap();
            assert_eq!(
                select_release(&raw, "0.9.3", kind)
                    .unwrap()
                    .unwrap()
                    .version,
                "0.10.0"
            );
            assert!(select_release(&raw, "0.10.0", kind).unwrap().is_none());
            assert!(select_release(&raw, "1.0.0", kind).unwrap().is_none());
        }
        for text in [
            "0.10.0-beta",
            "0.10",
            "0.10.0.1",
            "0.010.0",
            "../0.10.0",
            "1.0.0+build",
        ] {
            assert!(version(text).is_none());
        }
    }

    #[test]
    fn linux_updates_select_the_archive_including_sign_in_not_the_debian_package() {
        let raw = metadata("v0.12.0", Kind::Linux, &[0x1f, 0x8b, 0]);
        let release = select_release(&serde_json::to_vec(&raw).unwrap(), "0.11.3", Kind::Linux)
            .unwrap().unwrap();
        assert_eq!(release.asset.name, "MTUI-0.12.0-linux-x64.tar.gz");
        assert!(select_release(&serde_json::to_vec(&raw).unwrap(), "0.11.3", Kind::Portable).is_err());
        assert_eq!(package_kind(Path::new("/home/user/.local/lib/mtui/mtui")) == Kind::Linux, cfg!(target_os = "linux"));
    }

    #[test]
    fn unstable_missing_or_untrusted_assets_cannot_be_installed() {
        let source = metadata("v0.10.0", Kind::Portable, b"MZpayload");
        for (field, value) in [
            (
                "browser_download_url",
                serde_json::json!("https://example.com/update.exe"),
            ),
            ("size", serde_json::json!(MAX_PACKAGE + 1)),
            ("digest", serde_json::Value::Null),
            ("digest", serde_json::json!("sha256:bad")),
            ("state", serde_json::json!("new")),
        ] {
            let mut raw = source.clone();
            raw["assets"][0][field] = value;
            assert!(
                select_release(&serde_json::to_vec(&raw).unwrap(), "0.9.3", Kind::Portable)
                    .is_err()
            );
        }
        let mut draft = source.clone();
        draft["draft"] = true.into();
        assert!(
            select_release(
                &serde_json::to_vec(&draft).unwrap(),
                "0.9.3",
                Kind::Portable
            )
            .is_err()
        );
        let mut preview = source;
        preview["prerelease"] = true.into();
        assert!(
            select_release(
                &serde_json::to_vec(&preview).unwrap(),
                "0.9.3",
                Kind::Portable
            )
            .is_err()
        );
    }

    #[test]
    fn portable_replacement_preserves_the_old_file_when_staging_fails_and_verifies_bytes() {
        let directory = std::env::temp_dir().join(format!("mtui-update-test-{}", nonce()));
        fs::create_dir(&directory).unwrap();
        let target = directory.join("mtui.exe");
        let payload = directory.join("new.exe");
        fs::write(&target, b"previous").unwrap();
        assert!(replace_portable(&payload, &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"previous");
        fs::write(&payload, b"MZpayload").unwrap();
        let package = Package {
            executable: target.clone(),
            payload: payload.clone(),
            digest: format!("sha256:{:x}", Sha256::digest(b"MZpayload")),
            size: 9,
            kind: Kind::Portable,
            parent: 0,
        };
        verify(&package).unwrap();
        fs::write(&payload, b"MZchanged").unwrap();
        assert!(verify(&package).is_err());
        fs::write(&payload, b"MZpayload").unwrap();
        replace_portable(&payload, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"MZpayload");
        fs::remove_file(target).unwrap();
        fs::remove_file(payload).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn download_verifies_complete_bytes_and_discards_corrupt_or_truncated_packages() {
        use crate::source::test_http::{immediate, serve};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let http = reqwest::Client::new();
        for bytes in [
            b"MZpayload".as_slice(),
            b"MZcorrupt".as_slice(),
            b"MZ".as_slice(),
        ] {
            let reply = [
                b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\n".as_slice(),
                bytes,
            ]
            .concat();
            let (url, server) = serve(vec![immediate(&reply)]);
            let raw =
                serde_json::to_vec(&metadata("v0.10.0", Kind::Portable, b"MZpayload")).unwrap();
            let mut release = select_release(&raw, "0.9.3", Kind::Portable)
                .unwrap()
                .unwrap();
            release.asset.browser_download_url = url;
            let (sender, receiver) = mpsc::channel();
            let result = runtime.block_on(download(
                &http,
                release,
                PathBuf::from("unused.exe"),
                Kind::Portable,
                &sender,
            ));
            assert_eq!(result.is_ok(), bytes == b"MZpayload");
            if let Ok(package) = result {
                verify(&package).unwrap();
                assert!(
                    receiver
                        .try_iter()
                        .any(|event| matches!(event, Event::Progress(100)))
                );
                let directory = package.payload.parent().unwrap().to_path_buf();
                fs::remove_file(package.payload).unwrap();
                fs::remove_dir(directory).unwrap();
            }
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[test]
    fn updater_reports_progress_errors_and_completion_without_duplicate_workers() {
        let (sender, receiver) = mpsc::channel();
        let mut state = State {
            status: Status::Checking,
            receiver: Some(receiver),
        };
        state.activate();
        sender.send(Event::Checked(Ok(None))).unwrap();
        state.poll();
        assert!(matches!(state.status, Status::Current));
        state.status = Status::Downloading {
            version: "0.10.0".into(),
            percent: 0,
        };
        state.activate();
        sender.send(Event::Progress(65)).unwrap();
        state.poll();
        assert_eq!(state.label(), "Downloading 65%");
        sender
            .send(Event::Downloaded(Err("offline".into())))
            .unwrap();
        state.poll();
        assert!(state.failed());
        assert_eq!(state.detail(), "offline");
        let (sender, receiver) = mpsc::channel();
        state.receiver = Some(receiver);
        state.status = Status::Downloading {
            version: "0.10.0".into(),
            percent: 100,
        };
        sender
            .send(Event::Downloaded(Ok(Package {
                executable: "unused.exe".into(),
                payload: "unused-new.exe".into(),
                digest: "unused".into(),
                size: 0,
                kind: Kind::Portable,
                parent: 0,
            })))
            .unwrap();
        assert!(state.poll().is_some());
        assert!(matches!(state.status, Status::Installing));
        assert!(state.poll().is_none());
    }

    #[test]
    #[ignore = "downloads and verifies stable GitHub release packages; no installation"]
    fn latest_published_windows_release_has_both_verified_update_packages() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let http = client().unwrap();
        for kind in [Kind::Installed, Kind::Portable] {
            let release = runtime
                .block_on(latest(&http, "0.0.0", kind))
                .unwrap()
                .unwrap();
            println!(
                "{}: {} ({} bytes, SHA-256 provided)",
                release.version, release.asset.name, release.asset.size
            );
            let (sender, _) = mpsc::channel();
            let package = runtime
                .block_on(download(
                    &http,
                    release,
                    std::env::current_exe().unwrap(),
                    kind,
                    &sender,
                ))
                .unwrap();
            verify(&package).unwrap();
            let directory = package.payload.parent().unwrap().to_path_buf();
            fs::remove_file(package.payload).unwrap();
            fs::remove_dir(directory).unwrap();
        }
    }
}
