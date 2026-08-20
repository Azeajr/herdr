//! The per-peer control channel.
//!
//! One of these runs per configured peer. It identifies the peer, enumerates
//! its workspaces, follows its workspace events, and heartbeats — reconnecting
//! with backoff whenever any of that fails.
//!
//! Every failure is contained to this peer: errors become
//! [`PeerConnectionState`] transitions reported through [`AppEvent`], and are
//! never propagated to the server's own event loop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::api::client::{ApiClient, ConnectionTarget, SubscriptionStream};
use crate::api::schema::{
    EmptyParams, EventKind, EventsSubscribeParams, Method, PaneInfo, PaneListParams, Request,
    ResponseResult, Subscription, WorkspaceInfo,
};
use crate::app::peers::{PeerConnectionState, PeerHandle, PeerIdentity, PeerTarget};
use crate::events::AppEvent;

/// How long a read parks before the worker rechecks the shutdown flag.
const STREAM_READ_TIMEOUT: Duration = Duration::from_millis(500);
/// How long a send waits for room before rechecking the shutdown flag.
const EVENT_SEND_POLL: Duration = Duration::from_millis(50);
/// Bound on one-shot requests to a peer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How often to ping a peer that is otherwise quiet.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// Backoff bounds between reconnect attempts.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(500);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(10);
/// Workspace events arrive in bursts during a batch operation; collapse them
/// into one re-enumeration.
const ENUMERATION_DEBOUNCE: Duration = Duration::from_millis(150);
/// Pane events are hotter than workspace ones — an agent flipping state or a
/// prompt rewriting the title emits per pane — so they get their own, longer
/// window rather than dragging workspace enumeration along with them.
const PANE_ENUMERATION_DEBOUNCE: Duration = Duration::from_millis(400);
/// How often to re-read a peer's panes while this server has a view onto one.
///
/// Events cover most changes, but `pane.agent_status_changed` is a per-pane
/// subscription with its own probe, so following it for every view would mean
/// one subscription per open pane and re-subscribing as views come and go. A
/// poll bounded to peers that actually back a view is the smaller mechanism,
/// and an idle/working flip is the one change a stale sidebar shows most.
const PANE_POLL_INTERVAL: Duration = Duration::from_millis(1_500);

/// Why a peer connection ended.
#[derive(Debug)]
enum SessionEnd {
    /// Shutdown was requested; do not reconnect.
    Stopped,
    /// The connection failed or closed. Reconnect after backoff.
    Failed(String),
    /// The peer cannot be federated with as configured. Retrying would just
    /// hide a misconfiguration, so the loop stops and surfaces the reason.
    Unsupported(String),
}

pub(super) fn run(
    handle: PeerHandle,
    target: PeerTarget,
    event_tx: mpsc::Sender<AppEvent>,
    running: Arc<AtomicBool>,
    panes_wanted: Arc<AtomicBool>,
) {
    let mut attempt: u32 = 0;
    // Held across sessions on purpose: peer-backed terminals connect through
    // the same transport, so tearing it down on a control-channel hiccup would
    // kill every open remote pane along with it.
    let mut transport: Option<Transport> = None;

    send(
        &event_tx,
        &running,
        &handle,
        PeerConnectionState::Connecting,
        None,
    );

    while running.load(Ordering::Relaxed) {
        let session = match ensure_transport(&handle, &target, &mut transport, &event_tx, &running)
        {
            Ok(api_socket) => run_session(
                &handle,
                &api_socket,
                &event_tx,
                &running,
                &panes_wanted,
                &mut attempt,
            ),
            Err(end) => end,
        };

        match session {
            SessionEnd::Stopped => break,
            SessionEnd::Unsupported(message) => {
                warn!(peer = %handle, error = %message, "peer cannot be federated with");
                send(
                    &event_tx,
                    &running,
                    &handle,
                    PeerConnectionState::Error { message },
                    None,
                );
                break;
            }
            SessionEnd::Failed(message) => {
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                attempt = attempt.saturating_add(1);
                warn!(peer = %handle, attempt, error = %message, "peer connection lost");
                // Report the retry, not a terminal error: the state has to stay
                // truthful for the whole backoff window, not just the instant
                // the attempt failed.
                send(
                    &event_tx,
                    &running,
                    &handle,
                    PeerConnectionState::Reconnecting { attempt, message },
                    None,
                );
                if !sleep_until_stopped(backoff(attempt), &running) {
                    break;
                }
            }
        }
    }

    debug!(peer = %handle, "peer connection stopped");
}

