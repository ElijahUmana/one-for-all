//! SPEC §11 V3 V-R1 — host-state seeding for the FileVault-blocked path.
//!
//! When `clonefile(2)` fails (FileVault locked, cross-volume, non-APFS),
//! we cannot pre-populate the session UDD with the user's Chrome state.
//! V-R1 documents the fallback: seed via CDP after Chromium boots.
//!
//! This module owns the data shape — what gets seeded — without
//! depending on `cdp-client` (kept thin to avoid a circular dep).
//! `browser-engine` reads `SeedPlan` and issues the actual CDP calls.
//!
//! ## What the agent gets back
//!
//! The user requirement is broader than cookies alone: cookies +
//! localStorage + sessionStorage + IndexedDB + ServiceWorker
//! registrations + Cache Storage. We define a vocabulary for ALL of them so
//! the broker side can stage data incrementally without reshaping the type.
//!
//! ## Sources
//!
//! - Cookies — read directly from the host Chrome `Cookies` SQLite db
//!   (file COPY into a tempfile, then sqlite3 CLI read — no rusqlite dep).
//! - localStorage / sessionStorage — host Chrome stores these in
//!   `Default/Local Storage/leveldb/` and `Default/Session Storage/leveldb/`.
//!   leveldb format is not stable across Chrome versions; the broker may
//!   instead extract them via a CDP-time helper.
//! - IndexedDB — rows are serialized into seed records and replayed by
//!   `browser-engine` via per-origin bootstrap pages.
//! - ServiceWorkers — registrations are replayed by re-registering the
//!   script URL inside a same-origin bootstrap page.
//! - Cache Storage — entries are replayed inside a same-origin bootstrap
//!   page via `caches.open(...).put(...)`.
//!
//! Producer and consumer can therefore evolve independently, but the shape is
//! fixed and explicit: when a field is present in `SeedPlan`, the
//! `browser-engine` must attempt to apply it rather than silently ignoring it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::Error;
use crate::Result;

/// One cookie. Mirrors CDP `Network.CookieParam`'s required + common
/// optional fields. Serialized to JSON for the broker side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CookieRecord {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// Unix seconds; `None` means session cookie.
    pub expires: Option<f64>,
    pub http_only: bool,
    pub secure: bool,
    pub same_site: Option<String>,
}

/// One Web Storage entry (localStorage or sessionStorage).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageEntry {
    pub origin: String,
    pub key: String,
    pub value: String,
    /// `"local"` or `"session"`. Strings (not enums) so the broker side
    /// can match the CDP `StorageType` literally.
    pub kind: String,
}

/// One IndexedDB record.
///
/// `key_b64` and `value_b64` carry UTF-8 JSON payloads encoded with base64.
/// `browser-engine` decodes them and replays via `indexedDB.open(...).put(...)`
/// inside a same-origin bootstrap page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedDbRecord {
    pub origin: String,
    pub database_name: String,
    #[serde(default)]
    pub database_version: Option<u64>,
    pub object_store: String,
    pub key_b64: String,
    pub value_b64: String,
}

/// One ServiceWorker registration. Script content is re-fetched when the
/// sandboxed page re-registers the worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceWorkerReg {
    pub scope: String,
    pub script_url: String,
}

/// One HTTP header carried by a Cache Storage entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheHeader {
    pub name: String,
    pub value: String,
}

/// One Cache Storage entry. Replayed via the page's Cache API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStorageEntry {
    pub origin: String,
    pub cache_name: String,
    pub request_url: String,
    pub response_status: u16,
    #[serde(default)]
    pub response_status_text: Option<String>,
    #[serde(default)]
    pub response_headers: Vec<CacheHeader>,
    /// Base64-encoded response body.
    pub response_body_b64: String,
}

/// Aggregate of everything the V-R1 path needs to recreate the user's
/// host browser state inside a freshly-booted (and clonefile-empty)
/// session UDD.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SeedPlan {
    pub cookies: Vec<CookieRecord>,
    pub storage: Vec<StorageEntry>,
    pub indexed_db: Vec<IndexedDbRecord>,
    pub service_workers: Vec<ServiceWorkerReg>,
    #[serde(default)]
    pub cache_storage: Vec<CacheStorageEntry>,
}

