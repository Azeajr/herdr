//! Frame dump: what the client was sent, and what it wrote to the host.
//!
//! tmux capture answers "which glyphs are on screen" and was measured
//! character-identical to a `pyte` render, so it is enough for almost every
//! scenario. It cannot answer anything about the *cursor*: a capture records
//! cells, and cursor shape and visibility travel as escapes to the host terminal
//! that the capturing terminal consumes rather than reports. A scenario asking
//! "is the cursor a bar in insert mode" has had no instrument at all.
//!
//! This writes one JSON line per semantic frame: the cursor the server sent, and
//! the cursor escapes this client emitted for it. Both halves in one record is
//! the point — it separates "the server never sent a shape" from "the client was
//! sent one and did not emit it", which is otherwise two guesses.
//!
//! Two things this deliberately is not, matching the hitbox dump:
//!
//! - **Not a cargo feature.** `HERDR_CLIENT_FRAME_DUMP=<path>` gates it on the
//!   shipping binary the way `HERDR_LOG` does, so what gets tested is the binary
//!   users run.
//! - **Not an API method.** This is client-side presentation state and never goes
//!   onto the wire — see the runtime/client boundary guardrail in `CLAUDE.md`.
//!
//! Unlike the hitbox dump this really is client-side, and the hitbox lesson does
//! not transfer: hitboxes belong to whoever renders and hit-tests, which is the
//! server, whereas a frame is only a frame once a client has decoded one. The
//! host cursor escapes exist nowhere else at all.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use serde::Serialize;
use tracing::warn;

use crate::protocol::{CursorState, FrameData};

/// Path to write the dump to, from the environment. Absent = feature off.
pub(crate) const DUMP_PATH_ENV: &str = "HERDR_CLIENT_FRAME_DUMP";

#[derive(Serialize)]
struct CursorRecord {
    x: u16,
    y: u16,
    visible: bool,
    /// DECSCUSR parameter, 0 meaning the pane never set one.
    shape: u8,
}

impl From<&CursorState> for CursorRecord {
    fn from(cursor: &CursorState) -> Self {
        Self {
            x: cursor.x,
            y: cursor.y,
            visible: cursor.visible,
            shape: cursor.shape,
        }
    }
}

#[derive(Serialize)]
struct FrameRecord {
    /// Frames since this client started, not the server's sequence: the dump is
    /// per-client and a client can attach mid-session.
    seq: u64,
    width: u16,
    height: u16,
    /// Whether this client paints the cursor into cells instead of using the
    /// host's. When true the host cursor is deliberately suppressed, so an
    /// absent `emitted_show` below is correct rather than a defect.
    draw_host_cursor: bool,
    /// The cursor as the server sent it, before any drawn-cursor rewrite.
    cursor: Option<CursorRecord>,
    /// DECSCUSR parameters this frame's bytes wrote to the host terminal.
    ///
    /// Usually empty: the encoder only emits on a *change*, so a shape that
    /// holds steady across frames appears once and then not again.
    emitted_decscusr: Vec<u16>,
    /// Whether the frame's bytes ended by showing or hiding the host cursor.
    emitted_show: Option<bool>,
}

pub(crate) struct FrameDump {
    writer: BufWriter<File>,
    seq: u64,
}

impl FrameDump {
    /// Opens the dump named by the environment, or returns `None`.
    ///
    /// A path that cannot be opened disables the dump with a warning rather than
    /// failing the client: this is an instrument, and losing the session it was
    /// meant to observe would be a poor trade.
    pub(crate) fn from_env() -> Option<Self> {
        let path = PathBuf::from(std::env::var_os(DUMP_PATH_ENV)?);
        match File::create(&path) {
            Ok(file) => Some(Self {
                writer: BufWriter::new(file),
                seq: 0,
            }),
            Err(err) => {
                warn!(path = %path.display(), error = %err, "frame dump disabled");
                None
            }
        }
    }

