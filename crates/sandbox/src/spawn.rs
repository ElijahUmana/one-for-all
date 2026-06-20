//! `sandbox-exec` argv wrapper.
//!
//! `/usr/bin/sandbox-exec -f <profile.sb> -- <binary> <args...>` runs the
//! child under the named profile. We do NOT spawn here — `browser-engine`
//! already owns the spawn path (with `pre_exec` doing fd-3/4 dup2 and
//! `setrlimit`), and that surface is the right place for it. This module
//! just transforms the argv so spawn rolls under sandbox-exec.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::errors::Error;
use crate::Result;

/// Hard-coded path to macOS sandbox-exec. Apple stages the binary here on
/// every release since 10.5; if the path moves we want the failure loud,
/// not a fallback to "no sandboxing".
pub const SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";

/// Transform `(binary, argv)` into the argv that runs them under the named
/// sandbox profile. Caller substitutes the returned `binary` for the
/// original spawn binary and replaces argv.
///
/// Layout: `["sandbox-exec", "-f", "<profile>", "--", "<orig binary>", ...orig args]`.
///
/// We pass the original binary as a positional argument (after `--`) rather
/// than relying on PATH lookup; that matches the documented invocation in
/// `man sandbox-exec`.
pub fn wrap_argv(
    profile: &Path,
    original_binary: &Path,
    original_args: &[OsString],
) -> Result<(PathBuf, Vec<OsString>)> {
    let sb = PathBuf::from(SANDBOX_EXEC_PATH);
    if !sb.exists() {
        return Err(Error::SandboxExecMissing);
    }
    let mut argv: Vec<OsString> = Vec::with_capacity(original_args.len() + 4);
    argv.push(OsString::from("-f"));
    argv.push(profile.as_os_str().to_owned());
    argv.push(OsString::from("--"));
    argv.push(original_binary.as_os_str().to_owned());
    for a in original_args {
        argv.push(a.clone());
    }
    Ok((sb, argv))
}

/// Same as `wrap_argv` but does not check for `sandbox-exec` existence on
/// disk. Used by the unit tests so they can run on a Linux CI host.
#[cfg(test)]
pub(crate) fn wrap_argv_unchecked(
    profile: &Path,
    original_binary: &Path,
    original_args: &[OsString],
) -> (PathBuf, Vec<OsString>) {
    let mut argv: Vec<OsString> = Vec::with_capacity(original_args.len() + 4);
    argv.push(OsString::from("-f"));
    argv.push(profile.as_os_str().to_owned());
    argv.push(OsString::from("--"));
    argv.push(original_binary.as_os_str().to_owned());
    for a in original_args {
        argv.push(a.clone());
    }
    (PathBuf::from(SANDBOX_EXEC_PATH), argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_layout_is_sandbox_exec_dash_f_profile_dashdash_binary_args() {
        let profile = Path::new("/tmp/session.sb");
        let bin = Path::new("/Users/u/.one-for-all/chromium/12345/Chromium");
        let args: Vec<OsString> = vec![
            "--user-data-dir=/tmp/udd".into(),
            "--remote-debugging-pipe".into(),
            "--headless=new".into(),
        ];
        let (new_bin, new_args) = wrap_argv_unchecked(profile, bin, &args);
        assert_eq!(new_bin, PathBuf::from(SANDBOX_EXEC_PATH));
        assert_eq!(new_args.len(), args.len() + 4);
        assert_eq!(new_args[0], OsString::from("-f"));
        assert_eq!(new_args[1], OsString::from("/tmp/session.sb"));
        assert_eq!(new_args[2], OsString::from("--"));
        assert_eq!(
            new_args[3],
            OsString::from("/Users/u/.one-for-all/chromium/12345/Chromium")
        );
        assert_eq!(new_args[4], OsString::from("--user-data-dir=/tmp/udd"));
        assert_eq!(new_args[5], OsString::from("--remote-debugging-pipe"));
        assert_eq!(new_args[6], OsString::from("--headless=new"));
    }

    #[test]
    fn empty_args_still_wraps_correctly() {
        let p = Path::new("/p");
        let b = Path::new("/b");
        let (nb, na) = wrap_argv_unchecked(p, b, &[]);
        assert_eq!(nb, PathBuf::from(SANDBOX_EXEC_PATH));
        assert_eq!(
            na,
            vec![
                OsString::from("-f"),
                OsString::from("/p"),
                OsString::from("--"),
                OsString::from("/b"),
            ]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn live_sandbox_exec_is_available_on_macos() {
        // sandbox-exec ships with macOS; this test will only run on a real
        // macOS host (workspace CI). On Linux the wrap_argv() call would
        // return SandboxExecMissing — the unchecked variant is what the
        // unit tests above exercise.
        let p = Path::new("/tmp/dummy.sb");
        let b = Path::new("/bin/echo");
        let r = wrap_argv(p, b, &[]);
        assert!(r.is_ok(), "/usr/bin/sandbox-exec must exist on macOS");
    }
}
