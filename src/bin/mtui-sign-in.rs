#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

#[path = "../session_helper.rs"]
mod session_helper;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

const FORCE_ARG: &str = "--clear-session";
const PROFILE_ARG: &str = "--profile";

fn main() -> Result<()> {
    let (profile, force) = arguments()?;
    let header = session_helper::run(profile, force)?;
    println!("{header}");
    Ok(())
}

fn arguments() -> Result<(PathBuf, bool)> {
    let mut args = std::env::args_os().skip(1);
    let mut profile = None;
    let mut force = false;
    while let Some(arg) = args.next() {
        if arg == std::ffi::OsStr::new(PROFILE_ARG) {
            let value = args
                .next()
                .context("the sign-in helper needs a profile path")?;
            profile = Some(PathBuf::from(value));
        } else if arg == FORCE_ARG {
            force = true;
        } else {
            bail!("unknown sign-in helper argument: {}", arg.to_string_lossy());
        }
    }
    let profile = profile.context("the sign-in helper needs a profile path")?;
    Ok((profile, force))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_arguments_are_distinct() {
        assert_ne!(PROFILE_ARG, FORCE_ARG);
    }
}