/// How a peer's JSON API is reached.
enum Transport {
    /// A socket on this machine; nothing to stand up.
    Direct(PathBuf),
    /// A remote server, reached through local bridge sockets over ssh. Boxed
    /// because the bridge owns its ssh session and both listeners, dwarfing the
    /// direct case.
    Bridged(Box<crate::remote::PeerSshBridge>),
}

impl Transport {
    fn api_socket(&self) -> &Path {
        match self {
            Self::Direct(path) => path,
            Self::Bridged(bridge) => bridge.api_socket(),
        }
    }
}

/// Opens the peer's transport if it is not open yet, yielding the local socket
/// its JSON API answers on.
///
/// Setting up an ssh transport costs several ssh round trips, which is why this
/// runs on the peer's own thread and not in the reconciler that starts it.
///
/// Those round trips are also why `running` goes into the bridge rather than
/// only being checked around this call: a peer that stopped answering leaves
/// ssh waiting on the network, and this thread is joined during shutdown.
fn ensure_transport(
    handle: &PeerHandle,
    target: &PeerTarget,
    transport: &mut Option<Transport>,
    event_tx: &mpsc::Sender<AppEvent>,
    running: &Arc<AtomicBool>,
) -> Result<PathBuf, SessionEnd> {
    if transport.is_none() {
        let opened = match target {
            PeerTarget::SocketPath(path) => Transport::Direct(path.clone()),
            PeerTarget::Ssh {
                destination,
                session,
            } => match crate::remote::start_peer_ssh_bridge(
                destination,
                session.as_deref(),
                Arc::clone(running),
            ) {
                Ok(bridge) => {
                    debug!(peer = %handle, socket = %bridge.api_socket().display(), "peer ssh transport ready");
                    // The socket has to reach state before a terminal can be
                    // opened through it, and only this thread knows the path.
                    send_event(
                        event_tx,
                        running,
                        AppEvent::PeerTransportReady {
                            handle: handle.clone(),
                            api_socket: bridge.api_socket().to_path_buf(),
                        },
                    );
                    Transport::Bridged(Box::new(bridge))
                }
                Err(crate::remote::PeerSshBridgeError::Unreachable(message)) => {
                    return Err(SessionEnd::Failed(message))
                }
                Err(crate::remote::PeerSshBridgeError::Unsupported(message)) => {
                    return Err(SessionEnd::Unsupported(message))
                }
            },
        };
        *transport = Some(opened);
    }

    match transport.as_ref() {
        Some(open) => Ok(open.api_socket().to_path_buf()),
        // Unreachable: the branch above just populated it.
        None => Err(SessionEnd::Unsupported("peer transport is missing".into())),
    }
}

