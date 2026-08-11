//! Compiles MTUI's icon into the Windows binary.
//!
//! Done by hand rather than through one of the crates for it, on the same trade
//! the rest of this project makes about dependencies: the whole job is one call
//! to a resource compiler that already ships with whichever toolchain is
//! building, and the crates that wrap it bring a TOML parser along for the ride.
//!
//! Which compiler depends on the target. The GNU toolchain has `windres`, which
//! emits an object file; MSVC has `rc.exe` from the Windows SDK, which emits a
//! `.res` -- and the SDK does not put it on `PATH` outside a developer prompt,
//! so it gets looked for where it lives. Either output is handed straight to the
//! linker.
//!
//! Not finding a compiler is a warning and not an error. An icon is not worth
//! refusing to build over, and the failure is one an unusual toolchain can hit
//! through no fault of the person building -- so it says what happened, and the
//! binary comes out the same in every respect but the picture on it.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/mtui.rc");
    println!("cargo:rerun-if-changed=assets/mtui.ico");
    println!("cargo:rerun-if-env-changed=RC");
    println!("cargo:rerun-if-env-changed=WINDRES");

    // Every other platform draws no windows and has no notification area, so
    // there is nothing to put an icon on.
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
        Some(path) => {
            // `-bins` and not the unqualified form: the icon belongs to the
            // program, and adding it to every test harness as well would only
            // link the same 47 KB into each of them.
            println!("cargo:rustc-link-arg-bins={}", path.display());
        }
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
}

/// `windres` on the GNU toolchain, which writes a COFF object the linker takes
/// like any other.
fn compile_gnu(out: &Path) -> Option<PathBuf> {
    let object = out.join("mtui-icon.o");
    let windres = env::var_os("WINDRES").unwrap_or_else(|| OsString::from("windres"));

    let status = Command::new(&windres)
        // The script names the icon file bare, so the directory holding it has
        // to be on the search path.
        .args(["-I", "assets", "assets/mtui.rc", "-O", "coff", "-o"])
        .arg(&object)
        .status()
        .ok()?;

    status.success().then_some(object)
}

/// `rc.exe` on MSVC, which writes a `.res` -- also something `link.exe` accepts
/// as an input file.
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

/// Where `rc.exe` is. `PATH` first, since a developer prompt or a cross-build
/// setup will have put it there deliberately, then the SDK's own directory.
fn find_rc() -> Option<PathBuf> {
    if let Some(rc) = env::var_os("RC") {
        return Some(PathBuf::from(rc));
    }
    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return Some(PathBuf::from("rc.exe"));
    }

    // ...\Windows Kits\10\bin\10.0.22621.0\x64\rc.exe -- one directory per SDK
    // version installed, so the newest wins. The arch is the host's, not the
    // target's: this is a tool being run, not something being linked.
    let arch = match env::var("HOST").as_deref() {
        Ok(host) if host.starts_with("aarch64") => "arm64",
        Ok(host) if host.starts_with("i686") => "x86",
        _ => "x64",
    };

    let mut versions: Vec<PathBuf> = ["ProgramFiles(x86)", "ProgramFiles"]
        .iter()
        .filter_map(env::var_os)
        .flat_map(|root| {
            std::fs::read_dir(PathBuf::from(root).join("Windows Kits").join("10").join("bin"))
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
