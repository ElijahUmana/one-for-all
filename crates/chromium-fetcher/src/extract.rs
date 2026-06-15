//! Zip extraction.
//!
//! Owned by `chromium-fetcher`. Streaming extract of a Chrome-for-Testing zip
//! into `<install_root>/<rev>/`. Preserves unix mode bits (the macOS app
//! bundle's `MacOS/Google Chrome for Testing` binary needs `+x`).
//!
//! Idempotent: a `.complete` marker file inside the rev dir signals a
//! prior successful extract; re-running [`extract`] on a complete dir is a
//! no-op. A partial dir (no marker) is wiped before re-extraction so we
//! never end up with a half-laid-out tree.

use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

const COMPLETE_MARKER: &str = ".complete";

/// True if this rev dir has been fully extracted at least once.
pub fn is_complete(rev_dir: &Path) -> bool {
    rev_dir.join(COMPLETE_MARKER).exists()
}

/// Mark a rev dir as complete (atomic-ish: write the marker last).
fn mark_complete(rev_dir: &Path) -> Result<()> {
    std::fs::write(rev_dir.join(COMPLETE_MARKER), b"ok\n")
        .with_context(|| format!("write {COMPLETE_MARKER} in {}", rev_dir.display()))
}

fn rm_rf(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        std::fs::remove_dir_all(path).with_context(|| format!("remove_dir_all {}", path.display()))
    } else {
        std::fs::remove_file(path).with_context(|| format!("remove_file {}", path.display()))
    }
}

/// Extract `zip_path` into `dest_dir`. If `dest_dir` already has a
/// `.complete` marker we skip; if it exists but is incomplete we wipe it
/// first.
pub fn extract(zip_path: &Path, dest_dir: &Path) -> Result<()> {
    if is_complete(dest_dir) {
        tracing::info!(dir = %dest_dir.display(), "extract skip — already complete");
        return Ok(());
    }
    if dest_dir.exists() {
        tracing::warn!(dir = %dest_dir.display(), "removing partial extract dir");
        rm_rf(dest_dir)?;
    }
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("create dest {}", dest_dir.display()))?;

    let f = File::open(zip_path).with_context(|| format!("open zip {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(f)
        .with_context(|| format!("read zip header {}", zip_path.display()))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("zip entry {i} of {}", zip_path.display()))?;

        // Reject absolute or parent-traversing paths (zip-slip).
        let raw_name = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("zip entry {i} has unsafe name"))?;
        let out_path: PathBuf = dest_dir.join(raw_name);
        // Defense in depth: ensure out_path is still within dest_dir.
        let canon_dest = dest_dir
            .canonicalize()
            .unwrap_or_else(|_| dest_dir.to_path_buf());
        if let Some(parent) = out_path.parent() {
            if !parent.starts_with(&canon_dest) && !parent.starts_with(dest_dir) {
                return Err(anyhow!("zip entry {} escapes dest dir", out_path.display()));
            }
        }

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)
                .with_context(|| format!("mkdir {}", out_path.display()))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let mut out =
            File::create(&out_path).with_context(|| format!("create {}", out_path.display()))?;
        io::copy(&mut entry, &mut out).with_context(|| format!("write {}", out_path.display()))?;

        // Preserve unix permissions if present (Mach-O binaries need +x).
        #[cfg(unix)]
        {
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(mode);
                std::fs::set_permissions(&out_path, perms)
                    .with_context(|| format!("chmod {}", out_path.display()))?;
            }
        }
    }

    mark_complete(dest_dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn make_test_zip(zip_path: &Path) {
        let f = File::create(zip_path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o755);
        zw.add_directory("nested/", SimpleFileOptions::default())
            .unwrap();
        zw.start_file("nested/hello.txt", opts).unwrap();
        zw.write_all(b"hi\n").unwrap();
        zw.start_file("top.bin", opts).unwrap();
        zw.write_all(b"\x00\x01\x02").unwrap();
        zw.finish().unwrap();
    }

    #[test]
    fn extract_round_trip_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = tmp.path().join("a.zip");
        let dest = tmp.path().join("rev123");
        make_test_zip(&zip);

        extract(&zip, &dest).unwrap();
        assert!(is_complete(&dest));
        assert_eq!(
            std::fs::read_to_string(dest.join("nested/hello.txt")).unwrap(),
            "hi\n"
        );

        // Idempotent re-run is a no-op.
        let mtime_before = std::fs::metadata(dest.join(".complete"))
            .unwrap()
            .modified()
            .ok();
        extract(&zip, &dest).unwrap();
        let mtime_after = std::fs::metadata(dest.join(".complete"))
            .unwrap()
            .modified()
            .ok();
        assert_eq!(
            mtime_before, mtime_after,
            "second extract should be a no-op"
        );
    }

    #[test]
    fn extract_wipes_partial_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = tmp.path().join("a.zip");
        let dest = tmp.path().join("rev456");
        make_test_zip(&zip);

        // Plant a stale file with no completion marker.
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("stale.txt"), b"junk").unwrap();
        assert!(!is_complete(&dest));

        extract(&zip, &dest).unwrap();
        assert!(!dest.join("stale.txt").exists());
        assert!(dest.join("nested/hello.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn extract_preserves_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let zip = tmp.path().join("a.zip");
        let dest = tmp.path().join("rev789");
        make_test_zip(&zip);
        extract(&zip, &dest).unwrap();
        let mode = std::fs::metadata(dest.join("top.bin"))
            .unwrap()
            .permissions()
            .mode();
        // 0o755 → owner exec bit set.
        assert!(mode & 0o100 != 0, "expected +x on top.bin, got {mode:o}");
    }
}