/// One connect-and-serve cycle. Returns only when the connection ends.
///
/// `attempt` is the caller's consecutive-failure count, reset here the moment
/// the peer is proven to be answering. Backoff measures *consecutive* failures,
/// and a session that ran for hours before dropping is a fresh failure rather
/// than the continuation of one. Left to grow, a peer that flapped at startup
/// would still be waiting the full backoff a day later, and `peer.list` would
/// report a first failure as attempt 6.
fn run_session(
    handle: &PeerHandle,
    api_socket: &Path,
    event_tx: &mpsc::Sender<AppEvent>,
    running: &Arc<AtomicBool>,
    panes_wanted: &Arc<AtomicBool>,
    attempt: &mut u32,
) -> SessionEnd {
    let client = ApiClient::for_target(ConnectionTarget::SocketPath(api_socket.to_path_buf()));

    let identity = match identify(&client) {
        Ok(identity) => identity,
        Err(IdentifyError::Unreachable(message)) => return SessionEnd::Failed(message),
        Err(IdentifyError::Incompatible(message)) => return SessionEnd::Unsupported(message),
    };
    let session_instance_id = identity.instance_id.clone();

    // The event stream must be open before the first enumeration, or a
    // workspace created between the two would be missed until the next
    // heartbeat-triggered refresh.
    let mut events = match subscribe(&client, &session_instance_id) {
        Ok(events) => events,
        Err(message) => return SessionEnd::Failed(message),
    };

    // Identified and subscribed: this connection is a real session, so whatever
    // ends it later starts a fresh backoff curve rather than continuing the one
    // that got us here.
    *attempt = 0;
    send(
        event_tx,
        running,
        handle,
        PeerConnectionState::Connected,
        Some(identity),
    );

    match enumerate(&client, &session_instance_id) {
        Ok(workspaces) => send_workspaces(event_tx, running, handle, workspaces),
        Err(message) => return SessionEnd::Failed(message),
    }

    // Seeded on connect, not only on change: a view opened before this session
    // would otherwise wait for the peer's pane to change before it could be
    // labeled, and after a reconnect the peer's panes are new to us again.
    match enumerate_panes(&client, &session_instance_id) {
        Ok(panes) => send_panes(event_tx, running, handle, panes),
        Err(message) => return SessionEnd::Failed(message),
    }

    let mut last_heartbeat = Instant::now();
    let mut last_pane_poll = Instant::now();
    let mut refresh_due: Option<Instant> = None;
    let mut pane_refresh_due: Option<Instant> = None;

    loop {
        if !running.load(Ordering::Relaxed) {
            return SessionEnd::Stopped;
        }

        // A view that opened since the last enumeration has nothing to show
        // until the peer is asked again, and nothing on the peer had to change
        // for that to be true. The reconciler is the side that can see whether
        // any view is open, so it publishes that and this thread decides how
        // often to ask.
        if panes_wanted.load(Ordering::Relaxed) {
            pane_refresh_due.get_or_insert_with(|| last_pane_poll + PANE_POLL_INTERVAL);
        }

        match events.next_event() {
            Ok(Some(event)) => {
                if affects_workspaces(&event) {
                    refresh_due.get_or_insert_with(|| Instant::now() + ENUMERATION_DEBOUNCE);
                }
                if affects_panes(&event) {
                    pane_refresh_due
                        .get_or_insert_with(|| Instant::now() + PANE_ENUMERATION_DEBOUNCE);
                }
            }
            // No event within the read timeout — the stream is healthy.
            Ok(None) => {}
            Err(err) => {
                return SessionEnd::Failed(format!("peer event stream closed: {err}"));
            }
        }

        if refresh_due.is_some_and(|due| Instant::now() >= due) {
            refresh_due = None;
            match enumerate(&client, &session_instance_id) {
                Ok(workspaces) => send_workspaces(event_tx, running, handle, workspaces),
                Err(message) => return SessionEnd::Failed(message),
            }
            // Enumeration proves the peer is answering; no separate ping needed.
            last_heartbeat = Instant::now();
        }

        if pane_refresh_due.is_some_and(|due| Instant::now() >= due) {
            pane_refresh_due = None;
            match enumerate_panes(&client, &session_instance_id) {
                Ok(panes) => send_panes(event_tx, running, handle, panes),
                Err(message) => return SessionEnd::Failed(message),
            }
            last_pane_poll = Instant::now();
            last_heartbeat = last_pane_poll;
        }

        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            match identify(&client) {
                Ok(identity) if identity.instance_id != session_instance_id => {
                    return SessionEnd::Failed("peer server instance changed".into());
                }
                Ok(identity) => {
                    send(
                        event_tx,
                        running,
                        handle,
                        PeerConnectionState::Connected,
                        Some(identity),
                    );
                    last_heartbeat = Instant::now();
                }
                Err(IdentifyError::Unreachable(message)) => return SessionEnd::Failed(message),
                Err(IdentifyError::Incompatible(message)) => {
                    return SessionEnd::Unsupported(message)
                }
            }
        }
    }
}

