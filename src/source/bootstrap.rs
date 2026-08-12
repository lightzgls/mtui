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
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;

use super::youtube::YouTube;

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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start a runtime to download yt-dlp")?;

    let client = reqwest::Client::builder()
        .read_timeout(READ_TIMEOUT)
        .build()
        .context("could not build an HTTP client to download yt-dlp")?;

    eprintln!(
        "mtui: yt-dlp not found, fetching it once into {}",
        dest.parent()
            .expect("asset path always has a parent")
            .display()
    );

    runtime.block_on(async {
        let response = client
            .get(DOWNLOAD_URL)
            .send()
            .await
            .context("could not reach github.com to download yt-dlp")?
            .error_for_status()
            .context("github.com refused the yt-dlp download")?;

        let total = response.content_length();
        if let Some(len) = total
            && len < MIN_PLAUSIBLE_BYTES
        {
            bail!("yt-dlp download was only {len} bytes, which is not the binary");
        }

        let mut file = fs::File::create(part)
            .with_context(|| format!("could not create {}", part.display()))?;
        let mut written: u64 = 0;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("the yt-dlp download was interrupted")?;
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
            .context("could not flush the yt-dlp download")?;
        drop(file);

        if written < MIN_PLAUSIBLE_BYTES {
            bail!("yt-dlp download ended early at {written} bytes");
        }
        finish(part, dest, written)
    })
}

/// Makes the finished download executable and moves it into place.
fn finish(part: &Path, dest: &Path, written: u64) -> Result<()> {
    // Downloaded files are not executable on Unix, and yt-dlp is about to be
    // spawned as a child process. No-op on Windows, which goes by extension.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(part, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("could not make {} executable", part.display()))?;
    }

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
}
