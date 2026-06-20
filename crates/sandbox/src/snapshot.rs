//! `ofa snapshot` — git-stash for browser sessions.
//!
//! Saves the entire session UDD to a named on-disk archive via APFS
//! `clonefile(2)` so the operation is O(1) and disk-cheap. Restoring is
//! the inverse: clonefile the saved tree back into a fresh session UDD.
//!
//! ## Layout
//!
//! `~/.one-for-all/snapshots/<name>/` — top-level snapshot dir.
//!   ├── meta.json        — name, source session id, created_at
//!   ├── rootfs/          — clonefile of the session UDD at snapshot time
//!   └── sandbox.sb.copy  — the .sb profile, copied (not cloned) for
//!                          inspection without privilege escalation.
//!
//! ## Why a separate tool, not just `ofa merge`
//!
//! `ofa merge` promotes selected paths back to the host. Snapshot is the
//! orthogonal operation: capture the full session state for later replay
//! against a NEW session. The two do not overlap; we keep them in
//! separate binaries to keep argv shapes simple.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::clone::clone_tree;
use crate::errors::Error;
use crate::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub name: String,
    pub source_session_id: String,
    /// Caller-supplied — we don't call SystemTime::now ourselves to keep
    /// the unit tests deterministic. Production callers stamp this with
    /// `SystemTime::now()` before invoking `take_snapshot`.
    pub created_at_unix_ms: u64,
    pub bytes_apparent: u64,
    pub file_count: u64,
}

/// Resolve the snapshot root for a given name. Layout:
/// `~/.one-for-all/snapshots/<name>/`.
pub fn snapshot_root(name: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or(Error::HomeDirUnresolvable)?;
    Ok(home.join(".one-for-all").join("snapshots").join(name))
}

/// Capture `session_rootfs` into the named snapshot. Refuses to clobber
/// an existing snapshot (use a different name or `ofa snapshot rm <name>`).
pub fn take_snapshot(
    session_rootfs: &Path,
    snapshot_dir: &Path,
    meta: &mut SnapshotMeta,
) -> Result<()> {
    if snapshot_dir.exists() {
        return Err(Error::DestinationExists(snapshot_dir.to_path_buf()));
    }
    std::fs::create_dir_all(snapshot_dir).map_err(|e| Error::io(snapshot_dir, e))?;

    // Clonefile the rootfs.
    let rootfs_dst = snapshot_dir.join("rootfs");
    let stats = clone_tree(session_rootfs, &rootfs_dst)?;
    meta.bytes_apparent = stats.bytes_apparent;
    meta.file_count = stats.file_count;

    // Copy (not clone) the .sb profile so it's readable without sudoing
    // through the rootfs ownership.
    let sb_src = session_rootfs.join("sandbox.sb");
    let sb_dst = snapshot_dir.join("sandbox.sb.copy");
    if sb_src.exists() {
        std::fs::copy(&sb_src, &sb_dst).map_err(|e| Error::io(&sb_dst, e))?;
    }

    // Persist meta.
    let meta_path = snapshot_dir.join("meta.json");
    let meta_json = serde_json::to_vec_pretty(meta)
        .map_err(|e| Error::io(&meta_path, std::io::Error::other(e)))?;
    std::fs::write(&meta_path, meta_json).map_err(|e| Error::io(&meta_path, e))?;
    Ok(())
}

/// Restore a previously-taken snapshot into a fresh session UDD.
/// `target_session_rootfs` must NOT exist.
pub fn restore_snapshot(snapshot_dir: &Path, target_session_rootfs: &Path) -> Result<()> {
    if !snapshot_dir.exists() {
        return Err(Error::SourceMissing(snapshot_dir.to_path_buf()));
    }
    if target_session_rootfs.exists() {
        return Err(Error::DestinationExists(
            target_session_rootfs.to_path_buf(),
        ));
    }
    let src_rootfs = snapshot_dir.join("rootfs");
    if !src_rootfs.exists() {
        return Err(Error::SourceMissing(src_rootfs));
    }
    clone_tree(&src_rootfs, target_session_rootfs).map(|_| ())
}

/// Read a snapshot's metadata. Returns `Ok(None)` if the name is unknown.
pub fn read_snapshot_meta(snapshot_dir: &Path) -> Result<Option<SnapshotMeta>> {
    let p = snapshot_dir.join("meta.json");
    if !p.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&p).map_err(|e| Error::io(&p, e))?;
    let meta: SnapshotMeta =
        serde_json::from_slice(&bytes).map_err(|e| Error::io(&p, std::io::Error::other(e)))?;
    Ok(Some(meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn snapshot_round_trip_preserves_files() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        // Synthesize a session rootfs.
        let session = tmp.path().join("session");
        std::fs::create_dir_all(session.join("Default")).expect("mk");
        std::fs::write(session.join("Default/Cookies"), b"COOKIES").expect("write");
        std::fs::write(session.join("Default/History"), b"HIST").expect("write");
        std::fs::write(session.join("sandbox.sb"), b"(version 1)").expect("write");

        // Take snapshot.
        let snap_dir = tmp.path().join("snap");
        let mut meta = SnapshotMeta {
            name: "before-experiment".into(),
            source_session_id: "s_test".into(),
            created_at_unix_ms: 1_700_000_000_000,
            bytes_apparent: 0,
            file_count: 0,
        };
        take_snapshot(&session, &snap_dir, &mut meta).expect("take");
        assert!(snap_dir.join("rootfs/Default/Cookies").exists());
        assert!(snap_dir.join("sandbox.sb.copy").exists());
        let read = read_snapshot_meta(&snap_dir).expect("ok").expect("some");
        assert_eq!(read.name, "before-experiment");
        assert!(read.file_count >= 3);

        // Mutate session post-snapshot.
        std::fs::write(session.join("Default/Cookies"), b"AFTER").expect("write");
        // Restore into a NEW session UDD.
        let restored = tmp.path().join("session_restored");
        restore_snapshot(&snap_dir, &restored).expect("restore");
        assert_eq!(
            std::fs::read(restored.join("Default/Cookies")).expect("read"),
            b"COOKIES",
            "restore must yield the snapshot-time bytes, not the post-mutation bytes"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn take_snapshot_refuses_to_clobber_existing_dir() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let session = tmp.path().join("session");
        std::fs::create_dir_all(&session).expect("mk session");
        let snap = tmp.path().join("snap");
        std::fs::create_dir_all(&snap).expect("mk snap");
        let mut meta = SnapshotMeta {
            name: "x".into(),
            source_session_id: "s_x".into(),
            created_at_unix_ms: 0,
            bytes_apparent: 0,
            file_count: 0,
        };
        let err = take_snapshot(&session, &snap, &mut meta).expect_err("must refuse");
        assert!(matches!(err, Error::DestinationExists(_)));
    }

    #[test]
    fn snapshot_root_layout_under_home() {
        if dirs::home_dir().is_none() {
            return;
        }
        let p = snapshot_root("my-snap").expect("ok");
        assert!(p.ends_with(".one-for-all/snapshots/my-snap"));
    }

    #[test]
    fn read_meta_returns_none_when_absent() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let r = read_snapshot_meta(tmp.path()).expect("ok");
        assert!(r.is_none());
    }
}