/// Why a peer failed to identify.
enum IdentifyError {
    /// The peer did not answer. Retrying may work.
    Unreachable(String),
    /// The peer answered but cannot be federated with.
    Incompatible(String),
}

/// Pings the peer and validates that it can be federated with.
///
/// Timeout-bounded: a peer that accepts the connection and then stalls must not
/// pin this thread, or shutdown would block on it.
fn identify(client: &ApiClient) -> Result<PeerIdentity, IdentifyError> {
    let status = crate::api::read_runtime_status_at(&client.socket_path(), REQUEST_TIMEOUT)
        .map_err(|err| IdentifyError::Unreachable(format!("peer ping failed: {err}")))?
        .ok_or_else(|| IdentifyError::Unreachable("peer server is not running".to_string()))?;

    let Some(instance_id) = status.instance_id else {
        return Err(IdentifyError::Incompatible(
            "peer server does not report an instance id; it is too old to federate with".into(),
        ));
    };

    // Checked for shape here because this is the single ingest point that
    // claims to be one. `prefix_peer_id` concatenates whatever arrives, while
    // `split_peer_id` only splits 32 lowercase hex back apart, so an id of any
    // other shape produces enumerated workspace ids that read as *local*
    // everywhere downstream: opening one answers "no connected peer owns it",
    // and rename and close are handled here and fail as not-found, with nothing
    // saying why. A real herdr peer cannot report one — `instance_id::parse`
    // validates on both read and generate — so this is about what a modified or
    // non-herdr server on the other end can do to this one.
    if !crate::instance_id::is_instance_id(&instance_id) {
        return Err(IdentifyError::Incompatible(format!(
            "peer server reported a malformed instance id ({instance_id:?}); \
             it cannot be told apart from this server's own ids"
        )));
    }

    let Some(protocol) = status.protocol else {
        return Err(IdentifyError::Incompatible(
            "peer server does not report a protocol; it is too old to federate with".into(),
        ));
    };
    if protocol != crate::protocol::PROTOCOL_VERSION {
        return Err(IdentifyError::Incompatible(format!(
            "peer protocol {protocol} does not match local protocol {}",
            crate::protocol::PROTOCOL_VERSION
        )));
    }

    Ok(PeerIdentity {
        instance_id,
        version: status.version,
        protocol: status.protocol,
    })
}

fn subscribe(client: &ApiClient, instance_id: &str) -> Result<SubscriptionStream, String> {
    let request = Request {
        id: "peer:events".into(),
        method: Method::EventsSubscribe(EventsSubscribeParams {
            subscriptions: vec![
                Subscription::WorkspaceCreated {},
                Subscription::WorkspaceUpdated {},
                Subscription::WorkspaceRenamed {},
                Subscription::WorkspaceClosed {},
                Subscription::WorkspaceFocused {},
                Subscription::WorkspaceMetadataUpdated {},
                // Pane subscriptions carry no payload we read: each one only
                // says "ask again". Taking the facts from a fresh `pane.list`
                // instead of from five different event payloads keeps one shape
                // to parse and self-heals a missed event.
                Subscription::PaneCreated {},
                Subscription::PaneClosed {},
                Subscription::PaneUpdated {},
                Subscription::PaneExited {},
                Subscription::PaneAgentDetected {},
            ],
        }),
    };
    // The subscription's own reply gets the request budget, not the stream's
    // poll interval: opening it costs the same ssh round trips `identify` just
    // paid, and bounding it at half a second means a peer on a link with any
    // real latency can never finish connecting.
    client
        .subscribe_for_instance(request, instance_id, REQUEST_TIMEOUT, STREAM_READ_TIMEOUT)
        .map_err(|err| format!("peer event subscription failed: {err}"))
}

