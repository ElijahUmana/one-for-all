use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use vt100::{Callbacks, Color, MouseProtocolEncoding, MouseProtocolMode, Parser};

use crate::types::{
    TermAltScreenState, TermAttrRun, TermColor, TermCursor, TermExitState, TermMouseEncoding,
    TermMouseMode, TermScrollbackLine, TermSnapshot, TermSnapshotRow, TermStyle,
};

pub const DEFAULT_RAW_OUTPUT_BYTES: usize = 256 * 1024;
pub const DEFAULT_SCROLLBACK_LINES: usize = 4096;

#[derive(Clone)]
pub struct SharedState {
    parser: Arc<Mutex<Parser<ParserCallbacks>>>,
    callbacks: Arc<Mutex<CallbackState>>,
    output: Arc<Mutex<ByteRing>>,
    exit: Arc<Mutex<TermExitState>>,
    snapshot_seq: Arc<AtomicU64>,
    output_seq: Arc<AtomicU64>,
}

impl SharedState {
    pub fn new(rows: u16, cols: u16, raw_output_bytes: usize, scrollback_lines: usize) -> Self {
        let callbacks = Arc::new(Mutex::new(CallbackState::default()));
        let parser = Parser::new_with_callbacks(
            rows,
            cols,
            scrollback_lines,
            ParserCallbacks {
                shared: Arc::clone(&callbacks),
            },
        );
        Self {
            parser: Arc::new(Mutex::new(parser)),
            callbacks,
            output: Arc::new(Mutex::new(ByteRing::new(raw_output_bytes))),
            exit: Arc::new(Mutex::new(TermExitState {
                exited: false,
                exit_code: None,
                signal: None,
            })),
            snapshot_seq: Arc::new(AtomicU64::new(0)),
            output_seq: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn process_output(&self, data: &[u8]) -> u64 {
        let dropped = self.output.lock().push(data);
        self.parser.lock().process(data);
        dropped
    }

    pub fn next_output_seq(&self) -> u64 {
        self.output_seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn read_output(&self, max_bytes: usize) -> crate::types::TermReadChunk {
        let exit_eof = self.exit.lock().exited;
        self.output.lock().take(max_bytes, exit_eof)
    }

    pub fn mark_exit(&self, exit_code: Option<u32>, signal: Option<String>) {
        let mut exit = self.exit.lock();
        exit.exited = true;
        exit.exit_code = exit_code;
        exit.signal = signal;
    }

    pub fn build_snapshot(&self, session_id: &str) -> TermSnapshot {
        let snapshot_seq = self.snapshot_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let parser = self.parser.lock();
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let cursor = screen.cursor_position();
        let callback_state = self.callbacks.lock().clone();
        let exit = self.exit.lock().clone();
        let mut visible_rows = Vec::with_capacity(usize::from(rows));
        let row_texts: Vec<String> = screen.rows(0, cols).collect();
        for row_idx in 0..rows {
            let text = row_texts
                .get(usize::from(row_idx))
                .cloned()
                .unwrap_or_default();
            let attrs = row_attr_runs(screen, row_idx, cols);
            visible_rows.push(TermSnapshotRow {
                row: row_idx,
                text,
                wrapped: screen.row_wrapped(row_idx),
                attrs,
            });
        }
        TermSnapshot {
            snapshot_seq,
            session_id: session_id.to_owned(),
            rows,
            cols,
            cursor: TermCursor {
                row: cursor.0,
                col: cursor.1,
            },
            alt_screen_active: screen.alternate_screen(),
            mouse_mode: map_mouse_mode(screen.mouse_protocol_mode()),
            mouse_encoding: map_mouse_encoding(screen.mouse_protocol_encoding()),
            cursor_hidden: screen.hide_cursor(),
            window_title: callback_state.window_title,
            visible_rows,
            exit,
        }
    }

    pub fn alt_screen_state(&self) -> TermAltScreenState {
        let parser = self.parser.lock();
        let screen = parser.screen();
        let callback_state = self.callbacks.lock().clone();
        TermAltScreenState {
            active: screen.alternate_screen(),
            mouse_mode: map_mouse_mode(screen.mouse_protocol_mode()),
            mouse_encoding: map_mouse_encoding(screen.mouse_protocol_encoding()),
            cursor_hidden: screen.hide_cursor(),
            window_title: callback_state.window_title,
        }
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        self.parser.lock().screen_mut().set_size(rows, cols);
    }

    pub fn scrollback_lines(&self, n_lines: usize) -> Vec<TermScrollbackLine> {
        let parser = self.parser.lock();
        let screen = parser.screen().clone();
        let (rows, _cols) = screen.size();
        let mut probe = screen.clone();
        probe.set_scrollback(usize::MAX);
        let total = probe.scrollback();
        let start = total.saturating_sub(n_lines);
        let mut current = start;
        let mut lines = Vec::new();
        while current < total {
            let offset = total.saturating_sub(current);
            let mut window = screen.clone();
            window.set_scrollback(offset);
            let take_count = offset
                .min(usize::from(rows))
                .min(total.saturating_sub(current));
            let texts: Vec<String> = window
                .contents()
                .split('\n')
                .take(take_count)
                .map(ToOwned::to_owned)
                .collect();
            for (idx, text) in texts.into_iter().enumerate() {
                let row_index = u16::try_from(idx).unwrap_or(0);
                lines.push(TermScrollbackLine {
                    index: current + idx,
                    text,
                    wrapped: window.row_wrapped(row_index),
                });
            }
            current = current.saturating_add(take_count);
            if take_count == 0 {
                break;
            }
        }
        lines
    }
}

#[derive(Debug)]
pub struct ByteRing {
    max_bytes: usize,
    bytes: VecDeque<u8>,
    dropped_since_read: u64,
}

impl ByteRing {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            bytes: VecDeque::with_capacity(max_bytes.min(4096)),
            dropped_since_read: 0,
        }
    }

    fn push(&mut self, data: &[u8]) -> u64 {
        let mut dropped = 0_u64;
        for byte in data {
            if self.bytes.len() >= self.max_bytes {
                if self.bytes.pop_front().is_some() {
                    dropped = dropped.saturating_add(1);
                }
            }
            self.bytes.push_back(*byte);
        }
        self.dropped_since_read = self.dropped_since_read.saturating_add(dropped);
        dropped
    }

    fn take(&mut self, max_bytes: usize, eof: bool) -> crate::types::TermReadChunk {
        let take = max_bytes.min(self.bytes.len());
        let mut data = Vec::with_capacity(take);
        for _ in 0..take {
            if let Some(byte) = self.bytes.pop_front() {
                data.push(byte);
            }
        }
        let dropped_bytes = std::mem::take(&mut self.dropped_since_read);
        crate::types::TermReadChunk {
            data,
            eof,
            dropped_bytes,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct CallbackState {
    window_title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ParserCallbacks {
    shared: Arc<Mutex<CallbackState>>,
}

impl Callbacks for ParserCallbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.shared.lock().window_title = Some(String::from_utf8_lossy(title).into_owned());
    }

    fn set_window_icon_name(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.shared.lock().window_title = Some(String::from_utf8_lossy(title).into_owned());
    }
}

fn row_attr_runs(screen: &vt100::Screen, row: u16, cols: u16) -> Vec<TermAttrRun> {
    let mut interesting_end = 0_u16;
    let default_style = default_style();
    for col in 0..cols {
        if let Some(cell) = screen.cell(row, col) {
            let style = style_for_cell(cell);
            if cell.has_contents() || style != default_style || cell.is_wide_continuation() {
                interesting_end = col.saturating_add(1);
            }
        }
    }
    if interesting_end == 0 {
        return Vec::new();
    }

    let mut runs = Vec::new();
    let mut start = 0_u16;
    let mut current = default_style.clone();
    let mut seeded = false;
    for col in 0..interesting_end {
        let style = screen
            .cell(row, col)
            .map(style_for_cell)
            .unwrap_or_else(|| default_style.clone());
        if !seeded {
            current = style;
            start = col;
            seeded = true;
            continue;
        }
        if style != current {
            runs.push(TermAttrRun {
                start_col: start,
                end_col: col,
                style: current.clone(),
            });
            start = col;
            current = style;
        }
    }
    if seeded {
        runs.push(TermAttrRun {
            start_col: start,
            end_col: interesting_end,
            style: current,
        });
    }
    runs
}

fn default_style() -> TermStyle {
    TermStyle {
        fg: TermColor::Default,
        bg: TermColor::Default,
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        inverse: false,
    }
}

fn style_for_cell(cell: &vt100::Cell) -> TermStyle {
    TermStyle {
        fg: map_color(cell.fgcolor()),
        bg: map_color(cell.bgcolor()),
        bold: cell.bold(),
        dim: cell.dim(),
        italic: cell.italic(),
        underline: cell.underline(),
        inverse: cell.inverse(),
    }
}

fn map_color(color: Color) -> TermColor {
    match color {
        Color::Default => TermColor::Default,
        Color::Idx(idx) => TermColor::Indexed(idx),
        Color::Rgb(r, g, b) => TermColor::Rgb { r, g, b },
    }
}

pub fn map_mouse_mode(mode: MouseProtocolMode) -> TermMouseMode {
    match mode {
        MouseProtocolMode::None => TermMouseMode::None,
        MouseProtocolMode::Press => TermMouseMode::Press,
        MouseProtocolMode::PressRelease => TermMouseMode::PressRelease,
        MouseProtocolMode::ButtonMotion => TermMouseMode::ButtonMotion,
        MouseProtocolMode::AnyMotion => TermMouseMode::AnyMotion,
    }
}

pub fn map_mouse_encoding(encoding: MouseProtocolEncoding) -> TermMouseEncoding {
    match encoding {
        MouseProtocolEncoding::Default => TermMouseEncoding::Default,
        MouseProtocolEncoding::Utf8 => TermMouseEncoding::Utf8,
        MouseProtocolEncoding::Sgr => TermMouseEncoding::Sgr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_ring_tracks_drops() {
        let mut ring = ByteRing::new(4);
        assert_eq!(ring.push(b"abcdef"), 2);
        let chunk = ring.take(8, false);
        assert_eq!(chunk.data, b"cdef");
        assert_eq!(chunk.dropped_bytes, 2);
    }

    #[test]
    fn snapshot_tracks_alt_screen_and_title() {
        let state = SharedState::new(4, 10, 1024, 32);
        state.process_output(b"\x1b]2;demo\x07hello\n");
        state.process_output(b"\x1b[?1049hALT\x1b[?1049l");
        let snap = state.build_snapshot("term_1");
        assert_eq!(snap.window_title.as_deref(), Some("demo"));
        assert!(!snap.alt_screen_active);
        assert_eq!(snap.visible_rows[0].text, "hello");
    }

    #[test]
    fn chunk_boundaries_do_not_change_snapshot() {
        let combined = SharedState::new(4, 20, 1024, 32);
        combined.process_output(b"alpha\n\x1b[31mred\x1b[0m\n");

        let split = SharedState::new(4, 20, 1024, 32);
        split.process_output(b"alpha\n\x1b[31m");
        split.process_output(b"red");
        split.process_output(b"\x1b[0m\n");

        let left = combined.build_snapshot("term_a");
        let right = split.build_snapshot("term_b");
        assert_eq!(left.rows, right.rows);
        assert_eq!(left.cols, right.cols);
        assert_eq!(left.cursor.row, right.cursor.row);
        assert_eq!(left.cursor.col, right.cursor.col);
        assert_eq!(left.alt_screen_active, right.alt_screen_active);
        assert_eq!(left.visible_rows, right.visible_rows);
    }

    #[test]
    fn scrollback_pages_beyond_viewport() {
        let state = SharedState::new(3, 80, 1024, 64);
        for idx in 0..8 {
            let line = format!("line-{idx}\n");
            state.process_output(line.as_bytes());
        }
        let lines = state.scrollback_lines(5);
        assert!(!lines.is_empty());
        assert!(lines.iter().any(|line| line.text.contains("line-5")));
    }
}
