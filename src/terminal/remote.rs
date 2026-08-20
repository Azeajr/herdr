//! A terminal whose content is rendered by another herdr server.
//!
//! Herdr's client is a thin blitter: the server renders each pane into cells
//! and streams them. A remote pane reuses exactly that path. This runtime is a
//! protocol client of the peer, opened in `ControlTerminal` mode, sized by the
//! local layout, and it blits back the cells the peer returns.
//!
//! Consequences, accepted deliberately:
//!
//! - No VT parser runs locally for a remote pane. The peer already holds the
//!   screen, so scrollback, search, and text snapshots are unavailable here and
//!   have to become requests to the peer. `pane.read`, `pane.read_range`, and
//!   `pane.text_query` are those requests.
//! - Agent detection for a remote pane runs on the peer, where the screen is.
//!
//! What the frame carries beyond cells, and why each is on it rather than
//! polled beside it: the scroll position, so a screen row can be named as a
//! buffer row on the server that owns it, and the OSC 8 links, so one already
//! drawn can be clicked. Both describe *these* cells, and a copy taken from a
//! different moment's answer points at the wrong rows without looking wrong.
//!
//! Input modes travel separately, pushed when they change: whether the peer's
//! program wants bracketed paste, and whether it wants the mouse. A view has no
//! VT to ask, and both decisions are otherwise guesses — an unwrapped paste
//! runs its own newlines, and a click assumed to belong to the program means
//! this side never starts a selection.
//!
//! The last frame is retained after a disconnect so the pane renders stale
//! rather than blanking, and the view reconnects: a peer restart, an ssh blip,
//! or a broken control connection must not kill a pane permanently. The
//! reconnect itself is driven from the app event loop rather than from here,
//! because the socket to dial has to be re-resolved from peer state each time —
//! an ssh peer's bridge socket is a different path after the transport comes
//! back. This runtime only records what the loop needs to decide: whether the
//! connection is up, how many attempts have failed, and when the next is due.

use std::collections::VecDeque;
use std::io::{BufReader, BufWriter};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;
use tracing::{debug, info, warn};

use interprocess::TryClone as _;

use crate::ipc::LocalStream;
use crate::protocol::{
    AttachScrollDirection, AttachScrollSource, ClientKeybindings, ClientLaunchMode, ClientMessage,
    FrameData, RenderEncoding, ServerMessage, ShutdownCode, MAX_FRAME_SIZE, PROTOCOL_VERSION,
};
use crate::queue_budget::{QueueBudget, QueueLimits, QueueOverflow};

/// Backoff bounds between reconnect attempts, matching the peer control
/// channel's own curve so a peer and its views come back on the same schedule.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(500);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(10);

/// How many consecutive reconnects may be accepted and then closed without a
/// single frame before the view is declared dead.
///
/// A peer that closes the connection before rendering anything has rejected the
/// attach — another client holds it, a read is in progress, or it said nothing
/// at all — and retrying that forever would spin against a decision the peer has
/// already made. Transport failures never reach this counter, so a peer that is
/// merely unreachable is retried indefinitely.
///
/// A refusal that names the target as *gone* does not reach it either: that one
/// is authoritative on sight and retires the view immediately, without spending
/// two more round trips to be told the same thing. This counter is for the
/// rejections the peer did not explain.
const MAX_REJECTED_RECONNECTS: u32 = 3;

/// Bound on the opening handshake with a peer.
///
/// Connecting to an ssh peer's bridge socket succeeds instantly — the bridge is
/// local — and only then does an ssh child have to reach the other machine and
/// answer. Without a bound here, a peer whose link died silently would hold
/// whoever called [`RemoteTerminalRuntime::connect`] until TCP gave up, which is
/// minutes. Matches the peer control channel's own one-shot request timeout.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on the goodbye written while closing a view.
///
/// Short because it is spent on the event loop and buys only politeness: the
/// socket is shut down immediately afterwards either way.
const DETACH_WRITE_TIMEOUT: Duration = Duration::from_millis(250);

/// Bound on an ordinary write to the peer during the streaming phase.
///
/// Input, resize and scroll are written inline from the event loop, so an
/// unbounded write parks the whole UI: a peer that stops draining its socket
/// fills the buffer and every later keystroke blocks behind it. Long enough
/// that it is a liveness backstop rather than a latency target — a peer merely
/// slow to read must not be torn down, so this only fires once the connection
/// is wedged. Timing out is reported as a disconnect, which retires the socket
/// and reconnects on a fresh one, so a half-written frame desyncs nothing.
/// Matches [`HANDSHAKE_TIMEOUT`], the other bound on waiting for this peer.
const STREAM_WRITE_TIMEOUT: Duration = HANDSHAKE_TIMEOUT;

/// Bound on unwritten traffic for one remote view.
///
/// A paste arrives as a single `Input` message and can be large, so the byte
/// limit has to leave room for several of them; the item limit is what catches
/// a flood of keystrokes or mouse reports against a peer that has stopped
/// reading. Crossing either means this peer is not draining, and the view is
/// marked disconnected rather than allowed to grow.
const REMOTE_WRITER_QUEUE_LIMITS: QueueLimits = QueueLimits::new(4096, 16 * 1024 * 1024);

/// A message waiting to go to the peer.
enum RemoteWriteItem {
    /// Input, scroll, mouse and detach, which must arrive in order.
    Ordered(Vec<u8>),
    /// A resize, which may be skipped if a newer one was queued behind it.
    Resize { generation: u64, bytes: Vec<u8> },
}

impl RemoteWriteItem {
    fn bytes_len(&self) -> usize {
        match self {
            Self::Ordered(bytes) => bytes.len(),
            Self::Resize { bytes, .. } => bytes.len(),
        }
    }
}

/// What became of a message handed to the queue.
enum RemoteEnqueue {
    Queued,
    /// The connection is already ending; the message has nowhere to go.
    Closed,
    Overflow(QueueOverflow),
}

/// One remote view's outbound queue, drained by a thread of its own.
///
/// Input, resize, scroll and mouse messages used to be written to the peer
/// socket inline, from the one server loop. A peer that stopped reading parked
/// that loop for up to [`STREAM_WRITE_TIMEOUT`] — every local pane, client, API
/// call, timer and other peer stalled with it. Moving the write onto a
/// dedicated thread makes a wedged peer cost its own view and nothing else.
struct RemoteWriterQueue {
    state: Mutex<RemoteWriterState>,
    ready: Condvar,
    /// Signalled once the writer has flushed everything queued, so a closing
    /// view can give its detach a bounded chance to leave.
    drained: Condvar,
}

struct RemoteWriterState {
    items: VecDeque<RemoteWriteItem>,
    budget: QueueBudget,
    /// Generation of the newest resize queued so far.
    ///
    /// A drag produces a stream of distinct sizes. Only the last one describes
    /// the pane, so the writer skips any older resize still queued rather than
    /// making the peer redraw its way through sizes that are already wrong.
    /// Ordering is preserved by leaving the item in place and dropping it at
    /// the point it would have been written, rather than by reordering.
    latest_resize: u64,
    closed: bool,
}

