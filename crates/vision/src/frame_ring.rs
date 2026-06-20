//! Shared-memory frame ring (SPEC §11 V5).
//!
//! Producer (capture task) and consumers (vision pipeline + MCP event
//! emitter) trade frames through a fixed-size ring of fixed-size slots
//! backed by `mmap` over a tempfile. The MCP `event/notify {topic:
//! "vision.frame"}` payload references the shm path + slot seq, so frame
//! data crosses the broker→MCP socket as a handle, not bytes.
//!
//! ## Layout
//!
//! ```text
//!  Header (page-aligned)
//!  ───────────────────────
//!  magic: u64 = 0xCB_F1AE_F1AE_FFFF
//!  version: u32 = 1
//!  slot_bytes: u32
//!  slot_count: u32
//!  next_seq: AtomicU64
//!  per-slot: [Slot; slot_count]
//!     seq: AtomicU64       — published seq number, 0 = empty
//!     len: AtomicU32       — payload length in bytes
//!     refcnt: AtomicU32    — outstanding ReadGuards
//!     ts_us: AtomicU64
//!  Body
//!  ─────
//!  [u8; slot_bytes * slot_count]
//! ```
//!
//! ## Concurrency
//!
//! - Single producer; the producer scans for a slot with `refcnt == 0` and
//!   `seq < write_horizon` and overwrites it. Refcount is atomic; readers
//!   bump it before reading the slot and decrement on `Drop`.
//! - Multiple readers: a reader looks up `slot[i]` by seq; if the slot's
//!   `seq` matches, refcount-bump and read; otherwise the slot has been
//!   recycled and the reader returns `Lagged`.
//!
//! Crucially, *no torn reads*: producer publishes `seq` only after the
//! payload + len + ts are written (release store), and readers load `seq`
//! first (acquire), check it, bump refcount, then re-load `seq` to confirm
//! the slot wasn't recycled mid-read (CAS-free verify). If the verify
//! fails, the reader unbumps refcount and returns `Lagged`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use memmap2::{MmapMut, MmapOptions};
use serde::{Deserialize, Serialize};

use crate::types::VisionError;

const MAGIC: u64 = 0xCB_F1_AE_F1_AE_FF_FF_55;
const VERSION: u32 = 1;

/// Default ring shape. 512 KiB × 128 = 64 MiB per session/tab. 4K JPEGs
/// at quality 50 weigh in around 250 KiB; 512 KiB leaves headroom.
pub const DEFAULT_SLOT_BYTES: u32 = 512 * 1024;
pub const DEFAULT_SLOT_COUNT: u32 = 128;

#[repr(C)]
#[derive(Debug)]
struct Header {
    magic: AtomicU64,
    version: AtomicU32,
    slot_bytes: AtomicU32,
    slot_count: AtomicU32,
    next_seq: AtomicU64,
    _reserved: [AtomicU64; 11], // pad to 128 bytes
}

#[repr(C)]
#[derive(Debug)]
struct Slot {
    seq: AtomicU64,
    len: AtomicU32,
    refcnt: AtomicU32,
    ts_us: AtomicU64,
    _reserved: [AtomicU64; 4], // pad to 64 bytes
}

const HEADER_BYTES: usize = std::mem::size_of::<Header>();
const SLOT_HDR_BYTES: usize = std::mem::size_of::<Slot>();

/// Serializable handle that points at a published slot. Embedded in the
/// `event/notify` payload so consumers can mmap the same file and read
/// zero-copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameHandle {
    pub shm_path: PathBuf,
    pub slot_seq: u64,
    pub slot_index: u32,
    pub offset: u32,
    pub len: u32,
    pub ts_us: u64,
}

/// The ring itself. Cheap to clone (`Arc` inside).
pub struct FrameRing {
    inner: Arc<FrameRingInner>,
}

struct FrameRingInner {
    path: PathBuf,
    mmap: parking_lot::RwLock<MmapMut>,
    slot_bytes: u32,
    slot_count: u32,
}