fn enumerate(client: &ApiClient, instance_id: &str) -> Result<Vec<WorkspaceInfo>, String> {
    let request = Request {
        id: "peer:workspaces".into(),
        method: Method::WorkspaceList(EmptyParams::default()),
    };
    let response = client
        .request_value_for_instance_with_timeout(&request, instance_id, REQUEST_TIMEOUT)
        .and_then(crate::api::client::parse_response_value)
        .map_err(|err| format!("peer workspace enumeration failed: {err}"))?;
    match response.result {
        ResponseResult::WorkspaceList { workspaces } => Ok(workspaces),
        result => Err(format!(
            "peer returned an unexpected workspace list result: {result:?}"
        )),
    }
}

fn enumerate_panes(client: &ApiClient, instance_id: &str) -> Result<Vec<PaneInfo>, String> {
    let request = Request {
        id: "peer:panes".into(),
        // Every workspace: a view can be onto any pane the peer has, and asking
        // per workspace would cost one round trip per workspace to learn the
        // same thing.
        method: Method::PaneList(PaneListParams { workspace_id: None }),
    };
    let response = client
        .request_value_for_instance_with_timeout(&request, instance_id, REQUEST_TIMEOUT)
        .and_then(crate::api::client::parse_response_value)
        .map_err(|err| format!("peer pane enumeration failed: {err}"))?;
    match response.result {
        ResponseResult::PaneList { panes } => Ok(panes),
        result => Err(format!(
            "peer returned an unexpected pane list result: {result:?}"
        )),
    }
}

/// Whether an event from the peer invalidates its last workspace enumeration.
///
/// Only whole-session events subscribed above can appear here, but the stream
/// is untyped, so anything unrecognized is ignored rather than trusted.
fn affects_workspaces(event: &serde_json::Value) -> bool {
    let Some(kind) = event.get("event") else {
        return false;
    };
    matches!(
        serde_json::from_value::<EventKind>(kind.clone()),
        Ok(EventKind::WorkspaceCreated
            | EventKind::WorkspaceUpdated
            | EventKind::WorkspaceRenamed
            | EventKind::WorkspaceClosed
            | EventKind::WorkspaceFocused
            | EventKind::WorkspaceMetadataUpdated)
    )
}

/// Whether an event from the peer invalidates its last pane enumeration.
///
/// `pane.agent_status_changed` is deliberately absent: it cannot be subscribed
/// to without naming a pane, so status is followed by the poll instead.
fn affects_panes(event: &serde_json::Value) -> bool {
    let Some(kind) = event.get("event") else {
        return false;
    };
    matches!(
        serde_json::from_value::<EventKind>(kind.clone()),
        Ok(EventKind::PaneCreated
            | EventKind::PaneClosed
            | EventKind::PaneUpdated
            | EventKind::PaneExited
            | EventKind::PaneAgentDetected)
    )
}

/// Hands an event to the server, waiting for room only while this connection is
/// still meant to be running.
///
/// This thread is joined during shutdown, and the join runs on the very event
/// loop that drains this channel. `blocking_send` on a full channel would
/// therefore wait for a drain that cannot happen until this thread returns, and
/// the two would hold each other there: the server would stay alive with its
/// session already reported stopped, and the peer bridge's sockets — removed by
/// `Drop`, and only reached once the process unwinds — would outlive it too.
///
/// Waiting in slices keeps delivery intact whenever the server is draining
/// normally, and gives up the moment it is not.
fn send_event(event_tx: &mpsc::Sender<AppEvent>, running: &AtomicBool, event: AppEvent) {
    let mut pending = event;
    loop {
        match event_tx.try_send(pending) {
            Ok(()) => return,
            // Nothing will ever drain it again.
            Err(mpsc::error::TrySendError::Closed(_)) => return,
            Err(mpsc::error::TrySendError::Full(returned)) => {
                if !running.load(Ordering::Relaxed) {
                    return;
                }
                pending = returned;
                std::thread::sleep(EVENT_SEND_POLL);
            }
        }
    }
}

