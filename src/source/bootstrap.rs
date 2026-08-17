//! Making `yt-dlp` present, so that what ships is one file.
//!
//! `yt-dlp` is a fallback, not the main event -- [`super::innertube`] answers
//! most plays without it. But the cases it covers (age-gated, region-locked,
//! and the capped URLs that licensed music resolves to) are not rare enough to
//! drop, so the program still needs it on disk.
//!
//! Rather than making the user install it, this fetches it on first run into a
//! directory we own. It is deliberately *not* embedded in the binary: yt-dlp
//! tracks a moving target and is released roughly monthly -- a copy frozen at
//! build time would stop working within months, with nothing the user could do
//! but wait for us to ship again. A downloaded copy is whatever is current on
//! the day they first run the program.
//!
//! An install either completes or leaves nothing behind. The download lands on
//! a temporary path and is only moved into place after it has proven it runs;
//! any failure removes the partial file and reports, so there is no state where
//! a half-written binary is found and trusted on the next launch.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;

use super::{command, youtube::YouTube};

/// The release asset for this platform, and where it comes from.
///
/// yt-dlp publishes a self-contained binary per platform, which is why this can
/// be a plain download and not an unpack -- the Windows asset is a single
/// `.exe` with Python inside it.
///
/// The URL is GitHub's permanent redirect to the newest release, used instead
/// of the releases API so that no JSON is parsed and no rate limit applies: the
/// API allows 60 unauthenticated requests an hour per IP, and a first run is
/// the worst possible moment to be told to come back later.
///
/// The two are spelled out per platform rather than concatenated because
/// `concat!` takes only literals. A test asserts they agree.
#[cfg(windows)]
const ASSET: &str = "yt-dlp.exe";
#[cfg(windows)]
const DOWNLOAD_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";

#[cfg(target_os = "macos")]
const ASSET: &str = "yt-dlp_macos";
#[cfg(target_os = "macos")]
const DOWNLOAD_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos";

#[cfg(all(unix, not(target_os = "macos")))]
const ASSET: &str = "yt-dlp_linux";
#[cfg(all(unix, not(target_os = "macos")))]
const DOWNLOAD_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux";

/// Generous next to the player API's 5 s: this is a ~17 MB transfer over
/// whatever connection the user has, and it happens once. The timeout is per
/// read rather than for the whole download, so a slow line is tolerated while a
/// dead one is not.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Refuse a "download" that is obviously not the binary. GitHub serving an
/// error page through the redirect would otherwise be written to disk and only
/// fail later, when it is run.
const MIN_PLAUSIBLE_BYTES: u64 = 1024 * 1024;
const MIN_DENO_ARCHIVE_BYTES: u64 = 10 * 1024 * 1024;

#[cfg(windows)]
const DENO_BIN: &str = "deno.exe";
#[cfg(not(windows))]
const DENO_BIN: &str = "deno";

/// Where we keep binaries we fetched.
///
/// `%LOCALAPPDATA%` rather than the `%APPDATA%` that [`crate::config`] uses:
/// roaming profiles are synchronised to a server at logon, and a 17 MB
/// executable is precisely what should not be copied across a network for the
/// privilege of existing on a second machine that could fetch it itself.
fn bin_dir() -> Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    };
    let base = base.context("could not locate a local data directory")?;
    Ok(base.join("mtui").join("bin"))
}

/// Returns a `yt-dlp` that runs, fetching one if that is what it takes.
///
/// Order is a preference, not a fallback chain. A `yt-dlp` the user installed
/// themselves wins: they may have picked a specific version, or a build with
/// their own patches, and silently shadowing it with our copy would make that
/// choice quietly stop mattering. Only when there is none do we look at ours,
/// and only when there is neither do we download.
pub fn locate() -> Result<YouTube> {
    let system = YouTube::default();
    if system.version().is_ok() {
        return Ok(system);
    }

    let path = bin_dir()?.join(ASSET);
    let ours = YouTube::new(path.to_string_lossy().into_owned());
    if path.exists() && ours.version().is_ok() {
        return Ok(ours);
    }

    // Either it was never fetched, or what is there does not run -- a download
    // cut short by a power loss, say. Both are answered by fetching it again.
    install(&path)?;

    ours.version().with_context(|| {
        format!(
            "downloaded yt-dlp to {} but it does not run",
            path.display()
        )
    })?;
    Ok(ours)
}