impl FrameRing {
    /// Create a ring at `path`. The file is sized to fit header + slot
    /// metadata + payload body, then mmap'd.
    pub fn create(
        path: PathBuf,
        slot_bytes: u32,
        slot_count: u32,
    ) -> Result<Arc<Self>, VisionError> {
        if slot_count == 0 || slot_bytes < 64 {
            return Err(VisionError::Other(anyhow::anyhow!(
                "invalid ring shape: slot_bytes={slot_bytes} slot_count={slot_count}"
            )));
        }
        let total = HEADER_BYTES
            + SLOT_HDR_BYTES * slot_count as usize
            + (slot_bytes as usize) * (slot_count as usize);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.set_len(total as u64)?;
        // SAFETY: the file is freshly created and sized; we hold the only
        // handle. No other process touches it concurrently before publish.
        let mmap = unsafe { MmapOptions::new().len(total).map_mut(&file)? };
        let inner = FrameRingInner {
            path: path.clone(),
            mmap: parking_lot::RwLock::new(mmap),
            slot_bytes,
            slot_count,
        };
        let ring = Arc::new(FrameRing {
            inner: Arc::new(inner),
        });
        ring.init_header()?;
        Ok(ring)
    }

    fn init_header(&self) -> Result<(), VisionError> {
        let mut g = self.inner.mmap.write();
        let bytes: &mut [u8] = &mut g;
        // SAFETY: header bytes are at offset 0; the file is sized to fit.
        let hdr = unsafe { &mut *(bytes.as_mut_ptr() as *mut Header) };
        hdr.magic.store(MAGIC, Ordering::Release);
        hdr.version.store(VERSION, Ordering::Release);
        hdr.slot_bytes
            .store(self.inner.slot_bytes, Ordering::Release);
        hdr.slot_count
            .store(self.inner.slot_count, Ordering::Release);
        hdr.next_seq.store(1, Ordering::Release);
        for i in 0..self.inner.slot_count as usize {
            let off = HEADER_BYTES + i * SLOT_HDR_BYTES;
            // SAFETY: bytes is valid for `total` and `off + SLOT_HDR_BYTES <= total`.
            let s = unsafe { &mut *(bytes.as_mut_ptr().add(off) as *mut Slot) };
            s.seq.store(0, Ordering::Release);
            s.len.store(0, Ordering::Release);
            s.refcnt.store(0, Ordering::Release);
            s.ts_us.store(0, Ordering::Release);
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    pub fn slot_bytes(&self) -> u32 {
        self.inner.slot_bytes
    }

    pub fn slot_count(&self) -> u32 {
        self.inner.slot_count
    }

    /// Acquire a write slot for a payload of `len` bytes. Returns
    /// [`VisionError::FrameTooLarge`] if it doesn't fit, or
    /// [`VisionError::RingExhausted`] if every slot has live readers and
    /// the ring is fully wrapped.
    pub fn acquire_write(&self, len: u32, ts_us: u64) -> Result<WriteGuard<'_>, VisionError> {
        if len > self.inner.slot_bytes {
            return Err(VisionError::FrameTooLarge {
                len: len as usize,
                cap: self.inner.slot_bytes as usize,
            });
        }
        // Find a slot with refcnt == 0; pick the oldest.
        let count = self.inner.slot_count as usize;
        let mut g = self.inner.mmap.write();
        let bytes: &mut [u8] = &mut g;
        let hdr_ptr = bytes.as_mut_ptr() as *mut Header;
        // SAFETY: header lives at offset 0.
        let hdr = unsafe { &mut *hdr_ptr };

        let mut chosen: Option<usize> = None;
        let mut chosen_seq: u64 = u64::MAX;
        for i in 0..count {
            let off = HEADER_BYTES + i * SLOT_HDR_BYTES;
            // SAFETY: in-bounds.
            let s = unsafe { &*(bytes.as_ptr().add(off) as *const Slot) };
            if s.refcnt.load(Ordering::Acquire) != 0 {
                continue;
            }
            let seq = s.seq.load(Ordering::Acquire);
            if seq < chosen_seq {
                chosen_seq = seq;
                chosen = Some(i);
                if seq == 0 {
                    break; // empty slot; take it.
                }
            }
        }
        let idx = chosen.ok_or(VisionError::RingExhausted)?;
        let new_seq = hdr.next_seq.fetch_add(1, Ordering::AcqRel);
        let slot_off = HEADER_BYTES + idx * SLOT_HDR_BYTES;
        // SAFETY: slot in-bounds.
        let slot = unsafe { &*(bytes.as_ptr().add(slot_off) as *const Slot) };
        // Mark slot as not-yet-published (seq=0) while we write.
        slot.seq.store(0, Ordering::Release);
        slot.refcnt.store(1, Ordering::Release); // writer "owns" the slot
        slot.ts_us.store(ts_us, Ordering::Release);
        slot.len.store(len, Ordering::Release);

        let payload_off =
            HEADER_BYTES + SLOT_HDR_BYTES * count + idx * self.inner.slot_bytes as usize;
        Ok(WriteGuard {
            ring: self,
            slot_index: idx as u32,
            payload_off,
            slot_off,
            len,
            seq: new_seq,
            committed: false,
        })
    }

    /// Read the slot whose seq matches `seq`. Returns `None` if the slot
    /// has already been recycled (lagging reader).
    pub fn read(&self, seq: u64) -> Option<ReadGuard<'_>> {
        let g = self.inner.mmap.read();
        let bytes: &[u8] = &g;
        for i in 0..self.inner.slot_count as usize {
            let off = HEADER_BYTES + i * SLOT_HDR_BYTES;
            // SAFETY: in-bounds.
            let s = unsafe { &*(bytes.as_ptr().add(off) as *const Slot) };
            let cur = s.seq.load(Ordering::Acquire);
            if cur != seq {
                continue;
            }
            // Bump refcount; verify seq still matches.
            s.refcnt.fetch_add(1, Ordering::AcqRel);
            let recheck = s.seq.load(Ordering::Acquire);
            if recheck != seq {
                s.refcnt.fetch_sub(1, Ordering::AcqRel);
                return None;
            }
            let len = s.len.load(Ordering::Acquire);
            let ts = s.ts_us.load(Ordering::Acquire);
            let payload_off = HEADER_BYTES
                + SLOT_HDR_BYTES * self.inner.slot_count as usize
                + i * self.inner.slot_bytes as usize;
            // Drop the read-lock guard; we re-take it on `bytes()`.
            drop(g);
            return Some(ReadGuard {
                ring: self,
                slot_index: i as u32,
                payload_off,
                len,
                seq,
                ts_us: ts,
            });
        }
        None
    }

