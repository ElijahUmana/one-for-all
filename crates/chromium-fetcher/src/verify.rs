//! SHA-256 verification for downloaded Chromium zips.
//!
//! Owned by `chromium-fetcher`. Stateless. Per SPEC D12 we use TOFU
//! (trust-on-first-use) per revision: on first download for a given revision,
//! the hash is computed and pinned to disk; on subsequent fetches the cached
//! hash is required to match. This is local-cache integrity, not
//! supply-chain integrity — if the upstream zip changes for the same URL
//! we'll detect it the next time we try to verify a partial download or
//! refresh, but the first download is trusted by definition.
//!
//! The pin file lives at `<install_root>/<rev>/.sha256` next to the zip's
//! extracted contents. If absent, we compute and write it.

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::io::AsyncReadExt;

/// Compute the SHA-256 of a file as a lowercase hex string.
pub async fn sha256_hex(path: &Path) -> Result<String> {
    let mut f = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open {} for sha256", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await.context("read for sha256")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Verify the file at `path` against `expected_hex`. Returns `Ok(())` on
/// match, `Err(_)` on mismatch.
pub async fn verify_against(path: &Path, expected_hex: &str) -> Result<()> {
    let actual = sha256_hex(path).await?;
    if actual.eq_ignore_ascii_case(expected_hex) {
        Ok(())
    } else {
        Err(anyhow!(
            "sha256 mismatch for {}: expected {}, got {}",
            path.display(),
            expected_hex,
            actual
        ))
    }
}

/// Read the pinned hash from `<install_root>/<rev>/.sha256`, or `None` if
/// no pin exists yet.
pub fn read_pin(rev_dir: &Path) -> Result<Option<String>> {
    let pin = rev_dir.join(".sha256");
    match std::fs::read_to_string(&pin) {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read pin {}", pin.display())),
    }
}

/// Write a pinned hash atomically (`tmp` + rename).
pub fn write_pin(rev_dir: &Path, hex: &str) -> Result<()> {
    std::fs::create_dir_all(rev_dir)
        .with_context(|| format!("create rev dir {}", rev_dir.display()))?;
    let pin = rev_dir.join(".sha256");
    let tmp = rev_dir.join(".sha256.tmp");
    std::fs::write(&tmp, hex.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &pin).with_context(|| format!("rename pin -> {}", pin.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sha256_of_known_string() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("payload.bin");
        std::fs::write(&p, b"hello world").unwrap();
        let h = sha256_hex(&p).await.unwrap();
        // sha256("hello world")
        assert_eq!(
            h,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[tokio::test]
    async fn verify_match_and_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a");
        std::fs::write(&p, b"abc").unwrap();
        // sha256("abc")
        let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        verify_against(&p, good).await.unwrap();
        let bad = "deadbeef".to_string() + &"0".repeat(56);
        let err = verify_against(&p, &bad).await.unwrap_err();
        assert!(err.to_string().contains("mismatch"));
    }

    #[test]
    fn pin_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let rev = dir.path().join("1234567");
        assert!(read_pin(&rev).unwrap().is_none());
        write_pin(&rev, "abc123").unwrap();
        assert_eq!(read_pin(&rev).unwrap().as_deref(), Some("abc123"));
    }
}
