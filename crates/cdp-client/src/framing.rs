//! NUL-delimited JSON framing for `--remote-debugging-pipe`.
//!
//! Owned by `cdp-client`. Per SPEC §2, broker ↔ Chromium uses NUL-delimited
//! JSON over fd 3 (write) and fd 4 (read), 100MB cap per frame.
//!
//! Two pure functions:
//!
//! * [`encode_frame`] — serialize a `serde_json::Value` and append `0x00`.
//! * [`encode_frame_into`] — zero-alloc variant: serialize into a caller
//!   scratch buffer (SPEC §11 V5 hot path).
//! * [`Decoder`] — stateful byte stream → Vec<Value> turning NUL-delimited
//!   bytes into parsed JSON values, with a hard cap on per-frame size.
//!
//! Threading: `Decoder` owns its buffer and is `Send + !Sync` (the reader
//! task is single-owner). `encode_frame` is pure.
//!
//! # HOT PATH — SPEC §11 V5
//!
//! `Decoder::feed_into` and `encode_frame_into` are the zero-allocation
//! entry points. `Decoder::feed` (returns owned Vec) is retained for tests
//! and one-shot callers; the connection actors must use `feed_into`.

use crate::error::FramingError;
use serde_json::Value;

/// Per SPEC §2: 100 MiB hard cap on a single CDP frame. Anything larger
/// almost certainly indicates a runaway protocol payload (e.g. a screencast
/// frame stream wedged) and should kill the connection rather than OOM us.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 100 * 1024 * 1024;

/// Serialize `v` as JSON and append a single NUL byte.
pub fn encode_frame(v: &Value) -> std::result::Result<Vec<u8>, FramingError> {
    let mut buf = serde_json::to_vec(v)?;
    buf.push(0x00);
    Ok(buf)
}

/// Zero-alloc variant of [`encode_frame`] — serialize into a caller scratch
/// buffer. The caller is responsible for `clear()`-ing between writes; this
/// function only appends. SPEC §11 V5 hot path: every CDP request the
/// broker sends to Chromium goes through this.
///
/// On error the partial bytes are *not* truncated — callers must `clear()`
/// the scratch before retrying.
pub fn encode_frame_into(
    scratch: &mut Vec<u8>,
    v: &Value,
) -> std::result::Result<(), FramingError> {
    serde_json::to_writer(&mut *scratch, v)?;
    scratch.push(0x00);
    Ok(())
}

/// Streaming decoder that splits a byte stream on NUL and parses each frame.
pub struct Decoder {
    buf: Vec<u8>,
    max: usize,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME_BYTES)
    }
}