impl SeedPlan {
    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
            && self.storage.is_empty()
            && self.indexed_db.is_empty()
            && self.service_workers.is_empty()
            && self.cache_storage.is_empty()
    }

    pub fn count(&self) -> usize {
        self.cookies.len()
            + self.storage.len()
            + self.indexed_db.len()
            + self.service_workers.len()
            + self.cache_storage.len()
    }
}

/// Read cookies from the user's host Chrome `Cookies` SQLite database.
///
/// Strategy: copy the file to a tempfile (Chrome holds an exclusive
/// SQLite lock when running), then shell out to `/usr/bin/sqlite3` to
/// dump rows. We deliberately avoid `rusqlite` to keep dep surface flat.
///
/// macOS Chrome stores cookies at:
///   `~/Library/Application Support/Google/Chrome/Default/Network/Cookies`
///
/// Returns an empty `Vec` if the file is absent (most CI hosts).
pub fn read_host_cookies(host_profile: &Path) -> Result<Vec<CookieRecord>> {
    let cookies_db = host_profile.join("Network").join("Cookies");
    if !cookies_db.exists() {
        return Ok(Vec::new());
    }
    let tmp = tempfile::Builder::new()
        .prefix("ofa-cookies-")
        .tempfile()
        .map_err(|e| Error::io(&cookies_db, e))?;
    std::fs::copy(&cookies_db, tmp.path()).map_err(|e| Error::io(&cookies_db, e))?;

    // Chrome's cookie row schema (Chromium tip-of-tree, stable for 5+ years):
    //   creation_utc, host_key, name, value, path, expires_utc, ...,
    //   is_secure, is_httponly, ..., samesite, ...
    // Dump the columns we need as TSV (.mode tabs).
    let out = std::process::Command::new("/usr/bin/sqlite3")
        .arg(tmp.path())
        .arg(".mode tabs")
        .arg("SELECT name, value, host_key, path, expires_utc, is_httponly, is_secure, samesite FROM cookies;")
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            return Err(Error::Io {
                path: cookies_db.clone(),
                source: std::io::Error::other(format!(
                    "sqlite3 exit {} stderr={}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr)
                )),
            });
        }
        Err(e) => return Err(Error::io(cookies_db, e)),
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut cookies = Vec::new();
    for line in stdout.lines() {
        let mut cols = line.split('\t');
        let name = cols.next().unwrap_or("").to_string();
        let value = cols.next().unwrap_or("").to_string();
        let domain = cols.next().unwrap_or("").to_string();
        let path = cols.next().unwrap_or("/").to_string();
        let expires_us = cols.next().unwrap_or("0").parse::<i64>().unwrap_or(0);
        let httponly = cols.next().unwrap_or("0") == "1";
        let secure = cols.next().unwrap_or("0") == "1";
        let same_site = match cols.next().unwrap_or("-1") {
            "0" => Some("None".to_string()),
            "1" => Some("Lax".to_string()),
            "2" => Some("Strict".to_string()),
            _ => None,
        };
        // Chrome stores expires_utc as microseconds since 1601-01-01;
        // CDP wants Unix seconds. Convert; 0 = session cookie.
        let expires = if expires_us == 0 {
            None
        } else {
            // Windows-epoch microseconds → Unix seconds
            // 11644473600 = seconds between 1601 and 1970.
            Some((expires_us as f64) / 1_000_000.0 - 11_644_473_600.0)
        };
        if name.is_empty() && value.is_empty() {
            continue;
        }
        cookies.push(CookieRecord {
            name,
            value,
            domain,
            path,
            expires,
            http_only: httponly,
            secure,
            same_site,
        });
    }
    Ok(cookies)
}

/// Where the seed plan is staged on disk so the broker can hand it to
/// `browser-engine` on Chromium boot.
pub fn seed_plan_path(session_rootfs: &Path) -> PathBuf {
    session_rootfs.join("v_r1_seed.json")
}