fn send(
    event_tx: &mpsc::Sender<AppEvent>,
    running: &AtomicBool,
    handle: &PeerHandle,
    connection: PeerConnectionState,
    identity: Option<PeerIdentity>,
) {
    send_event(
        event_tx,
        running,
        AppEvent::PeerConnectionChanged {
            handle: handle.clone(),
            connection,
            identity,
        },
    );
}

fn send_workspaces(
    event_tx: &mpsc::Sender<AppEvent>,
    running: &AtomicBool,
    handle: &PeerHandle,
    workspaces: Vec<WorkspaceInfo>,
) {
    send_event(
        event_tx,
        running,
        AppEvent::PeerWorkspacesUpdated {
            handle: handle.clone(),
            workspaces,
        },
    );
}

fn send_panes(
    event_tx: &mpsc::Sender<AppEvent>,
    running: &AtomicBool,
    handle: &PeerHandle,
    panes: Vec<PaneInfo>,
) {
    send_event(
        event_tx,
        running,
        AppEvent::PeerPanesUpdated {
            handle: handle.clone(),
            panes,
        },
    );
}

fn backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    RECONNECT_BACKOFF_MIN
        .saturating_mul(1u32 << shift)
        .min(RECONNECT_BACKOFF_MAX)
}

/// Sleeps in short slices so shutdown is not delayed by a long backoff.
/// Returns false when shutdown was requested.
fn sleep_until_stopped(total: Duration, running: &Arc<AtomicBool>) -> bool {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if !running.load(Ordering::Relaxed) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(remaining.min(STREAM_READ_TIMEOUT));
    }
    running.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_saturates() {
        assert_eq!(backoff(1), RECONNECT_BACKOFF_MIN);
        assert_eq!(backoff(2), RECONNECT_BACKOFF_MIN * 2);
        assert_eq!(backoff(3), RECONNECT_BACKOFF_MIN * 4);
        assert_eq!(backoff(50), RECONNECT_BACKOFF_MAX);
        assert!(backoff(u32::MAX) <= RECONNECT_BACKOFF_MAX);
    }

    /// Backoff measures *consecutive* failures. `attempt` used to be declared
    /// outside the loop and only ever incremented, so a peer that flapped at
    /// startup and then ran healthily for a day still waited the full 10s cap on
    /// its next blip — and reported that first failure as attempt 6.
    #[test]
    fn a_proven_session_starts_the_backoff_curve_over() {
        let mut attempt: u32 = 0;

        // Two failures before anything connects.
        attempt = attempt.saturating_add(1);
        attempt = attempt.saturating_add(1);
        assert_eq!(backoff(attempt), RECONNECT_BACKOFF_MIN * 2);

        // A session that identified and subscribed.
        attempt = 0;

        // The next failure is the first of a new run, not the third of the old.
        attempt = attempt.saturating_add(1);
        assert_eq!(attempt, 1);
        assert_eq!(backoff(attempt), RECONNECT_BACKOFF_MIN);
    }

    #[test]
    fn workspace_events_trigger_re_enumeration() {
        for kind in [
            "workspace_created",
            "workspace_closed",
            "workspace_renamed",
            "workspace_focused",
        ] {
            let event = serde_json::json!({ "event": kind, "data": {} });
            assert!(affects_workspaces(&event), "{kind} must re-enumerate");
        }
    }

    #[test]
    fn unrelated_or_malformed_events_do_not_re_enumerate() {
        for event in [
            serde_json::json!({ "event": "pane.scroll_changed", "data": {} }),
            serde_json::json!({ "event": "tab_created", "data": {} }),
            serde_json::json!({ "event": 7 }),
            serde_json::json!({ "data": {} }),
        ] {
            assert!(!affects_workspaces(&event), "{event} must not re-enumerate");
        }
    }

    /// The join that stops this thread runs on the loop that drains the
    /// channel, so a send that waited for room on a stopped server would wait
    /// on itself.
    #[test]
    fn a_send_gives_up_on_a_full_channel_once_stopped() {
        let (event_tx, _events) = mpsc::channel(1);
        let handle = PeerHandle::new("alpha".to_string());
        let running = AtomicBool::new(true);

        send(
            &event_tx,
            &running,
            &handle,
            PeerConnectionState::Connecting,
            None,
        );
        running.store(false, Ordering::Relaxed);

        let started = Instant::now();
        send(
            &event_tx,
            &running,
            &handle,
            PeerConnectionState::Connected,
            None,
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// Giving up is only for a server that stopped draining. While it is still
    /// running, a send waits for room rather than dropping peer state.
    #[test]
    fn a_send_waits_for_room_while_running() {
        let (event_tx, mut events) = mpsc::channel(1);
        let handle = PeerHandle::new("alpha".to_string());
        let running = Arc::new(AtomicBool::new(true));

        send(
            &event_tx,
            &running,
            &handle,
            PeerConnectionState::Connecting,
            None,
        );

        let sender_running = Arc::clone(&running);
        let sender = std::thread::spawn(move || {
            send(
                &event_tx,
                &sender_running,
                &handle,
                PeerConnectionState::Connected,
                None,
            );
        });

        // Nothing can land until the first event is taken.
        std::thread::sleep(EVENT_SEND_POLL * 2);
        let first = events.try_recv().expect("first event queued");
        assert!(matches!(
            first,
            AppEvent::PeerConnectionChanged {
                connection: PeerConnectionState::Connecting,
                ..
            }
        ));

        sender.join().expect("sender finishes");
        let second = events.try_recv().expect("second event delivered");
        assert!(matches!(
            second,
            AppEvent::PeerConnectionChanged {
                connection: PeerConnectionState::Connected,
                ..
            }
        ));
    }

    /// A receiver nobody holds is not worth waiting on at all.
    #[test]
    fn a_send_returns_when_the_channel_is_closed() {
        let (event_tx, events) = mpsc::channel(1);
        let handle = PeerHandle::new("alpha".to_string());
        let running = AtomicBool::new(true);
        drop(events);

        let started = Instant::now();
        send(
            &event_tx,
            &running,
            &handle,
            PeerConnectionState::Connecting,
            None,
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn sleep_until_stopped_returns_early_when_stopped() {
        let running = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        assert!(!sleep_until_stopped(Duration::from_secs(30), &running));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn socket_path_transport_opens_without_ssh() {
        let (event_tx, mut events) = mpsc::channel(4);
        let handle = PeerHandle::new("alpha".to_string());
        let target = PeerTarget::SocketPath(PathBuf::from("/tmp/peer-alpha.sock"));
        let mut transport = None;

        let running = Arc::new(AtomicBool::new(true));
        let socket = ensure_transport(&handle, &target, &mut transport, &event_tx, &running)
            .expect("socket path transport opens");

        assert_eq!(socket, PathBuf::from("/tmp/peer-alpha.sock"));
        // A local target needs no announcement: it is already the socket to use.
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn an_open_transport_is_reused_across_sessions() {
        let (event_tx, _events) = mpsc::channel(4);
        let handle = PeerHandle::new("alpha".to_string());
        let target = PeerTarget::SocketPath(PathBuf::from("/tmp/peer-alpha.sock"));
        let mut transport = Some(Transport::Direct(PathBuf::from("/tmp/already-open.sock")));

        let running = Arc::new(AtomicBool::new(true));
        let socket = ensure_transport(&handle, &target, &mut transport, &event_tx, &running)
            .expect("open transport is reused");

        // Reconnecting must not re-derive the transport: peer-backed terminals
        // are already connected through the one that is open.
        assert_eq!(socket, PathBuf::from("/tmp/already-open.sock"));
    }
}