impl Decoder {
    /// Construct with a custom per-frame cap.
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            buf: Vec::with_capacity(8192),
            max: max_frame_bytes,
        }
    }

    /// Number of bytes currently buffered (= bytes of the partial frame at
    /// the head of the buffer that has not yet seen its NUL).
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Feed raw bytes from the pipe. Returns every fully-framed JSON value
    /// completed by this chunk, in order.
    ///
    /// On a `FrameTooLarge` error the decoder is left in a poisoned state —
    /// the caller should drop it and close the connection.
    pub fn feed(&mut self, chunk: &[u8]) -> std::result::Result<Vec<Value>, FramingError> {
        let mut out = Vec::new();
        self.feed_into(chunk, &mut out)?;
        Ok(out)
    }

    /// Zero-alloc variant of [`Self::feed`]: appends parsed values to the
    /// caller-provided `out` buffer. SPEC §11 V5 hot path — the connection
    /// reader actor reuses one `Vec<Value>` across every read so we eat the
    /// allocation exactly once.
    ///
    /// `out` is *appended to*; it is the caller's responsibility to drain
    /// or clear it between calls if a clean batch boundary is desired.
    pub fn feed_into(
        &mut self,
        chunk: &[u8],
        out: &mut Vec<Value>,
    ) -> std::result::Result<(), FramingError> {
        // Hot-path: append, then split on every NUL we find.
        if self.buf.len().saturating_add(chunk.len()) > self.max
            && !chunk.contains(&0u8)
            && self.buf.iter().all(|&b| b != 0)
        {
            // No NUL anywhere in the buffered + incoming bytes, but together
            // they would already cross the cap — fail eagerly.
            return Err(FramingError::FrameTooLarge { limit: self.max });
        }
        self.buf.extend_from_slice(chunk);

        let mut start: usize = 0;
        // Walk the buffer and emit each complete frame.
        for (i, &b) in self.buf.iter().enumerate() {
            if b == 0x00 {
                // Empty frame is allowed but skipped (some implementations
                // emit a leading or trailing NUL).
                if i > start {
                    let slice = &self.buf[start..i];
                    if slice.len() > self.max {
                        return Err(FramingError::FrameTooLarge { limit: self.max });
                    }
                    out.push(serde_json::from_slice::<Value>(slice)?);
                }
                start = i + 1;
            }
        }
        // Compact the residual partial frame to the front of the buffer.
        if start > 0 {
            self.buf.drain(..start);
        }
        // After draining, recheck the cap on the residual partial frame.
        if self.buf.len() > self.max {
            return Err(FramingError::FrameTooLarge { limit: self.max });
        }
        Ok(())
    }

    /// Called when the underlying pipe reports EOF. Returns `Ok(())` if
    /// there is no partial frame, `Err(UnexpectedEof)` otherwise.
    pub fn finish(self) -> std::result::Result<(), FramingError> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(FramingError::UnexpectedEof)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one(v: Value) -> Vec<u8> {
        encode_frame(&v).unwrap()
    }

    #[test]
    fn round_trip_single() {
        let mut dec = Decoder::default();
        let bytes = one(json!({"id": 1, "method": "Browser.getVersion"}));
        let frames = dec.feed(&bytes).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["method"], "Browser.getVersion");
        assert_eq!(dec.buffered(), 0);
    }

    #[test]
    fn round_trip_multi_in_one_chunk() {
        let mut dec = Decoder::default();
        let mut bytes = Vec::new();
        bytes.extend(one(json!({"id": 1})));
        bytes.extend(one(json!({"id": 2})));
        bytes.extend(one(json!({"id": 3})));
        let frames = dec.feed(&bytes).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0]["id"], 1);
        assert_eq!(frames[1]["id"], 2);
        assert_eq!(frames[2]["id"], 3);
    }

    #[test]
    fn split_across_many_reads() {
        let mut dec = Decoder::default();
        let bytes = one(json!({"id": 42, "result": {"protocolVersion": "1.3"}}));
        // Feed one byte at a time.
        let mut got = Vec::new();
        for b in &bytes {
            got.extend(dec.feed(std::slice::from_ref(b)).unwrap());
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["result"]["protocolVersion"], "1.3");
    }

    #[test]
    fn empty_frame_between_two_valid_frames() {
        let mut dec = Decoder::default();
        let mut bytes = Vec::new();
        bytes.extend(one(json!({"id": 1})));
        bytes.push(0x00); // empty frame
        bytes.extend(one(json!({"id": 2})));
        let frames = dec.feed(&bytes).unwrap();
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn frame_exceeding_cap_rejected() {
        // Cap at 1KB to keep test fast.
        let mut dec = Decoder::new(1024);
        // 2KB without a NUL → should error.
        let big = vec![b'a'; 2048];
        let err = dec.feed(&big).unwrap_err();
        assert!(matches!(err, FramingError::FrameTooLarge { limit: 1024 }));
    }

    #[test]
    fn frame_exactly_at_cap_ok() {
        let mut dec = Decoder::new(1024);
        // Build a JSON string of exactly 1024 bytes (without the NUL).
        // 1024 bytes of payload + 1 byte NUL.
        let payload = "\"".to_string() + &"a".repeat(1022) + "\"";
        assert_eq!(payload.len(), 1024);
        let mut bytes = payload.into_bytes();
        bytes.push(0x00);
        let frames = dec.feed(&bytes).unwrap();
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn finish_reports_unexpected_eof_when_partial() {
        let mut dec = Decoder::default();
        let _ = dec.feed(b"{\"id\":1}").unwrap(); // no NUL → still buffered
        let err = dec.finish().unwrap_err();
        assert!(matches!(err, FramingError::UnexpectedEof));
    }

    #[test]
    fn finish_clean_when_drained() {
        let mut dec = Decoder::default();
        dec.feed(&one(json!({"id": 1}))).unwrap();
        dec.finish().unwrap();
    }

    #[test]
    fn malformed_json_in_frame_returns_json_error() {
        let mut dec = Decoder::default();
        let mut bytes = b"{not json".to_vec();
        bytes.push(0x00);
        let err = dec.feed(&bytes).unwrap_err();
        assert!(matches!(err, FramingError::Json(_)));
    }

    // SPEC §11 V5 — zero-alloc paths.

    #[test]
    fn encode_frame_into_appends_payload_then_nul() {
        let mut scratch = Vec::with_capacity(64);
        encode_frame_into(&mut scratch, &json!({"id": 1, "method": "x"})).unwrap();
        // Last byte is NUL.
        assert_eq!(scratch.last().copied(), Some(0x00));
        // Round-trips through Decoder.
        let mut dec = Decoder::default();
        let mut out = Vec::new();
        dec.feed_into(&scratch, &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["method"], "x");
    }

    #[test]
    fn feed_into_appends_to_caller_buffer_across_chunks() {
        let mut dec = Decoder::default();
        let mut out: Vec<Value> = Vec::new();
        let bytes_a = encode_frame(&json!({"id": 1})).unwrap();
        let bytes_b = encode_frame(&json!({"id": 2})).unwrap();
        dec.feed_into(&bytes_a, &mut out).unwrap();
        dec.feed_into(&bytes_b, &mut out).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["id"], 1);
        assert_eq!(out[1]["id"], 2);
    }

    #[test]
    fn feed_into_reuses_caller_buffer_after_drain() {
        // The hot-path contract: caller drains `out`, calls again, and the
        // Vec capacity is preserved — no per-call allocation.
        let mut dec = Decoder::default();
        let mut out: Vec<Value> = Vec::with_capacity(8);
        let cap_before = out.capacity();
        for i in 0..16u64 {
            let bytes = encode_frame(&json!({"id": i})).unwrap();
            dec.feed_into(&bytes, &mut out).unwrap();
            assert_eq!(out.len(), 1);
            out.clear();
        }
        // Capacity should be ≥ what we started with (Vec never shrinks on
        // clear). The point is that callers can preserve their scratch.
        assert!(out.capacity() >= cap_before);
    }

    #[test]
    fn encode_frame_into_then_decode_via_feed_round_trips_many() {
        let mut scratch = Vec::with_capacity(256);
        for i in 0..32u64 {
            encode_frame_into(&mut scratch, &json!({"id": i})).unwrap();
        }
        let mut dec = Decoder::default();
        let mut out: Vec<Value> = Vec::new();
        dec.feed_into(&scratch, &mut out).unwrap();
        assert_eq!(out.len(), 32);
        for (i, v) in out.iter().enumerate() {
            assert_eq!(v["id"], i as u64);
        }
    }

    /// Deterministic-seed pseudo-fuzz: feed the decoder ~5 MB of random
    /// bytes split into random-sized chunks. The decoder must NEVER panic
    /// regardless of input. Errors are valid outcomes (`FrameTooLarge`,
    /// `Json`, etc.); panics are not. This is the unit-test analogue of
    /// the cargo-fuzz target the hardening doctrine calls for, runnable on
    /// every `cargo test` lane without nightly tooling.
    #[test]
    fn random_byte_stream_never_panics() {
        // 64-bit xorshift — small, deterministic, plenty of entropy for
        // structural fuzz. Seeded so failures reproduce exactly.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // Cap the per-frame size at 32 KiB so we exercise the cap-rejection
        // path AND the parse-error path in the same run; a 100 MiB cap
        // would basically never hit the `FrameTooLarge` branch on 5 MiB of
        // input.
        let mut dec = Decoder::new(32 * 1024);
        let total: usize = 5 * 1024 * 1024;
        let mut written = 0usize;
        let mut chunk = vec![0u8; 4096];
        let mut out: Vec<Value> = Vec::new();
        while written < total {
            let chunk_len = ((next() % 4096) + 1) as usize;
            for byte in chunk[..chunk_len].iter_mut() {
                *byte = (next() & 0xff) as u8;
            }
            written += chunk_len;
            // We don't care what `feed_into` returns — only that it
            // doesn't panic. After a `FrameTooLarge` the decoder is
            // poisoned, so reset it and continue.
            match dec.feed_into(&chunk[..chunk_len], &mut out) {
                Ok(()) => {}
                Err(_) => {
                    dec = Decoder::new(32 * 1024);
                    out.clear();
                }
            }
        }
    }

    /// Adversarial: a single 64 MiB no-NUL chunk with the cap raised to
    /// 100 MiB. Without the lazy-parse contract the decoder would buffer
    /// the entire chunk in RAM. With it, the buffer must hold exactly
    /// what we fed (no NUL → no frame emitted). Confirms the OOM-by-bad-
    /// peer attack surface is bounded.
    #[test]
    fn buffered_size_tracks_input_when_no_nul() {
        let mut dec = Decoder::new(100 * 1024 * 1024);
        let chunk = vec![b'a'; 64 * 1024 * 1024];
        let frames = dec.feed(&chunk).expect("under cap");
        assert!(frames.is_empty());
        assert_eq!(dec.buffered(), 64 * 1024 * 1024);
    }
}