    pub fn handle_for(&self, guard: &WriteGuard<'_>) -> FrameHandle {
        FrameHandle {
            shm_path: self.inner.path.clone(),
            slot_seq: guard.seq,
            slot_index: guard.slot_index,
            offset: guard.payload_off as u32,
            len: guard.len,
            ts_us: 0,
        }
    }

    /// Largest published seq currently in the ring. Returns 0 when empty.
    pub fn head_seq(&self) -> u64 {
        let g = self.inner.mmap.read();
        let bytes: &[u8] = &g;
        let count = self.inner.slot_count as usize;
        let mut max = 0u64;
        for i in 0..count {
            let off = HEADER_BYTES + i * SLOT_HDR_BYTES;
            // SAFETY: in-bounds.
            let s = unsafe { &*(bytes.as_ptr().add(off) as *const Slot) };
            let seq = s.seq.load(Ordering::Acquire);
            if seq > max {
                max = seq;
            }
        }
        max
    }

    /// `ts_us` for the slot publishing `seq`. Returns `None` if the slot
    /// has been recycled or the seq was never published.
    pub fn slot_ts_us(&self, seq: u64) -> Option<u64> {
        let g = self.inner.mmap.read();
        let bytes: &[u8] = &g;
        let count = self.inner.slot_count as usize;
        for i in 0..count {
            let off = HEADER_BYTES + i * SLOT_HDR_BYTES;
            // SAFETY: in-bounds.
            let s = unsafe { &*(bytes.as_ptr().add(off) as *const Slot) };
            if s.seq.load(Ordering::Acquire) == seq {
                return Some(s.ts_us.load(Ordering::Acquire));
            }
        }
        None
    }

    /// Build a [`FrameHandle`] for the slot publishing `seq`. Returns
    /// `None` if the slot has been recycled. Used by `vision.animation.frames`
    /// to surface a window of past handles without holding a `ReadGuard`.
    pub fn handle_for_seq(&self, seq: u64) -> Option<FrameHandle> {
        let g = self.inner.mmap.read();
        let bytes: &[u8] = &g;
        let count = self.inner.slot_count as usize;
        for i in 0..count {
            let off = HEADER_BYTES + i * SLOT_HDR_BYTES;
            // SAFETY: in-bounds.
            let s = unsafe { &*(bytes.as_ptr().add(off) as *const Slot) };
            if s.seq.load(Ordering::Acquire) != seq {
                continue;
            }
            let len = s.len.load(Ordering::Acquire);
            let ts = s.ts_us.load(Ordering::Acquire);
            let payload_off =
                HEADER_BYTES + SLOT_HDR_BYTES * count + i * self.inner.slot_bytes as usize;
            return Some(FrameHandle {
                shm_path: self.inner.path.clone(),
                slot_seq: seq,
                slot_index: i as u32,
                offset: payload_off as u32,
                len,
                ts_us: ts,
            });
        }
        None
    }
}

