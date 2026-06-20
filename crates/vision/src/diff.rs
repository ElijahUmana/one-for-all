//! SIMD tile-hash diff. Splits frames into N×N tiles, computes a 64-bit
//! xxhash3 over each tile's RGBA bytes, and returns the indices of tiles
//! whose hash changed against the prior frame.
//!
//! ## Why tile-hashing
//!
//! A full-frame hash tells us "something changed", but we want to know
//! *which* regions changed so we can downstream-OCR only those tiles. A
//! 64×64 tile is a decent trade-off between change resolution and hash
//! count overhead. At 1920×1080, 64×64 tiles → 30×17 ≈ 510 tiles; xxhash3
//! over 64×64×4 = 16 KiB chunks runs at GB/s on modern x86_64 + arm64.
//!
//! ## Allocation discipline
//!
//! Diff results are emitted into a caller-supplied [`bumpalo::Bump`] arena
//! to avoid hot-path allocations. The arena is reset per call by the
//! caller (typically the pipeline driver).

use std::hash::Hasher;

use bumpalo::collections::Vec as BumpVec;
use bumpalo::Bump;
use serde::{Deserialize, Serialize};
use twox_hash::XxHash64;
use wide::u32x8;

use crate::types::{DecodedImage, VisionError};

/// Default tile size in pixels. A 64-pixel side is the sweet spot for
/// xxhash3 throughput vs change-detection granularity.
pub const DEFAULT_TILE_SIZE: u32 = 64;

/// Inclusive-exclusive pixel rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Bbox {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Bbox {
    pub fn intersects(&self, other: &Bbox) -> bool {
        let ax2 = self.x + self.w;
        let ay2 = self.y + self.h;
        let bx2 = other.x + other.w;
        let by2 = other.y + other.h;
        !(ax2 <= other.x || bx2 <= self.x || ay2 <= other.y || by2 <= self.y)
    }
}

/// One tile that changed between two frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TileChange {
    pub tile_x: u32,
    pub tile_y: u32,
    pub bbox: Bbox,
    pub prev_hash: u64,
    pub next_hash: u64,
}

/// Result of a diff call. `prev_hashes` is the hash grid for the new
/// frame, ready to be reused as the `prev` argument on the next call —
/// the diff loop allocates that grid in a non-arena `Vec` so it can outlive
/// the per-call bump arena. Tile changes are arena-allocated.
pub struct DiffResult<'arena> {
    pub width: u32,
    pub height: u32,
    pub tile: u32,
    pub grid_w: u32,
    pub grid_h: u32,
    pub hashes: Vec<u64>,
    pub changes: BumpVec<'arena, TileChange>,
}

impl<'arena> DiffResult<'arena> {
    pub fn changed(&self) -> &[TileChange] {
        self.changes.as_slice()
    }
    pub fn is_identical(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Compute the per-tile hash grid for an image. Used to seed the prior
/// grid on the first frame (when there's no `prev`).
pub fn compute_hash_grid(img: &DecodedImage, tile: u32) -> Result<Vec<u64>, VisionError> {
    if tile == 0 {
        return Err(VisionError::Other(anyhow::anyhow!("tile size must be > 0")));
    }
    let grid_w = (img.width + tile - 1) / tile;
    let grid_h = (img.height + tile - 1) / tile;
    let mut hashes = vec![0u64; (grid_w * grid_h) as usize];
    for ty in 0..grid_h {
        for tx in 0..grid_w {
            let h = hash_tile(img, tx, ty, tile);
            hashes[(ty * grid_w + tx) as usize] = h;
        }
    }
    Ok(hashes)
}

/// Diff `next` against `prev_hashes`. Returns a fresh grid for the new
/// frame and a Bump-allocated change list. If frames have different
/// dimensions, returns [`VisionError::DimensionsMismatch`].
pub fn diff<'arena>(
    next: &DecodedImage,
    prev_hashes: &[u64],
    prev_dims: (u32, u32),
    tile: u32,
    arena: &'arena Bump,
) -> Result<DiffResult<'arena>, VisionError> {
    if (next.width, next.height) != prev_dims {
        return Err(VisionError::DimensionsMismatch {
            prev: prev_dims,
            next: (next.width, next.height),
        });
    }
    if tile == 0 {
        return Err(VisionError::Other(anyhow::anyhow!("tile size must be > 0")));
    }
    let grid_w = (next.width + tile - 1) / tile;
    let grid_h = (next.height + tile - 1) / tile;
    let expected = (grid_w * grid_h) as usize;
    if prev_hashes.len() != expected {
        return Err(VisionError::Other(anyhow::anyhow!(
            "prev hash grid len {} != expected {}",
            prev_hashes.len(),
            expected
        )));
    }
    let mut hashes = vec![0u64; expected];
    let mut changes = BumpVec::with_capacity_in(16, arena);

