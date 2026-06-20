//! SPEC §12 U7 — `clipboard.{read_string, write_string, read_files,
//! write_files, read_image, write_image, types, history}`.
//!
//! Backed by `[NSPasteboard generalPasteboard]`. A background poll task
//! samples `changeCount` every `POLL_INTERVAL_MS` ms; when it advances we
//! capture a [`ClipboardItem`] into a bounded ring (capacity
//! [`HISTORY_CAPACITY`], drop-oldest). The ring stores raw entries; redaction
//! is applied at READ time by [`crate::privacy::RedactionEngine`] so a
//! policy update affects already-captured history.
//!
//! Image payloads are returned as base64-encoded PNG via
//! `read_image`/`write_image` so JSON-RPC callers don't need to host a
//! separate binary channel for the most common clipboard image use case.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use objc2::msg_send;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypePNG, NSPasteboardTypeString,
    NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSArray, NSData, NSString, NSURL};
use parking_lot::Mutex;
use tracing::{debug, warn};

use crate::privacy::RedactionEngine;
use crate::types::{ClipboardItem, ClipboardKind, NativeControlError};

/// Bounded history ring capacity.
pub const HISTORY_CAPACITY: usize = 16;

/// Poll interval for `changeCount` sampling.
pub const POLL_INTERVAL_MS: u64 = 100;

/// Per-session clipboard cache. Cheap to clone (Arc'd internals).
#[derive(Clone)]
pub struct ClipboardCache {
    inner: Arc<CacheInner>,
}

struct CacheInner {
    history: Mutex<Vec<ClipboardItem>>,
    last_change_count: Mutex<i64>,
}

impl Default for ClipboardCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CacheInner {
                history: Mutex::new(Vec::with_capacity(HISTORY_CAPACITY)),
                last_change_count: Mutex::new(-1),
            }),
        }
    }

    /// Spawn the background poller. Returns a `tokio::task::JoinHandle` the
    /// caller (broker session lifetime) is responsible for aborting on
    /// shutdown.
    pub fn spawn_poller(&self) -> tokio::task::JoinHandle<()> {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(POLL_INTERVAL_MS));
            loop {
                tick.tick().await;
                if let Err(e) = poll_once(&inner).await {
                    warn!(err = %e, "clipboard poll error");
                }
            }
        })
    }

    /// Return the latest entry (head of history), redacted per `engine`.
    pub fn read(&self, engine: &RedactionEngine) -> Option<ClipboardItem> {
        let h = self.inner.history.lock();
        h.last().map(|item| engine.apply_clipboard(item))
    }

    /// Return the entire history (oldest → newest), redacted.
    pub fn history(&self, engine: &RedactionEngine) -> Vec<ClipboardItem> {
        let h = self.inner.history.lock();
        h.iter().map(|i| engine.apply_clipboard(i)).collect()
    }

    /// Synchronously refresh the head (caller might want a freshest read
    /// without waiting for the next poll tick).
    pub async fn refresh_now(&self) -> Result<(), NativeControlError> {
        poll_once(&self.inner).await
    }

    /// Push a synthesized item into the ring (used by `write_*`'s
    /// reflexive update + tests).
    pub fn push(&self, item: ClipboardItem) {
        let mut h = self.inner.history.lock();
        if h.len() == HISTORY_CAPACITY {
            h.remove(0);
        }
        h.push(item);
    }
}

