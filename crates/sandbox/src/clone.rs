//! APFS `clonefile(2)` recursive cloning.
//!
//! macOS gives us a copy-on-write directory clone in a single syscall:
//! `clonefile(src, dst, 0)` clones a file or recursively clones a directory
//! tree, sharing extents until either side writes. This is what makes
//! per-session forks of the user's `~/Library/Application Support/Google/Chrome/Default`
//! cheap (~milliseconds, ~zero disk).
//!
//! ## Why not `nix`?
//!
//! `nix 0.29` (the workspace pin) does not expose `clonefile(2)`, and `libc
//! 0.2.186` does not export it either. Bumping deps to land V3 would be a
//! bigger blast radius than declaring the FFI ourselves; the symbol has
//! been stable on macOS since 10.12 (Sierra, 2016).
//!
//! ## Failure model — explicit, not best-effort
//!
//! Per §12 we surface every failure mode as a distinct enum variant. The
//! caller (broker) chooses how to fall back:
//!
//! - `EXDEV` (cross-volume) → `Error::CloneUnsupported`.
//! - `ENOTSUP` (non-APFS volume) → `Error::CloneUnsupported`.
//! - `EEXIST` (dest exists) → `Error::DestinationExists`.
//! - `ENOENT` (src missing) → `Error::SourceMissing`.
//! - FileVault encrypted-but-unmounted state → `Error::FileVaultBlocked`
//!   (decided by the caller after `detect_filevault()`).

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::errors::Error;
use crate::Result;

/// Statistics returned from a successful clone. Used by the doctor probe and
/// the spawn-latency benchmark.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CloneStats {
    pub bytes_apparent: u64,
    pub file_count: u64,
    pub elapsed_ms: u64,
}

// `<sys/clonefile.h>` — `int clonefile(const char *src, const char *dst, uint32_t flags);`
//
// Available on macOS 10.12+. Returns 0 on success, -1 on error with errno
// set. When `src` is a directory, the implementation in xnu walks the tree
// and clones each entry; the caller does not need to recurse manually.
//
// (We use a regular `//` comment block on the extern below because rustdoc
// doesn't generate docs for extern blocks; the doc comment above is
// informational and lives on this anchor instead.)
#[cfg(target_os = "macos")]
#[link(name = "System", kind = "dylib")]
extern "C" {
    fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
}

/// SAFETY shim for non-macOS builds. The crate's public surface still
/// compiles on Linux so `cargo check --workspace` works on CI; runtime calls
/// hit `Error::CloneUnavailableOnPlatform`.
#[cfg(not(target_os = "macos"))]
unsafe fn clonefile(
    _src: *const libc::c_char,
    _dst: *const libc::c_char,
    _flags: u32,
) -> libc::c_int {
    // Caller-side guard prevents this stub from running.
    -1
}

/// FileVault state surfaced by `fdesetup status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FileVaultState {
    /// Disk is unencrypted. clonefile is fine.
    Off,
    /// Disk is FileVault-encrypted and currently unlocked. clonefile works
    /// while the user is logged in (the volume is decrypted in place).
    OnUnlocked,
    /// Disk is FileVault-encrypted and locked (no current user session).
    /// clonefile of files inside the encrypted volume returns I/O errors.
    /// The broker should fall back to V-R1.
    OnLocked,
    /// `fdesetup` not available (e.g. unit-test host).
    Unknown,
}

/// Recursively clone `src` to `dst`. Both must be on the same APFS volume.
///
/// Pre-conditions enforced for safety: `src` exists, `dst` does not exist,
/// `dst`'s parent directory exists. The latter two are the most common
/// foot-guns (clonefile errors are otherwise opaque `EINVAL`s).
///
/// ## Retry on `EDEADLK`
///
/// `clonefile(2)` against an actively-used directory (Spotlight `mds`,
/// Time Machine, fseventsd, Dropbox / iCloud sync workers — anything
/// holding a metadata lock) can return `EDEADLK` (errno 11, "Resource
/// deadlock avoided"). The kernel does this conservatively to avoid a
/// real deadlock when two processes are reaching for overlapping
/// metadata locks. The retry strategy is exponential backoff
/// (50ms / 100ms / 200ms / 400ms / 800ms) for up to 5 attempts.
///
/// On EDEADLK, we DO NOT crawl per-file — that's `clone_tree_with_fallback`'s
/// job. This entry point is "atomic clone or surface the error";
/// `clone_user_dirs` decides when to escalate.
pub fn clone_tree(src: &Path, dst: &Path) -> Result<CloneStats> {
    let start = std::time::Instant::now();
    if !src.exists() {
        return Err(Error::SourceMissing(src.to_path_buf()));
    }
    if dst.exists() {
        return Err(Error::DestinationExists(dst.to_path_buf()));
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (src, dst);
        return Err(Error::CloneUnavailableOnPlatform);
    }

    #[cfg(target_os = "macos")]
    {
        clonefile_with_retry(src, dst)?;
    }

    let stats = walk_stats(dst)?;
    Ok(CloneStats {
        bytes_apparent: stats.0,
        file_count: stats.1,
        elapsed_ms: start.elapsed().as_millis() as u64,
    })
}