impl RemoteWriterQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(RemoteWriterState {
                items: VecDeque::new(),
                budget: QueueBudget::new(REMOTE_WRITER_QUEUE_LIMITS),
                latest_resize: 0,
                closed: false,
            }),
            ready: Condvar::new(),
            drained: Condvar::new(),
        })
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, RemoteWriterState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn enqueue(&self, item: RemoteWriteItem) -> RemoteEnqueue {
        let mut state = self.lock_state();
        if state.closed {
            return RemoteEnqueue::Closed;
        }
        if let Err(overflow) = state.budget.admit(item.bytes_len()) {
            return RemoteEnqueue::Overflow(overflow);
        }
        state.items.push_back(item);
        state
            .budget
            .record("queue.remote_writer.items", "queue.remote_writer.bytes");
        self.ready.notify_one();
        RemoteEnqueue::Queued
    }

    /// Queues a resize, superseding any older one still waiting.
    fn enqueue_resize(&self, bytes: Vec<u8>) -> RemoteEnqueue {
        let generation = {
            let mut state = self.lock_state();
            state.latest_resize = state.latest_resize.saturating_add(1);
            state.latest_resize
        };
        self.enqueue(RemoteWriteItem::Resize { generation, bytes })
    }

    /// Blocks until the next message to write, or `None` once the queue is
    /// closed and drained.
    fn recv(&self) -> Option<Vec<u8>> {
        let mut state = self.lock_state();
        loop {
            while let Some(item) = state.items.pop_front() {
                state.budget.release(item.bytes_len());
                match item {
                    RemoteWriteItem::Ordered(bytes) => return Some(bytes),
                    RemoteWriteItem::Resize { generation, bytes } => {
                        if generation == state.latest_resize {
                            return Some(bytes);
                        }
                        // A newer size is already queued; this one describes a
                        // pane that no longer exists at that size.
                    }
                }
            }
            if state.closed {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Stops accepting messages and lets the writer finish what is queued.
    fn close(&self) {
        let mut state = self.lock_state();
        state.closed = true;
        self.ready.notify_all();
    }

    /// Drops anything still queued, for a connection already known to be gone.
    fn abandon(&self) {
        let mut state = self.lock_state();
        state.closed = true;
        state.items.clear();
        state.budget.clear();
        self.ready.notify_all();
        self.drained.notify_all();
    }

    /// Reports that everything queued has reached the socket.
    fn notify_if_drained(&self) {
        if self.lock_state().items.is_empty() {
            self.drained.notify_all();
        }
    }

    /// Waits, briefly, for the writer to flush what is queued.
    ///
    /// Only for closing a view: it buys the detach a chance to reach a peer
    /// that is still reading, and gives up on one that is not. Returning early
    /// is fine — the caller shuts the socket down next either way.
    fn wait_until_drained(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock_state();
        while !state.items.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (next, result) = self
                .drained
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
            if result.timed_out() {
                break;
            }
        }
    }
}

/// Drains one view's queue onto its socket.
fn run_remote_writer(
    queue: &Arc<RemoteWriterQueue>,
    mut writer: BufWriter<LocalStream>,
    shared: &Arc<RemoteShared>,
    target: &str,
) {
    while let Some(bytes) = queue.recv() {
        let started = crate::render_prof::timer();
        if let Err(err) = std::io::Write::write_all(&mut writer, &bytes) {
            warn!(target = %target, error = %err, "remote terminal write failed");
            shared.set_end_reason(err.to_string());
            shared.mark_disconnected();
            break;
        }
        if let Err(err) = std::io::Write::flush(&mut writer) {
            warn!(target = %target, error = %err, "remote terminal flush failed");
            shared.set_end_reason(err.to_string());
            shared.mark_disconnected();
            break;
        }
        crate::render_prof::histogram_since("remote.write", started);
        queue.notify_if_drained();
    }
    queue.abandon();
    debug!(target = %target, "remote terminal writer thread exiting");
}

/// State shared between the reader thread and the render path.
struct RemoteShared {
    /// Most recent frame from the peer, retained across disconnects.
    frame: Mutex<Option<FrameData>>,
    /// Set when the render path should redraw because a frame arrived.
    dirty: AtomicBool,
    /// Whether the peer connection is currently up.
    connected: AtomicBool,
    /// Whether this connection ever delivered a frame. A connection that ends
    /// without one was rejected rather than lost.
    saw_frame: AtomicBool,
    /// Why the peer closed the connection, when it said.
    end_reason: Mutex<Option<String>>,
    /// The refusal the peer itself sent, as distinct from why this connection
    /// ended.
    ///
    /// [`Self::end_reason`] answers "why did this stop", and a write that failed
    /// on this side writes it too. Only a message the peer sent is the peer's
    /// answer *about its own target*, and only that may retire a view on sight,
    /// so it is kept apart rather than recovered by parsing the reason back out.
    ///
    /// The code travels with the text because the text is the peer's wording,
    /// not its verdict.
    refusal: Mutex<Option<(ShutdownCode, String)>>,
    /// Whether the peer's terminal currently has bracketed paste on.
    ///
    /// A view has no VT of its own to ask, so the mode is carried from the peer
    /// rather than inferred. It decides whether pasted text is wrapped in
    /// `\x1b[200~`/`\x1b[201~` on the way out; guessing `false` would let an
    /// embedded newline submit a command line the user only meant to paste.
    bracketed_paste: AtomicBool,
    /// Whether the peer's program has asked to receive mouse events.
    ///
    /// Decides who owns a click on this pane. Assuming the program always
    /// wants it — which a view with no VT of its own has to, absent this — is
    /// what stopped a drag over a peer-backed pane ever starting a selection.
    mouse_reporting: AtomicBool,
    /// Wakes the server loop when this view needs drawing.
    ///
    /// Without it a frame only sets [`Self::dirty`] and waits to be noticed by
    /// the loop's next sweep, which parks for up to `CLIENT_ACCEPT_POLL_INTERVAL`
    /// when nothing local is happening. Local PTY output has never had to wait
    /// that way, and neither should a peer's.
    render_wake: Option<Arc<tokio::sync::Notify>>,
}

impl RemoteShared {
    fn new() -> Self {
        Self::with_wake(crate::render_signal::server_wake())
    }

    fn with_wake(render_wake: Option<Arc<tokio::sync::Notify>>) -> Self {
        Self {
            render_wake,
            frame: Mutex::new(None),
            // A connection nothing has drawn yet needs a draw. This matters
            // once `collect_dirty_patch` answers `Clean` for a remote pane with
            // no frame waiting: a reconnect installs a fresh `RemoteShared`
            // while `inherit_from` carries the previous frame over, and the
            // pane is no longer stale, so the dimming the last render drew
            // would otherwise stay on screen until the peer sent something.
            dirty: AtomicBool::new(true),
            connected: AtomicBool::new(true),
            saw_frame: AtomicBool::new(false),
            // Off until the peer says otherwise, which matches a terminal that
            // has not enabled the mode and is the safe default for the paste
            // path either way.
            bracketed_paste: AtomicBool::new(false),
            // Off until the peer says otherwise: a pane whose program has not
            // asked for the mouse is the ordinary case, and selecting is what
            // a user expects there.
            mouse_reporting: AtomicBool::new(false),
            end_reason: Mutex::new(None),
            refusal: Mutex::new(None),
        }
    }

    /// The peer's scroll position as of the retained frame.
    ///
    /// Read from the frame rather than kept alongside it so the position and
    /// the cells it describes cannot drift apart.
    fn scroll_metrics(&self) -> Option<crate::pane::ScrollMetrics> {
        let frame = self.frame.lock().ok()?;
        let scroll = frame.as_ref()?.scroll?;
        Some(crate::pane::ScrollMetrics {
            offset_from_bottom: scroll.offset_from_bottom as usize,
            max_offset_from_bottom: scroll.max_offset_from_bottom as usize,
            viewport_rows: scroll.viewport_rows as usize,
        })
    }

    fn set_input_modes(&self, bracketed_paste: bool, mouse_reporting: bool) {
        self.bracketed_paste
            .store(bracketed_paste, Ordering::Relaxed);
        self.mouse_reporting
            .store(mouse_reporting, Ordering::Relaxed);
    }

    fn set_bracketed_paste(&self, enabled: bool) {
        self.bracketed_paste.store(enabled, Ordering::Relaxed);
    }

    fn bracketed_paste(&self) -> bool {
        self.bracketed_paste.load(Ordering::Relaxed)
    }

    fn mouse_reporting(&self) -> bool {
        self.mouse_reporting.load(Ordering::Relaxed)
    }

    fn store_frame(&self, frame: FrameData) {
        let wait_started = crate::render_prof::timer();
        if let Ok(mut slot) = self.frame.lock() {
            crate::render_prof::histogram_since("remote.frame.store_lock_wait", wait_started);
            let hold_started = crate::render_prof::timer();
            *slot = Some(frame);
            drop(slot);
            crate::render_prof::histogram_since("remote.frame.store_lock_hold", hold_started);
        }
        self.saw_frame.store(true, Ordering::Relaxed);
        self.mark_dirty();
    }

    fn mark_disconnected(&self) {
        self.connected.store(false, Ordering::Relaxed);
        self.mark_dirty();
    }

    /// Marks this view as needing a draw, waking the loop only when that is
    /// news.
    ///
    /// Waking on the idle-to-dirty transition alone is what preserves
    /// coalescing: while a draw is already owed, further frames replace the
    /// retained one and cost nothing, exactly as they did when the loop found
    /// them by sweeping.
    fn mark_dirty(&self) {
        if !self.dirty.swap(true, Ordering::Relaxed) {
            if let Some(wake) = &self.render_wake {
                wake.notify_one();
            }
        }
    }

    fn set_end_reason(&self, reason: String) {
        if let Ok(mut slot) = self.end_reason.lock() {
            *slot = Some(reason);
        }
    }

    fn end_reason(&self) -> Option<String> {
        self.end_reason.lock().ok().and_then(|slot| slot.clone())
    }

    /// Records a reason the peer sent, which is also why the connection ended.
    fn set_refusal(&self, code: ShutdownCode, reason: String) {
        if let Ok(mut slot) = self.refusal.lock() {
            *slot = Some((code, reason.clone()));
        }
        self.set_end_reason(reason);
    }

    /// The peer's own wording, for the pane's status line.
    fn refusal(&self) -> Option<String> {
        self.refusal
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|(_, reason)| reason.clone()))
    }

    /// The peer's verdict, which is what may be acted on.
    fn refusal_code(&self) -> Option<ShutdownCode> {
        self.refusal
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|(code, _)| *code))
    }
}

/// What the peer last reported about the pane behind a view.
///
/// A remote pane has no local VT state, so the facts a sidebar shows — where the
/// shell is, what the title says, which agent is running — can only come from
/// the peer. They are cached rather than fetched because every one of them is
/// read per render frame, and a frame must never wait on another machine.
///
/// Absent until the peer has been asked once; a view opened before the first
/// enumeration reads exactly as it did before this existed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemotePaneMetadata {
    pub cwd: Option<std::path::PathBuf>,
    pub foreground_cwd: Option<std::path::PathBuf>,
    pub terminal_title: Option<String>,
    pub agent_osc_title: Option<String>,
    pub agent_osc_progress: Option<String>,
    /// Agent label as the peer's own detection reports it, if any.
    pub agent: Option<String>,
    pub agent_status: Option<crate::api::schema::AgentStatus>,
    /// Keyboard protocol the peer's program has enabled.
    ///
    /// Which encoding a keypress takes is VT state, so this is the only way the
    /// local side can get it right. Without it every key for a remote pane was
    /// encoded as legacy, and a program using the Kitty protocol saw Shift+Enter
    /// as a plain Enter — the same keypress working locally and not remotely,
    /// which reads as a broken pane rather than a limited one.
    pub keyboard_protocol: Option<crate::api::schema::KeyboardProtocolInfo>,
}

/// What the app event loop tracks about reconnecting one view.
///
/// Lives here rather than in a side table so it cannot outlive or fall out of
/// step with the connection it describes.
#[derive(Debug, Default)]
struct ReconnectState {
    /// Consecutive failed attempts, driving the backoff.
    attempt: u32,
    /// Consecutive attempts the peer accepted and then closed without a frame.
    rejected: u32,
    /// The last attempt never reached the peer at all.
    ///
    /// Set by [`RemoteTerminalRuntime::reconnect_failed`] and cleared by the
    /// next [`RemoteTerminalRuntime::begin_reconnect`], which reads it to keep
    /// a transport failure out of `rejected`. Without it the two are
    /// indistinguishable one attempt later: a connect that never landed leaves
    /// `saw_frame` false exactly like an attach the peer accepted and dropped,
    /// and an unreachable peer would retire the view in three tries.
    connect_failed: bool,
    /// When the next attempt may start. `None` means "as soon as noticed".
    next_due: Option<Instant>,
    /// An attempt is running on a worker thread.
    in_flight: bool,
    /// Why this view stopped retrying, once it has.
    dead: Option<String>,
    /// Whether [`Self::dead`] is the peer saying its own target no longer
    /// exists, which is the one death that also closes the local pane.
    dead_target_gone: bool,
    /// Liveness last announced to clients, so the sweep can tell a transition
    /// from a tick where nothing moved.
    ///
    /// Kept next to the connection it describes rather than in a side table for
    /// the same reason as the rest of this struct, and starts empty so a view's
    /// first sweep announces the state it opened in.
    reported: Option<crate::api::schema::PeerViewState>,
}

pub struct RemoteTerminalRuntime {
    shared: Arc<RemoteShared>,
    /// Outbound queue for the peer connection, drained by [`Self::writer`].
    ///
    /// Shared so a delayed send can outlive the borrow that scheduled it. A
    /// reconnect replaces the whole runtime, so a task still holding the old
    /// handle enqueues onto a closed queue and is dropped — the same thing that
    /// happens to a delayed local write when the pty behind it is gone.
    write_queue: Arc<RemoteWriterQueue>,
    running: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
    writer: Option<std::thread::JoinHandle<()>>,
    /// Handle on the socket kept apart from `writer`, so ending the connection
    /// never has to wait for whatever is currently writing to it.
    shutdown_stream: LocalStream,
    /// Size this runtime last told the peer to use, as
    /// `(cols, rows, cell_width_px, cell_height_px)`. The pixel metrics are part
    /// of the key because they are part of the message: a client can report the
    /// same grid with different cell dimensions, and dropping that update would
    /// leave the peer sizing graphics against stale metrics.
    size: Mutex<(u16, u16, u32, u32)>,
    /// Peer-side target (terminal id, pane id, or agent name) being controlled.
    ///
    /// Kept verbatim across reconnects. The peer resolves it again on every
    /// attach, so a pane id keeps naming the same pane after the peer restarts
    /// even though the terminal behind it is new.
    target: String,
    /// Peer backing this view.
    ///
    /// Recorded for every view, not only spawned ones: reconnecting has to
    /// re-resolve the peer's socket, and a view is reached from the runtime map
    /// where no workspace is in scope.
    peer: String,
    /// Instance id the answering server reported during the handshake.
    ///
    /// [`Self::target`] is a *peer-local* id, kept verbatim across reconnects
    /// because the peer resolves it again on every attach. That is only safe
    /// while the peer is the same server: a restart with a fresh session dir
    /// makes `w1:p1` name a completely unrelated pane. This is what lets a
    /// reconnect tell "the server came back" from "a different server answered",
    /// and it is why the id comes from the handshake rather than from peer state.
    ///
    /// `None` from a peer too old to report one, which is also a peer that
    /// cannot be re-targeted safely.
    peer_instance_id: Option<String>,
    /// Whether this view's pane was spawned on the peer for us.
    ///
    /// This is what makes closing a view able to close the pane behind it: we
    /// close only panes we created.
    spawned: bool,
    reconnect: ReconnectState,
    /// Last metadata the peer reported for [`Self::target`].
    metadata: Option<RemotePaneMetadata>,
}

/// Only the peer address is worth printing: the rest is connection machinery,
/// and a runtime rides in `AppEvent`, which is `Debug`.
impl std::fmt::Debug for RemoteTerminalRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteTerminalRuntime")
            .field("peer", &self.peer)
            .field("target", &self.target)
            .field("size", &self.current_size())
            .field("spawned", &self.spawned)
            .field("connected", &self.is_connected())
            .finish_non_exhaustive()
    }
}

