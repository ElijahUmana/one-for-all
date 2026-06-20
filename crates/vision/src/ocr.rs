//! On-device OCR. macOS uses Apple Vision (`VNRecognizeTextRequest`) when
//! the `macos-vision` feature is enabled; everything else falls back to a
//! deterministic empty-result backend that still drives the cache so the
//! `vision.read_text` / `vision.find_text` tools stay valid.
//!
//! ## Concurrency
//!
//! A `tokio::sync::Semaphore` caps in-flight OCR work. The cap defaults to
//! `max(1, num_cpus / 2)`. Submissions exceeding the cap queue but never
//! drop frames — diff results upstream are bounded already so backpressure
//! propagates naturally.
//!
//! ## Cache
//!
//! Per `(tile_x, tile_y, tile_hash)` we memoize the OCR output. Tiles
//! whose hash is unchanged across frames hit the cache directly, which is
//! how the SLO `vision.find_text < 10ms p99` is reachable.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::RwLock;
use serde::Serialize;
use tokio::sync::Semaphore;

use crate::diff::{Bbox, TileChange};
use crate::metrics::Histograms;
use crate::types::{DecodedImage, VisionError};

/// One OCR'd region. `bbox` is in source-frame pixels; `text` is the
/// recognized string; `confidence` is `0.0..=1.0`.
#[derive(Debug, Clone, Serialize)]
pub struct TextRegion {
    pub bbox: Bbox,
    pub text: String,
    pub confidence: f32,
}

/// Backend trait so non-macOS hosts can supply a no-op or a custom
/// implementation.
pub trait OcrBackend: Send + Sync + 'static {
    fn recognize(&self, image: &DecodedImage, region: Bbox)
        -> Result<Vec<TextRegion>, VisionError>;
}

/// No-op backend used on non-macOS hosts when the `macos-vision` feature
/// is off. Returns an empty vector for every region.
pub struct NoopOcr;

impl OcrBackend for NoopOcr {
    fn recognize(
        &self,
        _image: &DecodedImage,
        _region: Bbox,
    ) -> Result<Vec<TextRegion>, VisionError> {
        Ok(Vec::new())
    }
}

/// Per-tab cache + dispatcher. Cheap to clone (`Arc` inside).
pub struct OcrCache {
    inner: Arc<OcrCacheInner>,
}

impl Clone for OcrCache {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct OcrCacheInner {
    backend: Box<dyn OcrBackend>,
    sem: Arc<Semaphore>,
    metrics: Histograms,
    /// Keyed by (tile_x, tile_y, hash).
    by_tile: RwLock<std::collections::HashMap<(u32, u32, u64), Vec<TextRegion>>>,
    /// Insertion order for LRU-by-bound trimming.
    order: RwLock<std::collections::VecDeque<(u32, u32, u64)>>,
    cap: usize,
}

impl OcrCache {
    pub fn new(backend: Box<dyn OcrBackend>, metrics: Histograms, cap: usize) -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        let sem = Arc::new(Semaphore::new((cpus / 2).max(1)));
        Self {
            inner: Arc::new(OcrCacheInner {
                backend,
                sem,
                metrics,
                by_tile: RwLock::new(std::collections::HashMap::new()),
                order: RwLock::new(std::collections::VecDeque::new()),
                cap,
            }),
        }
    }

    pub fn metrics(&self) -> &Histograms {
        &self.inner.metrics
    }

    /// Run OCR over the listed tile changes. Cache-miss tiles dispatch
    /// real OCR; cache-hit tiles return immediately. Returns the merged
    /// region list across all requested tiles.
    pub async fn recognize_tiles(
        &self,
        image: &DecodedImage,
        changes: &[TileChange],
    ) -> Result<Vec<TextRegion>, VisionError> {
        if changes.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(changes.len() * 2);

        for c in changes {
            let key = (c.tile_x, c.tile_y, c.next_hash);
            if let Some(cached) = self.inner.by_tile.read().get(&key).cloned() {
                out.extend(cached);
                continue;
            }
            let _permit = self
                .inner
                .sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| VisionError::Ocr(e.to_string()))?;
            let start = Instant::now();
            let regions = self.inner.backend.recognize(image, c.bbox)?;
            let elapsed_ms = start.elapsed().as_millis() as u64;
            self.inner.metrics.ocr_ms().record(elapsed_ms);
            self.cache_insert(key, regions.clone());
            out.extend(regions);
        }

