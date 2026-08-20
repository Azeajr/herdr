use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;
use serde::de::DeserializeOwned;

use crate::api::schema::{
    ErrorResponse, Method, PingParams, Request, ResponseResult, SuccessResponse,
};
use crate::ipc::LocalStream;

/// API connection target resolved by clients at the process edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionTarget {
    LocalSession(Option<String>),
    SocketPath(PathBuf),
}

impl ConnectionTarget {
    fn socket_path(&self) -> PathBuf {
        match self {
            Self::LocalSession(None) => crate::api::socket_path(),
            Self::LocalSession(Some(name)) => crate::session::api_socket_path_for(Some(name)),
            Self::SocketPath(path) => path.clone(),
        }
    }
}

/// Reusable client for Herdr's newline-delimited JSON API.
#[derive(Debug, Clone)]
pub struct ApiClient {
    target: ConnectionTarget,
}

impl ApiClient {
    pub fn local() -> Self {
        Self::for_target(ConnectionTarget::LocalSession(None))
    }

    pub fn for_target(target: ConnectionTarget) -> Self {
        Self { target }
    }

    pub fn socket_path(&self) -> PathBuf {
        self.target.socket_path()
    }

    pub fn request(&self, request: Request) -> Result<SuccessResponse, ApiClientError> {
        let value = self.request_value(&request)?;
        parse_response_value(value)
    }

    pub fn request_value(&self, request: &Request) -> Result<serde_json::Value, ApiClientError> {
        let mut stream = self.connect()?;
        write_request(&mut stream, request, None)?;

        let mut reader = BufReader::new(stream);
        read_json_line(&mut reader)
    }

    pub fn request_value_with_timeout(
        &self,
        request: &Request,
        timeout: Duration,
    ) -> Result<serde_json::Value, ApiClientError> {
        let mut stream = self.connect()?;
        set_timeout_best_effort(&stream, TimeoutKind::Send, timeout)?;
        set_timeout_best_effort(&stream, TimeoutKind::Recv, timeout)?;
        write_request(&mut stream, request, None)?;

        let mut reader = BufReader::new(stream);
        read_json_line(&mut reader)
    }