    for ty in 0..grid_h {
        for tx in 0..grid_w {
            let idx = (ty * grid_w + tx) as usize;
            let h = hash_tile(next, tx, ty, tile);
            hashes[idx] = h;
            let prev = prev_hashes[idx];
            if prev != h {
                let x0 = tx * tile;
                let y0 = ty * tile;
                let w = (tile).min(next.width - x0);
                let height = (tile).min(next.height - y0);
                changes.push(TileChange {
                    tile_x: tx,
                    tile_y: ty,
                    bbox: Bbox {
                        x: x0,
                        y: y0,
                        w,
                        h: height,
                    },
                    prev_hash: prev,
                    next_hash: h,
                });
            }
        }
    }

    Ok(DiffResult {
        width: next.width,
        height: next.height,
        tile,
        grid_w,
        grid_h,
        hashes,
        changes,
    })
}

/// Hash a single tile. Uses `wide::u32x8` to fold 8 RGBA-pixel rows at a
/// time into a 256-bit accumulator, then finalizes with xxhash3 over the
/// reduced bytes. Tested below to match a scalar reference implementation.
#[inline]
fn hash_tile(img: &DecodedImage, tx: u32, ty: u32, tile: u32) -> u64 {
    let x0 = tx * tile;
    let y0 = ty * tile;
    let w = (tile).min(img.width - x0);
    let h = (tile).min(img.height - y0);

    let stride = img.width as usize * 4;
    let mut acc = u32x8::ZERO;
    let mut hasher = XxHash64::with_seed(0);

    for row in 0..h {
        let row_start = (y0 + row) as usize * stride + x0 as usize * 4;
        let row_end = row_start + w as usize * 4;
        let bytes = &img.rgba[row_start..row_end];
        // Process 32-byte chunks (8 × u32 lanes).
        let mut i = 0;
        while i + 32 <= bytes.len() {
            let chunk = &bytes[i..i + 32];
            // Safe: 32 bytes copy into 8 u32s (little-endian).
            let lanes = [
                u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
                u32::from_le_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]),
                u32::from_le_bytes([chunk[12], chunk[13], chunk[14], chunk[15]]),
                u32::from_le_bytes([chunk[16], chunk[17], chunk[18], chunk[19]]),
                u32::from_le_bytes([chunk[20], chunk[21], chunk[22], chunk[23]]),
                u32::from_le_bytes([chunk[24], chunk[25], chunk[26], chunk[27]]),
                u32::from_le_bytes([chunk[28], chunk[29], chunk[30], chunk[31]]),
            ];
            let v = u32x8::new(lanes);
            acc = acc ^ v;
            i += 32;
        }
        if i < bytes.len() {
            // Tail: hash the residual bytes directly so width-misaligned
            // tiles still distinguish themselves.
            hasher.write(&bytes[i..]);
        }
    }
    // Mix the SIMD accumulator into the hasher.
    let folded: [u32; 8] = acc.to_array();
    for lane in folded {
        hasher.write_u32(lane);
    }
    hasher.write_u32(w);
    hasher.write_u32(h);
    hasher.finish()
}