impl RemoteTerminalRuntime {
    /// Opens a control connection to `socket_path` and takes control of
    /// `target` on the peer.
    ///
    /// `cols`/`rows` are the local layout's size for this pane. The peer treats
    /// the controlling connection as authoritative for the terminal's size, so
    /// this is what the remote program will see.
    pub fn connect(
        socket_path: &std::path::Path,
        peer: String,
        target: String,
        cols: u16,
        rows: u16,
        takeover: bool,
    ) -> std::io::Result<Self> {
        let stream = crate::ipc::connect_local_stream(socket_path)?;
        let read_stream = stream.try_clone()?;
        // A third handle on the same socket, held only so [`Self::stop`] can
        // shut the connection down without taking the writer lock — which a
        // write parked against a wedged peer is exactly what would be holding.
        let shutdown_stream = stream.try_clone()?;
        // Bounded for the handshake only. A timeout here is fatal — the stream
        // is dropped — so it cannot desynchronize the framing, and it is the
        // only thing standing between an unreachable peer and an unbounded wait.
        // Both handles are dups of one socket, so this covers the pair.
        crate::ipc::set_local_stream_timeouts(&stream, Some(HANDSHAKE_TIMEOUT))?;
        let mut writer = BufWriter::new(stream);

        crate::protocol::write_message(
            &mut writer,
            &ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                cols,
                rows,
                // Kitty graphics are not forwarded across a peer boundary yet.
                cell_width_px: 0,
                cell_height_px: 0,
                requested_encoding: RenderEncoding::SemanticFrame,
                keybindings: ClientKeybindings::Server,
                launch_mode: ClientLaunchMode::TerminalAttach,
                // Identifies this server to the peer, so a pane it spawned for
                // us reads as attended while this connection lasts, and so a
                // reconnecting view can reclaim its own abandoned attach.
                instance_id: crate::instance_id::active(),
            },
        )
        .map_err(std::io::Error::other)?;

        let mut reader = BufReader::new(read_stream);
        let peer_instance_id =
            match crate::protocol::read_message::<_, ServerMessage>(&mut reader, MAX_FRAME_SIZE)
                .map_err(std::io::Error::other)?
            {
                ServerMessage::Welcome {
                    error: Some(error), ..
                } => {
                    return Err(std::io::Error::other(format!(
                        "peer rejected terminal control: {error}"
                    )))
                }
                ServerMessage::Welcome { version, .. } if version != PROTOCOL_VERSION => {
                    return Err(std::io::Error::other(format!(
                        "peer protocol {version} does not match local protocol {PROTOCOL_VERSION}"
                    )))
                }
                // Taken from the answer rather than from local peer state, which can
                // be up to one heartbeat stale: this has to name the server that
                // actually accepted the attach.
                ServerMessage::Welcome { instance_id, .. } => instance_id,
                other => {
                    return Err(std::io::Error::other(format!(
                        "peer sent {other:?} instead of a handshake response"
                    )))
                }
            };

        crate::protocol::write_message(
            &mut writer,
            &ClientMessage::ControlTerminal {
                target: target.clone(),
                takeover,
            },
        )
        .map_err(std::io::Error::other)?;

        // Reads go unbounded for the streaming phase. The framing reads a length
        // prefix and then exactly that many bytes, so a timeout firing partway
        // through a frame would consume half a message and desynchronize every
        // read after it. [`Self::stop`] interrupts the reader by shutting the
        // socket down instead, which has no such hazard.
        //
        // Writes keep a bound even though they now run on this view's own
        // writer thread rather than inline. It is no longer the event loop at
        // risk, but a wedged peer would still hold that thread and its queued
        // traffic forever, and the timeout is what turns that into a disconnect
        // the view can reconnect from.
        crate::ipc::set_local_stream_timeouts(reader.get_ref(), None)?;
        crate::ipc::set_local_stream_send_timeout(reader.get_ref(), Some(STREAM_WRITE_TIMEOUT))?;

        let shared = Arc::new(RemoteShared::new());
        let running = Arc::new(AtomicBool::new(true));

        let reader_shared = Arc::clone(&shared);
        let reader_running = Arc::clone(&running);
        let reader_target = target.clone();
        let reader = std::thread::Builder::new()
            .name("herdr-remote-term".to_string())
            .spawn(move || {
                read_frames(reader, &reader_shared, &reader_running, &reader_target);
            })?;

        // Started only after the handshake, which is written inline above: the
        // queue carries what happens once the connection is established, and
        // the handshake has to complete before any of it can be sent.
        let write_queue = RemoteWriterQueue::new();
        let writer_queue = Arc::clone(&write_queue);
        let writer_shared = Arc::clone(&shared);
        let writer_target = target.clone();
        let writer_thread = std::thread::Builder::new()
            .name("herdr-remote-write".to_string())
            .spawn(move || {
                run_remote_writer(&writer_queue, writer, &writer_shared, &writer_target);
            })?;