/// Backoff schedule for `EDEADLK` retries. Exposed for tests.
const EDEADLK_BACKOFFS_MS: &[u64] = &[50, 100, 200, 400, 800];

/// Wrapper around the raw `clonefile(2)` syscall that retries on
/// `EDEADLK` with exponential backoff and converts the final errno to
/// our typed `Error`.
#[cfg(target_os = "macos")]
fn clonefile_with_retry(src: &Path, dst: &Path) -> Result<()> {
    let c_src = CString::new(src.as_os_str().as_bytes())
        .map_err(|_| Error::SourceMissing(src.to_path_buf()))?;
    let c_dst = CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| Error::DestinationExists(dst.to_path_buf()))?;

    let mut attempt: usize = 0;
    loop {
        // SAFETY: pointers are valid for the duration of the call
        // (CStrings are stack-rooted), `flags = 0` is the documented
        // default.
        let rc = unsafe { clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
        if rc == 0 {
            if attempt > 0 {
                tracing::info!(
                    src = %src.display(),
                    attempts = attempt + 1,
                    "clonefile succeeded after EDEADLK retry"
                );
            }
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        let raw = err.raw_os_error().unwrap_or(0);
        if raw == libc::EDEADLK && attempt < EDEADLK_BACKOFFS_MS.len() {
            let sleep_ms = EDEADLK_BACKOFFS_MS[attempt];
            tracing::warn!(
                src = %src.display(),
                attempt = attempt + 1,
                max = EDEADLK_BACKOFFS_MS.len() + 1,
                sleep_ms,
                "clonefile EDEADLK; backing off (likely Spotlight/TimeMachine/fseventsd contention)"
            );
            std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
            attempt += 1;
            continue;
        }
        return Err(match raw {
            libc::EXDEV | libc::ENOTSUP => Error::CloneUnsupported {
                src: src.to_path_buf(),
                dst: dst.to_path_buf(),
            },
            libc::EEXIST => Error::DestinationExists(dst.to_path_buf()),
            libc::ENOENT => Error::SourceMissing(src.to_path_buf()),
            libc::EDEADLK => Error::CloneRetriesExhausted {
                src: src.to_path_buf(),
                dst: dst.to_path_buf(),
                attempts: attempt + 1,
            },
            _ => Error::io(src, err),
        });
    }
}

/// Try `clone_tree`; on directory-level failure modes (EDEADLK after
/// retries, EXDEV/ENOTSUP) walk the source and clone PER FILE. Falls
/// back further to `cp -cR` (which uses APFS file-level clones via
/// `copyfile` even when the directory-level clonefile fails) if the
/// per-file walk also stalls.
///
/// Returns the same `CloneStats` shape so the caller cannot tell which
/// path was taken; `tracing::info!` records the outcome at INFO level.
///
/// # Per-path timeout
///
/// `max_duration` bounds the entire fallback chain; if we hit it the
/// function returns whatever stats it accumulated and a warn-level log
/// line. We never block forever on a single allowlist entry.
pub fn clone_tree_with_fallback(
    src: &Path,
    dst: &Path,
    max_duration: std::time::Duration,
) -> Result<CloneStats> {
    let started = std::time::Instant::now();
    match clone_tree(src, dst) {
        Ok(s) => return Ok(s),
        Err(Error::DestinationExists(_)) => {
            return Err(Error::DestinationExists(dst.to_path_buf()))
        }
        Err(Error::SourceMissing(_)) => return Err(Error::SourceMissing(src.to_path_buf())),
        Err(e) => {
            tracing::warn!(
                src = %src.display(),
                error = %e,
                "directory-level clone failed; falling back to per-file clone walk"
            );
        }
    }

    // Per-file walk. mkdir -p each subdirectory, clonefile each regular
    // file. Less concurrent surface than the dir-level call, so EDEADLK
    // is far less likely.
    match per_file_clone_walk(src, dst, started, max_duration) {
        Ok(s) => {
            tracing::info!(
                src = %src.display(),
                files = s.file_count,
                bytes = s.bytes_apparent,
                "per-file clone walk succeeded"
            );
            return Ok(s);
        }
        Err(e) => {
            tracing::warn!(
                src = %src.display(),
                error = %e,
                "per-file clone walk failed; falling back to `cp -cR`"
            );
        }
    }

    // Last resort: shell out to `cp -cR`. The `-c` flag (macOS-specific,
    // since 10.12) tells cp to use APFS clones at file level via
    // `copyfile(3)`, so the COW story is preserved even though we're
    // not driving clonefile ourselves.
    cp_c_r_fallback(src, dst, started, max_duration)
}

/// Per-file clone walk. Recursively walks `src`, mkdir's each subdir
/// at the corresponding location under `dst`, clonefiles each file.
/// Honours `started + max_duration` as a hard cutoff.
fn per_file_clone_walk(
    src: &Path,
    dst: &Path,
    started: std::time::Instant,
    max_duration: std::time::Duration,
) -> Result<CloneStats> {
    use std::collections::VecDeque;
    if dst.exists() {
        return Err(Error::DestinationExists(dst.to_path_buf()));
    }
    std::fs::create_dir_all(dst).map_err(|e| Error::io(dst, e))?;
    let mut bytes: u64 = 0;
    let mut files: u64 = 0;
    let mut q: VecDeque<(PathBuf, PathBuf)> = VecDeque::new();
    q.push_back((src.to_path_buf(), dst.to_path_buf()));
    while let Some((s, d)) = q.pop_front() {
        if started.elapsed() > max_duration {
            tracing::warn!(
                src = %src.display(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "per-file clone hit max_duration; returning partial stats"
            );
            break;
        }
        let md = match std::fs::symlink_metadata(&s) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.file_type().is_dir() {
            if !d.exists() {
                std::fs::create_dir_all(&d).map_err(|e| Error::io(&d, e))?;
            }
            let rd = match std::fs::read_dir(&s) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for ent in rd.flatten() {
                let name = ent.file_name();
                q.push_back((ent.path(), d.join(&name)));
            }
        } else if md.file_type().is_file() {
            // file-level clonefile; on contention return a typed error
            // and let the caller decide.
            #[cfg(target_os = "macos")]
            {
                if let Err(e) = clonefile_with_retry(&s, &d) {
                    // Per-file failure is not fatal — log and continue;
                    // we'd rather get 99% of the agent's home than 0%.
                    tracing::debug!(
                        src = %s.display(),
                        error = %e,
                        "per-file clonefile skipped (continuing walk)"
                    );
                    continue;
                }
            }
            files += 1;
            bytes = bytes.saturating_add(md.len());
        } else if md.file_type().is_symlink() {
            // Recreate symlinks verbatim. Not security-sensitive: the
            // sandbox profile constrains where the agent can read; a
            // dangling symlink is no worse than a missing file.
            if let Ok(target) = std::fs::read_link(&s) {
                let _ = std::os::unix::fs::symlink(&target, &d);
            }
        }
    }
    Ok(CloneStats {
        bytes_apparent: bytes,
        file_count: files,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// `cp -cR` fallback. The `-c` flag asks `cp` to use APFS clonefile at
/// the file level via `copyfile(3, COPYFILE_CLONE)`, preserving the COW
/// story when the direct dir-level clonefile fails.
///
/// If `cp -cR` itself fails (notably `Cross-device link` when the source
/// profile lives on another volume), fall back again to `/usr/bin/ditto` for
/// a byte-preserving recursive copy. That second stage drops the COW story,
/// but keeps full profile correctness instead of forcing the broker down to
/// the cookie-only V-R1 seed-plan path.
fn cp_c_r_fallback(
    src: &Path,
    dst: &Path,
    started: std::time::Instant,
    max_duration: std::time::Duration,
) -> Result<CloneStats> {
    if dst.exists() {
        return Err(Error::DestinationExists(dst.to_path_buf()));
    }
    let remaining = max_duration.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(Error::CloneRetriesExhausted {
            src: src.to_path_buf(),
            dst: dst.to_path_buf(),
            attempts: usize::MAX,
        });
    }
    // We don't have a great cross-platform timeout-bounded process
    // launcher in std; we run the copy tools synchronously and rely on the
    // per-path budget being generous (~10s per allowlist entry).
    let cp_out = Command::new("/bin/cp")
        .arg("-cR")
        .arg(src)
        .arg(dst)
        .output()
        .map_err(|e| Error::io(src, e))?;
    if !cp_out.status.success() {
        tracing::warn!(
            src = %src.display(),
            dst = %dst.display(),
            stderr = %String::from_utf8_lossy(&cp_out.stderr),
            "cp -cR fallback failed; trying ditto copy"
        );
        let ditto_out = Command::new("/usr/bin/ditto")
            .arg(src)
            .arg(dst)
            .output()
            .map_err(|e| Error::io(src, e))?;
        if !ditto_out.status.success() {
            return Err(Error::Io {
                path: dst.to_path_buf(),
                source: std::io::Error::other(format!(
                    "cp -cR exited {} (stderr: {}); ditto exited {} (stderr: {})",
                    cp_out.status,
                    String::from_utf8_lossy(&cp_out.stderr),
                    ditto_out.status,
                    String::from_utf8_lossy(&ditto_out.stderr)
                )),
            });
        }
    }
    let stats = walk_stats(dst)?;
    Ok(CloneStats {
        bytes_apparent: stats.0,
        file_count: stats.1,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

const CHROME_PROFILE_CLONE_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

fn chrome_user_data_root() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(
        home.join("Library")
            .join("Application Support")
            .join("Google")
            .join("Chrome"),
    )
}

fn push_unique_profile_name(out: &mut Vec<String>, name: &str) {
    let trimmed = name.trim();
    if trimmed.is_empty() || out.iter().any(|existing| existing == trimmed) {
        return;
    }
    out.push(trimmed.to_string());
}

fn preferred_profile_names_from_local_state(chrome_root: &Path) -> Vec<String> {
    let local_state_path = chrome_root.join("Local State");
    let bytes = match std::fs::read(&local_state_path) {
        Ok(bytes) => bytes,
        Err(_) => return Vec::new(),
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let Some(profile) = value.get("profile") else {
        return Vec::new();
    };

    let mut out = Vec::new();
    if let Some(last_used) = profile.get("last_used").and_then(serde_json::Value::as_str) {
        push_unique_profile_name(&mut out, last_used);
    }
    for key in ["last_active_profiles", "profiles_order"] {
        let Some(entries) = profile.get(key).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for entry in entries {
            if let Some(name) = entry.as_str() {
                push_unique_profile_name(&mut out, name);
            }
        }
    }
    out
}

fn default_chrome_profile_path_from(chrome_root: &Path) -> Option<PathBuf> {
    for name in preferred_profile_names_from_local_state(chrome_root) {
        let candidate = chrome_root.join(&name);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }

    let default = chrome_root.join("Default");
    if default.is_dir() {
        return Some(default);
    }

    let mut candidates: Vec<PathBuf> = std::fs::read_dir(chrome_root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name == "Default" || name.starts_with("Profile "))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    candidates.into_iter().next()
}

/// Resolve the user's host Chrome profile path.
///
/// Prefers the active profile recorded in `Local State`
/// (`profile.last_used`/`last_active_profiles`/`profiles_order`), then falls
/// back to `Default`, then to the first `Profile *` directory on disk.
/// Returns `None` if the home directory is unresolvable or no Chrome profile
/// directory exists — in either case the broker should skip Chrome
/// inheritance and proceed with an empty UDD.
pub fn default_chrome_profile_path() -> Option<PathBuf> {
    let chrome_root = chrome_user_data_root()?;
    default_chrome_profile_path_from(&chrome_root)
}

fn clone_chrome_profile_via<F>(
    host_profile: &Path,
    dest_udd: &Path,
    clone_fn: F,
) -> Result<CloneStats>
where
    F: FnOnce(&Path, &Path) -> Result<CloneStats>,
{
    let dest = dest_udd.join("Default");
    std::fs::create_dir_all(dest_udd).map_err(|e| Error::io(dest_udd, e))?;
    match clone_fn(host_profile, &dest) {
        Ok(stats) => Ok(stats),
        Err(e @ Error::DestinationExists(_)) => Err(e),
        Err(e @ Error::SourceMissing(_)) => Err(e),
        Err(e @ Error::CloneUnsupported { .. } | e @ Error::CloneRetriesExhausted { .. }) => {
            tracing::warn!(
                src = %host_profile.display(),
                dst = %dest.display(),
                error = %e,
                "chrome profile clonefile path failed; trying lossless copy fallback"
            );
            match cp_c_r_fallback(
                host_profile,
                &dest,
                std::time::Instant::now(),
                CHROME_PROFILE_CLONE_BUDGET,
            ) {
                Ok(stats) => {
                    tracing::info!(
                        src = %host_profile.display(),
                        dst = %dest.display(),
                        files = stats.file_count,
                        bytes = stats.bytes_apparent,
                        elapsed_ms = stats.elapsed_ms,
                        "chrome profile copied into session UDD after clonefile fallback"
                    );
                    Ok(stats)
                }
                Err(copy_err) => {
                    tracing::warn!(
                        src = %host_profile.display(),
                        dst = %dest.display(),
                        error = %copy_err,
                        "chrome profile copy fallback failed; returning original clone error"
                    );
                    Err(e)
                }
            }
        }
        Err(e) => Err(e),
    }
}

/// Clone the user's active Chrome profile into `dest_udd`. The destination is
/// the per-session `--user-data-dir` Chromium will be told to use, so it must
/// end with the path Chromium expects to be the *parent* of `Default/`.
///
/// In practice, `dest_udd` is `~/.one-for-all/sessions/<id>/` and we clone
/// the resolved host profile (`Default`, `Profile 6`, etc.) into
/// `dest_udd/Default` so Chromium spawned with `--user-data-dir=dest_udd`
/// finds the inherited state under `dest_udd/Default/`.
pub fn clone_chrome_profile(host_profile: &Path, dest_udd: &Path) -> Result<CloneStats> {
    clone_chrome_profile_via(host_profile, dest_udd, clone_tree)
}

/// Per-allowlist-entry budget for `clone_user_dirs`. The team-lead's
/// real-host failure shows ~/Documents can be 50GB+; we don't want a
/// single slow path to wedge `session.register`.
const PER_PATH_CLONE_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Clone every entry in `allowlist` into `dest_root`. RW vs RO mode is a
/// sandbox-exec concern (handled by `sbpl.rs`); the *clone itself* is always
/// a writable copy because clonefile produces independent copies once
/// touched.
///
/// Resilience contract:
/// - **EDEADLK** retried with exponential backoff inside `clonefile_with_retry`.
/// - **Dir-level failure** falls back to per-file walk + `cp -cR`.
/// - **Per-path timeout** (10s) — exceeded paths are skipped with a
///   `warn!` log, not failed.
/// - **Failed paths are skipped** rather than aborting the whole
///   register: the agent gets *partial* inheritance, broker
///   `session.register` still succeeds.
pub fn clone_user_dirs(
    allowlist: &[crate::inherit::InheritSpec],
    dest_root: &Path,
) -> Result<Vec<(crate::inherit::InheritSpec, CloneStats)>> {
    std::fs::create_dir_all(dest_root).map_err(|e| Error::io(dest_root, e))?;
    let mut out = Vec::with_capacity(allowlist.len());
    for spec in allowlist {
        let src = &spec.host_path;
        if !src.exists() {
            tracing::debug!(src = %src.display(), "clone_user_dirs: skipping missing source");
            continue;
        }
        // Mirror the host path's last component into the dest root.
        let leaf = match src.file_name() {
            Some(l) => l,
            None => {
                tracing::warn!(src = %src.display(), "clone_user_dirs: source has no leaf component; skipping");
                continue;
            }
        };
        let dst = dest_root.join(leaf);
        match clone_tree_with_fallback(src, &dst, PER_PATH_CLONE_BUDGET) {
            Ok(s) => out.push((spec.clone(), s)),
            Err(Error::DestinationExists(_)) => {
                tracing::debug!(dst = %dst.display(), "destination already populated; skipping");
            }
            Err(e) => {
                // Single-path failure no longer fails the whole register —
                // log loudly, continue with the rest of the allowlist.
                tracing::warn!(
                    src = %src.display(),
                    dst = %dst.display(),
                    error = %e,
                    "clone_user_dirs: skipping path after retry+fallback failure"
                );
            }
        }
    }
    Ok(out)
}

/// Detect FileVault state by parsing `fdesetup status`.
///
/// Output strings (per `man fdesetup`):
/// - `FileVault is Off.`
/// - `FileVault is On.`
/// - `FileVault is On and Deferred enabled.`
///
/// "Locked" state is only meaningful before the user logs in; in a normal
/// agent runtime the volume is unlocked. We map any "On" + the user's home
/// being readable to `OnUnlocked`; "On" + home unreadable to `OnLocked`.
pub fn detect_filevault() -> FileVaultState {
    detect_filevault_via(|| {
        Command::new("/usr/bin/fdesetup")
            .arg("status")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
    })
}

/// Pure helper for unit testing `detect_filevault`. The closure returns the
/// raw `fdesetup status` output (or `None` if unavailable).
pub(crate) fn detect_filevault_via<F: FnOnce() -> Option<String>>(reader: F) -> FileVaultState {
    match reader() {
        None => FileVaultState::Unknown,
        Some(s) => {
            let t = s.trim();
            if t.starts_with("FileVault is Off") {
                FileVaultState::Off
            } else if t.starts_with("FileVault is On") {
                // We can't tell unlocked-vs-locked from `fdesetup` alone in
                // every release. Probe by trying to read $HOME — if it fails
                // we're locked.
                let home_readable = dirs::home_dir()
                    .map(|h| std::fs::read_dir(&h).is_ok())
                    .unwrap_or(false);
                if home_readable {
                    FileVaultState::OnUnlocked
                } else {
                    FileVaultState::OnLocked
                }
            } else {
                FileVaultState::Unknown
            }
        }
    }
}

/// Recursively count files + sum apparent sizes under `root`. Used purely
/// for stats — clone success doesn't depend on it. Errors during the walk
/// are absorbed (we still return what we managed to count).
fn walk_stats(root: &Path) -> Result<(u64, u64)> {
    let mut bytes: u64 = 0;
    let mut count: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let md = match std::fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.file_type().is_dir() {
            let rd = match std::fs::read_dir(&p) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for ent in rd.flatten() {
                stack.push(ent.path());
            }
        } else if md.file_type().is_file() {
            count += 1;
            bytes = bytes.saturating_add(md.len());
        }
    }
    Ok((bytes, count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[cfg(target_os = "macos")]
    #[test]
    fn clones_a_directory_tree_byte_for_byte() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("nested/deep")).expect("mkdir");
        let mut f = fs::File::create(src.join("nested/a.txt")).expect("create a");
        f.write_all(b"alpha").expect("write");
        let mut g = fs::File::create(src.join("nested/deep/b.bin")).expect("create b");
        g.write_all(&[0u8, 1, 2, 3, 4]).expect("write");

        let dst = tmp.path().join("dst");
        let stats = clone_tree(&src, &dst).expect("clone_tree");
        assert!(
            stats.file_count >= 2,
            "expected ≥2 files, got {}",
            stats.file_count
        );

        assert_eq!(
            fs::read(dst.join("nested/a.txt")).expect("read a"),
            b"alpha"
        );
        assert_eq!(
            fs::read(dst.join("nested/deep/b.bin")).expect("read b"),
            vec![0u8, 1, 2, 3, 4]
        );

        // Inodes must differ — clonefile creates an independent inode that
        // shares extents COW-style; it is NOT a hardlink.
        use std::os::unix::fs::MetadataExt;
        let m_src = fs::metadata(src.join("nested/a.txt")).expect("md src");
        let m_dst = fs::metadata(dst.join("nested/a.txt")).expect("md dst");
        assert_ne!(
            m_src.ino(),
            m_dst.ino(),
            "clonefile must produce a distinct inode"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn clones_a_single_file() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let src = tmp.path().join("only.txt");
        fs::write(&src, b"single").expect("write");
        let dst = tmp.path().join("clone.txt");
        let _ = clone_tree(&src, &dst).expect("clone single file");
        assert_eq!(fs::read(&dst).expect("read clone"), b"single");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refuses_to_overwrite_existing_dest() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let src = tmp.path().join("s");
        fs::write(&src, b"hi").expect("write s");
        let dst = tmp.path().join("d");
        fs::write(&dst, b"already-here").expect("write d");
        let err = clone_tree(&src, &dst).expect_err("must refuse overwrite");
        match err {
            Error::DestinationExists(p) => assert_eq!(p, dst),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refuses_missing_source() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let src = tmp.path().join("nope");
        let dst = tmp.path().join("dst");
        let err = clone_tree(&src, &dst).expect_err("must refuse missing src");
        assert!(matches!(err, Error::SourceMissing(_)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fast_clone_under_100ms_for_1000_small_files() {
        // Synthesizes a 1k-file Chrome-shaped tree (cookies.db sized files
        // dominated by small <512B blobs), clones it, asserts < 500ms with
        // 5x slack so CI on slow loaner hardware doesn't flake. The product
        // SLO is < 100ms; this guards a 5x regression.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let src = tmp.path().join("profile");
        fs::create_dir_all(&src).expect("mk src");
        for i in 0..1000u32 {
            let p = src.join(format!("file_{i:04}.bin"));
            fs::write(&p, [b'x'; 256]).expect("write");
        }
        let dst = tmp.path().join("clone");
        let stats = clone_tree(&src, &dst).expect("clone_tree");
        assert_eq!(stats.file_count, 1000);
        assert!(
            stats.elapsed_ms < 500,
            "1000-file clone took {}ms (want < 500 to guard 5x regression vs 100ms SLO)",
            stats.elapsed_ms
        );
    }

    #[test]
    fn detect_filevault_parses_off() {
        let s = detect_filevault_via(|| Some("FileVault is Off.\n".into()));
        assert_eq!(s, FileVaultState::Off);
    }

    #[test]
    fn detect_filevault_parses_on() {
        let s = detect_filevault_via(|| Some("FileVault is On.\n".into()));
        // Whether the decision lands on OnUnlocked vs OnLocked depends on
        // host state; the parser is not what we're asserting here, only that
        // we recognise "On".
        assert!(matches!(
            s,
            FileVaultState::OnUnlocked | FileVaultState::OnLocked
        ));
    }

    #[test]
    fn detect_filevault_unknown_when_unavailable() {
        let s = detect_filevault_via(|| None);
        assert_eq!(s, FileVaultState::Unknown);
    }

    #[test]
    fn detect_filevault_handles_garbage() {
        let s = detect_filevault_via(|| Some("explosions".into()));
        assert_eq!(s, FileVaultState::Unknown);
    }

    #[test]
    fn clone_chrome_profile_preserves_non_cookie_artifacts() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let src = tmp.path().join("profile");
        fs::create_dir_all(src.join("Network")).expect("mk network");
        fs::create_dir_all(src.join("Local Storage/leveldb")).expect("mk local storage");
        fs::create_dir_all(src.join("Service Worker/CacheStorage")).expect("mk cache storage");
        fs::write(src.join("Preferences"), b"prefs").expect("write prefs");
        fs::write(src.join("History"), b"history").expect("write history");
        fs::write(src.join("Network/Cookies"), b"cookies").expect("write cookies");
        fs::write(src.join("Local Storage/leveldb/LOG"), b"leveldb-log")
            .expect("write local storage log");
        fs::write(
            src.join("Service Worker/CacheStorage/index"),
            b"cache-index",
        )
        .expect("write cache index");

        let dst_udd = tmp.path().join("session_udd");
        let stats = clone_chrome_profile(&src, &dst_udd).expect("clone chrome profile");
        assert!(
            stats.file_count >= 5,
            "expected multiple profile artifacts to survive clone"
        );
        let dst = dst_udd.join("Default");
        assert_eq!(
            fs::read(dst.join("Preferences")).expect("read prefs"),
            b"prefs"
        );
        assert_eq!(
            fs::read(dst.join("History")).expect("read history"),
            b"history"
        );
        assert_eq!(
            fs::read(dst.join("Network/Cookies")).expect("read cookies"),
            b"cookies"
        );
        assert_eq!(
            fs::read(dst.join("Local Storage/leveldb/LOG")).expect("read local storage log"),
            b"leveldb-log"
        );
        assert_eq!(
            fs::read(dst.join("Service Worker/CacheStorage/index")).expect("read cache index"),
            b"cache-index"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cp_c_r_fallback_cross_device_uses_ditto() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let img = tmp.path().join("test.dmg");
        let mount = tmp.path().join("mount");
        let dest = tmp.path().join("dest");
        fs::create_dir_all(&mount).expect("mk mount");
        fs::create_dir_all(&dest).expect("mk dest");

        let create = Command::new("/usr/bin/hdiutil")
            .args([
                "create", "-size", "16m", "-fs", "HFS+", "-volname", "CBTEST",
            ])
            .arg(&img)
            .output()
            .expect("hdiutil create");
        assert!(
            create.status.success(),
            "hdiutil create failed: {}",
            String::from_utf8_lossy(&create.stderr)
        );

        let attach = Command::new("/usr/bin/hdiutil")
            .args(["attach", "-nobrowse", "-mountpoint"])
            .arg(&mount)
            .arg(&img)
            .output()
            .expect("hdiutil attach");
        assert!(
            attach.status.success(),
            "hdiutil attach failed: {}",
            String::from_utf8_lossy(&attach.stderr)
        );

        struct MountGuard(PathBuf);
        impl Drop for MountGuard {
            fn drop(&mut self) {
                let _ = Command::new("/usr/bin/hdiutil")
                    .arg("detach")
                    .arg(&self.0)
                    .output();
            }
        }
        let _guard = MountGuard(mount.clone());

        let src = mount.join("srcdir");
        fs::create_dir_all(src.join("sub")).expect("mk src");
        fs::write(src.join("file.txt"), b"hi").expect("write file");
        fs::write(src.join("sub/other.txt"), b"there").expect("write nested file");

        let started = std::time::Instant::now();
        let stats = cp_c_r_fallback(
            &src,
            &dest.join("copy"),
            started,
            std::time::Duration::from_secs(30),
        )
        .expect("cp/ditto fallback");
        assert!(
            stats.file_count >= 2,
            "expected copied files after ditto fallback"
        );
        assert_eq!(
            fs::read(dest.join("copy/sub/other.txt")).expect("read copied nested file"),
            b"there"
        );
    }

    #[test]
    fn clone_chrome_profile_falls_back_after_clone_unsupported() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let src = tmp.path().join("profile");
        fs::create_dir_all(src.join("Network")).expect("mk network");
        fs::write(src.join("Preferences"), b"prefs").expect("write prefs");
        fs::write(src.join("Network/Cookies"), b"cookies").expect("write cookies");

        let dst_udd = tmp.path().join("session_udd");
        let stats = clone_chrome_profile_via(&src, &dst_udd, |host, dest| {
            Err(Error::CloneUnsupported {
                src: host.to_path_buf(),
                dst: dest.to_path_buf(),
            })
        })
        .expect("clone fallback");
        assert!(
            stats.file_count >= 2,
            "copy fallback should preserve profile artifacts"
        );
        let dst = dst_udd.join("Default");
        assert_eq!(
            fs::read(dst.join("Preferences")).expect("read prefs"),
            b"prefs"
        );
        assert_eq!(
            fs::read(dst.join("Network/Cookies")).expect("read cookies"),
            b"cookies"
        );
    }

    #[test]
    fn default_chrome_profile_path_from_prefers_local_state_priority() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let chrome_root = tmp.path().join("Chrome");
        fs::create_dir_all(chrome_root.join("Profile 1")).expect("mk profile1");
        fs::create_dir_all(chrome_root.join("Profile 6")).expect("mk profile6");
        fs::write(
            chrome_root.join("Local State"),
            r#"{
              "profile": {
                "last_used": "Profile 6",
                "last_active_profiles": ["Profile 6", "Profile 1"],
                "profiles_order": ["Profile 1", "Profile 6"]
              }
            }"#,
        )
        .expect("write local state");
        let names = preferred_profile_names_from_local_state(&chrome_root);
        assert_eq!(
            names,
            vec!["Profile 6".to_string(), "Profile 1".to_string()]
        );
        let resolved = default_chrome_profile_path_from(&chrome_root).expect("resolved profile");
        assert_eq!(resolved, chrome_root.join("Profile 6"));
    }

    #[test]
    fn default_chrome_profile_path_from_falls_back_to_default_then_profile_dirs() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let chrome_root = tmp.path().join("Chrome");
        fs::create_dir_all(chrome_root.join("Default")).expect("mk default");
        fs::create_dir_all(chrome_root.join("Profile 2")).expect("mk profile2");
        let resolved = default_chrome_profile_path_from(&chrome_root).expect("resolved default");
        assert_eq!(resolved, chrome_root.join("Default"));

        fs::remove_dir_all(chrome_root.join("Default")).expect("rm default");
        fs::create_dir_all(chrome_root.join("Profile 7")).expect("mk profile7");
        let resolved =
            default_chrome_profile_path_from(&chrome_root).expect("resolved profile dir");
        assert_eq!(resolved, chrome_root.join("Profile 2"));
    }

    #[test]
    fn preferred_profile_names_from_local_state_dedupes_and_preserves_priority() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let chrome_root = tmp.path().join("Chrome");
        fs::create_dir_all(&chrome_root).expect("mk root");
        fs::write(
            chrome_root.join("Local State"),
            r#"{
              "profile": {
                "last_used": "Profile 6",
                "last_active_profiles": ["Profile 6", "Profile 1"],
                "profiles_order": ["Profile 1", "Profile 6", "Profile 3"]
              }
            }"#,
        )
        .expect("write local state");
        let names = preferred_profile_names_from_local_state(&chrome_root);
        assert_eq!(
            names,
            vec![
                "Profile 6".to_string(),
                "Profile 1".to_string(),
                "Profile 3".to_string(),
            ]
        );
    }
}