/// Naive scalar reference, used by a property test below.
#[cfg(test)]
fn hash_tile_scalar(img: &DecodedImage, tx: u32, ty: u32, tile: u32) -> u64 {
    let x0 = tx * tile;
    let y0 = ty * tile;
    let w = tile.min(img.width - x0);
    let h = tile.min(img.height - y0);
    let stride = img.width as usize * 4;
    let mut acc = [0u32; 8];
    let mut hasher = XxHash64::with_seed(0);
    for row in 0..h {
        let row_start = (y0 + row) as usize * stride + x0 as usize * 4;
        let row_end = row_start + w as usize * 4;
        let bytes = &img.rgba[row_start..row_end];
        let mut i = 0;
        while i + 32 <= bytes.len() {
            for lane in 0..8 {
                let off = i + lane * 4;
                let v = u32::from_le_bytes([
                    bytes[off],
                    bytes[off + 1],
                    bytes[off + 2],
                    bytes[off + 3],
                ]);
                acc[lane] ^= v;
            }
            i += 32;
        }
        if i < bytes.len() {
            hasher.write(&bytes[i..]);
        }
    }
    for lane in acc {
        hasher.write_u32(lane);
    }
    hasher.write_u32(w);
    hasher.write_u32(h);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fixture(w: u32, h: u32, fill: u8) -> DecodedImage {
        let bytes = vec![fill; (w * h * 4) as usize];
        DecodedImage {
            width: w,
            height: h,
            rgba: Arc::new(bytes),
            captured_us: 0,
        }
    }

    #[test]
    fn identical_frames_yield_empty() {
        let a = fixture(256, 128, 0xAB);
        let prev = compute_hash_grid(&a, 64).expect("grid");
        let arena = Bump::new();
        let res = diff(&a, &prev, (256, 128), 64, &arena).expect("diff");
        assert!(res.is_identical(), "got {:?}", res.changed());
    }

    #[test]
    fn single_tile_changed() {
        let a = fixture(256, 128, 0xAB);
        let prev = compute_hash_grid(&a, 64).expect("grid");
        // Mutate one pixel inside tile (1, 1).
        let mut bytes = (*a.rgba).clone();
        let stride = (a.width as usize) * 4;
        let px = 64 * 1 + 5; // x within tile 1
        let py = 64 * 1 + 5; // y within tile 1
        bytes[(py as usize) * stride + (px as usize) * 4] ^= 0xFF;
        let b = DecodedImage {
            width: a.width,
            height: a.height,
            rgba: Arc::new(bytes),
            captured_us: 0,
        };
        let arena = Bump::new();
        let res = diff(&b, &prev, (256, 128), 64, &arena).expect("diff");
        assert_eq!(res.changed().len(), 1, "{:?}", res.changed());
        let c = res.changed()[0];
        assert_eq!(c.tile_x, 1);
        assert_eq!(c.tile_y, 1);
        assert_eq!(
            c.bbox,
            Bbox {
                x: 64,
                y: 64,
                w: 64,
                h: 64
            }
        );
    }

    #[test]
    fn dim_mismatch_errors() {
        let a = fixture(64, 64, 0);
        let prev = compute_hash_grid(&a, 64).expect("grid");
        let b = fixture(128, 64, 0);
        let arena = Bump::new();
        let res = diff(&b, &prev, (64, 64), 64, &arena);
        match res {
            Err(VisionError::DimensionsMismatch { .. }) => {}
            Err(e) => panic!("wrong error variant: {e}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn wide_simd_matches_scalar_reference() {
        // Deterministic pseudo-random pixel pattern.
        let w = 256;
        let h = 192;
        let mut bytes = vec![0u8; (w * h * 4) as usize];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((i * 2654435761usize) & 0xFF) as u8;
        }
        let img = DecodedImage {
            width: w,
            height: h,
            rgba: Arc::new(bytes),
            captured_us: 0,
        };
        for ty in 0..(h / 64) {
            for tx in 0..(w / 64) {
                let s = hash_tile_scalar(&img, tx, ty, 64);
                let v = hash_tile(&img, tx, ty, 64);
                assert_eq!(s, v, "mismatch at tile ({tx},{ty})");
            }
        }
    }

    #[test]
    fn bbox_intersects() {
        let a = Bbox {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        };
        let b = Bbox {
            x: 5,
            y: 5,
            w: 10,
            h: 10,
        };
        let c = Bbox {
            x: 100,
            y: 100,
            w: 5,
            h: 5,
        };
        assert!(a.intersects(&b));
        assert!(!a.intersects(&c));
    }
}