/// Reuses a supported JavaScript runtime, or installs a private copy of Deno.
/// A failed install is non-fatal: simpler yt-dlp paths still work without it.
pub fn with_js_runtime(yt: YouTube) -> Result<YouTube> {
    for runtime in ["deno", "node", "bun"] {
        if runtime_supported(runtime) {
            return Ok(yt.with_js_runtime(Some(runtime.to_string())));
        }
    }

    let managed = bin_dir()?.join(DENO_BIN);
    if runtime_supported_at("deno", &managed) {
        return Ok(yt.with_js_runtime(Some(runtime_arg(&managed))));
    }

    if let Err(error) = install_deno(&managed) {
        eprintln!("mtui: could not install Deno: {error:#}");
        eprintln!("mtui: continuing without a JavaScript runtime");
        return Ok(yt);
    }
    Ok(yt.with_js_runtime(Some(runtime_arg(&managed))))
}

fn runtime_supported(runtime: &str) -> bool {
    runtime_supported_at(runtime, runtime)
}

fn runtime_supported_at(runtime: &str, executable: impl AsRef<std::ffi::OsStr>) -> bool {
    let Ok(output) = command(executable)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = match runtime {
        "deno" => stdout
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1)),
        "node" => Some(stdout.trim().trim_start_matches('v')),
        "bun" => Some(stdout.trim()),
        _ => None,
    };
    let Some(version) = version.and_then(parse_version) else {
        return false;
    };
    match runtime {
        "deno" => version >= (2, 3, 0),
        "node" => version >= (22, 0, 0),
        // yt-dlp deprecated Bun and supports no release after 1.3.14.
        "bun" => ((1, 2, 11)..=(1, 3, 14)).contains(&version),
        _ => false,
    }
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.split('-').next()?.parse().ok()?;
    Some((major, minor, patch))
}

fn runtime_arg(path: &Path) -> String {
    format!("deno:{}", path.to_string_lossy())
}

fn deno_download_url() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok(
            "https://github.com/denoland/deno/releases/latest/download/deno-x86_64-pc-windows-msvc.zip",
        ),
        ("windows", "aarch64") => Ok(
            "https://github.com/denoland/deno/releases/latest/download/deno-aarch64-pc-windows-msvc.zip",
        ),
        ("linux", "x86_64") => Ok(
            "https://github.com/denoland/deno/releases/latest/download/deno-x86_64-unknown-linux-gnu.zip",
        ),
        ("linux", "aarch64") => Ok(
            "https://github.com/denoland/deno/releases/latest/download/deno-aarch64-unknown-linux-gnu.zip",
        ),
        ("macos", "x86_64") => Ok(
            "https://github.com/denoland/deno/releases/latest/download/deno-x86_64-apple-darwin.zip",
        ),
        ("macos", "aarch64") => Ok(
            "https://github.com/denoland/deno/releases/latest/download/deno-aarch64-apple-darwin.zip",
        ),
        (os, arch) => bail!("automatic Deno installation is not available for {os}/{arch}"),
    }
}