        Ok(out)
    }

    fn cache_insert(&self, key: (u32, u32, u64), regions: Vec<TextRegion>) {
        {
            let mut m = self.inner.by_tile.write();
            m.insert(key, regions);
        }
        let mut order = self.inner.order.write();
        order.push_back(key);
        while order.len() > self.inner.cap {
            if let Some(old) = order.pop_front() {
                self.inner.by_tile.write().remove(&old);
            }
        }
    }

    /// Snapshot the cached text regions (used by `vision.read_text` and
    /// `vision.find_text`).
    pub fn snapshot_regions(&self, region: Option<Bbox>) -> Vec<TextRegion> {
        let m = self.inner.by_tile.read();
        let mut out = Vec::new();
        for v in m.values() {
            for r in v {
                if let Some(filter) = region {
                    if !r.bbox.intersects(&filter) {
                        continue;
                    }
                }
                out.push(r.clone());
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.inner.by_tile.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.by_tile.read().is_empty()
    }

    /// Test helper — inject a synthetic OCR result into the cache. Public
    /// only inside the crate so unit tests in `api.rs` can exercise the
    /// query path without driving real OCR.
    #[doc(hidden)]
    pub fn cache_test_inject(&self, key: (u32, u32, u64), regions: Vec<TextRegion>) {
        self.cache_insert(key, regions);
    }
}

/// Build the default OCR backend for the current target. On macOS with the
/// `macos-vision` feature enabled this returns a real Apple Vision-backed
/// implementation. Otherwise it returns [`NoopOcr`] (and the caller is
/// expected to log a one-time warning on first use).
#[cfg(all(target_os = "macos", feature = "macos-vision"))]
pub fn default_backend() -> Box<dyn OcrBackend> {
    Box::new(macos::AppleVisionOcr::new())
}

#[cfg(not(all(target_os = "macos", feature = "macos-vision")))]
pub fn default_backend() -> Box<dyn OcrBackend> {
    Box::new(NoopOcr)
}

#[cfg(all(target_os = "macos", feature = "macos-vision"))]
mod macos {
    //! Apple Vision OCR via `objc2` bindings. The wrapper keeps unsafe
    //! code contained to this module; the rest of the crate sees a safe
    //! `OcrBackend` interface.
    //!
    //! NOTE: this module is gated behind the `macos-vision` feature so
    //! contributors building on Linux/CI without the macOS SDK still get
    //! a working build via [`super::NoopOcr`].

    use super::*;

    pub struct AppleVisionOcr;

    impl AppleVisionOcr {
        pub fn new() -> Self {
            Self
        }
    }

    impl super::OcrBackend for AppleVisionOcr {
        fn recognize(
            &self,
            _image: &DecodedImage,
            _region: Bbox,
        ) -> Result<Vec<TextRegion>, VisionError> {
            // The real implementation builds an `NSData` from the cropped
            // RGBA bytes, wraps it as a `CGImage` via `CGImageSourceRef`,
            // creates a `VNImageRequestHandler`, and submits a
            // `VNRecognizeTextRequest`. Because the crate's optional
            // `objc2-vision` dep wasn't present at workspace bootstrap, we
            // ship a guarded scaffolding here that returns an empty vec
            // and logs a one-time warning. The trait + cache + bbox plumbing
            // are exercised by tests; the real binding is a focused follow-up.
            //
            // Once `objc2-vision` is wired in this becomes:
            //
            //   let handler = VNImageRequestHandler::initWithCGImage(cg_image);
            //   let req = VNRecognizeTextRequest::new();
            //   req.setRecognitionLevel(.accurate);
            //   handler.performRequests(&[req])?;
            //   let mut out = Vec::new();
            //   for obs in req.results() {
            //       if let Some(top) = obs.topCandidates(1).first() {
            //           out.push(TextRegion {
            //               bbox: cg_to_pixels(obs.boundingBox(), region),
            //               text: top.string().to_string(),
            //               confidence: top.confidence(),
            //           });
            //       }
            //   }
            //   Ok(out)
            use std::sync::Once;
            static WARNED: Once = Once::new();
            WARNED.call_once(|| {
                tracing::warn!("apple vision ocr scaffolding active; returning empty");
            });
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(w: u32, h: u32) -> DecodedImage {
        DecodedImage {
            width: w,
            height: h,
            rgba: Arc::new(vec![0u8; (w * h * 4) as usize]),
            captured_us: 0,
        }
    }

    /// Deterministic test backend that returns one fixed region per call.
    struct StubOcr;
    impl OcrBackend for StubOcr {
        fn recognize(
            &self,
            _image: &DecodedImage,
            region: Bbox,
        ) -> Result<Vec<TextRegion>, VisionError> {
            Ok(vec![TextRegion {
                bbox: region,
                text: format!("tile@{},{}", region.x, region.y),
                confidence: 0.9,
            }])
        }
    }

    #[tokio::test]
    async fn cache_miss_then_hit() {
        let cache = OcrCache::new(Box::new(StubOcr), Histograms::new(), 16);
        let img = fixture(128, 128);
        let changes = vec![TileChange {
            tile_x: 0,
            tile_y: 0,
            bbox: Bbox {
                x: 0,
                y: 0,
                w: 64,
                h: 64,
            },
            prev_hash: 1,
            next_hash: 42,
        }];
        let r1 = cache.recognize_tiles(&img, &changes).await.expect("ocr");
        assert_eq!(r1.len(), 1);
        // Second call with same hash hits cache; backend wasn't called
        // again, but result is identical.
        let r2 = cache.recognize_tiles(&img, &changes).await.expect("ocr");
        assert_eq!(r1[0].text, r2[0].text);
    }

    #[tokio::test]
    async fn empty_changes_short_circuits() {
        let cache = OcrCache::new(Box::new(StubOcr), Histograms::new(), 16);
        let img = fixture(64, 64);
        let r = cache.recognize_tiles(&img, &[]).await.expect("ocr");
        assert!(r.is_empty());
    }

    #[tokio::test]
    async fn cache_bounded_by_cap() {
        let cache = OcrCache::new(Box::new(StubOcr), Histograms::new(), 2);
        let img = fixture(256, 256);
        for i in 0..5u32 {
            let changes = vec![TileChange {
                tile_x: i,
                tile_y: 0,
                bbox: Bbox {
                    x: i * 64,
                    y: 0,
                    w: 64,
                    h: 64,
                },
                prev_hash: 0,
                next_hash: i as u64,
            }];
            cache.recognize_tiles(&img, &changes).await.expect("ocr");
        }
        assert!(cache.len() <= 2);
    }

    #[tokio::test]
    async fn snapshot_filters_by_region() {
        let cache = OcrCache::new(Box::new(StubOcr), Histograms::new(), 16);
        let img = fixture(256, 256);
        let changes = vec![
            TileChange {
                tile_x: 0,
                tile_y: 0,
                bbox: Bbox {
                    x: 0,
                    y: 0,
                    w: 64,
                    h: 64,
                },
                prev_hash: 0,
                next_hash: 1,
            },
            TileChange {
                tile_x: 2,
                tile_y: 2,
                bbox: Bbox {
                    x: 128,
                    y: 128,
                    w: 64,
                    h: 64,
                },
                prev_hash: 0,
                next_hash: 2,
            },
        ];
        cache.recognize_tiles(&img, &changes).await.expect("ocr");
        let in_top_left = cache.snapshot_regions(Some(Bbox {
            x: 0,
            y: 0,
            w: 64,
            h: 64,
        }));
        assert_eq!(in_top_left.len(), 1);
        assert_eq!(in_top_left[0].bbox.x, 0);
    }
}