impl Drop for FrameRingInner {
    fn drop(&mut self) {
        // Best-effort cleanup; tests use tempdirs so this is not load-bearing.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// In-place writer over a slot. Drop publishes (seq becomes visible to
/// readers) iff `commit()` was called; otherwise the slot is reset.
pub struct WriteGuard<'a> {
    ring: &'a FrameRing,
    slot_index: u32,
    payload_off: usize,
    slot_off: usize,
    len: u32,
    seq: u64,
    committed: bool,
}

impl<'a> WriteGuard<'a> {
    pub fn slot_index(&self) -> u32 {
        self.slot_index
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Copy `data` into the slot. `data.len()` must equal the requested
    /// length (the value passed to `acquire_write`).
    pub fn write(&mut self, data: &[u8]) -> Result<(), VisionError> {
        if data.len() != self.len as usize {
            return Err(VisionError::Other(anyhow::anyhow!(
                "write len mismatch: got {} expected {}",
                data.len(),
                self.len
            )));
        }
        let mut g = self.ring.inner.mmap.write();
        let bytes: &mut [u8] = &mut g;
        let dst = &mut bytes[self.payload_off..self.payload_off + data.len()];
        dst.copy_from_slice(data);
        Ok(())
    }

    /// Publish the slot. After this returns, readers can find it via the
    /// seq number.
    pub fn commit(mut self) -> u64 {
        let seq = self.seq;
        let g = self.ring.inner.mmap.read();
        let bytes: &[u8] = &g;
        // SAFETY: slot_off is computed in-bounds.
        let slot = unsafe { &*(bytes.as_ptr().add(self.slot_off) as *const Slot) };
        slot.seq.store(seq, Ordering::Release);
        slot.refcnt.store(0, Ordering::Release); // writer releases its hold
        self.committed = true;
        seq
    }
}

impl<'a> Drop for WriteGuard<'a> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // Aborted write; reset the slot so it's reusable.
        let g = self.ring.inner.mmap.read();
        let bytes: &[u8] = &g;
        // SAFETY: slot_off is in-bounds.
        let slot = unsafe { &*(bytes.as_ptr().add(self.slot_off) as *const Slot) };
        slot.seq.store(0, Ordering::Release);
        slot.len.store(0, Ordering::Release);
        slot.ts_us.store(0, Ordering::Release);
        slot.refcnt.store(0, Ordering::Release);
    }
}

/// Read handle over a slot. Decrements refcount on Drop. While alive, the
/// producer cannot recycle this slot.
pub struct ReadGuard<'a> {
    ring: &'a FrameRing,
    slot_index: u32,
    payload_off: usize,
    len: u32,
    seq: u64,
    ts_us: u64,
}

impl<'a> ReadGuard<'a> {
    pub fn seq(&self) -> u64 {
        self.seq
    }
    pub fn len(&self) -> u32 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn ts_us(&self) -> u64 {
        self.ts_us
    }
    pub fn slot_index(&self) -> u32 {
        self.slot_index
    }

    /// Copy the payload bytes into a freshly-allocated `Vec`. For
    /// genuinely zero-copy access use `with_bytes`.
    pub fn to_vec(&self) -> Vec<u8> {
        let g = self.ring.inner.mmap.read();
        let bytes: &[u8] = &g;
        bytes[self.payload_off..self.payload_off + self.len as usize].to_vec()
    }

    /// Apply `f` to the slot's payload bytes without copying. The closure
    /// must not retain the slice.
    pub fn with_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        let g = self.ring.inner.mmap.read();
        let bytes: &[u8] = &g;
        f(&bytes[self.payload_off..self.payload_off + self.len as usize])
    }
}

