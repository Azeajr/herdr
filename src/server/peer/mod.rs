//! Outbound connections to federated peer servers.
//!
//! This is the runtime half of remote workspaces: one control connection per
//! configured peer, owned by this server. The state those connections produce
//! lives in `crate::app::peers`, reached only through [`AppEvent`].
//!
//! Nothing here is a herdr *client* in the process sense. A peer connection
//! speaks the JSON API directly instead of spawning `herdr client`, so a peer
//! going away cannot disturb this server's own workspaces or any other peer.
//!
//! Each peer owns one OS thread because the API socket is blocking I/O. A
//! thread parks in `next_event` with a bounded read timeout, so it notices a
//! shutdown request between events.

mod control;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::app::peers::{PeerHandle, PeerTarget};
use crate::events::AppEvent;

/// One running peer connection.
struct PeerConnection {
    running: Arc<AtomicBool>,
    /// Whether this server currently has a view onto one of this peer's panes.
    ///
    /// Published by the reconciler, read by the connection thread: only the
    /// reconciler can see the local views, and only the thread should decide
    /// how often to ask the peer about them.
    panes_wanted: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl PeerConnection {
    fn stop(self, handle: &PeerHandle) {
        self.running.store(false, Ordering::Relaxed);
        // Bounded because every wait the worker can be in watches this flag: a
        // stream read and a backoff sleep park for at most one read timeout,
        // and the ssh round trips that set the transport up carry the flag into
        // the child and kill it. It was the transport setup that was missing —
        // ssh dialing a host that had gone away returned only when the kernel
        // gave up on the TCP connect, holding shutdown for over two minutes.
        if self.thread.join().is_err() {
            warn!(peer = %handle, "peer connection thread panicked");
        }
    }
}

/// Owns every outbound peer connection this server maintains.
pub struct PeerRuntimeRegistry {
    connections: HashMap<PeerHandle, PeerConnection>,
    event_tx: mpsc::Sender<AppEvent>,
}

impl PeerRuntimeRegistry {
    pub fn new(event_tx: mpsc::Sender<AppEvent>) -> Self {
        Self {
            connections: HashMap::new(),
            event_tx,
        }
    }

    pub fn is_connected(&self, handle: &PeerHandle) -> bool {
        self.connections.contains_key(handle)
    }

    /// Handles of every peer with a running connection.
    pub fn handles(&self) -> impl Iterator<Item = &PeerHandle> {
        self.connections.keys()
    }

    /// Starts maintaining a connection to `target`.
    ///
    /// Returns `false` when a connection for this handle is already running;
    /// the caller decides whether that is an error.
    pub fn connect(&mut self, handle: PeerHandle, target: PeerTarget) -> bool {
        if self.connections.contains_key(&handle) {
            return false;
        }

        let running = Arc::new(AtomicBool::new(true));
        let panes_wanted = Arc::new(AtomicBool::new(false));
        let worker_running = Arc::clone(&running);
        let worker_panes_wanted = Arc::clone(&panes_wanted);
        let worker_handle = handle.clone();
        let event_tx = self.event_tx.clone();
        let thread = std::thread::Builder::new()
            .name(format!("herdr-peer-{handle}"))
            .spawn(move || {
                control::run(
                    worker_handle,
                    target,
                    event_tx,
                    worker_running,
                    worker_panes_wanted,
                );
            });

        match thread {
            Ok(thread) => {
                debug!(peer = %handle, "peer connection started");
                self.connections.insert(
                    handle,
                    PeerConnection {
                        running,
                        panes_wanted,
                        thread,
                    },
                );
                true
            }
            Err(err) => {
                warn!(peer = %handle, error = %err, "failed to spawn peer connection thread");
                false
            }
        }
    }

    /// Records which peers currently back a view here, so their connections
    /// know whether anyone is looking at their panes.
    ///
    /// Takes the whole set rather than one handle so a peer whose last view
    /// closed is turned off in the same pass that turns another on. Borrowed
    /// names rather than owned handles because this runs every tick and the set
    /// it builds does not outlive the call.
    pub fn set_peers_backing_views<'a>(&self, backing: impl IntoIterator<Item = &'a str>) {
        let backing: std::collections::HashSet<&str> = backing.into_iter().collect();
        for (handle, connection) in &self.connections {
            connection
                .panes_wanted
                .store(backing.contains(handle.as_str()), Ordering::Relaxed);
        }
    }

    /// Stops the connection for `handle`, blocking until its thread exits.
    pub fn disconnect(&mut self, handle: &PeerHandle) -> bool {
        match self.connections.remove(handle) {
            Some(connection) => {
                connection.stop(handle);
                true
            }
            None => false,
        }
    }

    /// Stops every peer connection. Signals all of them before joining any, so
    /// shutdown costs one read timeout in total rather than one per peer.
    pub fn shutdown(&mut self) {
        for connection in self.connections.values() {
            connection.running.store(false, Ordering::Relaxed);
        }
        for (handle, connection) in self.connections.drain() {
            connection.stop(&handle);
        }
    }
}

impl Drop for PeerRuntimeRegistry {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Waits for the connection thread to have filled the channel and be parked
    /// trying to add to it.
    fn wait_until_channel_is_full(rx: &mpsc::Receiver<AppEvent>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while rx.capacity() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(rx.capacity(), 0, "peer never filled the event channel");
    }

    /// The regression this guards: `shutdown` joins the connection threads, and
    /// it runs on the event loop that drains their channel. A thread parked on
    /// a full channel would be waiting for a drain that only returning from
    /// this join could allow, so the server would never exit — leaving the peer
    /// bridge's ssh and socket cleanup, which happens on drop, unreached.
    #[test]
    fn shutdown_does_not_wait_on_a_channel_it_is_blocking() {
        // Never read from: this stands in for an event loop that has stopped
        // draining because it is inside `complete_shutdown`.
        let (event_tx, event_rx) = mpsc::channel(1);
        let mut registry = PeerRuntimeRegistry::new(event_tx);

        // A socket that is not there fails fast and reconnects, so the thread
        // keeps producing state without needing a peer to exist.
        assert!(registry.connect(
            PeerHandle::new("alpha".to_string()),
            PeerTarget::SocketPath(std::path::PathBuf::from(
                "/nonexistent/herdr-peer-shutdown-test.sock",
            )),
        ));
        wait_until_channel_is_full(&event_rx);

        let started = Instant::now();
        registry.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "shutdown took {:?}",
            started.elapsed()
        );
        assert!(registry.handles().next().is_none());
    }

    /// Removing one peer runs from the same event loop, so it has the same
    /// hazard as a full shutdown.
    #[test]
    fn disconnect_does_not_wait_on_a_channel_it_is_blocking() {
        let (event_tx, event_rx) = mpsc::channel(1);
        let mut registry = PeerRuntimeRegistry::new(event_tx);
        let handle = PeerHandle::new("alpha".to_string());

        assert!(registry.connect(
            handle.clone(),
            PeerTarget::SocketPath(std::path::PathBuf::from(
                "/nonexistent/herdr-peer-disconnect-test.sock",
            )),
        ));
        wait_until_channel_is_full(&event_rx);

        let started = Instant::now();
        assert!(registry.disconnect(&handle));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "disconnect took {:?}",
            started.elapsed()
        );
    }
}