        Ok(Self {
            shared,
            write_queue,
            running,
            reader: Some(reader),
            writer: Some(writer_thread),
            shutdown_stream,
            size: Mutex::new((cols, rows, 0, 0)),
            target,
            peer,
            peer_instance_id,
            spawned: false,
            reconnect: ReconnectState::default(),
            metadata: None,
        })
    }

    /// Records that the peer spawned this view's pane on our behalf.
    ///
    /// Only a split does this. A view onto a pane the peer already had stays
    /// adopted, so closing it never closes anything on the peer.
    pub fn mark_spawned_on_peer(&mut self) {
        self.spawned = true;
    }

    /// The peer that spawned this view's pane, or `None` when the pane was
    /// already there.
    pub fn spawned_on_peer(&self) -> Option<&str> {
        self.spawned.then_some(self.peer.as_str())
    }

    /// Drops the spawned-on-peer claim, reporting whether there was one.
    pub fn clear_spawned_on_peer(&mut self) -> bool {
        std::mem::replace(&mut self.spawned, false)
    }

    /// The peer-side id this runtime controls.
    ///
    /// This is the only place the peer address of a single pane is recorded: a
    /// workspace knows which peer backs it, but after a split its panes view
    /// different terminals on that peer.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Whether the target names a pane (`w1:p2`) or a terminal (`term_…`)
    /// rather than an agent name.
    ///
    /// The distinction decides what a peer's "not found" refusal means. For a
    /// pane or terminal id it is authoritative — those ids are never reused
    /// while the answering server lives, so the pane is gone. An agent name
    /// stops resolving when the agent exits even though its pane lives on, so
    /// "not found" there must not retire the view.
    pub(crate) fn target_is_a_pane_or_terminal(&self) -> bool {
        self.target.contains(':') || self.target.starts_with("term_")
    }

    /// The peer backing this view, spawned there or not.
    /// The instance the answering server reported, when it reported one.
    ///
    /// A peer-local pane id only means anything while the server that issued it
    /// is still the one answering, so anything that acts on such an id later has
    /// to compare this first.
    pub fn peer_instance_id(&self) -> Option<&str> {
        self.peer_instance_id.as_deref()
    }

    pub fn peer(&self) -> &str {
        &self.peer
    }

    /// Whether this view is attached to a server other than `instance_id`.
    ///
    /// A view that never learned its server's id answers `true`: it cannot be
    /// shown to be on the right one, and the cost of being wrong here is
    /// rendering somebody else's terminal or closing their pane.
    pub fn is_on_other_server(&self, instance_id: &str) -> bool {
        self.peer_instance_id.as_deref() != Some(instance_id)
    }

    /// Whether `pane` is the peer pane this view is onto.
    ///
    /// A target is whatever the view was opened with — a pane id, a terminal id,
    /// or an agent name — and the peer reports both ids for the same pane, so
    /// either one identifies it. An agent-name target matches nothing here and
    /// falls back to having no metadata, which is honest: the name may point at
    /// a different pane than when the view opened.
    pub fn views_peer_pane(&self, peer: &str, pane_id: &str, terminal_id: &str) -> bool {
        self.peer == peer && (self.target == pane_id || self.target == terminal_id)
    }

    /// Records what the peer last reported about this view's pane, answering
    /// whether anything changed.
    pub fn set_metadata(&mut self, metadata: RemotePaneMetadata) -> bool {
        if self.metadata.as_ref() == Some(&metadata) {
            return false;
        }
        self.metadata = Some(metadata);
        true
    }

    /// What the peer last reported, or `None` before it has been asked.
    pub fn metadata(&self) -> Option<&RemotePaneMetadata> {
        self.metadata.as_ref()
    }

    /// The keyboard protocol the peer reported, before it is defaulted.
    pub fn reported_keyboard_protocol(&self) -> Option<crate::input::KeyboardProtocol> {
        use crate::api::schema::KeyboardProtocolInfo;
        match self.metadata.as_ref()?.keyboard_protocol? {
            KeyboardProtocolInfo::Legacy => Some(crate::input::KeyboardProtocol::Legacy),
            KeyboardProtocolInfo::Kitty { flags } => {
                Some(crate::input::KeyboardProtocol::Kitty { flags })
            }
        }
    }

    /// How keys for this view should be encoded.
    ///
    /// Legacy until the peer has said otherwise: a peer too old to report it,
    /// or a view opened before the first pane enumeration, gets exactly the
    /// behaviour it had before this existed.
    pub fn keyboard_protocol(&self) -> crate::input::KeyboardProtocol {
        self.reported_keyboard_protocol()
            .unwrap_or(crate::input::KeyboardProtocol::Legacy)
    }

    /// Whether the control connection to the peer is currently up.
    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Relaxed)
    }

    /// Why this view stopped trying to reconnect, once it has.
    pub fn dead_reason(&self) -> Option<&str> {
        self.reconnect.dead.as_deref()
    }

    /// What clients are told about the connection behind this view.
    ///
    /// The single answer both the API and the change sweep read, so a pane's
    /// reported liveness and the event announcing it can never disagree.
    pub fn view_state(&self) -> crate::api::schema::PeerViewInfo {
        use crate::api::schema::{PeerViewInfo, PeerViewState};
        match self.reconnect.dead.as_deref() {
            Some(reason) => PeerViewInfo {
                state: PeerViewState::Disconnected,
                reason: Some(reason.to_string()),
            },
            None if self.is_connected() => PeerViewInfo {
                state: PeerViewState::Connected,
                reason: None,
            },
            // Reconnecting covers the gap before the first attempt as well as
            // the gaps between them: from a client's side both are the same
            // thing, a pane showing a frame nothing is feeding.
            None => PeerViewInfo {
                state: PeerViewState::Reconnecting,
                reason: self.shared.end_reason(),
            },
        }
    }

    /// Reports the liveness change since this was last called, if any.
    ///
    /// Only the state is compared. A reason that changes while the state does
    /// not is a different failure on the way to the same place, and announcing
    /// each one would turn a peer that is down into a stream of events.
    pub fn take_view_state_change(&mut self) -> Option<crate::api::schema::PeerViewInfo> {
        let current = self.view_state();
        if self.reconnect.reported == Some(current.state) {
            return None;
        }
        self.reconnect.reported = Some(current.state);
        Some(current)
    }

    /// Takes and clears the redraw flag set when a new frame arrived.
    pub fn take_dirty(&self) -> bool {
        self.shared.dirty.swap(false, Ordering::Relaxed)
    }

    /// Whether a frame is waiting, without consuming the flag.
    ///
    /// Separate from [`Self::take_dirty`] because the event loop owns the
    /// consuming read: it is what decides to render at all. This one answers
    /// the retained-render path's question — "did *this* pane change" — which
    /// has to be askable without stealing the answer from the loop.
    pub fn frame_pending(&self) -> bool {
        self.shared.dirty.load(Ordering::Relaxed)
    }

    /// The cursor the peer reported in its most recent frame, mapped into
    /// `area`.
    ///
    /// Exists because [`crate::terminal::TerminalRuntime::cursor_state`]
    /// answered `None` for every remote pane: it delegates to the local pty,
    /// which a peer-backed pane does not have. That reads as "this pane has no
    /// cursor" rather than "this side cannot see one", and the pane grid
    /// believed it — `focused_terminal_owns_host_cursor` is true for a remote
    /// pane, which gates off the `rendered_cursor()` fallback that would
    /// otherwise have recovered the position, so the client was told to hide
    /// the cursor entirely.
    ///
    /// Answering from the retained frame rather than by loosening that gate is
    /// what preserves the peer's **shape**: `rendered_cursor()` reports a bare
    /// position and hardcodes shape 0, so routing a remote pane through it would
    /// trade an invisible cursor for one stuck at the terminal default.
    pub fn cursor_state(
        &self,
        area: Rect,
        show_cursor: bool,
    ) -> Option<crate::pane::TerminalCursorState> {
        // The same two conditions [`Self::render`] draws a cursor under. A frame
        // from before a disconnect keeps being shown, but its cursor does not:
        // it marks a position nothing is updating any more.
        if !show_cursor || !self.is_connected() {
            return None;
        }
        let frame = self.shared.frame.lock().ok()?;
        let cursor = frame.as_ref()?.cursor.as_ref()?;
        if cursor.x >= area.width || cursor.y >= area.height {
            return None;
        }
        Some(crate::pane::TerminalCursorState {
            x: area.x + cursor.x,
            y: area.y + cursor.y,
            visible: cursor.visible,
            shape: cursor.shape,
        })
    }

    /// The size this view last told the peer to render at.
    ///
    /// The field behind it stores the wire order — `(cols, rows)`, which is how
    /// [`ClientMessage::Resize`] carries it — so this is the one place that
    /// order is read back, and it is named on the way out. It answered
    /// `(cols, rows)` to callers once, which is the defect
    /// [`crate::terminal::TerminalSize`] now makes unwritable.
    pub fn current_size(&self) -> crate::terminal::TerminalSize {
        let (cols, rows, ..) = self.size.lock().map(|size| *size).unwrap_or((0, 0, 0, 0));
        crate::terminal::TerminalSize::new(rows, cols)
    }

    /// Whether a reconnect attempt should start now.
    ///
    /// Deliberately not a self-starting check: the caller owns peer state and
    /// has to resolve the socket, which may have moved since the last attempt.
    pub fn reconnect_due(&self, now: Instant) -> bool {
        !self.is_connected()
            && !self.reconnect.in_flight
            && self.reconnect.dead.is_none()
            && self.reconnect.next_due.is_none_or(|due| now >= due)
    }

    /// Marks an attempt as starting, after judging how the last one ended.
    ///
    /// Returns `false` when the view has just been declared dead instead, which
    /// happens when the peer keeps accepting the connection and closing it
    /// without rendering: that is a rejection, and repeating it cannot change
    /// the answer.
    pub fn begin_reconnect(&mut self) -> bool {
        // A peer that names the target as gone has already given the whole
        // answer. Pane and terminal ids are never reused while the answering
        // server lives, so no further attempt can find it, and each one costs a
        // full connect — a measured 1.05s per cycle on a 200ms link, where
        // waiting for the third turned a dead view gray for 3.4s. Everything
        // else the peer can say still goes through the counter below.
        if let Some(refusal) = self.retiring_refusal() {
            self.mark_dead_target_gone(refusal);
            return false;
        }
        if self.shared.saw_frame.load(Ordering::Relaxed) {
            // The last connection was a real session, so whatever ended it is a
            // fresh failure rather than a continuing rejection.
            self.reconnect.rejected = 0;
        } else if std::mem::take(&mut self.reconnect.connect_failed) {
            // The last attempt never reached the peer, so there is no decision
            // of the peer's to spin against. Transport failures belong to the
            // backoff curve alone, which is what `MAX_REJECTED_RECONNECTS`
            // promises: a peer that is merely unreachable is retried forever.
        } else {
            self.reconnect.rejected = self.reconnect.rejected.saturating_add(1);
            if self.reconnect.rejected >= MAX_REJECTED_RECONNECTS {
                let reason = self
                    .shared
                    .end_reason()
                    .unwrap_or_else(|| "peer closed the connection without rendering".to_string());
                self.mark_dead(reason);
                return false;
            }
        }
        self.reconnect.in_flight = true;
        true
    }

    /// The peer's refusal, when it is the kind that ends a view outright.
    ///
    /// Both halves are required. The verdict has to be the peer's own — a local
    /// write failure lands in [`RemoteShared::end_reason`] and must never be
    /// read as the peer's — and the target has to be one whose absence is
    /// authoritative, which an agent name is not: a name can be reassigned to a
    /// different pane, so "gone" about a name says nothing about this view.
    fn retiring_refusal(&self) -> Option<String> {
        if !self.target_is_a_pane_or_terminal() {
            return None;
        }
        if self.shared.refusal_code() != Some(ShutdownCode::TargetGone) {
            return None;
        }
        self.shared.refusal()
    }

    /// Records a failed attempt and schedules the next one.
    pub fn reconnect_failed(&mut self, now: Instant, error: &str) {
        self.reconnect.in_flight = false;
        self.reconnect.attempt = self.reconnect.attempt.saturating_add(1);
        self.reconnect.next_due = Some(now + backoff(self.reconnect.attempt));
        self.reconnect.connect_failed = true;
        self.shared.set_end_reason(error.to_string());
        debug!(
            peer = %self.peer,
            target = %self.target,
            attempt = self.reconnect.attempt,
            error,
            "remote terminal reconnect failed"
        );
    }

    /// Stops retrying, recording why.
    ///
    /// A target the peer reports as gone is not a warning: the pane exited
    /// there, the sweep retires this view, and the interesting line is the
    /// retirement, not the refusal. Everything else — a replaced server, an
    /// unreachable peer — keeps the warning, because the view stays behind.
    pub fn mark_dead(&mut self, reason: impl Into<String>) {
        self.mark_dead_inner(reason.into(), false);
    }

    /// Stops retrying because the peer said its own target is gone.
    ///
    /// Separate from [`Self::mark_dead`] so the verdict is carried rather than
    /// recovered: the sweep has to know whether to close the local pane too,
    /// and it used to answer that by matching the tail of the stored reason
    /// string — a destructive action keyed on wording the peer is free to
    /// change.
    pub fn mark_dead_target_gone(&mut self, reason: impl Into<String>) {
        self.mark_dead_inner(reason.into(), true);
    }

    fn mark_dead_inner(&mut self, reason: String, target_gone: bool) {
        if target_gone {
            info!(peer = %self.peer, target = %self.target, reason = %reason, "peer says the pane is gone; retiring the view");
        } else {
            warn!(peer = %self.peer, target = %self.target, reason = %reason, "remote terminal view is not coming back");
        }
        self.reconnect.in_flight = false;
        self.reconnect.next_due = None;
        self.reconnect.dead = Some(reason);
        self.reconnect.dead_target_gone = target_gone;
    }

    /// Whether this view stopped because the peer said its target no longer
    /// exists, as opposed to any other reason a view can die.
    pub fn died_because_target_is_gone(&self) -> bool {
        self.reconnect.dead.is_some() && self.reconnect.dead_target_gone
    }

    /// Clears the backoff so the next sweep retries immediately.
    ///
    /// Used when the peer's own control channel comes back: waiting out a
    /// backoff window that was earned while the peer was down would leave the
    /// pane frozen for no reason.
    pub fn retry_now(&mut self) {
        if self.reconnect.dead.is_none() {
            self.reconnect.next_due = None;
        }
    }

    /// Takes over the previous view's identity after a successful reconnect.
    ///
    /// The pane keeps rendering the peer's last frame until the reconnected
    /// connection produces its own, so a reconnect never blanks the pane, and
    /// the claim on a pane spawned for us survives — the pane on the peer is
    /// the same one, so closing this view must still close it.
    ///
    /// The rejection count carries over too: connecting is not proof of
    /// anything when the peer closes the connection right afterwards, so a
    /// rejection loop has to be counted across the runtimes it produces. The
    /// failed-attempt count does not — the connect itself worked.
    pub fn inherit_from(&mut self, previous: &Self) {
        self.spawned = previous.spawned;
        self.reconnect.rejected = previous.reconnect.rejected;
        // What clients were last told carries over, or the reconnect that just
        // succeeded would look like a view opening rather than one coming back.
        self.reconnect.reported = previous.reconnect.reported;
        // The pane on the peer is the same one, so what it was last known to be
        // is still the best answer until the peer reports again. Dropping it
        // would blank a reconnected pane's label for a poll interval.
        self.metadata.clone_from(&previous.metadata);
        // Same reasoning as the metadata above: it is the same terminal on the
        // peer, so its last known paste mode beats assuming "off" for the gap
        // before the reconnected peer reports it again. Assuming "off" there
        // would unwrap a paste that arrives in that window.
        self.shared
            .set_bracketed_paste(previous.shared.bracketed_paste());
        if let (Ok(mut frame), Ok(previous_frame)) =
            (self.shared.frame.lock(), previous.shared.frame.lock())
        {
            if frame.is_none() {
                frame.clone_from(&previous_frame);
            }
        }
    }

    /// Whether the peer's terminal currently has bracketed paste on.
    pub fn bracketed_paste(&self) -> bool {
        self.shared.bracketed_paste()
    }

    /// Whether the peer's program has asked to receive mouse events.
    pub fn mouse_reporting(&self) -> bool {
        self.shared.mouse_reporting()
    }

    /// Where the peer's viewport sits in its scrollback, as of the last frame.
    pub fn scroll_metrics(&self) -> Option<crate::pane::ScrollMetrics> {
        self.shared.scroll_metrics()
    }

    /// The OSC 8 links on the peer's screen, in this pane's screen coordinates.
    ///
    /// Answered from the retained frame rather than by asking the peer: the
    /// URIs and the cells that carry them are already in it, so this is a
    /// lookup rather than a round trip, and it cannot disagree with what is
    /// drawn because it is read from what was drawn.
    pub fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        let Ok(frame) = self.shared.frame.lock() else {
            return Vec::new();
        };
        let Some(frame) = frame.as_ref() else {
            return Vec::new();
        };
        if frame.width == 0 {
            return Vec::new();
        }
        let width = usize::from(frame.width);
        let mut links = Vec::new();
        for (index, cell) in frame.cells.iter().enumerate() {
            let Some(uri_index) = cell.hyperlink else {
                continue;
            };
            let Some(uri) = frame.hyperlinks.get(uri_index as usize) else {
                continue;
            };
            let (Ok(row), Ok(col)) = (u16::try_from(index / width), u16::try_from(index % width))
            else {
                continue;
            };
            if row >= area.height || col >= area.width {
                continue;
            }
            links.push((
                (area.x.saturating_add(col), area.y.saturating_add(row)),
                cell.symbol.clone(),
                uri.clone(),
            ));
        }
        links
    }

    /// Tells the peer what size to render this terminal at.
    ///
    /// The peer's `direct_attach_resize_locks` makes the controlling connection
    /// authoritative, so the peer's own layout will not fight this.
    pub fn resize(&self, rows: u16, cols: u16, cell_width_px: u32, cell_height_px: u32) {
        if let Ok(mut size) = self.size.lock() {
            if *size == (cols, rows, cell_width_px, cell_height_px) {
                return;
            }
            *size = (cols, rows, cell_width_px, cell_height_px);
        }
        let message = ClientMessage::Resize {
            cols,
            rows,
            cell_width_px,
            cell_height_px,
        };
        let mut bytes = Vec::new();
        if let Err(err) = crate::protocol::write_message(&mut bytes, &message) {
            warn!(target = %self.target, error = %err, "remote terminal encode failed");
            self.shared.set_end_reason(err.to_string());
            self.shared.mark_disconnected();
            return;
        }
        // Queued as a resize rather than as ordered traffic so a drag does not
        // make the peer redraw its way through every intermediate size.
        match self.write_queue.enqueue_resize(bytes) {
            RemoteEnqueue::Queued | RemoteEnqueue::Closed => {}
            RemoteEnqueue::Overflow(overflow) => {
                warn!(
                    target = %self.target,
                    peer = %self.peer,
                    overflow = %overflow,
                    "peer stopped reading; abandoning its view rather than queueing more"
                );
                self.shared.set_end_reason(overflow.to_string());
                self.shared.mark_disconnected();
                self.write_queue.abandon();
            }
        }
    }

    /// Scrolls the peer's copy of this terminal.
    ///
    /// There is no local scrollback to move, but the peer's attach-scroll path
    /// is exactly the one its own client uses, so this is not an approximation:
    /// the peer consults the running program's wheel routing and either scrolls
    /// its host scrollback or synthesizes the mouse report the program asked
    /// for. Encoding that locally is impossible — the routing depends on VT
    /// state only the peer has.
    ///
    /// Fire-and-forget on the control connection that is already open, like
    /// input and resize. The peer answers with a frame, which is what makes the
    /// scroll visible here.
    pub fn scroll(&self, direction: AttachScrollDirection, lines: usize) {
        self.send(&ClientMessage::AttachScroll {
            source: AttachScrollSource::Wheel,
            direction,
            lines: u16::try_from(lines).unwrap_or(u16::MAX),
            // A wheel event that never reached a local VT has no cell to report.
            // The peer only reads these for mouse-report routing, where it
            // falls back to the terminal's own origin.
            column: None,
            row: None,
            modifiers: 0,
        });
    }

    pub fn send_bytes(&self, bytes: Bytes) {
        self.send(&ClientMessage::Input {
            data: bytes.to_vec(),
        });
    }

    /// Reports a press, release, or drag to the peer.
    ///
    /// Unencoded, for the same reason [`Self::scroll`] is: whether the running
    /// program wants mouse reports, and in which protocol, is VT state only the
    /// peer has. It answers with a frame, which is what makes the click visible
    /// here.
    ///
    /// Returns whether the event was one the peer can act on; motion without a
    /// button is not carried, so a hover is refused here rather than sent and
    /// ignored on the other side.
    pub fn send_mouse(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> bool {
        if !matches!(
            kind,
            crossterm::event::MouseEventKind::Down(_)
                | crossterm::event::MouseEventKind::Up(_)
                | crossterm::event::MouseEventKind::Drag(_)
        ) {
            return false;
        }
        let Some(kind) = crate::protocol::ClientMouseKind::from_crossterm(kind) else {
            return false;
        };
        self.send(&ClientMessage::AttachMouse {
            kind,
            column,
            row,
            modifiers: modifiers.bits(),
        });
        true
    }

    pub fn send_paste(&self, text: String) {
        // Sent as a structured paste, not as raw input bytes: only the peer
        // knows whether the program on its side asked for bracketed paste, and
        // it re-applies that from its own terminal state on the way to the pty.
        //
        // Raw `Input` here would arrive as ordinary typing, and a pasted
        // newline would submit a command line the user only meant to paste.
        self.send(&ClientMessage::InputEvents {
            events: vec![crate::protocol::ClientInputEvent::Paste { text }],
        });
    }

    /// Sends a terminal-scoped focus transition to the owning peer.
    ///
    /// The peer still decides whether the target terminal enabled focus
    /// reporting. Keeping that decision beside the peer-owned VT state avoids
    /// injecting focus escapes into programs that never requested them.
    pub fn send_focus_event(&self, event: crate::ghostty::FocusEvent) {
        let event = match event {
            crate::ghostty::FocusEvent::Gained => crate::protocol::ClientInputEvent::FocusGained,
            crate::ghostty::FocusEvent::Lost => crate::protocol::ClientInputEvent::FocusLost,
        };
        self.send(&ClientMessage::InputEvents {
            events: vec![event],
        });
    }

    /// Sends input to the peer after `delay`.
    ///
    /// Dropping this on the floor is what makes `agent.prompt` type its text
    /// into a peer-backed agent and never submit it: the trailing Enter is
    /// deliberately delayed so the agent's own input handling sees the text
    /// first.
    pub fn send_bytes_after(&self, bytes: Bytes, delay: std::time::Duration) {
        let queue = Arc::clone(&self.write_queue);
        let shared = Arc::clone(&self.shared);
        let target = self.target.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let message = ClientMessage::Input {
                data: bytes.to_vec(),
            };
            let mut encoded = Vec::new();
            if let Err(err) = crate::protocol::write_message(&mut encoded, &message) {
                warn!(target = %target, error = %err, "delayed remote terminal encode failed");
                shared.set_end_reason(err.to_string());
                shared.mark_disconnected();
                return;
            }
            // Enqueuing rather than writing keeps this off the Tokio worker
            // too: a peer that stopped reading would otherwise park a runtime
            // thread here for the whole write timeout.
            if let RemoteEnqueue::Overflow(overflow) =
                queue.enqueue(RemoteWriteItem::Ordered(encoded))
            {
                warn!(target = %target, overflow = %overflow, "delayed remote terminal send dropped");
                shared.set_end_reason(overflow.to_string());
                shared.mark_disconnected();
                queue.abandon();
            }
        });
    }

    /// Serializes `message` and hands it to the writer thread.
    ///
    /// Framing happens here rather than on the writer so a message that cannot
    /// be encoded is reported to the caller that produced it, but the socket
    /// write itself never runs on this thread. This is called from the server
    /// loop, which is the whole reason it must not block.
    fn send(&self, message: &ClientMessage) {
        let mut bytes = Vec::new();
        if let Err(err) = crate::protocol::write_message(&mut bytes, message) {
            warn!(target = %self.target, error = %err, "remote terminal encode failed");
            self.shared.set_end_reason(err.to_string());
            self.shared.mark_disconnected();
            return;
        }
        self.deliver(RemoteWriteItem::Ordered(bytes));
    }

    fn deliver(&self, item: RemoteWriteItem) {
        match self.write_queue.enqueue(item) {
            RemoteEnqueue::Queued | RemoteEnqueue::Closed => {}
            RemoteEnqueue::Overflow(overflow) => {
                warn!(
                    target = %self.target,
                    peer = %self.peer,
                    overflow = %overflow,
                    "peer stopped reading; abandoning its view rather than queueing more"
                );
                self.shared.set_end_reason(overflow.to_string());
                self.shared.mark_disconnected();
                self.write_queue.abandon();
            }
        }
    }

    /// Blits the peer's most recent frame into `area`.
    ///
    /// A frame from before a disconnect is drawn dimmed rather than dropped, so
    /// the pane keeps showing what the peer last had, with a line saying whether
    /// it is coming back.
    pub fn render(&self, frame: &mut Frame, area: Rect, show_cursor: bool) {
        let stale = !self.is_connected();
        let wait_started = crate::render_prof::timer();
        if let Ok(data) = self.shared.frame.lock() {
            crate::render_prof::histogram_since("remote.frame.render_lock_wait", wait_started);
            let hold_started = crate::render_prof::timer();
            // Straight from the frame's cells into the render buffer. Building a
            // `Buffer` first would allocate one the size of the whole frame and
            // then copy every cell into it, on every render rather than on every
            // frame — and the retained-render path is disabled for remote panes,
            // so "every render" means every render the whole UI does.
            if let Some(source) = data.as_ref().filter(|data| data.is_well_formed()) {
                let buffer = frame.buffer_mut();
                let rows = area.height.min(source.height);
                let cols = area.width.min(source.width);
                for row in 0..rows {
                    for col in 0..cols {
                        let Some(target) = buffer.cell_mut((area.x + col, area.y + row)) else {
                            continue;
                        };
                        source.write_cell_into(col, row, target);
                        if stale {
                            target.set_fg(Color::DarkGray);
                            target.set_bg(Color::Reset);
                            target.modifier.remove(Modifier::BOLD);
                            target.modifier.insert(Modifier::DIM);
                        }
                    }
                }
            }

            if show_cursor && !stale {
                if let Some(cursor) = data.as_ref().and_then(|data| data.cursor.as_ref()) {
                    if cursor.visible && cursor.x < area.width && cursor.y < area.height {
                        frame.set_cursor_position((area.x + cursor.x, area.y + cursor.y));
                    }
                }
            }
            drop(data);
            crate::render_prof::histogram_since("remote.frame.render_lock_hold", hold_started);
        }

        if stale {
            self.render_status_line(frame, area);
        }
    }

    /// Draws one dim line over the stale frame saying why it is stale.
    ///
    /// Without it a view that is coming back and a view that never will look
    /// identical, which is the same reason a peer reports `reconnecting`
    /// separately from `error`.
    fn render_status_line(&self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let text = match self.dead_reason() {
            Some(reason) => format!(" disconnected from {}: {reason} ", self.peer),
            None if self.reconnect.attempt == 0 => {
                format!(" reconnecting to {}… ", self.peer)
            }
            None => format!(
                " reconnecting to {}… (attempt {}) ",
                self.peer, self.reconnect.attempt
            ),
        };
        let line = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        };
        Paragraph::new(Span::styled(
            crate::ui::text::truncate_end(&text, usize::from(area.width)),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        ))
        .render(line, frame.buffer_mut());
    }

    pub fn shutdown(mut self) {
        self.stop();
    }

    /// Ends the connection and releases the reader thread.
    ///
    /// Runs on the event loop — every close path reaches it — so nothing here
    /// may wait on the peer. The reader is parked in an unbounded framed read
    /// that only ends when the socket does, so it is woken by shutting the
    /// socket down rather than by waiting for it to notice `running`.
    fn stop(&mut self) {
        if !self.running.swap(false, Ordering::Relaxed) {
            return;
        }
        // Detach politely so the peer releases the control lock instead of
        // waiting for the socket to break. A connection that already broke has
        // nothing to say goodbye to, and writing to it would report a failure
        // that is neither new nor actionable — every reconnect ends by dropping
        // one of these.
        //
        // Best-effort, and free to this thread now that the write happens
        // elsewhere: the detach is queued behind whatever the writer already
        // has and goes out only if the peer is still draining. A peer that is
        // not gets its socket shut down below, which ends the writer's parked
        // write; a partial Detach cannot desync anything, because the shutdown
        // ends the connection either way.
        if self.is_connected() {
            let mut bytes = Vec::new();
            if crate::protocol::write_message(&mut bytes, &ClientMessage::Detach).is_ok() {
                let _ = self.write_queue.enqueue(RemoteWriteItem::Ordered(bytes));
            }
        }
        // Closing rather than abandoning gives the detach its chance to leave,
        // bounded by the same budget the inline detach write used to have. A
        // peer still reading takes it immediately; one that is not costs this
        // much and no more before the shutdown below ends it.
        self.write_queue.close();
        self.write_queue.wait_until_drained(DETACH_WRITE_TIMEOUT);

        // Never takes the writer lock, so this runs even while a write is
        // parked against the peer — and is what unparks it.
        let interrupted = crate::ipc::shutdown_local_stream(&self.shutdown_stream).unwrap_or(false);

        match self.reader.take() {
            // The reader is guaranteed to be leaving its read, so joining it
            // costs a scheduling hop and keeps the thread from outliving the
            // runtime that named it.
            Some(reader) if interrupted => {
                let _ = reader.join();
            }
            // Nothing can wake it — a platform without socket shutdown, or a
            // socket that refused it. Abandoning the thread strands a stack and
            // a descriptor until the connection breaks on its own, which is the
            // strictly better trade against freezing the server.
            Some(reader) => {
                warn!(
                    peer = %self.peer,
                    target = %self.target,
                    "remote terminal reader could not be interrupted; abandoning it"
                );
                drop(reader);
            }
            None => {}
        }

        match self.writer.take() {
            // Same bargain as the reader: the shutdown ends any parked write,
            // so joining costs a scheduling hop.
            Some(writer) if interrupted => {
                let _ = writer.join();
            }
            // Nothing can wake it, and waiting for a write parked against a
            // peer that stopped reading is exactly the freeze this queue
            // exists to prevent. Abandon the thread instead.
            Some(writer) => {
                self.write_queue.abandon();
                drop(writer);
            }
            None => {}
        }
    }
}