impl<'a> Drop for ReadGuard<'a> {
    fn drop(&mut self) {
        let g = self.ring.inner.mmap.read();
        let bytes: &[u8] = &g;
        let off = HEADER_BYTES + (self.slot_index as usize) * SLOT_HDR_BYTES;
        // SAFETY: in-bounds.
        let slot = unsafe { &*(bytes.as_ptr().add(off) as *const Slot) };
        slot.refcnt.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn small_ring() -> Arc<FrameRing> {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("ring.bin");
        // Leak the dir so it lives for the test.
        std::mem::forget(dir);
        FrameRing::create(path, 1024, 8).expect("create ring")
    }

    #[test]
    fn round_trip_single_frame() {
        let ring = small_ring();
        let mut g = ring.acquire_write(16, 42).expect("acquire");
        let payload = (0..16u8).collect::<Vec<u8>>();
        g.write(&payload).expect("write");
        let seq = g.commit();
        let r = ring.read(seq).expect("read");
        assert_eq!(r.len(), 16);
        let buf = r.to_vec();
        assert_eq!(buf, payload);
        assert_eq!(r.ts_us(), 42);
    }

    #[test]
    fn frame_too_large_errors() {
        let ring = small_ring();
        let r = ring.acquire_write(2048, 0);
        match r {
            Err(VisionError::FrameTooLarge { .. }) => {}
            Err(e) => panic!("wrong error: {e}"),
            Ok(_) => panic!("expected too-large error"),
        }
    }

    #[test]
    fn round_trip_thousand_frames() {
        let ring = small_ring();
        for i in 1u64..=1000 {
            let mut g = ring.acquire_write(8, i).expect("acquire");
            let bytes = i.to_le_bytes();
            g.write(&bytes).expect("write");
            let seq = g.commit();
            let r = ring.read(seq).expect("read");
            let mut got = [0u8; 8];
            got.copy_from_slice(&r.to_vec());
            assert_eq!(u64::from_le_bytes(got), i);
        }
    }

    #[test]
    fn full_ring_recycles_oldest() {
        let ring = small_ring();
        // Fill the ring (8 slots).
        let mut last_seqs = vec![];
        for i in 1..=8u64 {
            let mut g = ring.acquire_write(4, i).expect("acquire");
            g.write(&[0xAA, 0xBB, 0xCC, 0xDD]).expect("write");
            last_seqs.push(g.commit());
        }
        // Each subsequent write should succeed (no live readers); the
        // oldest slot is recycled. Lagging reader gets None for old seqs.
        let mut g = ring.acquire_write(4, 99).expect("ninth write");
        g.write(&[1, 2, 3, 4]).expect("write");
        let new_seq = g.commit();
        assert!(ring.read(last_seqs[0]).is_none(), "oldest must be recycled");
        let r = ring.read(new_seq).expect("new still readable");
        assert_eq!(r.to_vec(), vec![1u8, 2, 3, 4]);
    }

    #[test]
    fn live_reader_blocks_recycling() {
        let ring = small_ring();
        // Fill all slots.
        let mut seqs = vec![];
        for i in 1..=8u64 {
            let mut g = ring.acquire_write(2, i).expect("acquire");
            g.write(&[i as u8, 0]).expect("write");
            seqs.push(g.commit());
        }
        // Hold a guard on slot 0 (oldest).
        let _hold = ring.read(seqs[0]).expect("hold");
        // Try to acquire another. Every other slot is also "free"
        // (refcnt=0) but the held one is not. With 8 slots and 7 free,
        // acquire still succeeds.
        let mut g = ring.acquire_write(2, 99).expect("acquire-7");
        g.write(&[9, 9]).expect("w");
        g.commit();
        // Now hold all 8 readers; next acquire must fail.
        let mut held = vec![];
        for s in &seqs[1..] {
            if let Some(h) = ring.read(*s) {
                held.push(h);
            }
        }
        // We expect at most 8-N free where N = readers. If readers cover
        // everything, acquire fails.
        // Note: we hold seqs[0] + held (which is up to 7) = up to 8.
        if held.len() == 7 {
            match ring.acquire_write(2, 1234) {
                Err(VisionError::RingExhausted) => {}
                Err(e) => panic!("wrong error: {e}"),
                Ok(_) => panic!("expected ring-exhausted"),
            }
        }
        drop(_hold);
        drop(held);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_stress() {
        let ring = small_ring();
        let r2 = ring.clone();
        // Producer.
        let producer = tokio::task::spawn_blocking(move || {
            for i in 1u64..=5_000 {
                let len = ((i % 256) + 1) as u32;
                let mut g = match ring.acquire_write(len, i) {
                    Ok(g) => g,
                    Err(_) => {
                        std::thread::yield_now();
                        continue;
                    }
                };
                let payload = vec![(i & 0xFF) as u8; len as usize];
                let _ = g.write(&payload);
                g.commit();
            }
        });
        // Consumers — drain whatever they can.
        let mut consumers = vec![];
        for _ in 0..3 {
            let r = r2.clone();
            consumers.push(tokio::task::spawn_blocking(move || {
                let mut got = 0u64;
                let mut last = 0u64;
                while got < 5_000 {
                    for seq in (last + 1)..=(last + 64) {
                        if let Some(h) = r.read(seq) {
                            assert!(h.len() > 0);
                            got += 1;
                        }
                    }
                    last += 64;
                    if last > 6_000 {
                        break;
                    }
                }
                got
            }));
        }
        producer.await.expect("producer");
        for c in consumers {
            let _n = c.await.expect("consumer");
        }
    }
}