fn install_deno(dest: &Path) -> Result<()> {
    let dir = dest.parent().expect("Deno path always has a parent");
    fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let archive = dir.join("deno.zip.part");
    let executable = dest.with_extension("part");
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_file(&executable);

    let result = (|| {
        download_asset(
            &archive,
            deno_download_url()?,
            "Deno",
            MIN_DENO_ARCHIVE_BYTES,
        )?;
        let file = fs::File::open(&archive)
            .with_context(|| format!("could not open {}", archive.display()))?;
        let mut zip = zip::ZipArchive::new(file).context("the Deno download was not a zip file")?;
        let mut entry = zip
            .by_name(DENO_BIN)
            .with_context(|| format!("the Deno archive did not contain {DENO_BIN}"))?;
        let mut output = fs::File::create(&executable)
            .with_context(|| format!("could not create {}", executable.display()))?;
        io::copy(&mut entry, &mut output).context("could not extract Deno")?;
        output
            .flush()
            .context("could not flush the Deno executable")?;
        drop(output);

        make_executable(&executable)?;
        if dest.exists() {
            fs::remove_file(dest)
                .with_context(|| format!("could not replace {}", dest.display()))?;
        }
        fs::rename(&executable, dest)
            .with_context(|| format!("could not move Deno to {}", dest.display()))?;
        if !runtime_supported_at("deno", dest) {
            bail!("downloaded Deno to {} but it does not run", dest.display());
        }
        eprintln!("mtui: installed Deno into {}", dir.display());
        Ok(())
    })();

    let _ = fs::remove_file(&archive);
    let _ = fs::remove_file(&executable);
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

/// Downloads the asset to `dest`, atomically and verifiably or not at all.
fn install(dest: &Path) -> Result<()> {
    let dir = dest.parent().expect("asset path always has a parent");
    fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;

    // Alongside the destination rather than in the system temp directory, so
    // the rename below is within one filesystem and therefore atomic. A rename
    // across volumes is a copy, which can itself be interrupted halfway.
    let part = dest.with_extension("part");
    // A leftover from an interrupted run is not worth resuming: it may be from
    // an older release, and there is no way to tell from the bytes.
    let _ = fs::remove_file(&part);

    match download(&part, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            // The whole point of the temporary path: a failure takes its
            // evidence with it, so the next launch sees a clean slate rather
            // than a truncated binary that looks installed.
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}

/// The download proper. Split out so that [`install`] has exactly one place to
/// clean up from, whatever went wrong.
fn download(part: &Path, dest: &Path) -> Result<()> {
    let written = download_asset(part, DOWNLOAD_URL, "yt-dlp", MIN_PLAUSIBLE_BYTES)?;
    finish(part, dest, written)
}

fn download_asset(part: &Path, url: &str, name: &str, minimum: u64) -> Result<u64> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .with_context(|| format!("could not start a runtime to download {name}"))?;

    let client = reqwest::Client::builder()
        .read_timeout(READ_TIMEOUT)
        .build()
        .with_context(|| format!("could not build an HTTP client to download {name}"))?;

    eprintln!(
        "mtui: fetching {name} into {}",
        part.parent()
            .expect("download path always has a parent")
            .display()
    );

    runtime.block_on(async {
        let response = client
            .get(url)
            .send()
            .await
            .with_context(|| format!("could not reach github.com to download {name}"))?
            .error_for_status()
            .with_context(|| format!("github.com refused the {name} download"))?;

        let total = response.content_length();
        if let Some(len) = total
            && len < minimum
        {
            bail!("{name} download was only {len} bytes, which is not the expected file");
        }

        let mut file = fs::File::create(part)
            .with_context(|| format!("could not create {}", part.display()))?;
        let mut written: u64 = 0;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("the {name} download was interrupted"))?;
            file.write_all(&chunk)
                .with_context(|| format!("could not write to {}", part.display()))?;
            written += chunk.len() as u64;
            report(written, total);
        }

        // Flushed and closed before the rename: on Windows a file cannot be
        // renamed while a handle is open, and a buffered tail that never
        // reached the disk would produce exactly the truncated binary this
        // whole path exists to avoid.
        file.flush()
            .with_context(|| format!("could not flush the {name} download"))?;
        drop(file);

        if written < minimum {
            bail!("{name} download ended early at {written} bytes");
        }
        Ok(written)
    })
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("could not make {} executable", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Makes the finished download executable and moves it into place.
fn finish(part: &Path, dest: &Path, written: u64) -> Result<()> {
    // Downloaded files are not executable on Unix, and yt-dlp is about to be
    // spawned as a child process. No-op on Windows, which goes by extension.
    make_executable(part)?;

    // Windows refuses a rename onto an existing file. Only reachable when the
    // copy already there failed to run, which is why replacing it is the point.
    if dest.exists() {
        fs::remove_file(dest).with_context(|| format!("could not replace {}", dest.display()))?;
    }
    fs::rename(part, dest)
        .with_context(|| format!("could not move the download into {}", dest.display()))?;

    eprintln!(
        "mtui: fetched yt-dlp ({:.1} MB)",
        written as f64 / 1_048_576.0
    );
    Ok(())
}

/// Prints progress, but only to a terminal.
///
/// Redirected output is usually a log, and a log does not want several hundred
/// carriage-returned lines describing a transfer that has already finished.
fn report(written: u64, total: Option<u64>) {
    let mut err = std::io::stderr();
    if !err.is_terminal() {
        return;
    }
    let mb = written as f64 / 1_048_576.0;
    match total {
        Some(total) if total > 0 => {
            let pct = written as f64 / total as f64 * 100.0;
            let _ = write!(
                err,
                "\r  {mb:.1} MB / {:.1} MB ({pct:.0}%)",
                total as f64 / 1_048_576.0
            );
        }
        _ => {
            let _ = write!(err, "\r  {mb:.1} MB");
        }
    }
    let _ = err.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_asset_name_matches_the_download_url() {
        // The two are declared separately, and a mismatch would fetch the wrong
        // platform's binary into a path named for this one.
        assert!(DOWNLOAD_URL.ends_with(ASSET));
    }

    #[test]
    fn the_binary_directory_is_not_the_config_directory() {
        // A 17 MB executable in the roaming profile is the thing bin_dir exists
        // to avoid, so it is worth failing loudly if the two ever converge.
        let (bin, config) = (bin_dir().unwrap(), crate::config::dir().unwrap());
        assert_ne!(bin, config);
    }

    #[test]
    fn runtime_versions_are_compared_numerically() {
        assert_eq!(parse_version("2.3.0"), Some((2, 3, 0)));
        assert_eq!(parse_version("24.11.1"), Some((24, 11, 1)));
        assert_eq!(parse_version("1.3.14-canary"), Some((1, 3, 14)));
        assert_eq!(parse_version("not-a-version"), None);
    }

    #[test]
    fn this_platform_has_a_deno_asset() {
        let url = deno_download_url().expect("CI platforms should have a Deno release");
        assert!(url.starts_with("https://github.com/denoland/deno/releases/latest/download/"));
        assert!(url.ends_with(".zip"));
    }

    /// The contract [`install`] offers: whatever happens, the temporary file is
    /// gone.
    ///
    /// Ignored for the same reason every other network test here is. There is
    /// no way to drive this without the network -- the cleanup being checked is
    /// the one [`download`] performs -- and on a working connection it fetches
    /// the whole 17 MB binary, which is not something a plain `cargo test`
    /// should do on every run.
    ///
    /// `cargo test a_partial_download -- --ignored --nocapture`
    #[test]
    #[ignore = "downloads yt-dlp from github.com"]
    fn a_partial_download_is_not_left_behind() {
        let dir = std::env::temp_dir().join("mtui-bootstrap-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join(ASSET);
        let part = dest.with_extension("part");

        fs::write(&part, b"leftover from an interrupted run").unwrap();
        // Not asserting on the result: with no network this fails to connect,
        // and with one it downloads 17 MB. Either way the partial must be gone.
        let _ = install(&dest);
        assert!(!part.exists(), "install() left {} behind", part.display());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "downloads Deno from github.com"]
    fn installs_a_working_deno() {
        let dir = std::env::temp_dir().join("mtui-deno-bootstrap-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join(DENO_BIN);

        install_deno(&dest).expect("Deno should install");
        assert!(runtime_supported_at("deno", &dest));

        let _ = fs::remove_dir_all(&dir);
    }
}