impl Drop for RemoteTerminalRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Delay before attempt `attempt`, capped so a long outage still retries.
fn backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    RECONNECT_BACKOFF_MIN
        .saturating_mul(1u32 << shift)
        .min(RECONNECT_BACKOFF_MAX)
}

/// Hands a peer's clipboard write to this server's loop.
///
/// Decoded here rather than passed along as base64 so a malformed payload is
/// dropped at the boundary it arrived on, instead of travelling further as an
/// event that cannot be applied.
fn forward_peer_clipboard(data: &str, target: &str) {
    use base64::Engine as _;

    let Ok(content) = base64::engine::general_purpose::STANDARD.decode(data) else {
        debug!(target = %target, "discarded peer clipboard write that was not valid base64");
        return;
    };
    let Some(events) = crate::events::server_events() else {
        return;
    };
    if events
        .try_send(crate::events::AppEvent::ClipboardWrite {
            pane_id: None,
            content,
        })
        .is_err()
    {
        warn!(target = %target, "failed to queue peer clipboard write");
    }
}

fn read_frames(
    mut reader: BufReader<LocalStream>,
    shared: &Arc<RemoteShared>,
    running: &Arc<AtomicBool>,
    target: &str,
) {
    while running.load(Ordering::Relaxed) {
        match crate::protocol::read_message::<_, ServerMessage>(&mut reader, MAX_FRAME_SIZE) {
            Ok(ServerMessage::Frame(data)) => shared.store_frame(data),
            Ok(ServerMessage::TerminalInputModes {
                bracketed_paste,
                mouse_reporting,
            }) => {
                shared.set_input_modes(bracketed_paste, mouse_reporting);
            }
            // The peer's pane wrote the system clipboard, and the peer cannot
            // reach the machine the user is sitting at. Re-emitted here as an
            // ordinary local clipboard write so it takes the same path as one
            // from a local pane, including onward to a server federating this
            // one. Carries no pane id: the pane it names is the peer's, and
            // this server routes the write by its own foreground client.
            Ok(ServerMessage::Clipboard { data }) => {
                forward_peer_clipboard(&data, target);
            }
            Ok(ServerMessage::ServerShutdown { reason, code }) => {
                debug!(target = %target, reason = ?reason, ?code, "peer terminal shut down");
                // Kept for the pane's status line: this is the peer's own words
                // for why the view stopped, including an attach it refused. It
                // is recorded as a refusal rather than a plain end reason
                // because it is the peer speaking about its own target, which
                // is the only thing allowed to retire a view.
                shared.set_refusal(
                    code,
                    reason.unwrap_or_else(|| "peer closed the connection".to_string()),
                );
                break;
            }
            // Everything else is host-local presentation the peer's own client
            // would handle: notifications, clipboard, window title, graphics.
            // A remote pane is not the foreground client, so it ignores them.
            Ok(_) => {}
            Err(err) => {
                if running.load(Ordering::Relaxed) {
                    debug!(target = %target, error = %err, "remote terminal stream ended");
                    shared.set_end_reason(err.to_string());
                }
                break;
            }
        }
    }
    shared.mark_disconnected();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_saturates() {
        assert_eq!(backoff(1), RECONNECT_BACKOFF_MIN);
        assert_eq!(backoff(2), RECONNECT_BACKOFF_MIN * 2);
        assert_eq!(backoff(50), RECONNECT_BACKOFF_MAX);
        assert!(backoff(u32::MAX) <= RECONNECT_BACKOFF_MAX);
    }

    /// The classification the whole retire-on-refusal path hangs on, now that
    /// it is the peer's code rather than the tail of the peer's prose.
    ///
    /// `headless.rs` pins which refusal carries which code from the producing
    /// side; this pins what each code does here. The wording is deliberately
    /// wrong-looking in both directions — a `TargetGone` that does not end in
    /// "not found", and a `TargetUnavailable` that does — because the point of
    /// the code is that the wording no longer decides.
    #[test]
    fn only_a_target_gone_code_retires_a_view() {
        let (_peer, mut runtime) = connected_view("code-gone", "w1:p2", false);
        runtime
            .shared
            .set_refusal(ShutdownCode::TargetGone, "the pane exited".to_string());
        assert!(
            !runtime.begin_reconnect(),
            "gone retires on the first sight"
        );
        assert!(runtime.died_because_target_is_gone());

        for code in [
            ShutdownCode::TargetUnavailable,
            ShutdownCode::Detached,
            ShutdownCode::ServerStopping,
            ShutdownCode::Unspecified,
        ] {
            let (_peer, mut runtime) = connected_view("code-transient", "w1:p2", false);
            runtime
                .shared
                .set_refusal(code, "terminal term_18d2 not found".to_string());
            assert!(
                runtime.begin_reconnect(),
                "{code:?} is not authoritative however it is worded"
            );
            assert!(!runtime.died_because_target_is_gone(), "{code:?}");
        }
    }

    /// A gone verdict about an *agent name* still does not retire: a name can
    /// be reassigned to a different pane, so it says nothing about this view.
    #[test]
    fn a_target_gone_code_about_a_name_does_not_retire() {
        let (_peer, mut runtime) = connected_view("code-name", "claude", false);
        runtime.shared.set_refusal(
            ShutdownCode::TargetGone,
            "terminal target claude not found".to_string(),
        );
        assert!(runtime.begin_reconnect());
        assert!(!runtime.died_because_target_is_gone());
    }

    /// Instance id the mute peer reports, so a view records one.
    const MUTE_PEER_INSTANCE: &str = "00000000000000000000000000000001";

    /// A peer that answers the handshake and then goes silent forever, without
    /// closing the connection. This is what a dead ssh link looks like from
    /// here: the local bridge socket is still open, so nothing reports an error.
    struct MutePeer {
        path: std::path::PathBuf,
        accepted: std::sync::mpsc::Receiver<crate::ipc::LocalStream>,
        _listener: std::thread::JoinHandle<()>,
    }

    impl MutePeer {
        fn start(name: &str, send_frame: bool) -> Self {
            let path = std::env::temp_dir().join(format!(
                "herdr-mute-peer-{name}-{}-{:?}.sock",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_file(&path);
            let listener = crate::ipc::bind_local_listener(&path).expect("bind mute peer");
            let (tx, accepted) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                use interprocess::local_socket::traits::Listener as _;
                let Ok(mut stream) = listener.accept() else {
                    return;
                };
                let hello = crate::protocol::read_message::<_, ClientMessage>(
                    &mut BufReader::new(&mut stream),
                    MAX_FRAME_SIZE,
                );
                if hello.is_err() {
                    return;
                }
                let mut writer = BufWriter::new(&mut stream);
                let _ = crate::protocol::write_message(
                    &mut writer,
                    &ServerMessage::Welcome {
                        version: PROTOCOL_VERSION,
                        encoding: RenderEncoding::SemanticFrame,
                        error: None,
                        instance_id: Some(MUTE_PEER_INSTANCE.to_string()),
                    },
                );
                if send_frame {
                    let _ = crate::protocol::write_message(
                        &mut writer,
                        &ServerMessage::Frame(FrameData {
                            cells: Vec::new(),
                            width: 0,
                            height: 0,
                            cursor: None,
                            hyperlinks: Vec::new(),
                            graphics: Vec::new(),
                            scroll: None,
                        }),
                    );
                }
                let _ = std::io::Write::flush(&mut writer);
                drop(writer);
                // Hold the connection open, saying nothing, until the test
                // hands the stream back and drops it.
                let _ = tx.send(stream);
            });
            Self {
                path,
                accepted,
                _listener: thread,
            }
        }
    }

    impl Drop for MutePeer {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Pasted text must cross as a paste, not as typing.
    ///
    /// Only the peer knows whether the program on its side asked for bracketed
    /// paste, so it re-applies that itself — but only if what arrives is still
    /// recognisable as a paste. Sent as raw `Input` it arrives as keystrokes,
    /// and an embedded newline submits a command line the user meant to paste.
    #[test]
    fn send_paste_crosses_as_a_paste_rather_than_as_typed_input() {
        let (peer, runtime) = connected_view("paste-shape", "w1:p2", false);
        let mut held = peer
            .accepted
            .recv_timeout(Duration::from_secs(5))
            .expect("peer stream");

        runtime.send_paste("echo one\necho two".to_string());

        let mut reader = BufReader::new(&mut held);
        // The view takes control and reports its size before anything the test
        // asked for, so read on until whatever carried the paste.
        let message = loop {
            let message =
                crate::protocol::read_message::<_, ClientMessage>(&mut reader, MAX_FRAME_SIZE)
                    .expect("a message from the view");
            if matches!(
                message,
                ClientMessage::InputEvents { .. } | ClientMessage::Input { .. }
            ) {
                break message;
            }
        };

        match message {
            ClientMessage::InputEvents { events } => assert_eq!(
                events,
                vec![crate::protocol::ClientInputEvent::Paste {
                    text: "echo one\necho two".to_string(),
                }],
            ),
            other => panic!("paste must cross as a structured paste, got {other:?}"),
        }
    }

    #[test]
    fn send_focus_crosses_as_a_terminal_scoped_event() {
        let (peer, runtime) = connected_view("focus-shape", "w1:p2", false);
        let mut held = peer
            .accepted
            .recv_timeout(Duration::from_secs(5))
            .expect("peer stream");

        runtime.send_focus_event(crate::ghostty::FocusEvent::Lost);

        let mut reader = BufReader::new(&mut held);
        let message = loop {
            let message =
                crate::protocol::read_message::<_, ClientMessage>(&mut reader, MAX_FRAME_SIZE)
                    .expect("a message from the view");
            if matches!(message, ClientMessage::InputEvents { .. }) {
                break message;
            }
        };

        assert_eq!(
            message,
            ClientMessage::InputEvents {
                events: vec![crate::protocol::ClientInputEvent::FocusLost],
            }
        );
    }

    /// The mode is the peer's to report, and a reconnect is the same terminal
    /// on the same peer. Resetting to "off" in the gap before it reports again
    /// would unwrap a paste that lands in that window.
    #[test]
    fn a_reconnect_keeps_the_peers_last_known_paste_mode() {
        let (_peer, previous) = connected_view("paste-mode-old", "w1:p2", false);
        let (_next_peer, mut next) = connected_view("paste-mode-new", "w1:p2", false);

        previous.shared.set_bracketed_paste(true);
        assert!(!next.bracketed_paste());

        next.inherit_from(&previous);

        assert!(
            next.bracketed_paste(),
            "a reconnected view keeps the paste mode the peer last reported"
        );
    }

    /// The gate that keeps an agent-name target from being retired when the
    /// agent — but not its pane — exits on the peer.
    #[test]
    fn only_pane_and_terminal_targets_are_retirable() {
        // One mute peer per connect: its listener accepts a single connection.
        for (name, target, expected) in [
            ("pane", "w1:p2", true),
            ("terminal", "term_18d2f0a1", true),
            ("agent", "claude", false),
            ("workspace", "w1", false),
        ] {
            let peer = MutePeer::start(name, true);
            let runtime = RemoteTerminalRuntime::connect(
                &peer.path,
                "beta".to_string(),
                target.to_string(),
                80,
                24,
                false,
            )
            .expect("connect to mute peer");
            assert_eq!(
                runtime.target_is_a_pane_or_terminal(),
                expected,
                "target {target}"
            );
        }
    }

    /// A runtime connected to a mute peer, for driving [`begin_reconnect`]
    /// against a refusal without standing up a whole app.
    ///
    /// `send_frame` decides what the last connection was: one that rendered
    /// resets the rejection counter, one that did not feeds it.
    fn connected_view(
        name: &str,
        target: &str,
        send_frame: bool,
    ) -> (MutePeer, RemoteTerminalRuntime) {
        let peer = MutePeer::start(name, send_frame);
        let runtime = RemoteTerminalRuntime::connect(
            &peer.path,
            "beta".to_string(),
            target.to_string(),
            80,
            24,
            false,
        )
        .expect("connect to mute peer");
        (peer, runtime)
    }

    /// The peer is told about a cell-metric change even when the grid holds
    /// still, because the metrics ride in the same message.
    ///
    /// Two clients can report one grid with different cell dimensions, so a
    /// dedup keyed on `(cols, rows)` alone would leave the peer sizing graphics
    /// against whichever arrived first.
    #[test]
    fn resize_forwards_a_cell_metric_change_at_an_unchanged_grid() {
        let (_peer, runtime) = connected_view("resize-px", "w1:p2", false);

        runtime.resize(24, 80, 10, 20);
        assert_eq!(*runtime.size.lock().expect("size"), (80, 24, 10, 20));

        // Same grid, same metrics: nothing to say.
        runtime.resize(24, 80, 10, 20);
        assert_eq!(*runtime.size.lock().expect("size"), (80, 24, 10, 20));

        // Same grid, new metrics: still a change.
        runtime.resize(24, 80, 12, 24);
        assert_eq!(*runtime.size.lock().expect("size"), (80, 24, 12, 24));
    }

    /// A frame carrying nothing but a cursor.
    ///
    /// The cells are left empty on purpose: [`RemoteTerminalRuntime::cursor_state`]
    /// bounds-checks against the *area* it is asked about, never against the
    /// frame's own dimensions, and a test that supplied both could not tell
    /// which one it was proving.
    fn frame_with_cursor(cursor: Option<crate::protocol::CursorState>) -> FrameData {
        FrameData {
            cells: Vec::new(),
            width: 0,
            height: 0,
            cursor,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
            scroll: None,
        }
    }

    fn set_frame(runtime: &RemoteTerminalRuntime, frame: FrameData) {
        *runtime.shared.frame.lock().expect("frame lock") = Some(frame);
    }

    fn cell(symbol: &str, hyperlink: Option<u32>) -> crate::protocol::CellData {
        crate::protocol::CellData {
            symbol: symbol.to_string(),
            fg: 0,
            bg: 0,
            modifier: 0,
            skip: false,
            hyperlink,
        }
    }

    /// A link on the peer's screen is clickable here.
    ///
    /// The frame already carries the URIs and the cells that point at them, so
    /// this is a lookup rather than a round trip. Answering empty is what left
    /// a link underlined on screen and dead to the mouse.
    #[test]
    fn hyperlinks_are_read_from_the_frame_in_screen_coordinates() {
        let (_peer, runtime) = connected_view("frame-links", "w1:p2", false);
        set_frame(
            &runtime,
            FrameData {
                cells: vec![
                    cell("a", None),
                    cell("L", Some(0)),
                    cell("x", None),
                    cell("M", Some(1)),
                ],
                width: 2,
                height: 2,
                cursor: None,
                hyperlinks: vec!["https://one.example".into(), "https://two.example".into()],
                graphics: Vec::new(),
                scroll: None,
            },
        );

        // Offset origin: the caller asks in screen coordinates, and a pane that
        // is not at 0,0 must not report its links at the screen's top-left.
        let links = runtime.visible_hyperlinks(Rect::new(10, 5, 2, 2));

        assert_eq!(
            links,
            vec![
                ((11, 5), "L".to_string(), "https://one.example".to_string()),
                ((11, 6), "M".to_string(), "https://two.example".to_string()),
            ]
        );
    }

    /// Cells outside the asked-about area are not reported.
    ///
    /// The frame is sized by the peer and the area by the local layout, and a
    /// resize lands one before the other. Reporting a link at a coordinate the
    /// pane no longer covers puts it under a neighbouring pane.
    #[test]
    fn hyperlinks_outside_the_area_are_dropped() {
        let (_peer, runtime) = connected_view("frame-links-clip", "w1:p2", false);
        set_frame(
            &runtime,
            FrameData {
                cells: vec![cell("L", Some(0)), cell("M", Some(0))],
                width: 1,
                height: 2,
                cursor: None,
                hyperlinks: vec!["https://one.example".into()],
                graphics: Vec::new(),
                scroll: None,
            },
        );

        let links = runtime.visible_hyperlinks(Rect::new(0, 0, 1, 1));

        assert_eq!(links.len(), 1, "only the row inside the area survives");
    }

    /// Who owns a click on a peer-backed pane follows the peer's own mode.
    ///
    /// The regression this pins is not a wrong answer but a missing one: taking
    /// every click meant `Selection::anchor` was never reached, so a drag over
    /// such a pane selected nothing and Ctrl-C had nothing to copy. Declining
    /// while the peer's program has not asked for the mouse is the same rule a
    /// local pane applies through `encode_mouse_button`.
    #[test]
    fn a_click_is_declined_until_the_peer_asks_for_the_mouse() {
        let peer = MutePeer::start("mouse-modes", false);
        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &peer.path,
            "beta".to_string(),
            "w1:p2".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to mute peer");
        let crate::terminal::TerminalRuntime::Remote(view) = &runtime else {
            panic!("expected a remote runtime");
        };

        let click = crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left);
        let position = crate::input::mouse::Position::Cell { column: 3, row: 4 };

        assert!(
            !runtime.try_send_mouse_button(click, position, crossterm::event::KeyModifiers::NONE),
            "a pane whose program never asked for the mouse leaves the click to the selection"
        );

        view.shared.set_input_modes(false, true);

        assert!(
            runtime.try_send_mouse_button(click, position, crossterm::event::KeyModifiers::NONE),
            "once the peer reports mouse reporting the click belongs to its program"
        );
    }

    /// Both modes travel together and are applied together.
    #[test]
    fn input_modes_from_the_peer_are_applied_as_a_pair() {
        let (_peer, runtime) = connected_view("input-modes", "w1:p2", false);
        assert!(!runtime.bracketed_paste());
        assert!(!runtime.mouse_reporting());

        runtime.shared.set_input_modes(true, false);

        assert!(runtime.bracketed_paste());
        assert!(
            !runtime.mouse_reporting(),
            "a terminal can want bracketed paste without wanting the mouse"
        );
    }

    /// The scroll position is read from the frame it describes.
    ///
    /// Kept together on purpose: a position from a different moment than the
    /// cells maps screen rows onto the wrong buffer rows, and nothing about the
    /// result looks wrong.
    #[test]
    fn scroll_metrics_come_from_the_retained_frame() {
        let (_peer, runtime) = connected_view("frame-scroll", "w1:p2", false);
        assert!(
            runtime.scroll_metrics().is_none(),
            "no frame has arrived yet"
        );

        set_frame(
            &runtime,
            FrameData {
                cells: Vec::new(),
                width: 0,
                height: 0,
                cursor: None,
                hyperlinks: Vec::new(),
                graphics: Vec::new(),
                scroll: Some(crate::protocol::FrameScroll {
                    offset_from_bottom: 15,
                    max_offset_from_bottom: 175,
                    viewport_rows: 31,
                }),
            },
        );

        let metrics = runtime.scroll_metrics().expect("metrics from the frame");
        assert_eq!(metrics.offset_from_bottom, 15);
        assert_eq!(metrics.max_offset_from_bottom, 175);
        assert_eq!(metrics.viewport_rows, 31);
    }

    /// The regression pin for a peer-backed pane rendering no cursor at all.
    ///
    /// Asserts the two things the old `self.pty().and_then(...)` could not do:
    /// report a cursor for a remote pane, and report the peer's **shape** with
    /// it. Shape is the half a fallback through `rendered_cursor()` would still
    /// have got wrong, since that hardcodes 0.
    #[test]
    fn a_remote_pane_reports_the_peers_cursor_and_shape() {
        let (_peer, runtime) = connected_view("cursor-shape", "w1:p2", false);
        set_frame(
            &runtime,
            frame_with_cursor(Some(crate::protocol::CursorState {
                x: 3,
                y: 2,
                visible: true,
                // Steady bar: the shape a block-only pipeline cannot fake.
                shape: 6,
            })),
        );

        let cursor = runtime
            .cursor_state(Rect::new(10, 5, 80, 24), true)
            .expect("a connected peer view reports the cursor it was sent");

        // Offset into the pane's area, exactly as the local pty arm does.
        assert_eq!((cursor.x, cursor.y), (13, 7));
        assert!(cursor.visible);
        assert_eq!(cursor.shape, 6, "the peer's shape must survive the hop");
    }

    /// The pin for the defect itself, which lived one layer up.
    ///
    /// [`crate::terminal::TerminalRuntime::cursor_state`] delegated through
    /// `pty()`, so it answered `None` for every remote pane no matter what the
    /// peer had sent. Asserting through the enum is what makes this fail if that
    /// arm is ever dropped again; the direct-method tests above cannot, because
    /// the pane grid never calls the method directly.
    #[test]
    fn the_terminal_runtime_enum_reports_a_remote_panes_cursor() {
        let (_peer, runtime) = connected_view("cursor-enum", "w1:p2", false);
        set_frame(
            &runtime,
            frame_with_cursor(Some(crate::protocol::CursorState {
                x: 4,
                y: 1,
                visible: true,
                shape: 5,
            })),
        );

        let runtime = crate::terminal::TerminalRuntime::Remote(Box::new(runtime));
        let cursor = runtime
            .cursor_state(Rect::new(0, 0, 80, 24), true)
            .expect("a remote pane must not report itself as having no cursor");

        assert_eq!((cursor.x, cursor.y), (4, 1));
        assert_eq!(cursor.shape, 5);
    }

    /// The peer's own hidden cursor is reported as hidden rather than as
    /// absent: "no cursor here" and "a cursor that is turned off" reach the
    /// client as the same escape, but only the first one lets the drawn-cursor
    /// and IME paths tell that a cursor exists at all.
    #[test]
    fn a_remote_pane_reports_a_hidden_cursor_as_hidden() {
        let (_peer, runtime) = connected_view("cursor-hidden", "w1:p2", false);
        set_frame(
            &runtime,
            frame_with_cursor(Some(crate::protocol::CursorState {
                x: 1,
                y: 1,
                visible: false,
                shape: 2,
            })),
        );

        let cursor = runtime
            .cursor_state(Rect::new(0, 0, 80, 24), true)
            .expect("a hidden cursor is still a cursor");
        assert!(!cursor.visible);
    }

    /// Matches what [`RemoteTerminalRuntime::render`] draws: the stale frame
    /// stays on screen after a disconnect, but its cursor does not, because it
    /// marks a position nothing is updating any more.
    #[test]
    fn a_disconnected_remote_pane_reports_no_cursor() {
        let (_peer, runtime) = connected_view("cursor-stale", "w1:p2", false);
        set_frame(
            &runtime,
            frame_with_cursor(Some(crate::protocol::CursorState {
                x: 3,
                y: 2,
                visible: true,
                shape: 6,
            })),
        );
        assert!(runtime
            .cursor_state(Rect::new(0, 0, 80, 24), true)
            .is_some());

        runtime.shared.mark_disconnected();

        assert!(
            runtime
                .cursor_state(Rect::new(0, 0, 80, 24), true)
                .is_none(),
            "a frame nothing is feeding must not keep showing a cursor"
        );
    }

    /// A peer that is still reporting a cursor for a size this side has already
    /// shrunk past must not park the host cursor outside the pane.
    #[test]
    fn a_remote_cursor_outside_the_area_is_dropped() {
        let (_peer, runtime) = connected_view("cursor-oob", "w1:p2", false);
        set_frame(
            &runtime,
            frame_with_cursor(Some(crate::protocol::CursorState {
                x: 40,
                y: 2,
                visible: true,
                shape: 6,
            })),
        );

        assert!(runtime
            .cursor_state(Rect::new(0, 0, 20, 24), true)
            .is_none());
        // ...and `show_cursor: false` is honoured the same way the pty arm does.
        assert!(runtime
            .cursor_state(Rect::new(0, 0, 80, 24), false)
            .is_none());
    }

    /// The peer's first "not found" is the whole answer, so the view dies on it
    /// rather than spending two more connects to be told the same thing. The
    /// last connection rendered, which is what really happens when a pane is
    /// closed on the peer: the view was live until the moment it was refused.
    #[test]
    fn a_target_gone_refusal_retires_on_the_first_attempt() {
        let reason = "terminal session control failed: terminal target w1:p2 not found";
        let (_peer, mut runtime) = connected_view("gone-first", "w1:p2", true);
        runtime
            .shared
            .set_refusal(ShutdownCode::TargetGone, reason.to_string());

        assert!(
            !runtime.begin_reconnect(),
            "one authoritative refusal must be enough"
        );
        assert_eq!(runtime.dead_reason(), Some(reason));
    }

    /// An agent name stops resolving when the agent exits even though its pane
    /// lives on, so the same words must not retire a view that targets one.
    #[test]
    fn a_target_gone_refusal_does_not_retire_an_agent_view() {
        let (_peer, mut runtime) = connected_view("gone-agent", "claude", true);
        runtime.shared.set_refusal(
            ShutdownCode::TargetGone,
            "terminal attach failed: terminal term_18d2 not found".to_string(),
        );

        assert!(runtime.begin_reconnect(), "an agent view keeps retrying");
        assert!(runtime.dead_reason().is_none());
    }

    /// A rejection the peer did not explain as a gone target still gets the
    /// full three strikes: another client holding the terminal is a condition
    /// that passes, and retiring on the first one would close a live pane.
    #[test]
    fn a_transient_refusal_still_takes_three_attempts() {
        let (_peer, mut runtime) = connected_view("transient-strikes", "w1:p2", false);
        runtime.shared.set_refusal(
            ShutdownCode::TargetUnavailable,
            "terminal attach failed: terminal term_18d2 has a read in progress; retry".to_string(),
        );

        assert!(runtime.begin_reconnect());
        assert!(runtime.begin_reconnect());
        assert!(
            !runtime.begin_reconnect(),
            "the third rejection is still the final one"
        );
        assert!(runtime.dead_reason().is_some());
    }

    /// A peer that cannot be reached never refused anything, so it must not
    /// spend the strikes reserved for a peer that did.
    ///
    /// The two are indistinguishable one attempt later — a connect that never
    /// landed leaves `saw_frame` false exactly like an attach the peer accepted
    /// and dropped — so before [`ReconnectState::connect_failed`] existed, three
    /// refused connects retired the view with "peer closed the connection
    /// without rendering", which the peer never said and never did.
    #[test]
    fn transport_failures_never_retire_a_view() {
        let (_peer, mut runtime) = connected_view("transport-strikes", "w1:p2", false);
        let now = Instant::now();

        // The session that just ended without a frame is a strike on its own;
        // what follows it here is only ever a connect that failed to land.
        assert!(runtime.begin_reconnect());
        for attempt in 0..5 {
            runtime.reconnect_failed(now, "connection refused");
            assert!(
                runtime.begin_reconnect(),
                "an unreachable peer is retried indefinitely (attempt {attempt})"
            );
        }

        assert!(
            runtime.dead_reason().is_none(),
            "a peer that never answered has not retired this view"
        );
    }

    /// Why the refusal is kept apart from the end reason: this side writes the
    /// end reason too, and a local failure whose text happens to end the same
    /// way is not the peer saying anything about its target.
    #[test]
    fn a_local_failure_that_reads_like_a_refusal_does_not_retire() {
        let (_peer, mut runtime) = connected_view("local-failure", "w1:p2", true);
        runtime
            .shared
            .set_end_reason("matching remote herdr not found".to_string());

        assert!(
            runtime.begin_reconnect(),
            "only the peer's own words retire"
        );
        assert!(runtime.dead_reason().is_none());
    }

    /// The regression this guards: closing a peer-backed view runs on the event
    /// loop, and the reader it disposes of is parked in a read that no timeout
    /// bounds. Waiting for that thread to notice on its own means waiting for
    /// TCP to give up — minutes — with the whole server frozen behind it.
    #[test]
    fn dropping_a_runtime_does_not_wait_on_a_silent_peer() {
        let peer = MutePeer::start("drop", true);
        let runtime = RemoteTerminalRuntime::connect(
            &peer.path,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to mute peer");

        // The peer is holding the connection open and will never write again.
        let held = peer.accepted.recv_timeout(Duration::from_secs(5)).ok();

        let started = Instant::now();
        drop(runtime);
        let elapsed = started.elapsed();
        drop(held);

        assert!(
            elapsed < Duration::from_secs(1),
            "dropping a view onto a silent peer took {elapsed:?}"
        );
    }

    /// The other half of the same guarantee: the reader is not the only thing
    /// closing a view can wait on.
    ///
    /// The writer thread parks against a peer that stopped reading. Closing the
    /// view must not wait for it: the socket is shut down through a handle of
    /// its own, which is what ends that parked write.
    #[test]
    fn dropping_a_runtime_does_not_wait_on_a_busy_writer() {
        let peer = MutePeer::start("drop-busy", true);
        let runtime = RemoteTerminalRuntime::connect(
            &peer.path,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to mute peer");
        let held = peer.accepted.recv_timeout(Duration::from_secs(5)).ok();

        // Enough traffic to fill the socket buffer and park the writer thread
        // mid-write, which is the state a close has to survive.
        for _ in 0..64 {
            runtime.send_bytes(Bytes::from(vec![b'x'; 64 * 1024]));
        }

        let started = Instant::now();
        drop(runtime);
        let elapsed = started.elapsed();

        drop(held);

        assert!(
            elapsed < Duration::from_secs(1),
            "dropping a view whose writer was busy took {elapsed:?}"
        );
    }

    /// P1: the defect this queue exists for.
    ///
    /// Every one of these calls used to write to the peer socket inline from
    /// the server loop, so a peer that accepted the connection and then stopped
    /// reading parked that loop for the whole write timeout — with every local
    /// pane, client, API call and other peer behind it.
    #[test]
    fn sending_to_a_wedged_peer_never_blocks_the_caller() {
        let peer = MutePeer::start("wedged-send", true);
        let runtime = RemoteTerminalRuntime::connect(
            &peer.path,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to mute peer");
        let held = peer.accepted.recv_timeout(Duration::from_secs(5)).ok();

        // Fill the socket buffer so the writer thread is genuinely parked, then
        // keep going: it is the calls after that point which used to block.
        for _ in 0..64 {
            runtime.send_bytes(Bytes::from(vec![b'x'; 64 * 1024]));
        }

        let started = Instant::now();
        for index in 0..200 {
            runtime.send_bytes(Bytes::from(vec![b'y'; 16]));
            runtime.resize(24, 80 + (index % 8), 0, 0);
            runtime.scroll(AttachScrollDirection::Up, 1);
        }
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(1),
            "600 sends to a wedged peer took {elapsed:?}; \
             the write timeout alone is {STREAM_WRITE_TIMEOUT:?}"
        );

        drop(runtime);
        drop(held);
    }

    #[test]
    fn a_wedged_peer_stops_at_the_queue_bound_instead_of_growing() {
        let peer = MutePeer::start("wedged-bound", true);
        let runtime = RemoteTerminalRuntime::connect(
            &peer.path,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to mute peer");
        let held = peer.accepted.recv_timeout(Duration::from_secs(5)).ok();

        // Comfortably past the byte limit, against a peer that will never
        // drain. Kept just past it rather than far past: every send allocates
        // and encodes its payload, and this runs in a debug build.
        let sends = 2 * REMOTE_WRITER_QUEUE_LIMITS.max_bytes / (64 * 1024);
        for _ in 0..sends {
            runtime.send_bytes(Bytes::from(vec![b'x'; 64 * 1024]));
        }

        let state = runtime.write_queue.lock_state();
        assert!(
            state.budget.bytes() <= REMOTE_WRITER_QUEUE_LIMITS.max_bytes,
            "queued {} bytes against a limit of {}",
            state.budget.bytes(),
            REMOTE_WRITER_QUEUE_LIMITS.max_bytes,
        );
        assert!(
            state.budget.items() <= REMOTE_WRITER_QUEUE_LIMITS.max_items,
            "queued {} items against a limit of {}",
            state.budget.items(),
            REMOTE_WRITER_QUEUE_LIMITS.max_items,
        );
        assert!(
            !runtime.is_connected(),
            "a peer that overran the bound must be marked disconnected"
        );
        drop(state);

        drop(runtime);
        drop(held);
    }

    /// How long to allow for a wake that should already have happened, and to
    /// wait before believing one did not.
    const WAKE_PROBE: Duration = Duration::from_millis(100);

    async fn was_woken(wake: &tokio::sync::Notify) -> bool {
        tokio::time::timeout(WAKE_PROBE, wake.notified())
            .await
            .is_ok()
    }

    /// P2: a frame used to set the dirty flag and wait to be noticed, which on
    /// an otherwise idle server meant up to a full poll interval.
    #[tokio::test]
    async fn a_frame_wakes_the_server_loop() {
        let wake = Arc::new(tokio::sync::Notify::new());
        let shared = RemoteShared::with_wake(Some(Arc::clone(&wake)));
        // A fresh view starts dirty, so clear it the way the loop's sweep does.
        shared.dirty.swap(false, Ordering::Relaxed);

        shared.store_frame(frame_with_cursor(None));

        assert!(
            was_woken(&wake).await,
            "a frame must wake the loop rather than wait for its next sweep"
        );
    }

    #[tokio::test]
    async fn frames_arriving_while_a_draw_is_owed_do_not_wake_again() {
        // Coalescing: while the loop already owes this view a draw, further
        // frames replace the retained one and cost nothing.
        let wake = Arc::new(tokio::sync::Notify::new());
        let shared = RemoteShared::with_wake(Some(Arc::clone(&wake)));
        shared.dirty.swap(false, Ordering::Relaxed);

        shared.store_frame(frame_with_cursor(None));
        assert!(was_woken(&wake).await, "the first frame wakes the loop");

        shared.store_frame(frame_with_cursor(None));
        shared.store_frame(frame_with_cursor(None));

        assert!(
            !was_woken(&wake).await,
            "a draw is already owed, so these must not wake the loop again"
        );
    }

    #[tokio::test]
    async fn a_disconnect_wakes_the_loop_so_the_pane_can_be_dimmed() {
        let wake = Arc::new(tokio::sync::Notify::new());
        let shared = RemoteShared::with_wake(Some(Arc::clone(&wake)));
        shared.dirty.swap(false, Ordering::Relaxed);

        shared.mark_disconnected();

        assert!(was_woken(&wake).await);
    }

    #[test]
    fn a_superseded_resize_is_dropped_rather_than_sent() {
        // A drag produces a stream of sizes; only the last describes the pane.
        let queue = RemoteWriterQueue::new();
        queue.enqueue(RemoteWriteItem::Ordered(b"input".to_vec()));
        for width in 0..5u8 {
            queue.enqueue_resize(vec![width]);
        }
        queue.enqueue(RemoteWriteItem::Ordered(b"after".to_vec()));
        queue.close();

        let mut written = Vec::new();
        while let Some(bytes) = queue.recv() {
            written.push(bytes);
        }

        assert_eq!(
            written,
            vec![b"input".to_vec(), vec![4u8], b"after".to_vec()],
            "only the newest resize survives, and ordering is preserved"
        );
        assert_eq!(queue.lock_state().budget.items(), 0);
    }

    #[test]
    fn closing_the_queue_lets_the_writer_finish_what_is_queued() {
        // The detach is queued behind existing traffic and must still go out.
        let queue = RemoteWriterQueue::new();
        queue.enqueue(RemoteWriteItem::Ordered(b"first".to_vec()));
        queue.close();

        assert!(matches!(
            queue.enqueue(RemoteWriteItem::Ordered(b"late".to_vec())),
            RemoteEnqueue::Closed
        ));
        assert_eq!(queue.recv(), Some(b"first".to_vec()));
        assert_eq!(queue.recv(), None);
    }

    /// The same hazard reached through the connect path: a peer that accepts and
    /// never answers the handshake must not hold the caller, because
    /// `peer.workspace.open` reaches this from the event loop.
    #[test]
    fn connecting_to_a_peer_that_never_answers_gives_up() {
        let path = std::env::temp_dir().join(format!(
            "herdr-silent-peer-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = crate::ipc::bind_local_listener(&path).expect("bind silent peer");
        let accepted = std::thread::spawn(move || {
            use interprocess::local_socket::traits::Listener as _;
            // Accept and then say nothing at all, holding the stream open.
            let stream = listener.accept();
            std::thread::sleep(Duration::from_secs(30));
            drop(stream);
        });

        let started = Instant::now();
        let result = RemoteTerminalRuntime::connect(
            &path,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        );
        let elapsed = started.elapsed();

        assert!(
            result.is_err(),
            "a peer that never answers must not connect"
        );
        assert!(
            elapsed < HANDSHAKE_TIMEOUT * 2,
            "connect to a silent peer took {elapsed:?}"
        );

        let _ = std::fs::remove_file(&path);
        drop(accepted);
    }
}