/// One sampling pass: if `changeCount` advanced, capture a snapshot.
async fn poll_once(inner: &Arc<CacheInner>) -> Result<(), NativeControlError> {
    let inner_b = Arc::clone(inner);
    tokio::task::spawn_blocking(move || poll_once_blocking(&inner_b))
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn poll_once_blocking(inner: &Arc<CacheInner>) -> Result<(), NativeControlError> {
    // SAFETY: NSPasteboard.generalPasteboard is a thread-safe singleton.
    let pb = unsafe { NSPasteboard::generalPasteboard() };
    let cc = unsafe { pb.changeCount() } as i64;
    {
        let mut last = inner.last_change_count.lock();
        if cc <= *last {
            return Ok(());
        }
        *last = cc;
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let item = capture(&pb, cc, now_ms);
    debug!(change_count = cc, kind = ?item.kind, types = ?item.types, "clipboard captured");
    let mut h = inner.history.lock();
    if h.len() == HISTORY_CAPACITY {
        h.remove(0);
    }
    h.push(item);
    Ok(())
}

fn capture(pb: &Retained<NSPasteboard>, cc: i64, now_ms: u64) -> ClipboardItem {
    let mut item = ClipboardItem {
        change_count: cc,
        timestamp_ms: now_ms,
        types: vec![],
        kind: ClipboardKind::Other,
        text: None,
        files: vec![],
        redacted: false,
    };
    // SAFETY: pb is a valid retained NSPasteboard.
    let types: Option<Retained<NSArray<NSString>>> = unsafe { pb.types() };
    if let Some(types) = types {
        let count = types.count();
        for i in 0..count {
            // SAFETY: in-bounds.
            let t: Retained<NSString> = unsafe { types.objectAtIndex(i) };
            item.types.push(t.to_string());
        }
    }
    // Files (URL list)?
    // SAFETY: NSPasteboardTypeFileURL is a static symbol.
    let url_type = unsafe { NSPasteboardTypeFileURL };
    if has_type(&item.types, &url_type.to_string()) {
        if let Some(files) = read_file_urls(pb) {
            item.kind = ClipboardKind::Files;
            item.files = files;
            return item;
        }
    }
    // String?
    let str_type = unsafe { NSPasteboardTypeString };
    if has_type(&item.types, &str_type.to_string()) {
        // SAFETY: stringForType requires a valid NSString type name.
        if let Some(s) =
            unsafe { pb.stringForType(str_type) }.map(|r: Retained<NSString>| r.to_string())
        {
            item.kind = ClipboardKind::String;
            item.text = Some(s);
            return item;
        }
    }
    // Image?
    let png_type = unsafe { NSPasteboardTypePNG };
    let tiff_type = unsafe { NSPasteboardTypeTIFF };
    if has_type(&item.types, &png_type.to_string()) || has_type(&item.types, &tiff_type.to_string())
    {
        item.kind = ClipboardKind::Image;
        return item;
    }
    item
}

fn has_type(types: &[String], needle: &str) -> bool {
    types.iter().any(|t| t == needle)
}

fn read_file_urls(pb: &Retained<NSPasteboard>) -> Option<Vec<String>> {
    // The objc2-app-kit binding exposes propertyListForType / readObjectsForClasses
    // in slightly different forms across versions; we read using
    // `propertyListForType` keyed by `public.file-url` and parse each
    // resulting NSURL's `path`.
    let url_type = unsafe { NSPasteboardTypeFileURL };
    let s_opt: Option<Retained<NSString>> = unsafe { pb.stringForType(url_type) };
    if let Some(s) = s_opt {
        // Single URL string. Convert to an NSURL.
        // SAFETY: path conversion via NSURL +URLWithString.
        let url_obj: Option<Retained<NSURL>> = unsafe { NSURL::URLWithString(&s) };
        if let Some(u) = url_obj {
            if let Some(path) = unsafe { u.path() }.map(|r: Retained<NSString>| r.to_string()) {
                return Some(vec![path]);
            }
        }
        return Some(vec![s.to_string()]);
    }
    None
}

// ---------- Public read / write API used by the broker --------------------

/// Read the head item as a string (when applicable). Returns `None` when the
/// most-recent entry is not a string OR is redacted.
pub async fn read_string(
    cache: &ClipboardCache,
    engine: &RedactionEngine,
) -> Result<Option<String>, NativeControlError> {
    cache.refresh_now().await?;
    let item = cache.read(engine);
    Ok(item.and_then(|i| i.text))
}

/// Write a string to the pasteboard.
pub async fn write_string(cache: &ClipboardCache, text: &str) -> Result<(), NativeControlError> {
    let text = text.to_string();
    let cache = cache.clone();
    tokio::task::spawn_blocking(move || -> Result<(), NativeControlError> {
        // SAFETY: thread-safe singleton.
        let pb = unsafe { NSPasteboard::generalPasteboard() };
        unsafe {
            pb.clearContents();
        }
        let s = NSString::from_str(&text);
        let str_type = unsafe { NSPasteboardTypeString };
        // SAFETY: setString takes valid NSString and a known type.
        let ok: bool = unsafe { pb.setString_forType(&s, str_type) };
        if !ok {
            return Err(NativeControlError::Internal(
                "NSPasteboard setString returned false".into(),
            ));
        }
        let cc = unsafe { pb.changeCount() } as i64;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        cache.push(ClipboardItem {
            change_count: cc,
            timestamp_ms: now_ms,
            types: vec![str_type.to_string()],
            kind: ClipboardKind::String,
            text: Some(text),
            files: vec![],
            redacted: false,
        });
        Ok(())
    })
    .await
    .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

/// Read file path list from clipboard (when applicable).
pub async fn read_files(
    cache: &ClipboardCache,
    _engine: &RedactionEngine,
) -> Result<Vec<String>, NativeControlError> {
    cache.refresh_now().await?;
    Ok(cache
        .inner
        .history
        .lock()
        .last()
        .map(|i| i.files.clone())
        .unwrap_or_default())
}

/// Write file paths.
pub async fn write_files(
    cache: &ClipboardCache,
    paths: Vec<String>,
) -> Result<(), NativeControlError> {
    if paths.is_empty() {
        return Err(NativeControlError::Internal(
            "write_files requires ≥1 path".into(),
        ));
    }
    let cache = cache.clone();
    tokio::task::spawn_blocking(move || -> Result<(), NativeControlError> {
        // SAFETY: thread-safe singleton.
        let pb = unsafe { NSPasteboard::generalPasteboard() };
        unsafe {
            pb.clearContents();
        }
        // Build NSArray<NSURL> of file URLs and call writeObjects:.
        let mut urls: Vec<Retained<NSURL>> = Vec::with_capacity(paths.len());
        for p in &paths {
            let pb_path = PathBuf::from(p);
            let path_str = NSString::from_str(&pb_path.display().to_string());
            // SAFETY: fileURLWithPath returns a +1 NSURL.
            let url: Retained<NSURL> = unsafe { NSURL::fileURLWithPath(&path_str) };
            urls.push(url);
        }
        let arr: Retained<NSArray<NSURL>> = NSArray::from_id_slice(&urls);
        // writeObjects: takes NSArray<ProtocolObject<NSPasteboardWriting>>;
        // NSURL conforms but the typed wrapper requires an explicit cast we
        // don't have. Dispatch via raw msg_send.
        // SAFETY: NSURL conforms to NSPasteboardWriting at runtime.
        let _ok: bool = unsafe { msg_send![&*pb, writeObjects:&*arr] };
        let cc = unsafe { pb.changeCount() } as i64;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        cache.push(ClipboardItem {
            change_count: cc,
            timestamp_ms: now_ms,
            types: vec![unsafe { NSPasteboardTypeFileURL }.to_string()],
            kind: ClipboardKind::Files,
            text: None,
            files: paths,
            redacted: false,
        });
        Ok(())
    })
    .await
    .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

/// Read image as raw PNG bytes (or TIFF if PNG isn't available). Caller
/// base64-encodes for JSON-RPC transport.
pub async fn read_image(_cache: &ClipboardCache) -> Result<Vec<u8>, NativeControlError> {
    tokio::task::spawn_blocking(read_image_blocking)
        .await
        .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

fn read_image_blocking() -> Result<Vec<u8>, NativeControlError> {
    // SAFETY: pasteboard singleton.
    let pb = unsafe { NSPasteboard::generalPasteboard() };
    let png_type = unsafe { NSPasteboardTypePNG };
    let tiff_type = unsafe { NSPasteboardTypeTIFF };
    for t in [&png_type, &tiff_type] {
        // SAFETY: dataForType takes a valid NSPasteboardType.
        let data_opt = unsafe { pb.dataForType(t) };
        if let Some(data) = data_opt {
            // SAFETY: bytes() returns the borrowed contents of the NSData.
            let slice: &[u8] = data.bytes();
            if slice.is_empty() {
                continue;
            }
            return Ok(slice.to_vec());
        }
    }
    Err(NativeControlError::Internal(
        "no image data on clipboard".into(),
    ))
}

/// Write raw PNG bytes to the clipboard.
pub async fn write_image(
    cache: &ClipboardCache,
    png_bytes: Vec<u8>,
) -> Result<(), NativeControlError> {
    if png_bytes.is_empty() {
        return Err(NativeControlError::Internal(
            "write_image requires non-empty bytes".into(),
        ));
    }
    let cache = cache.clone();
    tokio::task::spawn_blocking(move || -> Result<(), NativeControlError> {
        // SAFETY: pasteboard singleton.
        let pb = unsafe { NSPasteboard::generalPasteboard() };
        unsafe {
            pb.clearContents();
        }
        // Wrap bytes in NSData via the safe `with_bytes` helper.
        let data = NSData::with_bytes(&png_bytes);
        let png_type = unsafe { NSPasteboardTypePNG };
        // SAFETY: setData_forType is the canonical pasteboard write.
        let ok: bool = unsafe { pb.setData_forType(Some(&data), png_type) };
        if !ok {
            return Err(NativeControlError::Internal(
                "NSPasteboard setData_forType returned false".into(),
            ));
        }
        let cc = unsafe { pb.changeCount() } as i64;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        cache.push(ClipboardItem {
            change_count: cc,
            timestamp_ms: now_ms,
            types: vec![png_type.to_string()],
            kind: ClipboardKind::Image,
            text: None,
            files: vec![],
            redacted: false,
        });
        Ok(())
    })
    .await
    .map_err(|e| NativeControlError::Internal(format!("spawn_blocking: {e}")))?
}

/// List the available types on the head clipboard entry.
pub async fn types(cache: &ClipboardCache) -> Result<Vec<String>, NativeControlError> {
    cache.refresh_now().await?;
    Ok(cache
        .inner
        .history
        .lock()
        .last()
        .map(|i| i.types.clone())
        .unwrap_or_default())
}

/// History as a vec, oldest → newest, with redaction applied.
pub async fn history(
    cache: &ClipboardCache,
    engine: &RedactionEngine,
) -> Result<Vec<ClipboardItem>, NativeControlError> {
    cache.refresh_now().await?;
    Ok(cache.history(engine))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PrivacyPolicy;

    #[tokio::test]
    async fn ring_caps_at_capacity() {
        let cache = ClipboardCache::new();
        for i in 0..(HISTORY_CAPACITY + 8) {
            cache.push(ClipboardItem {
                change_count: i as i64 + 1,
                timestamp_ms: 0,
                types: vec![],
                kind: ClipboardKind::String,
                text: Some(format!("entry {i}")),
                files: vec![],
                redacted: false,
            });
        }
        let h = cache.inner.history.lock();
        assert_eq!(h.len(), HISTORY_CAPACITY);
        // The oldest 8 must have been dropped (FIFO).
        assert_eq!(h.first().unwrap().text.as_deref(), Some("entry 8"));
        assert_eq!(
            h.last().unwrap().text.as_deref(),
            Some(&*format!("entry {}", HISTORY_CAPACITY + 7))
        );
    }

    #[tokio::test]
    async fn read_applies_redaction() {
        let cache = ClipboardCache::new();
        cache.push(ClipboardItem {
            change_count: 1,
            timestamp_ms: 0,
            types: vec!["public.utf8-plain-text".into()],
            kind: ClipboardKind::String,
            text: Some("ssn 123-45-6789".into()),
            files: vec![],
            redacted: false,
        });
        let engine = RedactionEngine::new();
        engine.install(&PrivacyPolicy {
            redact_patterns: vec![r"\d{3}-\d{2}-\d{4}".into()],
            app_blocklist: vec![],
        });
        let item = cache.read(&engine).unwrap();
        assert!(item.redacted);
        assert!(item.text.is_none());
    }

    #[tokio::test]
    async fn empty_write_files_rejected() {
        let cache = ClipboardCache::new();
        let r = write_files(&cache, vec![]).await;
        match r {
            Err(NativeControlError::Internal(_)) => {}
            other => panic!("expected Internal, got {other:?}"),
        }
    }
}
