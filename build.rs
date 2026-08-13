//! Compiles MTUI's icon and stages WebView2's loader on Windows.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/mtui.rc");
    println!("cargo:rerun-if-changed=assets/mtui.ico");
    println!("cargo:rerun-if-env-changed=RC");
    println!("cargo:rerun-if-env-changed=WINDRES");

    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo always sets OUT_DIR"));
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let built = if msvc {
        compile_msvc(&out)
    } else {
        compile_gnu(&out)
    };

    match built {
        Some(path) => println!("cargo:rustc-link-arg-bins={}", path.display()),
        None => println!(
            "cargo:warning=no resource compiler found, so mtui.exe is being built without its \
             icon. Install one ({}), or point RC/WINDRES at it.",
            if msvc {
                "the Windows SDK, for rc.exe"
            } else {
                "binutils, for windres"
            }
        ),
    }

    if !msvc {
        stage_webview_loader(&out);
    }
}

fn compile_gnu(out: &Path) -> Option<PathBuf> {
    let object = out.join("mtui-icon.o");
    let windres = env::var_os("WINDRES").unwrap_or_else(|| OsString::from("windres"));
    let status = Command::new(&windres)
        .args(["-I", "assets", "assets/mtui.rc", "-O", "coff", "-o"])
        .arg(&object)
        .status()
        .ok()?;
    status.success().then_some(object)
}

fn compile_msvc(out: &Path) -> Option<PathBuf> {
    let res = out.join("mtui-icon.res");
    let rc = find_rc()?;
    let status = Command::new(&rc)
        .args(["/nologo", "/I", "assets", "/fo"])
        .arg(&res)
        .arg("assets/mtui.rc")
        .status()
        .ok()?;
    status.success().then_some(res)
}

fn find_rc() -> Option<PathBuf> {
    if let Some(rc) = env::var_os("RC") {
        return Some(PathBuf::from(rc));
    }
    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return Some(PathBuf::from("rc.exe"));
    }

    let arch = match env::var("HOST").as_deref() {
        Ok(host) if host.starts_with("aarch64") => "arm64",
        Ok(host) if host.starts_with("i686") => "x86",
        _ => "x64",
    };
    let mut versions: Vec<PathBuf> = ["ProgramFiles(x86)", "ProgramFiles"]
        .iter()
        .filter_map(env::var_os)
        .flat_map(|root| {
            fs::read_dir(
                PathBuf::from(root)
                    .join("Windows Kits")
                    .join("10")
                    .join("bin"),
            )
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
        })
        .collect();
    versions.sort();
    versions
        .iter()
        .rev()
        .map(|version| version.join(arch).join("rc.exe"))
        .find(|rc| rc.is_file())
}

fn stage_webview_loader(out: &Path) {
    let Some(profile) = out.ancestors().nth(3) else {
        return;
    };
    let Some(build) = out.ancestors().nth(2) else {
        return;
    };
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x64",
        Ok("x86") => "x86",
        Ok("aarch64") => "arm64",
        _ => return,
    };
    let loader = fs::read_dir(build)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("webview2-com-sys-")
        })
        .map(|entry| {
            entry
                .path()
                .join("out")
                .join(arch)
                .join("WebView2Loader.dll")
        })
        .find(|path| path.is_file());

    if let Some(loader) = loader
        && let Err(err) = fs::copy(&loader, profile.join("WebView2Loader.dll"))
    {
        println!(
            "cargo:warning=could not stage {} beside mtui.exe: {err}",
            loader.display()
        );
    }
}