/// Persist the seed plan to disk. The broker writes; `browser-engine`
/// reads and dispatches.
pub fn write_seed_plan(session_rootfs: &Path, plan: &SeedPlan) -> Result<PathBuf> {
    let p = seed_plan_path(session_rootfs);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let json =
        serde_json::to_vec_pretty(plan).map_err(|e| Error::io(&p, std::io::Error::other(e)))?;
    std::fs::write(&p, json).map_err(|e| Error::io(&p, e))?;
    Ok(p)
}

/// Read a previously-persisted seed plan. Returns `Ok(None)` if absent
/// (clonefile path was used; no fallback needed).
pub fn read_seed_plan(session_rootfs: &Path) -> Result<Option<SeedPlan>> {
    let p = seed_plan_path(session_rootfs);
    if !p.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&p).map_err(|e| Error::io(&p, e))?;
    let plan: SeedPlan =
        serde_json::from_slice(&bytes).map_err(|e| Error::io(&p, std::io::Error::other(e)))?;
    Ok(Some(plan))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_seed_plan_is_empty() {
        let p = SeedPlan::default();
        assert!(p.is_empty());
        assert_eq!(p.count(), 0);
    }

    #[test]
    fn nonempty_seed_plan_counts() {
        let p = SeedPlan {
            cookies: vec![CookieRecord {
                name: "n".into(),
                value: "v".into(),
                domain: ".example.com".into(),
                path: "/".into(),
                expires: None,
                http_only: false,
                secure: true,
                same_site: Some("Lax".into()),
            }],
            storage: vec![StorageEntry {
                origin: "https://example.com".into(),
                key: "k".into(),
                value: "v".into(),
                kind: "local".into(),
            }],
            cache_storage: vec![CacheStorageEntry {
                origin: "https://example.com".into(),
                cache_name: "v1".into(),
                request_url: "https://example.com/api".into(),
                response_status: 200,
                response_status_text: Some("OK".into()),
                response_headers: vec![CacheHeader {
                    name: "content-type".into(),
                    value: "application/json".into(),
                }],
                response_body_b64: "e30=".into(),
            }],
            ..Default::default()
        };
        assert!(!p.is_empty());
        assert_eq!(p.count(), 3);
    }

    #[test]
    fn seed_plan_round_trips_to_disk() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let original = SeedPlan {
            cookies: vec![CookieRecord {
                name: "auth".into(),
                value: "abc123".into(),
                domain: ".github.com".into(),
                path: "/".into(),
                expires: Some(2_000_000_000.0),
                http_only: true,
                secure: true,
                same_site: Some("Strict".into()),
            }],
            cache_storage: vec![CacheStorageEntry {
                origin: "https://example.com".into(),
                cache_name: "preload".into(),
                request_url: "https://example.com/api/bootstrap".into(),
                response_status: 200,
                response_status_text: Some("OK".into()),
                response_headers: vec![CacheHeader {
                    name: "content-type".into(),
                    value: "application/json".into(),
                }],
                response_body_b64: "e30=".into(),
            }],
            ..Default::default()
        };
        let path = write_seed_plan(tmp.path(), &original).expect("write");
        assert!(path.exists());
        let read_back = read_seed_plan(tmp.path()).expect("read").expect("some");
        assert_eq!(read_back.cookies.len(), 1);
        assert_eq!(read_back.cookies[0].name, "auth");
        assert_eq!(read_back.cookies[0].domain, ".github.com");
        assert_eq!(read_back.cache_storage.len(), 1);
        assert_eq!(read_back.cache_storage[0].cache_name, "preload");
    }

    #[test]
    fn read_seed_plan_returns_none_when_absent() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let r = read_seed_plan(tmp.path()).expect("ok");
        assert!(r.is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn read_host_cookies_is_empty_when_no_profile() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        // No cookies db exists under tmp/Default/Network/Cookies → must
        // return empty without erroring.
        let r = read_host_cookies(tmp.path()).expect("ok");
        assert!(r.is_empty());
    }
}