    /// Sends a request only if the socket still belongs to `instance_id`.
    pub fn request_value_for_instance_with_timeout(
        &self,
        request: &Request,
        instance_id: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, ApiClientError> {
        let mut stream = self.connect()?;
        set_timeout_best_effort(&stream, TimeoutKind::Send, timeout)?;
        set_timeout_best_effort(&stream, TimeoutKind::Recv, timeout)?;
        write_request(&mut stream, request, Some(instance_id))?;

        let mut reader = BufReader::new(stream);
        read_json_line(&mut reader)
    }

    pub fn status(&self) -> Result<crate::api::RuntimeStatus, ApiClientError> {
        let response = self.request(Request {
            id: "api-client:status".into(),
            method: Method::Ping(PingParams::default()),
        })?;
        match response.result {
            ResponseResult::Pong {
                version,
                protocol,
                capabilities,
                instance_id,
            } => Ok(crate::api::RuntimeStatus {
                version: Some(version),
                protocol: Some(protocol),
                capabilities,
                instance_id,
            }),
            result => Err(ApiClientError::UnexpectedResult(format!("{result:?}"))),
        }
    }

    /// Opens a long-lived `events.subscribe` stream.
    ///
    /// Unlike the one-shot request path, the server keeps this connection open
    /// and pushes one JSON line per event, so the reader must outlive the
    /// initial response.
    ///
    /// The two timeouts mean different things and must not be given the same
    /// value. `start_timeout` is a deadline for the `subscription_started`
    /// reply, which is an ordinary round trip: it has to cover whatever the
    /// transport costs, and over an ssh bridge that includes standing up an ssh
    /// session. `read_timeout` bounds every read after it and is a *poll
    /// interval*, not a deadline — its expiry surfaces as `Ok(None)` from
    /// [`SubscriptionStream::next_event`] so a caller can check for shutdown
    /// between events.
    ///
    /// Passing the poll interval for both is what this signature exists to
    /// prevent: it turns a sub-second recheck period into the handshake's
    /// deadline, and a peer one round trip further away than the loopback then
    /// fails to subscribe on every single attempt.
    #[cfg(test)]
    pub fn subscribe(
        &self,
        request: Request,
        start_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<SubscriptionStream, ApiClientError> {
        self.subscribe_inner(request, None, start_timeout, read_timeout)
    }

    /// Opens a subscription only if the socket still belongs to `instance_id`.
    pub fn subscribe_for_instance(
        &self,
        request: Request,
        instance_id: &str,
        start_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<SubscriptionStream, ApiClientError> {
        self.subscribe_inner(request, Some(instance_id), start_timeout, read_timeout)
    }

    fn subscribe_inner(
        &self,
        request: Request,
        instance_id: Option<&str>,
        start_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<SubscriptionStream, ApiClientError> {
        let mut stream = self.connect()?;
        set_timeout_best_effort(&stream, TimeoutKind::Send, start_timeout)?;
        write_request(&mut stream, &request, instance_id)?;
        // Applied after the request so a slow first response does not race the
        // write.
        set_timeout_best_effort(&stream, TimeoutKind::Recv, start_timeout)?;

        let mut reader = BufReader::new(stream);
        let started: serde_json::Value = read_json_line(&mut reader)?;
        match parse_response_value(started)?.result {
            ResponseResult::SubscriptionStarted {} => {
                // Only once the stream is established does the timeout become a
                // poll interval.
                set_timeout_best_effort(reader.get_ref(), TimeoutKind::Recv, read_timeout)?;
                Ok(SubscriptionStream { reader })
            }
            result => Err(ApiClientError::UnexpectedResult(format!("{result:?}"))),
        }
    }

    fn connect(&self) -> io::Result<LocalStream> {
        crate::ipc::connect_local_stream(&self.socket_path())
    }
}

/// A live `events.subscribe` connection.
pub struct SubscriptionStream {
    reader: BufReader<LocalStream>,
}

impl SubscriptionStream {
    /// Reads the next event.
    ///
    /// The stream is heterogeneous: whole-session subscriptions emit
    /// `EventEnvelope` (snake_case `EventKind`) while pane subscriptions emit
    /// `SubscriptionEventEnvelope` (dotted names). Callers get the raw value
    /// and pick the shape they care about.
    ///
    /// `Ok(None)` means no event arrived before the read timeout — the stream
    /// is still healthy. A closed stream surfaces as
    /// [`ApiClientError::EmptyResponse`].
    pub fn next_event(&mut self) -> Result<Option<serde_json::Value>, ApiClientError> {
        match read_json_line::<serde_json::Value>(&mut self.reader) {
            Ok(event) => Ok(Some(event)),
            Err(ApiClientError::Io(err)) if is_read_timeout(&err) => Ok(None),
            Err(err) => Err(err),
        }
    }
}

fn is_read_timeout(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

enum TimeoutKind {
    Send,
    Recv,
}

fn set_timeout_best_effort(
    stream: &LocalStream,
    kind: TimeoutKind,
    timeout: Duration,
) -> io::Result<()> {
    let result = match kind {
        TimeoutKind::Send => stream.set_send_timeout(Some(timeout)),
        TimeoutKind::Recv => stream.set_recv_timeout(Some(timeout)),
    };
    match result {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(err) if err.kind() == io::ErrorKind::Unsupported => Ok(()),
        Err(err) => Err(err),
    }
}

#[derive(Debug)]
pub enum ApiClientError {
    Io(io::Error),
    Json(serde_json::Error),
    ErrorResponse(ErrorResponse),
    EmptyResponse,
    UnexpectedResult(String),
}

impl fmt::Display for ApiClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::ErrorResponse(response) => write!(f, "{}", response.error.message),
            Self::EmptyResponse => write!(f, "empty api response"),
            Self::UnexpectedResult(result) => write!(f, "unexpected api result: {result}"),
        }
    }
}

impl std::error::Error for ApiClientError {}

impl From<io::Error> for ApiClientError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<serde_json::Error> for ApiClientError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

fn write_request(
    stream: &mut LocalStream,
    request: &Request,
    if_instance_id: Option<&str>,
) -> Result<(), ApiClientError> {
    #[derive(serde::Serialize)]
    struct BorrowedRequestEnvelope<'a> {
        #[serde(flatten)]
        request: &'a Request,
        #[serde(skip_serializing_if = "Option::is_none")]
        if_instance_id: Option<&'a str>,
    }

    let envelope = BorrowedRequestEnvelope {
        request,
        if_instance_id,
    };
    stream.write_all(serde_json::to_string(&envelope)?.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn read_json_line<T: DeserializeOwned>(
    reader: &mut BufReader<LocalStream>,
) -> Result<T, ApiClientError> {
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
    if read == 0 || line.trim().is_empty() {
        return Err(ApiClientError::EmptyResponse);
    }
    serde_json::from_str(&line).map_err(ApiClientError::Json)
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum WireResponse {
    Success(Box<SuccessResponse>),
    Error(ErrorResponse),
}

pub(crate) fn parse_response_value(
    value: serde_json::Value,
) -> Result<SuccessResponse, ApiClientError> {
    match serde_json::from_value(value)? {
        WireResponse::Success(response) => Ok(*response),
        WireResponse::Error(response) => Err(ApiClientError::ErrorResponse(response)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_session_target_resolves_named_session_socket() {
        let client = ApiClient::for_target(ConnectionTarget::LocalSession(Some("work".into())));
        assert!(client.socket_path().ends_with("sessions/work/herdr.sock"));
    }

    #[test]
    fn socket_path_target_uses_explicit_path() {
        let path = PathBuf::from("/tmp/herdr-test.sock");
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        assert_eq!(client.socket_path(), path);
    }

    /// A server that answers `events.subscribe` only after `reply_after`, so a
    /// test can put a round trip's worth of latency in front of the handshake
    /// the way an ssh bridge does.
    #[cfg(unix)]
    struct SlowSubscribeServer {
        path: PathBuf,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    #[cfg(unix)]
    impl SlowSubscribeServer {
        fn start(name: &str, reply_after: Duration) -> Self {
            use std::os::unix::net::UnixListener;

            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.subsec_nanos())
                .unwrap_or_default();
            let path = std::env::temp_dir()
                .join(format!("herdr-{name}-{}-{nanos}.sock", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).expect("bind test socket");
            let thread = std::thread::spawn(move || {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                std::thread::sleep(reply_after);
                let started = serde_json::to_string(&SuccessResponse {
                    id: "test:subscribe".into(),
                    result: ResponseResult::SubscriptionStarted {},
                })
                .expect("serialize the subscription reply");
                let _ = writeln!(stream, "{started}");
                let _ = stream.flush();
                // Held open so the client's post-handshake read blocks on an
                // idle stream rather than seeing a closed one.
                std::thread::sleep(Duration::from_millis(500));
            });
            Self {
                path,
                thread: Some(thread),
            }
        }

        fn client(&self) -> ApiClient {
            ApiClient::for_target(ConnectionTarget::SocketPath(self.path.clone()))
        }
    }

    #[cfg(unix)]
    impl Drop for SlowSubscribeServer {
        fn drop(&mut self) {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[cfg(unix)]
    fn subscribe_request() -> Request {
        Request {
            id: "test:subscribe".into(),
            method: Method::EventsSubscribe(crate::api::schema::EventsSubscribeParams {
                subscriptions: vec![crate::api::schema::Subscription::WorkspaceCreated {}],
            }),
        }
    }

    /// The regression this signature exists for: a peer whose transport costs
    /// more than the stream's recheck period must still be able to subscribe.
    #[cfg(unix)]
    #[test]
    fn a_slow_subscription_reply_is_bounded_by_the_start_timeout() {
        let server = SlowSubscribeServer::start("subscribe-slow", Duration::from_millis(300));
        let mut stream = server
            .client()
            .subscribe(
                subscribe_request(),
                Duration::from_secs(5),
                Duration::from_millis(50),
            )
            .expect("a reply slower than the poll interval must still open the stream");

        // And the poll interval took over afterwards: no event is coming, and
        // that has to read as "not yet" rather than as a broken stream.
        assert!(matches!(stream.next_event(), Ok(None)));
    }

    /// Pins the other half: the poll interval really is too short to be the
    /// handshake's deadline, which is why the two are separate arguments.
    #[cfg(unix)]
    #[test]
    fn the_poll_interval_alone_would_not_survive_the_handshake() {
        let server = SlowSubscribeServer::start("subscribe-strict", Duration::from_millis(300));
        match server.client().subscribe(
            subscribe_request(),
            Duration::from_millis(50),
            Duration::from_millis(50),
        ) {
            Err(ApiClientError::Io(err)) => assert!(
                is_read_timeout(&err),
                "expected the handshake to time out, got {err:?}"
            ),
            Err(other) => panic!("expected a timed-out handshake, got {other:?}"),
            Ok(_) => panic!("a 50ms deadline must not survive a 300ms reply"),
        }
    }
}