    /// Appends one record. Write failures are dropped on purpose — see
    /// [`Self::from_env`].
    pub(crate) fn record(
        &mut self,
        frame: &FrameData,
        source_cursor: Option<&CursorState>,
        draw_host_cursor: bool,
        encoded: &[u8],
    ) {
        self.seq = self.seq.saturating_add(1);
        let record = FrameRecord {
            seq: self.seq,
            width: frame.width,
            height: frame.height,
            draw_host_cursor,
            cursor: source_cursor.map(CursorRecord::from),
            emitted_decscusr: scan_decscusr(encoded),
            emitted_show: scan_cursor_visibility(encoded),
        };
        let Ok(line) = serde_json::to_string(&record) else {
            return;
        };
        let _ = self.writer.write_all(line.as_bytes());
        let _ = self.writer.write_all(b"\n");
        // Flushed per frame: a scenario reads this file while the client is
        // still running, and a buffered tail would read as "no cursor yet".
        let _ = self.writer.flush();
    }
}

/// Every `CSI Ps SP q` in `bytes`, in order.
fn scan_decscusr(bytes: &[u8]) -> Vec<u16> {
    let mut found = Vec::new();
    let mut i = 0usize;
    while let Some(start) = find_csi(bytes, i) {
        let mut j = start;
        let mut param: Option<u16> = None;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            let digit = u16::from(bytes[j] - b'0');
            param = Some(param.unwrap_or(0).saturating_mul(10).saturating_add(digit));
            j += 1;
        }
        // DECSCUSR is the space intermediate followed by `q`; anything else is
        // one of the many other CSI sequences in a frame.
        if bytes.get(j) == Some(&b' ') && bytes.get(j + 1) == Some(&b'q') {
            found.push(param.unwrap_or(0));
        }
        i = start.max(i + 1);
    }
    found
}

/// Whether the bytes last showed (`true`) or hid (`false`) the host cursor.
fn scan_cursor_visibility(bytes: &[u8]) -> Option<bool> {
    let mut last = None;
    let mut i = 0usize;
    while let Some(start) = find_csi(bytes, i) {
        if bytes[start..].starts_with(b"?25h") {
            last = Some(true);
        } else if bytes[start..].starts_with(b"?25l") {
            last = Some(false);
        }
        i = start.max(i + 1);
    }
    last
}

/// Index just past the next `ESC [`, at or after `from`.
fn find_csi(bytes: &[u8], from: usize) -> Option<usize> {
    (from..bytes.len().saturating_sub(1))
        .find(|&i| bytes[i] == 0x1b && bytes[i + 1] == b'[')
        .map(|i| i + 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decscusr_is_found_among_the_other_csi_sequences_of_a_frame() {
        // A realistic frame prefix: SGR, a cursor move, the shape, then show.
        let bytes = b"\x1b[0m\x1b[12;5H\x1b[6 q\x1b[?25h";
        assert_eq!(scan_decscusr(bytes), vec![6]);
        assert_eq!(scan_cursor_visibility(bytes), Some(true));
    }

    /// `CSI 0 SP q` is the sequence that resets the shape, and telling it from
    /// "no DECSCUSR at all" is the entire reason this scanner exists.
    #[test]
    fn a_shape_reset_is_recorded_rather_than_read_as_absent() {
        assert_eq!(scan_decscusr(b"\x1b[0 q"), vec![0]);
        assert!(scan_decscusr(b"\x1b[0m\x1b[2J").is_empty());
    }

    #[test]
    fn the_last_visibility_change_in_a_frame_wins() {
        assert_eq!(scan_cursor_visibility(b"\x1b[?25l\x1b[?25h"), Some(true));
        assert_eq!(scan_cursor_visibility(b"\x1b[?25h\x1b[?25l"), Some(false));
        assert_eq!(scan_cursor_visibility(b"\x1b[0m"), None);
    }

    /// A CSI that runs off the end of the buffer must not read past it.
    #[test]
    fn a_truncated_sequence_is_not_a_panic() {
        assert!(scan_decscusr(b"\x1b[").is_empty());
        assert!(scan_decscusr(b"\x1b[6").is_empty());
        assert!(scan_decscusr(b"\x1b[6 ").is_empty());
        assert!(scan_cursor_visibility(b"\x1b[?2").is_none());
    }
}
