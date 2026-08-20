//! Remote thin-client launcher over SSH command stdio.

use super::shell_quote;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use interprocess::local_socket::traits::Listener as _;
#[cfg(windows)]
use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::ListenerNonblockingMode;
use interprocess::TryClone as _;
use serde::Deserialize;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const BRIDGE_ACCEPT_POLL: Duration = Duration::from_millis(50);
#[cfg(windows)]
const BRIDGE_IO_POLL: Duration = Duration::from_millis(1);
const BRIDGE_SOCKET_PERMISSION_MODE: u32 = 0o600;
const REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);
const REMOTE_SERVER_SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CURRENT_PROTOCOL: u32 = crate::protocol::PROTOCOL_VERSION;
const STABLE_UPDATE_MANIFEST_URL: &str = "https://herdr.dev/latest.json";
const PREVIEW_UPDATE_MANIFEST_URL: &str = "https://herdr.dev/preview.json";
const REMOTE_BINARY_ENV_VAR: &str = "HERDR_REMOTE_BINARY";
const SSH_CONTROL_SOCKET_NAME: &str = "ctl";
#[cfg(unix)]
const PEER_SSH_KEY_NAME: &str = "peer_id_ed25519";
#[cfg(unix)]
const PEER_SSH_KEY_COMMENT_PREFIX: &str = "herdr-peer";
#[cfg(unix)]
/// The comment every herdr release before per-client comments wrote.
const PEER_SSH_LEGACY_KEY_COMMENT: &str = "herdr-peer";
#[cfg(unix)]
const PEER_SSH_IDENTITY_COMPONENT_LIMIT: usize = 64;
/// How long ssh may spend on the TCP connect to a peer.
///
/// Without it ssh inherits the OS default, which on Linux is roughly two
/// minutes of SYN retries. A peer is dialed by a server that reconnects on its
/// own and is joined during shutdown, so waiting out the kernel there is never
/// the right answer — the host is either up or worth retrying later.
const PEER_SSH_CONNECT_TIMEOUT_SECS: u32 = 10;
/// How often an interruptible ssh wait rechecks whether it should still wait.
const PEER_SSH_CANCEL_POLL: Duration = Duration::from_millis(100);
pub(crate) const REATTACH_COMMAND_ENV_VAR: &str = "HERDR_REATTACH_COMMAND";

pub(crate) const REMOTE_KEYBINDINGS_ENV_VAR: &str = "HERDR_REMOTE_KEYBINDINGS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteKeybindings {
    Local,
    Server,
}

impl RemoteKeybindings {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "local" => Ok(Self::Local),
            "server" => Ok(Self::Server),
            _ => Err("--remote-keybindings must be 'local' or 'server'".to_string()),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Server => "server",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteLaunch {
    pub(crate) target: String,
    pub(crate) keybindings: RemoteKeybindings,
    pub(crate) live_handoff: bool,
}

pub(crate) fn extract_remote_args(
    args: &[String],
) -> Result<(Vec<String>, Option<RemoteLaunch>), String> {
    let mut cleaned = Vec::with_capacity(args.len());
    if let Some(program) = args.first() {
        cleaned.push(program.clone());
    }

    let mut remote_target = None;
    let mut keybindings = RemoteKeybindings::Local;
    let mut keybindings_seen = false;
    let mut live_handoff = false;
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            cleaned.extend_from_slice(&args[index..]);
            break;
        }
        if arg == "--handoff" {
            live_handoff = true;
            index += 1;
            continue;
        }
        if arg == "--remote" {
            if remote_target.is_some() {
                return Err("--remote can only be specified once".to_string());
            }
            let Some(value) = args.get(index + 1) else {
                return Err("missing value for --remote".to_string());
            };
            remote_target = Some(validate_remote_target(value)?.to_owned());
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--remote=") {
            if remote_target.is_some() {
                return Err("--remote can only be specified once".to_string());
            }
            remote_target = Some(validate_remote_target(value)?.to_owned());
            index += 1;
            continue;
        }
        if arg == "--remote-keybindings" {
            if keybindings_seen {
                return Err("--remote-keybindings can only be specified once".to_string());
            }
            let Some(value) = args.get(index + 1) else {
                return Err("missing value for --remote-keybindings".to_string());
            };
            keybindings = RemoteKeybindings::parse(value)?;
            keybindings_seen = true;
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--remote-keybindings=") {
            if keybindings_seen {
                return Err("--remote-keybindings can only be specified once".to_string());
            }
            keybindings = RemoteKeybindings::parse(value)?;
            keybindings_seen = true;
            index += 1;
            continue;
        }

        cleaned.push(arg.clone());
        index += 1;
    }

    let remote = remote_target.map(|target| RemoteLaunch {
        target,
        keybindings,
        live_handoff,
    });
    if remote.is_none() && keybindings_seen {
        return Err("--remote-keybindings requires --remote".to_string());
    }
    if remote.is_none() && live_handoff {
        cleaned.push("--handoff".to_string());
    }

    Ok((cleaned, remote))
}

fn validate_remote_target(target: &str) -> Result<&str, String> {
    if target.is_empty() {
        return Err("missing value for --remote".to_string());
    }
    if target.starts_with('-') {
        return Err("--remote target must not start with '-'".to_string());
    }
    Ok(target)
}

pub(crate) fn run_remote(remote: RemoteLaunch) -> io::Result<()> {
    let session_name = crate::session::active_name()
        .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string());
    let local_socket = local_forward_socket_path(&remote.target, &session_name);
    let program = std::env::args()
        .next()
        .unwrap_or_else(|| "herdr".to_string());
    let reattach_command = reattach_command(
        &program,
        &remote.target,
        &session_name,
        remote.keybindings,
        remote.live_handoff,
    );
    let manage_ssh_config = crate::config::Config::load()
        .config
        .remote
        .manage_ssh_config;
    let remote_ssh = RemoteSsh::new(remote.target.clone(), manage_ssh_config);
    let prepared_remote = prepare_remote_herdr(&remote_ssh, remote.live_handoff)?;
    ensure_remote_server_ready(
        &remote_ssh,
        &prepared_remote.remote_herdr,
        prepared_remote.installed_or_replaced,
        prepared_remote.stop_after_install_approved,
        remote.live_handoff,
    )?;

    let _bridge = SshStdioBridge::start(
        remote.target,
        prepared_remote.remote_herdr,
        local_socket.clone(),
        session_name,
        BridgeSocket::Client,
        remote_ssh.options(),
    )?;

    run_client_process(&local_socket, &reattach_command, remote.keybindings)
}

/// Which of a server's two sockets a bridge carries.
///
/// Both are needed to federate with a remote server: the JSON API socket for
/// identification, workspace enumeration and events, and the client protocol
/// socket for terminal frames. They are separate listeners, so each needs its
/// own bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BridgeSocket {
    /// The client protocol socket.
    Client,
    /// The newline-delimited JSON API socket.
    // Peer federation is Unix-only for now: the bridge that opens this socket
    // and the remote host side that serves it both live behind `cfg(unix)`.
    #[cfg_attr(windows, allow(dead_code))]
    Api,
}

impl BridgeSocket {
    fn subcommand(self) -> &'static str {
        match self {
            Self::Client => "remote-client-bridge",
            Self::Api => "remote-api-bridge",
        }
    }

    // Only the Unix remote host side reads these; see `Api` above.
    #[cfg_attr(windows, allow(dead_code))]
    pub(super) fn local_path(self) -> PathBuf {
        match self {
            Self::Client => crate::server::socket_paths::client_socket_path(),
            Self::Api => crate::api::socket_path(),
        }
    }

    #[cfg_attr(windows, allow(dead_code))]
    pub(super) fn describe(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Api => "API",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemotePlatform {
    os: &'static str,
    arch: &'static str,
}

impl RemotePlatform {
    fn from_uname(os: &str, arch: &str) -> Option<Self> {
        let os = match os.trim() {
            "Linux" => "linux",
            "Darwin" => "macos",
            _ => return None,
        };
        let arch = match arch.trim() {
            "x86_64" | "amd64" => "x86_64",
            "aarch64" | "arm64" => "aarch64",
            _ => return None,
        };
        Some(Self { os, arch })
    }

    fn local() -> Self {
        let os = if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else {
            "unknown"
        };

        let arch = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            "unknown"
        };

        Self { os, arch }
    }

    fn asset_key(&self) -> String {
        format!("{}-{}", self.os, self.arch)
    }
}

#[derive(Debug, Clone)]
struct RemoteHerdr {
    install_suffix: String,
    shell_path: String,
    platform: RemotePlatform,
}

impl RemoteHerdr {
    fn for_platform(platform: RemotePlatform) -> Self {
        let install_suffix = ".local/bin/herdr".to_string();
        let shell_path = format!("\"$HOME/{install_suffix}\"");
        Self {
            install_suffix,
            shell_path,
            platform,
        }
    }

    fn with_shell_path(mut self, shell_path: String) -> Self {
        self.shell_path = shell_path;
        self
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RemoteAssetRef {
    Url(String),
    Object { url: String, sha256: Option<String> },
}

impl RemoteAssetRef {
    fn url(&self) -> &str {
        match self {
            Self::Url(url) => url,
            Self::Object { url, .. } => url,
        }
    }

    fn sha256(&self) -> Option<&str> {
        match self {
            Self::Url(_) => None,
            Self::Object { sha256, .. } => {
                sha256.as_deref().filter(|value| !value.trim().is_empty())
            }
        }
    }
}

#[derive(Deserialize)]
struct RemoteUpdateManifest {
    version: String,
    protocol: Option<u32>,
    assets: BTreeMap<String, RemoteAssetRef>,
    #[serde(default)]
    sha256: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "deserialize_remote_manifest_releases")]
    releases: BTreeMap<String, RemoteReleaseMetadata>,
}

#[derive(Deserialize)]
struct RemoteReleaseMetadata {
    protocol: Option<u32>,
    #[serde(default)]
    assets: BTreeMap<String, RemoteAssetRef>,
    #[serde(default)]
    sha256: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct RemotePreviewManifest {
    build_id: String,
    protocol: u32,
    assets: BTreeMap<String, RemoteAssetRef>,
    #[serde(default)]
    builds: BTreeMap<String, RemotePreviewBuildMetadata>,
}

#[derive(Deserialize)]
struct RemotePreviewBuildMetadata {
    protocol: u32,
    assets: BTreeMap<String, RemoteAssetRef>,
}

fn deserialize_remote_manifest_releases<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, RemoteReleaseMetadata>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::Object(object)) => object
            .into_iter()
            .filter_map(|(version, release)| {
                serde_json::from_value::<RemoteReleaseMetadata>(release)
                    .ok()
                    .map(|metadata| (version, metadata))
            })
            .collect(),
        _ => BTreeMap::new(),
    })
}

impl RemoteUpdateManifest {
    fn release_for_version(&self, version: &str) -> Option<RemoteManifestReleaseRef<'_>> {
        if self.version.trim_start_matches('v') == version {
            return Some(RemoteManifestReleaseRef {
                protocol: self.protocol,
                assets: &self.assets,
                sha256: &self.sha256,
            });
        }

        self.releases.get(version).and_then(|release| {
            (!release.assets.is_empty()).then_some(RemoteManifestReleaseRef {
                protocol: release.protocol,
                assets: &release.assets,
                sha256: &release.sha256,
            })
        })
    }
}

#[derive(Clone, Copy)]
struct RemoteManifestReleaseRef<'a> {
    protocol: Option<u32>,
    assets: &'a BTreeMap<String, RemoteAssetRef>,
    sha256: &'a BTreeMap<String, String>,
}

fn current_version() -> String {
    crate::build_info::version()
}

fn current_channel() -> &'static str {
    crate::build_info::channel()
}

struct InstallSource {
    path: PathBuf,
    temporary_dir: Option<PathBuf>,
}

struct RemoteReleaseAsset {
    url: String,
    sha256: Option<String>,
}

struct PreparedRemoteHerdr {
    remote_herdr: RemoteHerdr,
    installed_or_replaced: bool,
    stop_after_install_approved: bool,
}

#[derive(Clone)]
struct ManagedSshOptions {
    config_path: PathBuf,
    control_path: Option<PathBuf>,
}

struct ManagedSshConfig {
    options: ManagedSshOptions,
}

impl Drop for ManagedSshConfig {
    fn drop(&mut self) {
        if let Some(dir) = self.options.config_path.parent() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

struct RemoteSsh {
    target: String,
    managed_config: Option<ManagedSshConfig>,
    /// Extra identity to offer, used only by the peer path.
    ///
    /// Passed as `-i` rather than written into the managed ssh config so it
    /// still applies when `remote.manage_ssh_config` is off and there is no
    /// managed config to write into.
    identity: Option<PathBuf>,
    /// Refuse anything interactive, used only by the peer path.
    ///
    /// A peer is dialed by the server, which has no terminal, so a prompt there
    /// can only fail — slowly, after ssh exhausts its password attempts, and
    /// with three copies of the same refusal in the reported error. Declaring
    /// that up front turns it into one clean failure that
    /// [`ssh_failed_to_authenticate`] can classify.
    batch: bool,
    /// Whether the connection these commands serve is still wanted, used only
    /// by the peer path.
    ///
    /// A peer's thread is joined during shutdown, so every ssh it is parked on
    /// has to be abandonable. [`PEER_SSH_CONNECT_TIMEOUT_SECS`] bounds the
    /// common case, but it covers the TCP connect alone — a host that answers
    /// and then stalls in the handshake is bounded by nothing. Set, the waits
    /// below kill their child the moment this turns false; unset, they wait the
    /// way an interactive `herdr --remote` should.
    running: Option<Arc<AtomicBool>>,
}

impl RemoteSsh {
    fn new(target: String, manage_ssh_config: bool) -> Self {
        let managed_config = if manage_ssh_config {
            write_managed_ssh_config()
                .inspect_err(|err| {
                    tracing::debug!(%err, "could not write managed ssh config; using plain ssh");
                })
                .ok()
        } else {
            None
        };

        Self {
            target,
            managed_config,
            identity: None,
            batch: false,
            running: None,
        }
    }

    #[cfg(unix)]
    /// A session for reaching a peer server.
    ///
    /// Identical to [`Self::new`] except that it offers herdr's own peer key
    /// when one has been set up. The key is only offered when it exists, so a
    /// user whose existing agent or default key already works never has herdr's
    /// key pushed into their authentication attempts.
    fn for_peer(target: String, manage_ssh_config: bool) -> Self {
        let mut ssh = Self::new(target, manage_ssh_config);
        let key = peer_ssh_key_path();
        if key.is_file() {
            ssh.identity = Some(key);
        }
        ssh.batch = true;
        ssh
    }

    #[cfg(unix)]
    /// Ties these commands to the lifetime of a peer connection, so shutdown
    /// does not have to wait for ssh to notice the peer is gone.
    fn cancelled_with(mut self, running: Arc<AtomicBool>) -> Self {
        self.running = Some(running);
        self
    }

    fn target(&self) -> &str {
        &self.target
    }

    fn options(&self) -> Option<&ManagedSshOptions> {
        self.managed_config.as_ref().map(|config| &config.options)
    }

    fn command(&self) -> Command {
        let mut command = self.base_command();
        command.arg("-T").arg(&self.target);
        command
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new("ssh");
        apply_managed_ssh_options(&mut command, self.options());
        if let Some(identity) = &self.identity {
            command.arg("-i").arg(identity);
        }
        if self.batch {
            // Both restrictions belong to the peer path: it is dialed by a
            // headless server that cannot answer a prompt and must not sit on
            // an unreachable host. Passed on the command line rather than
            // written into the managed ssh config so they still apply when
            // `remote.manage_ssh_config` is off and there is no config to write.
            command
                .arg("-o")
                .arg("BatchMode=yes")
                .arg("-o")
                .arg(format!("ConnectTimeout={PEER_SSH_CONNECT_TIMEOUT_SECS}"));
        }
        command
    }

    fn sh_output(&self, script: &str) -> io::Result<Output> {
        let mut child = self
            .command()
            .arg("/bin/sh -s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let write_result = if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(script.as_bytes())
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ssh bootstrap stdin missing",
            ))
        };
        let output = self.wait_for_output(child)?;
        write_result?;
        Ok(output)
    }

    fn user_shell_output(&self, command: &str) -> io::Result<Output> {
        let child = self
            .command()
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        self.wait_for_output(child)
    }

    /// Collects `child`'s output, giving up on it if this connection stops
    /// being wanted while it runs.
    ///
    /// Only the peer path sets [`Self::running`]; everything else waits exactly
    /// as [`Child::wait_with_output`] does. The pipes are drained on their own
    /// threads even while polling, because a child that fills a pipe buffer
    /// blocks until someone reads it — polling for exit without reading would
    /// reintroduce the hang from the other end.
    fn wait_for_output(&self, mut child: Child) -> io::Result<Output> {
        let Some(running) = self.running.as_ref() else {
            return child.wait_with_output();
        };

        let stdout = child.stdout.take().map(spawn_pipe_reader);
        let stderr = child.stderr.take().map(spawn_pipe_reader);

        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if !running.load(Ordering::Relaxed) {
                tracing::debug!(target = %self.target, "abandoning ssh command; peer connection stopped");
                // A kill can only fail because the child already exited, which
                // is the outcome being asked for either way.
                let _ = child.kill();
                break child.wait()?;
            }
            thread::sleep(PEER_SSH_CANCEL_POLL);
        };

        // The readers end when their pipe closes, which the kill above
        // guarantees. A panicked reader costs its stream, not the wait.
        let collect = |reader: Option<JoinHandle<Vec<u8>>>| {
            reader
                .map(|reader| reader.join().unwrap_or_default())
                .unwrap_or_default()
        };
        Ok(Output {
            status,
            stdout: collect(stdout),
            stderr: collect(stderr),
        })
    }

    fn install_herdr(&self, remote_herdr: &RemoteHerdr, source_path: &Path) -> io::Result<()> {
        let output = self.sh_output(&remote_install_prepare_script(remote_herdr))?;
        if !output.status.success() {
            return Err(command_failed("remote install preparation failed", &output));
        }
        let (tmp_path, dest_path) = parse_remote_install_paths(&output.stdout)?;

        let mut child = self
            .command()
            .arg(remote_install_stream_command(&tmp_path))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|err| {
                io::Error::new(err.kind(), format!("failed to start ssh install: {err}"))
            })?;

        let mut source = File::open(source_path)?;
        let copy_result = if let Some(mut stdin) = child.stdin.take() {
            io::copy(&mut source, &mut stdin).map(|_| ())
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ssh install stdin missing",
            ))
        };
        let status = child.wait()?;
        copy_result?;

        if status.success() {
            let output = self.sh_output(&remote_install_commit_script(&tmp_path, &dest_path))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(command_failed("remote install commit failed", &output))
            }
        } else {
            Err(io::Error::other(format!(
                "remote install exited with {status}"
            )))
        }
    }
}

fn remote_install_prepare_script(remote_herdr: &RemoteHerdr) -> String {
    format!(
        r#"set -eu
dest="$HOME/{install_suffix}"
dir="${{dest%/*}}"
mkdir -p "$dir"
tmp="${{dest}}.tmp.$$"
printf '%s\0%s\0' "$tmp" "$dest"
"#,
        install_suffix = remote_herdr.install_suffix
    )
}

fn parse_remote_install_paths(stdout: &[u8]) -> io::Result<(String, String)> {
    let mut parts = stdout.split(|byte| *byte == 0);
    let tmp_path = parts.next().unwrap_or_default();
    let dest_path = parts.next().unwrap_or_default();
    if tmp_path.is_empty() || dest_path.is_empty() {
        return Err(io::Error::other(
            "remote install preparation did not return destination paths",
        ));
    }
    let tmp_path = String::from_utf8(tmp_path.to_vec()).map_err(|err| {
        io::Error::other(format!(
            "remote install temporary path is not valid UTF-8: {err}"
        ))
    })?;
    let dest_path = String::from_utf8(dest_path.to_vec()).map_err(|err| {
        io::Error::other(format!(
            "remote install destination path is not valid UTF-8: {err}"
        ))
    })?;
    Ok((tmp_path, dest_path))
}

fn remote_install_stream_command(tmp_path: &str) -> String {
    format!("tee {}", shell_quote(tmp_path))
}

fn remote_install_commit_script(tmp_path: &str, dest_path: &str) -> String {
    format!(
        "set -eu\nchmod 755 {tmp_path}\nmv {tmp_path} {dest_path}\n",
        tmp_path = shell_quote(tmp_path),
        dest_path = shell_quote(dest_path)
    )
}

impl Drop for RemoteSsh {
    fn drop(&mut self) {
        let Some(_options) = self
            .managed_config
            .as_ref()
            .map(|config| &config.options)
            .filter(|options| options.control_path.is_some())
        else {
            return;
        };

        let _ = self
            .base_command()
            .arg("-O")
            .arg("exit")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg(&self.target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Reads a child's pipe to end on its own thread.
fn spawn_pipe_reader<R: io::Read + Send + 'static>(mut pipe: R) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = pipe.read_to_end(&mut buffer);
        buffer
    })
}

fn apply_managed_ssh_options(command: &mut Command, options: Option<&ManagedSshOptions>) {
    let Some(options) = options else {
        return;
    };

    command.arg("-F").arg(&options.config_path);
    if let Some(control_path) = &options.control_path {
        command
            .arg("-S")
            .arg(control_path)
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg("ControlPersist=yes");
    }
}

impl InstallSource {
    fn persistent(path: PathBuf) -> Self {
        Self {
            path,
            temporary_dir: None,
        }
    }

    fn temporary(path: PathBuf, temporary_dir: PathBuf) -> Self {
        Self {
            path,
            temporary_dir: Some(temporary_dir),
        }
    }

    fn cleanup(&self) {
        if let Some(dir) = &self.temporary_dir {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

fn prepare_remote_herdr(
    ssh: &RemoteSsh,
    live_handoff_enabled: bool,
) -> io::Result<PreparedRemoteHerdr> {
    let platform = detect_remote_platform(ssh)?;
    let remote_herdr = RemoteHerdr::for_platform(platform);
    let override_binary = remote_binary_override_path()?;
    let remote_binary_candidates = remote_binary_candidates(ssh, &remote_herdr)?;

    if override_binary.is_none() {
        for candidate in &remote_binary_candidates {
            if remote_binary_matches(ssh, candidate).unwrap_or(false) {
                return Ok(PreparedRemoteHerdr {
                    remote_herdr: candidate.clone(),
                    installed_or_replaced: false,
                    stop_after_install_approved: false,
                });
            }
        }
        if remote_binary_matches(ssh, &remote_herdr)? {
            return Ok(PreparedRemoteHerdr {
                remote_herdr,
                installed_or_replaced: false,
                stop_after_install_approved: false,
            });
        }
    }

    let mut stop_after_install_approved = false;
    if let Some(status_probe_herdr) = remote_binary_candidates.first().or_else(|| {
        remote_binary_exists(ssh, &remote_herdr)
            .ok()
            .and_then(|exists| exists.then_some(&remote_herdr))
    }) {
        stop_after_install_approved = confirm_remote_install_with_running_server(
            ssh,
            status_probe_herdr,
            live_handoff_enabled,
        )?;
    }
    confirm_remote_install(
        ssh.target(),
        &remote_herdr,
        &install_source_description(&remote_herdr.platform, override_binary.as_deref()),
    )?;
    let source = resolve_install_source(&remote_herdr.platform, override_binary)?;
    let install_result = ssh.install_herdr(&remote_herdr, &source.path);
    source.cleanup();
    install_result?;

    if !remote_binary_matches(ssh, &remote_herdr)? {
        return Err(io::Error::other(format!(
            "installed remote herdr at {}, but it did not report version {}",
            remote_herdr.shell_path,
            current_version()
        )));
    }
    warn_if_remote_bin_not_on_path(ssh)?;

    Ok(PreparedRemoteHerdr {
        remote_herdr,
        installed_or_replaced: true,
        stop_after_install_approved,
    })
}

/// Whether ssh gave up because it could not authenticate.
///
/// Retrying this cannot help: the server has no terminal, so the credential it
/// was missing on the first attempt is missing on every attempt. Detected from
/// ssh's own words rather than the exit status alone, because 255 is also what
/// an unreachable host produces and that one *is* worth retrying.
fn ssh_failed_to_authenticate(output: &Output) -> bool {
    if output.status.code() != Some(255) {
        return false;
    }
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("permission denied")
        || stderr.contains("too many authentication failures")
        || stderr.contains("no supported authentication methods")
}

fn detect_remote_platform(ssh: &RemoteSsh) -> io::Result<RemotePlatform> {
    let output = ssh.sh_output("uname -s\nuname -m\n")?;
    if !output.status.success() {
        if ssh_failed_to_authenticate(&output) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                command_failed("ssh authentication failed", &output).to_string(),
            ));
        }
        return Err(command_failed("remote platform detection failed", &output));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let os = lines.next().unwrap_or_default();
    let arch = lines.next().unwrap_or_default();
    RemotePlatform::from_uname(os, arch).ok_or_else(|| {
        io::Error::other(format!(
            "unsupported remote platform: {} {}",
            os.trim(),
            arch.trim()
        ))
    })
}

fn remote_binary_candidates(
    ssh: &RemoteSsh,
    remote_herdr: &RemoteHerdr,
) -> io::Result<Vec<RemoteHerdr>> {
    let mut candidates = Vec::new();

    if let Some(path_candidate) = remote_binary_on_path_any(ssh, remote_herdr)? {
        push_if_new_remote_binary_candidate(&mut candidates, path_candidate);
    }

    let output = ssh.sh_output(&known_remote_binary_candidate_script(
        &remote_herdr.platform,
    ))?;
    if !output.status.success() {
        return Err(command_failed("remote binary discovery failed", &output));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for candidate in remote_herdrs_from_path_discovery(remote_herdr, &stdout) {
        push_if_new_remote_binary_candidate(&mut candidates, candidate);
    }

    Ok(candidates)
}

fn push_if_new_remote_binary_candidate(candidates: &mut Vec<RemoteHerdr>, candidate: RemoteHerdr) {
    if !candidates
        .iter()
        .any(|existing| existing.shell_path == candidate.shell_path)
    {
        candidates.push(candidate);
    }
}

fn known_remote_binary_candidate_script(platform: &RemotePlatform) -> String {
    let mut script = String::from(
        r#"home=${HOME:-}
user=${USER:-}
version="#,
    );
    script.push_str(&shell_quote(&current_version()));
    script.push_str(
        r#"
emit() {
    path=$1
    if [ -n "$path" ] && [ -x "$path" ]; then
        printf '%s\n' "$path"
    fi
}
if [ -n "$home" ]; then
    emit "$home/.local/bin/herdr"
fi
"#,
    );
    if platform.os == "macos" {
        script.push_str(
            r#"    emit "/opt/homebrew/bin/herdr"
    emit "/usr/local/bin/herdr"
"#,
        );
    } else if platform.os == "linux" {
        script.push_str(
            r#"    emit "/home/linuxbrew/.linuxbrew/bin/herdr"
"#,
        );
    }
    script.push_str(
        r#"if [ -n "$home" ]; then
    emit "$home/.local/share/mise/installs/herdr/$version/bin/herdr"
    emit "$home/.local/share/mise/installs/herdr/$version/herdr"
    emit "$home/.local/share/mise/installs/github-ogulcancelik-herdr/$version/herdr"
    emit "$home/.nix-profile/bin/herdr"
fi
if [ -n "$user" ]; then
    emit "/etc/profiles/per-user/$user/bin/herdr"
fi
emit "/nix/var/nix/profiles/default/bin/herdr"
emit "/run/current-system/sw/bin/herdr"
"#,
    );

    script
}

fn remote_binary_on_path_any(
    ssh: &RemoteSsh,
    remote_herdr: &RemoteHerdr,
) -> io::Result<Option<RemoteHerdr>> {
    let output = ssh.user_shell_output("command -v herdr")?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(candidate) = remote_herdr_from_path_discovery(remote_herdr, &stdout) {
            return Ok(Some(candidate));
        }
    }

    // Non-POSIX login shells such as xonsh reject `command -v`; retry through
    // /bin/sh while retaining the login-shell probe for shell-initialized PATHs.
    let output = ssh.sh_output("command -v herdr\n")?;
    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(remote_herdr_from_path_discovery(remote_herdr, &stdout))
}

fn remote_herdrs_from_path_discovery(remote_herdr: &RemoteHerdr, stdout: &str) -> Vec<RemoteHerdr> {
    stdout
        .lines()
        .filter_map(|path| remote_herdr_from_path(remote_herdr, path))
        .collect()
}

fn remote_herdr_from_path_discovery(
    remote_herdr: &RemoteHerdr,
    stdout: &str,
) -> Option<RemoteHerdr> {
    stdout
        .lines()
        .find_map(|path| remote_herdr_from_path(remote_herdr, path))
}

fn remote_herdr_from_path(remote_herdr: &RemoteHerdr, path: &str) -> Option<RemoteHerdr> {
    let path = path.trim();
    if !path.starts_with('/') {
        return None;
    }
    if is_mise_shim_path(path) {
        return None;
    }
    Some(remote_herdr.clone().with_shell_path(shell_quote(path)))
}

fn is_mise_shim_path(path: &str) -> bool {
    path.ends_with("/mise/shims/herdr")
}

fn remote_binary_matches(ssh: &RemoteSsh, remote_herdr: &RemoteHerdr) -> io::Result<bool> {
    let command = format!(
        "test -x {0} && {0} --version && {0} status client --json",
        remote_herdr.shell_path
    );
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Ok(false);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let version = lines.next().unwrap_or_default().trim();
    let status = lines.next().unwrap_or_default();
    Ok(version == format!("herdr {}", current_version())
        && parse_client_status_json(status)
            .map(|status| status.protocol == CURRENT_PROTOCOL)
            .unwrap_or(false))
}

fn remote_binary_exists(ssh: &RemoteSsh, remote_herdr: &RemoteHerdr) -> io::Result<bool> {
    let command = format!("test -x {}", remote_herdr.shell_path);
    Ok(ssh.sh_output(&command)?.status.success())
}

fn remote_binary_override_path() -> io::Result<Option<PathBuf>> {
    let Some(value) = std::env::var_os(REMOTE_BINARY_ENV_VAR) else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{REMOTE_BINARY_ENV_VAR} must not be empty"),
        ));
    }

    let path = PathBuf::from(value);
    let metadata = fs::metadata(&path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to inspect {REMOTE_BINARY_ENV_VAR} path {}: {err}",
                path.display()
            ),
        )
    })?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{REMOTE_BINARY_ENV_VAR} path is not a file: {}",
                path.display()
            ),
        ));
    }

    Ok(Some(path))
}

fn install_source_description(platform: &RemotePlatform, override_binary: Option<&Path>) -> String {
    install_source_description_for(
        platform,
        override_binary,
        local_binary_can_seed_remote(platform),
    )
}

fn install_source_description_for(
    platform: &RemotePlatform,
    override_binary: Option<&Path>,
    local_binary_can_seed_remote: bool,
) -> String {
    if let Some(path) = override_binary {
        return format!("{REMOTE_BINARY_ENV_VAR} ({})", path.display());
    }

    if local_binary_can_seed_remote {
        "the current local herdr binary".to_string()
    } else {
        format!(
            "the {} {} asset for {}",
            current_version(),
            current_channel(),
            platform.asset_key()
        )
    }
}

fn resolve_install_source(
    platform: &RemotePlatform,
    override_binary: Option<PathBuf>,
) -> io::Result<InstallSource> {
    if let Some(path) = override_binary {
        return Ok(InstallSource::persistent(path));
    }

    if *platform == RemotePlatform::local() {
        let path = std::env::current_exe()?;
        if !crate::update::is_package_manager_managed_exe_path(&path) {
            return Ok(InstallSource::persistent(path));
        }
    }

    download_release_asset(platform)
}

fn local_binary_can_seed_remote(platform: &RemotePlatform) -> bool {
    if *platform != RemotePlatform::local() {
        return false;
    }

    std::env::current_exe()
        .map(|path| !crate::update::is_package_manager_managed_exe_path(&path))
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteServerStatus {
    Running {
        version: Option<String>,
        protocol: Option<u32>,
        live_handoff: bool,
        detached_server_daemon: bool,
    },
    NotRunning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteServerRestartReason {
    ProtocolMismatch,
    DaemonDetachMissing,
    BinaryUpdated,
    VersionMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteInstallRunningServerPlan {
    KeepRunning,
    LiveHandoff,
    StopRequired(RemoteServerRestartReason),
}

fn ensure_remote_server_ready(
    ssh: &RemoteSsh,
    remote_herdr: &RemoteHerdr,
    remote_binary_changed: bool,
    stop_after_install_approved: bool,
    live_handoff_enabled: bool,
) -> io::Result<()> {
    let status = remote_server_status(ssh, remote_herdr)?;
    let RemoteServerStatus::Running {
        version,
        protocol,
        live_handoff,
        detached_server_daemon,
    } = status
    else {
        return Ok(());
    };

    let Some(reason) = remote_server_restart_reason(
        version.as_deref(),
        protocol,
        detached_server_daemon,
        remote_binary_changed,
    ) else {
        return Ok(());
    };

    if live_handoff_enabled && live_handoff {
        match live_handoff_remote_server(ssh, remote_herdr) {
            Ok(()) => return Ok(()),
            Err(err) => {
                eprintln!("remote live handoff failed: {err}");
                eprintln!("falling back to remote server restart.");
            }
        }
    }

    if stop_after_install_approved {
        stop_remote_server(ssh, remote_herdr)?;
        return Ok(());
    }

    if confirm_remote_server_stop(ssh.target(), version.as_deref(), protocol, reason)? {
        stop_remote_server(ssh, remote_herdr)?;
    }
    Ok(())
}

fn remote_server_restart_reason(
    version: Option<&str>,
    protocol: Option<u32>,
    detached_server_daemon: bool,
    remote_binary_changed: bool,
) -> Option<RemoteServerRestartReason> {
    if protocol != Some(CURRENT_PROTOCOL) {
        return Some(RemoteServerRestartReason::ProtocolMismatch);
    }
    if !detached_server_daemon {
        return Some(RemoteServerRestartReason::DaemonDetachMissing);
    }
    if version != Some(current_version().as_str()) {
        return Some(RemoteServerRestartReason::VersionMismatch);
    }
    if remote_binary_changed {
        return Some(RemoteServerRestartReason::BinaryUpdated);
    }
    None
}

fn confirm_remote_install_with_running_server(
    ssh: &RemoteSsh,
    remote_herdr: &RemoteHerdr,
    live_handoff_enabled: bool,
) -> io::Result<bool> {
    let target = ssh.target();
    let status = match remote_server_status(ssh, remote_herdr) {
        Ok(status) => status,
        Err(err) => {
            if !io::stdin().is_terminal() {
                return Err(io::Error::other(format!(
                    "could not inspect the running remote herdr server on {target} before installing: {err}; run from an interactive terminal to approve updating the remote binary"
                )));
            }
            eprintln!(
                "could not inspect the running remote herdr server on {target} before installing: {err}"
            );
            eprint!("continue installing the remote herdr binary? [y/N] ");
            io::stderr().flush()?;

            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            let answer = answer.trim().to_ascii_lowercase();
            if answer != "y" && answer != "yes" {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "remote herdr install cancelled",
                ));
            }
            return Ok(false);
        }
    };
    let RemoteServerStatus::Running {
        version,
        protocol,
        live_handoff,
        detached_server_daemon,
    } = &status
    else {
        return Ok(false);
    };
    let plan = remote_install_running_server_plan(
        version.as_deref(),
        *protocol,
        *detached_server_daemon,
        true,
        *live_handoff,
        live_handoff_enabled,
    );

    if plan == RemoteInstallRunningServerPlan::KeepRunning {
        if io::stdin().is_terminal() {
            eprintln!("remote herdr server on {target} is already compatible:");
            eprintln!("  server: v{}", version_label(version.as_deref()));
            eprintln!(
                "Herdr will install {} without stopping the running remote server.",
                current_version()
            );
        }
        return Ok(false);
    }

    if !io::stdin().is_terminal() {
        match plan {
            RemoteInstallRunningServerPlan::LiveHandoff => return Ok(false),
            RemoteInstallRunningServerPlan::StopRequired(_) => {
                return Err(io::Error::other(format!(
                    "remote herdr server on {target} is running v{}; run from an interactive terminal to approve stopping it for the update",
                    version_label(version.as_deref())
                )));
            }
            RemoteInstallRunningServerPlan::KeepRunning => return Ok(false),
        }
    }

    if plan == RemoteInstallRunningServerPlan::LiveHandoff {
        eprintln!("remote herdr server on {target} is currently running:");
        eprintln!("  server: v{}", version_label(version.as_deref()));
        eprintln!(
            "Herdr will install {} and hand off live pane processes to the prepared server.",
            current_version()
        );
        return Ok(false);
    }

    eprintln!("remote herdr server on {target} is currently running:");
    eprintln!("  server: v{}", version_label(version.as_deref()));
    eprintln!(
        "To complete the remote update, Herdr must stop the running remote server after installing."
    );
    eprintln!("This stops active remote pane processes, including shells, dev servers, and tests.");
    eprintln!();
    eprint!(
        "Install {} and stop the remote server now? [y/N] ",
        current_version()
    );
    io::stderr().flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    if answer != "y" && answer != "yes" {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote herdr install cancelled",
        ));
    }

    Ok(true)
}

fn remote_install_running_server_plan(
    version: Option<&str>,
    protocol: Option<u32>,
    detached_server_daemon: bool,
    remote_binary_changed: bool,
    live_handoff: bool,
    live_handoff_enabled: bool,
) -> RemoteInstallRunningServerPlan {
    let Some(reason) = remote_server_restart_reason(
        version,
        protocol,
        detached_server_daemon,
        remote_binary_changed,
    ) else {
        return RemoteInstallRunningServerPlan::KeepRunning;
    };

    if live_handoff_enabled && live_handoff {
        return RemoteInstallRunningServerPlan::LiveHandoff;
    }

    RemoteInstallRunningServerPlan::StopRequired(reason)
}

fn remote_server_status(
    ssh: &RemoteSsh,
    remote_herdr: &RemoteHerdr,
) -> io::Result<RemoteServerStatus> {
    let command = format!("{} status server --json", remote_herdr.shell_path);
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server status failed", &output));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_remote_server_status_json(stdout.trim())
}

#[derive(Debug, Deserialize)]
struct RemoteClientStatusJson {
    protocol: u32,
}

#[derive(Debug, Deserialize)]
struct RemoteServerStatusJson {
    running: bool,
    version: Option<String>,
    protocol: Option<u32>,
    capabilities: Option<RemoteServerCapabilitiesJson>,
}

#[derive(Debug, Deserialize)]
struct RemoteServerCapabilitiesJson {
    live_handoff: bool,
    #[serde(default)]
    detached_server_daemon: bool,
}

fn parse_client_status_json(status: &str) -> Option<RemoteClientStatusJson> {
    serde_json::from_str(status).ok()
}

fn parse_remote_server_status_json(status: &str) -> io::Result<RemoteServerStatus> {
    let parsed: RemoteServerStatusJson = serde_json::from_str(status).map_err(|err| {
        io::Error::other(format!(
            "could not parse remote server status JSON from `{status}`: {err}"
        ))
    })?;
    if !parsed.running {
        return Ok(RemoteServerStatus::NotRunning);
    }

    let capabilities = parsed.capabilities;

    Ok(RemoteServerStatus::Running {
        version: parsed.version,
        protocol: parsed.protocol,
        live_handoff: capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.live_handoff),
        detached_server_daemon: capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.detached_server_daemon),
    })
}

fn confirm_remote_server_stop(
    target: &str,
    version: Option<&str>,
    _protocol: Option<u32>,
    reason: RemoteServerRestartReason,
) -> io::Result<bool> {
    if !io::stdin().is_terminal() {
        if reason == RemoteServerRestartReason::ProtocolMismatch {
            return Err(io::Error::other(format!(
                "remote herdr server on {target} must stop before this client can attach; run from an interactive terminal to approve stopping it"
            )));
        }

        eprintln!(
            "remote herdr server on {target} is still running v{}; it will use {} after it restarts.",
            version_label(version),
            current_version()
        );
        return Ok(false);
    }

    eprintln!("remote herdr server on {target} is currently running:");
    eprintln!("  server: v{}", version_label(version));
    eprintln!("  prepared binary: {}", current_version());
    eprintln!();

    match reason {
        RemoteServerRestartReason::ProtocolMismatch => {
            eprintln!("the remote server must stop before this client can attach.");
        }
        RemoteServerRestartReason::DaemonDetachMissing => {
            eprintln!(
                "the remote server was started by a herdr build that may not survive SSH connection loss. restart it so network drops disconnect only this client."
            );
        }
        RemoteServerRestartReason::BinaryUpdated => {
            eprintln!(
                "the remote herdr binary was installed or replaced. restart the remote server so it uses the prepared binary."
            );
        }
        RemoteServerRestartReason::VersionMismatch => {
            eprintln!(
                "the remote server is still running a different herdr version. restart it so it uses the prepared binary."
            );
        }
    }

    let prompt = if reason == RemoteServerRestartReason::ProtocolMismatch {
        "stop the remote server and continue attaching? [Y/n] "
    } else {
        "restart the remote server now? [y/N] "
    };
    eprint!("{prompt}");
    io::stderr().flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    if answer == "y" || answer == "yes" {
        return Ok(true);
    }
    if answer.is_empty() && reason == RemoteServerRestartReason::ProtocolMismatch {
        return Ok(true);
    }
    if reason == RemoteServerRestartReason::ProtocolMismatch {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote herdr server stop cancelled",
        ));
    }

    Ok(false)
}

fn live_handoff_remote_server(ssh: &RemoteSsh, remote_herdr: &RemoteHerdr) -> io::Result<()> {
    let command = format!(
        "{} server live-handoff --import-exe {} --expected-protocol {} --expected-version {}",
        remote_herdr.shell_path,
        remote_herdr.shell_path,
        CURRENT_PROTOCOL,
        current_version()
    );
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server live handoff failed", &output));
    }

    eprintln!(
        "handed off the remote herdr server on {}; reconnecting to the prepared server.",
        ssh.target()
    );
    Ok(())
}

fn stop_remote_server(ssh: &RemoteSsh, remote_herdr: &RemoteHerdr) -> io::Result<()> {
    let command = format!("{} server stop", remote_herdr.shell_path);
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server stop failed", &output));
    }

    wait_for_remote_server_shutdown(ssh, remote_herdr)?;
    eprintln!(
        "stopped the remote herdr server on {}; it will restart when the remote client bridge attaches.",
        ssh.target()
    );
    Ok(())
}

fn wait_for_remote_server_shutdown(ssh: &RemoteSsh, remote_herdr: &RemoteHerdr) -> io::Result<()> {
    let deadline = Instant::now() + REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT;
    loop {
        if remote_server_status(ssh, remote_herdr)? == RemoteServerStatus::NotRunning {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "shutdown was requested, but the old remote herdr server on {target} is still responding after {} seconds",
                    REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT.as_secs(),
                    target = ssh.target()
                ),
            ));
        }
        thread::sleep(REMOTE_SERVER_SHUTDOWN_POLL_INTERVAL);
    }
}

fn version_label(version: Option<&str>) -> &str {
    version.unwrap_or("unknown")
}

fn warn_if_remote_bin_not_on_path(ssh: &RemoteSsh) -> io::Result<()> {
    let output = ssh.user_shell_output("command -v herdr")?;
    if output.status.success()
        && remote_shell_resolves_managed_install(&String::from_utf8_lossy(&output.stdout))
    {
        return Ok(());
    }

    eprintln!(
        "herdr: installed remote binary to ~/.local/bin/herdr, but the remote shell does not resolve `herdr` to that path"
    );
    Ok(())
}

fn remote_shell_resolves_managed_install(stdout: &str) -> bool {
    stdout
        .lines()
        .next()
        .map(str::trim)
        .is_some_and(|path| path.ends_with("/.local/bin/herdr"))
}

fn download_release_asset(platform: &RemotePlatform) -> io::Result<InstallSource> {
    let asset_key = platform.asset_key();
    let asset = remote_release_asset(&asset_key)?;

    let dir = private_download_dir(&asset_key)?;
    let path = dir.join("herdr.tmp");
    let status = crate::noninteractive_process::curl_command()
        .args(["-sfL", "--max-time", "120", "-o"])
        .arg(&path)
        .arg(&asset.url)
        .status()
        .map_err(|err| io::Error::new(err.kind(), format!("download failed: {err}")))?;
    if !status.success() {
        let _ = fs::remove_dir_all(&dir);
        return Err(io::Error::other("download failed"));
    }
    if let Some(expected) = &asset.sha256 {
        if let Err(err) = crate::checksum::verify_sha256(&path, expected) {
            let _ = fs::remove_dir_all(&dir);
            return Err(io::Error::new(
                err.kind(),
                format!("downloaded remote asset checksum verification failed: {err}"),
            ));
        }
    }

    Ok(InstallSource::temporary(path, dir))
}

fn fetch_remote_manifest(url: &str) -> io::Result<Vec<u8>> {
    let output = crate::noninteractive_process::curl_command()
        .args([
            "-sfL",
            "--retry",
            "3",
            "--connect-timeout",
            "10",
            "--max-time",
            "20",
            url,
        ])
        .output()
        .map_err(|err| io::Error::new(err.kind(), format!("curl failed: {err}")))?;
    if !output.status.success() {
        return Err(command_failed("failed to fetch update manifest", &output));
    }
    Ok(output.stdout)
}

fn remote_asset_info(asset: &RemoteAssetRef) -> RemoteReleaseAsset {
    RemoteReleaseAsset {
        url: asset.url().to_string(),
        sha256: asset.sha256().map(str::to_string),
    }
}

fn preview_assets_for_build<'a>(
    manifest: &'a RemotePreviewManifest,
    build_id: &str,
) -> io::Result<(u32, &'a BTreeMap<String, RemoteAssetRef>)> {
    if manifest.build_id == build_id {
        return Ok((manifest.protocol, &manifest.assets));
    }
    let build = manifest.builds.get(build_id).ok_or_else(|| {
        io::Error::other(format!(
            "preview manifest no longer includes build {build_id}; run `herdr update` locally or set {REMOTE_BINARY_ENV_VAR}=target/release/herdr"
        ))
    })?;
    Ok((build.protocol, &build.assets))
}

fn remote_release_asset(asset_key: &str) -> io::Result<RemoteReleaseAsset> {
    if crate::build_info::is_preview() {
        let build_id = crate::build_info::build_id().ok_or_else(|| {
            io::Error::other("preview client has no build id; set HERDR_REMOTE_BINARY or install Herdr on the remote manually")
        })?;
        let manifest_bytes = fetch_remote_manifest(PREVIEW_UPDATE_MANIFEST_URL)?;
        let manifest: RemotePreviewManifest =
            serde_json::from_slice(&manifest_bytes).map_err(|err| {
                io::Error::other(format!("failed to parse preview manifest JSON: {err}"))
            })?;
        let (protocol, assets) = preview_assets_for_build(&manifest, build_id)?;
        if protocol != CURRENT_PROTOCOL {
            return Err(io::Error::other(format!(
                "preview manifest has build {build_id} protocol {protocol}, but this client needs protocol {CURRENT_PROTOCOL}; set {REMOTE_BINARY_ENV_VAR}=target/release/herdr or install a matching Herdr on the remote host manually"
            )));
        }
        return assets.get(asset_key).map(remote_asset_info).ok_or_else(|| {
            io::Error::other(format!(
                "no {asset_key} binary in the preview manifest for build {build_id}"
            ))
        });
    }

    let current_version = current_version();
    let manifest_bytes = fetch_remote_manifest(STABLE_UPDATE_MANIFEST_URL)?;
    let manifest: RemoteUpdateManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|err| io::Error::other(format!("failed to parse update manifest JSON: {err}")))?;
    let release = manifest.release_for_version(&current_version).ok_or_else(|| {
        io::Error::other(format!(
            "release manifest does not include herdr {current_version}; build herdr for {} or install it there manually",
            asset_key
        ))
    })?;
    if let Some(protocol) = release.protocol {
        if protocol != CURRENT_PROTOCOL {
            return Err(io::Error::other(format!(
                "release manifest has herdr {current_version} protocol {protocol}, but this client needs protocol {CURRENT_PROTOCOL}; set {REMOTE_BINARY_ENV_VAR}=target/release/herdr or install a matching herdr on the remote host manually"
            )));
        }
    }
    let asset = release.assets.get(asset_key).ok_or_else(|| {
        io::Error::other(format!(
            "no {asset_key} binary in the release manifest for herdr {current_version}"
        ))
    })?;
    let mut asset = remote_asset_info(asset);
    asset.sha256 = asset
        .sha256
        .or_else(|| release.sha256.get(asset_key).cloned());
    if asset.sha256.is_none() {
        return Err(io::Error::other(format!(
            "release manifest asset {asset_key} is missing a SHA-256 checksum"
        )));
    }
    Ok(asset)
}

fn private_download_dir(asset_key: &str) -> io::Result<PathBuf> {
    let base = crate::platform::remote_private_temp_base();
    fs::create_dir_all(&base)?;
    for attempt in 0..100 {
        let dir = base.join(format!(
            "herdr-remote-{}-{}-{attempt}",
            std::process::id(),
            asset_key
        ));
        match crate::platform::create_remote_private_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to create private herdr remote download directory",
    ))
}

fn confirm_remote_install(
    target: &str,
    remote_herdr: &RemoteHerdr,
    source_description: &str,
) -> io::Result<()> {
    if !io::stdin().is_terminal() {
        return Err(io::Error::other(format!(
            "matching remote herdr {} is not installed at {}; run from an interactive terminal to approve installation",
            current_version(),
            remote_herdr.shell_path
        )));
    }

    eprintln!(
        "matching herdr {} is not installed on {target} for {}.",
        current_version(),
        remote_herdr.platform.asset_key()
    );
    eprint!(
        "Install {} to {}? [Y/n] ",
        source_description, remote_herdr.shell_path
    );
    io::stderr().flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    if answer == "n" || answer == "no" {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote herdr installation cancelled",
        ));
    }

    Ok(())
}

fn remote_bridge_command(
    remote_herdr: &RemoteHerdr,
    session_name: &str,
    socket: BridgeSocket,
) -> String {
    let mut command = format!("exec {}", remote_herdr.shell_path);
    if session_name != crate::session::DEFAULT_SESSION_NAME {
        command.push_str(" --session ");
        command.push_str(&shell_quote(session_name));
    }
    command.push(' ');
    command.push_str(socket.subcommand());
    command
}

fn reattach_command(
    program: &str,
    target: &str,
    session_name: &str,
    keybindings: RemoteKeybindings,
    live_handoff: bool,
) -> String {
    let program = crate::platform::remote_reattach_program(program);
    let target = crate::platform::remote_reattach_argument(target);
    let mut command = format!("{program} --remote {target}");
    if keybindings != RemoteKeybindings::Local {
        command.push_str(" --remote-keybindings ");
        command.push_str(keybindings.as_str());
    }
    if live_handoff {
        command.push_str(" --handoff");
    }
    if session_name != crate::session::DEFAULT_SESSION_NAME {
        command.push_str(" --session ");
        command.push_str(&crate::platform::remote_reattach_argument(session_name));
    }
    command
}

fn command_failed(context: &str, output: &Output) -> io::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        io::Error::other(format!("{context}: {}", output.status))
    } else {
        io::Error::other(format!("{context}: {stderr}"))
    }
}

struct SshStdioBridge {
    local_socket: PathBuf,
    socket_identity: crate::ipc::SocketFileIdentity,
    should_stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SshStdioBridge {
    fn start(
        target: String,
        remote_herdr: RemoteHerdr,
        local_socket: PathBuf,
        session_name: String,
        socket: BridgeSocket,
        ssh_options: Option<&ManagedSshOptions>,
    ) -> io::Result<Self> {
        crate::ipc::prepare_socket_path(&local_socket, |path| {
            format!("remote bridge is already listening at {}", path.display())
        })?;
        let listener = crate::ipc::bind_private_local_listener(&local_socket)?;
        let socket_identity = crate::ipc::socket_file_identity(&local_socket)?;
        if let Err(err) =
            crate::ipc::restrict_socket_permissions(&local_socket, BRIDGE_SOCKET_PERMISSION_MODE)
        {
            let _ = crate::ipc::remove_socket_file_if_owned(&local_socket, &socket_identity);
            return Err(err);
        }
        if let Err(err) = listener.set_nonblocking(ListenerNonblockingMode::Accept) {
            let _ = crate::ipc::remove_socket_file_if_owned(&local_socket, &socket_identity);
            return Err(err);
        }

        let should_stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&should_stop);
        let thread_ssh_options = ssh_options.cloned();
        let thread = thread::spawn(move || {
            // Each accepted connection gets its own ssh child and its own
            // thread. A federating server holds a long-lived event
            // subscription and still has to issue requests and open terminals
            // over the same bridge, so serving connections one at a time would
            // deadlock everything behind the subscription.
            let mut connections: Vec<JoinHandle<()>> = Vec::new();

            while !thread_stop.load(Ordering::Acquire) {
                connections.retain(|connection| !connection.is_finished());

                match listener.accept() {
                    Ok(stream) => {
                        let stream = match prepare_remote_bridge_stream(stream) {
                            Ok(stream) => stream,
                            Err(err) => {
                                tracing::error!(
                                    error = %err,
                                    "remote bridge failed to prepare client socket"
                                );
                                continue;
                            }
                        };
                        let target = target.clone();
                        let remote_herdr = remote_herdr.clone();
                        let session_name = session_name.clone();
                        let ssh_options = thread_ssh_options.clone();
                        let connection_stop = Arc::clone(&thread_stop);
                        match thread::Builder::new()
                            .name("herdr-bridge-conn".to_string())
                            .spawn(move || {
                                if let Err(err) = bridge_connection(
                                    stream,
                                    &target,
                                    &remote_herdr,
                                    &session_name,
                                    socket,
                                    ssh_options.as_ref(),
                                    &connection_stop,
                                ) {
                                    eprintln!("herdr: remote bridge failed: {err}");
                                }
                            }) {
                            Ok(connection) => connections.push(connection),
                            Err(err) => {
                                eprintln!("herdr: remote bridge could not spawn connection: {err}")
                            }
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(BRIDGE_ACCEPT_POLL);
                    }
                    Err(err) => {
                        eprintln!("herdr: remote bridge listener failed: {err}");
                        break;
                    }
                }
            }

            // Connections cut themselves short once the stop flag is set, so
            // joining them here reaps every ssh child without waiting on the
            // remote side to finish.
            for connection in connections {
                let _ = connection.join();
            }
        });

        Ok(Self {
            local_socket,
            socket_identity,
            should_stop,
            thread: Some(thread),
        })
    }
}

fn prepare_remote_bridge_stream(
    mut stream: crate::ipc::LocalStream,
) -> io::Result<crate::ipc::LocalStream> {
    crate::ipc::set_local_stream_polling(&mut stream, false)?;
    Ok(stream)
}

impl Drop for SshStdioBridge {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Release);
        #[cfg(unix)]
        let _ = crate::ipc::remove_socket_file_if_owned(&self.local_socket, &self.socket_identity);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        #[cfg(windows)]
        let _ = crate::ipc::remove_socket_file_if_owned(&self.local_socket, &self.socket_identity);
    }
}

#[cfg(unix)]
/// Local name of an ssh peer's bridged JSON API socket.
const PEER_API_SOCKET_NAME: &str = "api.sock";
#[cfg(unix)]
/// Local name of an ssh peer's bridged client protocol socket. Must stay what
/// [`crate::server::socket_paths::derive_client_socket_from_api_socket`] derives
/// from [`PEER_API_SOCKET_NAME`]; a test below pins that.
const PEER_CLIENT_SOCKET_NAME: &str = "api-client.sock";

#[cfg(unix)]
/// Why an ssh peer bridge could not be stood up.
pub(crate) enum PeerSshBridgeError {
    /// The remote could not be reached. Retrying may work.
    Unreachable(String),
    /// The remote answered but cannot be federated with as configured.
    /// Retrying would only hide the misconfiguration.
    Unsupported(String),
}

#[cfg(unix)]
/// Makes a remote herdr server reachable as if it were listening locally.
///
/// Federating with a peer needs both of its sockets — the JSON API for
/// identity, workspaces and events, and the client protocol for terminal
/// frames — so this owns one bridge each. The two local sockets are named the
/// way a server names its own pair, so everything downstream derives the client
/// socket from the API socket and treats an ssh peer exactly like a socket-path
/// peer.
pub(crate) struct PeerSshBridge {
    api_socket: PathBuf,
    /// Held only for their `Drop`. Declaration order is load-bearing: both
    /// bridges must stop before the ssh session carrying them is torn down, and
    /// both sockets must be gone before their directory is removed.
    _api_bridge: SshStdioBridge,
    _client_bridge: SshStdioBridge,
    _ssh: RemoteSsh,
    _socket_dir: PrivateDir,
}

#[cfg(unix)]
impl PeerSshBridge {
    /// Local socket the peer's JSON API is reachable on.
    pub(crate) fn api_socket(&self) -> &Path {
        &self.api_socket
    }
}

/// Stands up a bridge to the herdr server at `destination`.
///
/// Blocking and ssh-bound: several round trips before it returns. Callers must
/// run it off the event loop.
///
/// `running` is the caller's own stop flag. Setting it up costs one ssh round
/// trip per step and the calling thread is joined during shutdown, so the flag
/// is carried into every one of them rather than checked between them: the wait
/// that has to end is the one already in progress.
#[cfg(unix)]
pub(crate) fn start_peer_ssh_bridge(
    destination: &str,
    session: Option<&str>,
    running: Arc<AtomicBool>,
) -> Result<PeerSshBridge, PeerSshBridgeError> {
    let session_name = session
        .unwrap_or(crate::session::DEFAULT_SESSION_NAME)
        .to_string();
    let manage_ssh_config = crate::config::Config::load()
        .config
        .remote
        .manage_ssh_config;
    let ssh =
        RemoteSsh::for_peer(destination.to_string(), manage_ssh_config).cancelled_with(running);
    let remote_herdr = resolve_peer_remote_herdr(&ssh)?;

    let socket_dir = PrivateDir(private_peer_socket_dir().map_err(|err| {
        PeerSshBridgeError::Unreachable(format!("could not create peer socket directory: {err}"))
    })?);
    let api_socket = socket_dir.path().join(PEER_API_SOCKET_NAME);
    let client_socket =
        crate::server::socket_paths::derive_client_socket_from_api_socket(&api_socket);

    let start = |local_socket: PathBuf, socket: BridgeSocket| {
        SshStdioBridge::start(
            destination.to_string(),
            remote_herdr.clone(),
            local_socket,
            session_name.clone(),
            socket,
            ssh.options(),
        )
        .map_err(|err| {
            PeerSshBridgeError::Unreachable(format!(
                "could not start peer {} bridge: {err}",
                socket.describe()
            ))
        })
    };

    let api_bridge = start(api_socket.clone(), BridgeSocket::Api)?;
    let client_bridge = start(client_socket, BridgeSocket::Client)?;

    Ok(PeerSshBridge {
        api_socket,
        _api_bridge: api_bridge,
        _client_bridge: client_bridge,
        _ssh: ssh,
        _socket_dir: socket_dir,
    })
}

#[cfg(unix)]
fn private_peer_socket_dir() -> io::Result<PathBuf> {
    private_runtime_dir("herdr-peer", PEER_CLIENT_SOCKET_NAME)
}

#[cfg(unix)]
/// Finds a remote herdr this build can federate with.
///
/// Unlike `herdr --remote`, this never installs or replaces anything: the
/// server doing the federating is headless and cannot ask anyone to approve
/// writing a binary to another machine. A missing or mismatched remote build is
/// therefore a configuration problem to report, not something to fix silently.
fn resolve_peer_remote_herdr(ssh: &RemoteSsh) -> Result<RemoteHerdr, PeerSshBridgeError> {
    // An unsupported remote platform lands here as `Unreachable` and so retries
    // forever with its reason showing. That is worth accepting rather than
    // pattern-matching on this error's text to reclassify it.
    //
    // An authentication failure is the exception, and it is reported by kind
    // rather than by text: the server has no terminal, so whatever ssh wanted
    // typed will still be missing on every retry. Spinning on it for the rest
    // of the peer's life only buries the reason.
    let platform = detect_remote_platform(ssh).map_err(|err| {
        if err.kind() == io::ErrorKind::PermissionDenied {
            PeerSshBridgeError::Unsupported(format!(
                "{err}; run `herdr peer setup-ssh {}` once from a terminal to install a key",
                shell_quote(ssh.target())
            ))
        } else {
            PeerSshBridgeError::Unreachable(err.to_string())
        }
    })?;

    let default = RemoteHerdr::for_platform(platform);
    let mut candidates = remote_binary_candidates(ssh, &default)
        .map_err(|err| PeerSshBridgeError::Unreachable(err.to_string()))?;
    push_if_new_remote_binary_candidate(&mut candidates, default);

    for candidate in &candidates {
        match remote_binary_matches(ssh, candidate) {
            Ok(true) => return Ok(candidate.clone()),
            Ok(false) => {}
            Err(err) => {
                return Err(PeerSshBridgeError::Unreachable(format!(
                    "remote herdr version check failed: {err}"
                )))
            }
        }
    }

    Err(PeerSshBridgeError::Unsupported(format!(
        "no herdr {} speaking protocol {CURRENT_PROTOCOL} found on {}; run `herdr --remote {}` once to install a matching build",
        current_version(),
        ssh.target(),
        shell_quote(ssh.target()),
    )))
}

#[cfg(unix)]
/// Makes sure `destination` has a herdr this build can federate with,
/// installing one interactively if it does not.
///
/// The server refuses to install ([`resolve_peer_remote_herdr`]) because it
/// cannot ask anyone to approve writing a binary to another machine. The CLI
/// can, so it reuses `herdr --remote`'s own preparation rather than sending the
/// user away to run that command and come back.
///
/// Deliberately stops at the binary: [`ensure_remote_server_ready`] is not
/// called, because the peer bridge boots the remote server itself and
/// `identify()` already gates protocol.
pub(crate) fn ensure_peer_remote_binary(destination: &str) -> io::Result<()> {
    let manage_ssh_config = crate::config::Config::load()
        .config
        .remote
        .manage_ssh_config;
    let ssh = RemoteSsh::for_peer(destination.to_string(), manage_ssh_config);
    prepare_remote_herdr(&ssh, false)?;
    Ok(())
}

#[cfg(unix)]
/// Where herdr keeps the key it dials peer servers with.
fn peer_ssh_key_path() -> PathBuf {
    crate::config::config_dir().join(PEER_SSH_KEY_NAME)
}

#[cfg(unix)]
fn peer_ssh_public_key_path() -> PathBuf {
    peer_ssh_key_path().with_extension("pub")
}

/// The `authorized_keys` comment that identifies this client's peer key.
///
/// A peer target keeps herdr's key until something removes it, and the only
/// thing that can remove it is a later setup run from the same client. That
/// makes the comment the identity: it has to name the client rather than the
/// key, so a regenerated key replaces its own predecessor instead of stacking
/// another entry, and it has to live outside the config directory so wiping
/// that directory does not also lose the ability to find what it installed.
///
/// The build's directory name is part of it because a debug build keeps its
/// state in `herdr-dev`; treating the two as one client would have each evict
/// the other's key on the same machine.
#[cfg(unix)]
fn peer_ssh_key_comment() -> String {
    format!(
        "{PEER_SSH_KEY_COMMENT_PREFIX} {}/{}@{}",
        crate::config::app_dir_name(),
        local_user_name(),
        local_host_name()
    )
}

#[cfg(unix)]
fn local_user_name() -> String {
    let name = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    sanitized_identity_component(&name, "user")
}

#[cfg(unix)]
fn local_host_name() -> String {
    let from_command = Command::new("hostname")
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_default();
    let name = if from_command.trim().is_empty() {
        fs::read_to_string("/etc/hostname").unwrap_or_default()
    } else {
        from_command
    };

    sanitized_identity_component(name.lines().next().unwrap_or_default(), "host")
}

#[cfg(unix)]
/// Keeps a comment to characters that survive both `authorized_keys` line
/// splitting and `awk -v`, which would otherwise read a backslash as the start
/// of an escape.
fn sanitized_identity_component(value: &str, fallback: &str) -> String {
    let cleaned: String = value
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        .take(PEER_SSH_IDENTITY_COMPONENT_LIMIT)
        .collect();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

#[cfg(unix)]
/// Whether ssh can reach `destination` with nobody watching.
///
/// This is the only question that matters for a peer. The server dials peers
/// from a process with no controlling terminal, and ssh reads passwords and
/// passphrases from `/dev/tty` rather than stdin, so anything that would prompt
/// fails there with no way to say why — on the first connection and on every
/// automatic reconnect after it. `BatchMode=yes` reproduces that restriction
/// here, in a process that can still do something about the answer.
fn peer_ssh_auth_works(destination: &str) -> bool {
    let manage_ssh_config = crate::config::Config::load()
        .config
        .remote
        .manage_ssh_config;
    let ssh = RemoteSsh::for_peer(destination.to_string(), manage_ssh_config);
    // `BatchMode=yes` and `ConnectTimeout` ride along on every peer command,
    // which is exactly the restriction being probed for here.
    let mut command = ssh.base_command();
    command
        .arg("-T")
        .arg(destination)
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    matches!(command.status(), Ok(status) if status.success())
}

/// Makes sure `destination` can be reached without a prompt, installing a key
/// interactively if it cannot.
///
/// Runs in the CLI process on purpose: that is the only part of herdr that has
/// a terminal for ssh to prompt against. The server never calls this.
#[cfg(unix)]
pub(crate) fn ensure_peer_ssh_ready(destination: &str, assume_yes: bool) -> io::Result<()> {
    if peer_ssh_auth_works(destination) {
        return Ok(());
    }

    if !io::stdin().is_terminal() {
        return Err(io::Error::other(format!(
            "ssh cannot reach {destination} without a prompt, and herdr's server has no terminal to prompt from; run `herdr peer setup-ssh {}` from an interactive terminal once",
            shell_quote(destination)
        )));
    }

    eprintln!("ssh could not reach {destination} without asking for something interactively.");
    eprintln!(
        "herdr's server dials peers with no terminal attached, so it cannot answer a password"
    );
    eprintln!(
        "prompt — not for the first connection, and not for the reconnects it makes on its own."
    );

    let key = peer_ssh_key_path();
    if !assume_yes
        && !confirm_peer_setup(&format!(
            "Install herdr's ssh key ({}) on {destination}? [Y/n] ",
            key.display()
        ))?
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "ssh key setup declined",
        ));
    }

    ensure_peer_ssh_key()?;
    install_peer_ssh_key(destination)?;

    if !peer_ssh_auth_works(destination) {
        return Err(io::Error::other(format!(
            "installed herdr's ssh key on {destination}, but ssh still cannot connect without a prompt"
        )));
    }

    eprintln!("ssh key installed; {destination} can now be reached without a prompt.");
    Ok(())
}

/// Creates herdr's peer key if it is missing.
///
/// The key deliberately has no passphrase. A peer view reconnects on its own
/// for as long as the peer might come back, from a server process that may have
/// been started by a service manager and inherited no ssh-agent — a key that
/// needs anything unlocked would turn every one of those reconnects into the
/// same silent failure this whole path exists to remove.
#[cfg(unix)]
fn ensure_peer_ssh_key() -> io::Result<PathBuf> {
    let key = peer_ssh_key_path();
    if key.is_file() {
        return Ok(key);
    }

    if let Some(parent) = key.parent() {
        fs::create_dir_all(parent)?;
    }

    let output = Command::new("ssh-keygen")
        .arg("-t")
        .arg("ed25519")
        .arg("-N")
        .arg("")
        .arg("-C")
        .arg(peer_ssh_key_comment())
        .arg("-f")
        .arg(&key)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| io::Error::new(err.kind(), format!("could not run ssh-keygen: {err}")))?;
    if !output.status.success() {
        return Err(command_failed(
            "could not generate herdr's ssh key",
            &output,
        ));
    }

    eprintln!("generated {}", key.display());
    Ok(key)
}

/// The `authorized_keys` line to install for herdr's peer key.
///
/// The comment stored on disk is ignored in favour of [`peer_ssh_key_comment`].
/// Keys generated by earlier herdr versions carry a comment that names no
/// client, so reading it back would leave those installs unable to recognise
/// their own entry on the target forever.
#[cfg(unix)]
fn peer_ssh_key_line() -> io::Result<String> {
    let path = peer_ssh_public_key_path();
    let text = fs::read_to_string(&path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("could not read {}: {err}", path.display()),
        )
    })?;

    let mut fields = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .split_whitespace();
    let (Some(key_type), Some(blob)) = (fields.next(), fields.next()) else {
        return Err(io::Error::other(format!(
            "{} does not hold an ssh public key",
            path.display()
        )));
    };

    Ok(format!("{key_type} {blob} {}", peer_ssh_key_comment()))
}

/// Installs herdr's public key on `destination`.
///
/// `ssh-copy-id` is deliberately not used. It can only append, so it cannot
/// retire the entry a previous key of this client left behind, and pairing it
/// with a separate cleanup connection would ask the human for their password
/// twice. The script below does both in the one connection, and handles the
/// remote directory permissions `ssh-copy-id` was otherwise wanted for.
///
/// Stdout and stderr are inherited so ssh's password prompt reaches the
/// terminal this process is attached to. This is the one moment a human types
/// their password; everything afterwards authenticates with the key.
#[cfg(unix)]
fn install_peer_ssh_key(destination: &str) -> io::Result<()> {
    install_peer_ssh_key_over_ssh(destination, &peer_ssh_key_line()?)
}

#[cfg(unix)]
/// Runs the install script on the peer.
///
/// The script goes over stdin while stdout and stderr stay inherited: ssh
/// reads a password from `/dev/tty`, not stdin, so piping the script in does
/// not cost the prompt its terminal.
fn install_peer_ssh_key_over_ssh(destination: &str, key_line: &str) -> io::Result<()> {
    let mut child = Command::new("ssh")
        .arg("-T")
        .arg(destination)
        .arg("/bin/sh -s")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| io::Error::new(err.kind(), format!("could not run ssh: {err}")))?;

    let write_result = if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(peer_ssh_key_install_script(key_line.trim()).as_bytes())
            .and_then(|()| stdin.flush())
    } else {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "ssh key install stdin missing",
        ))
    };
    let status = child.wait()?;
    write_result?;

    if !status.success() {
        return Err(io::Error::other(format!(
            "could not install herdr's ssh key on {destination}: {status}"
        )));
    }
    Ok(())
}

#[cfg(unix)]
/// Replaces this client's entry in `authorized_keys` rather than adding one.
///
/// Every line herdr has ever installed for this client is dropped before the
/// current key is written back, so a key that was regenerated — after the
/// config directory was cleared, say — retires the entry its predecessor left
/// rather than stranding it there with nothing able to remove it later.
///
/// What it will not touch is anything it cannot prove it owns. Lines carrying
/// options are left alone even when their comment matches, because herdr never
/// writes options and a line that has them was edited by hand. Another
/// machine's herdr key carries that machine's comment and survives untouched;
/// only [`PEER_SSH_LEGACY_KEY_COMMENT`], which no herdr version wrote from more
/// than one client without also being replaceable this way, is claimed on sight
/// so that upgrading migrates the old entry instead of doubling it.
///
/// The file is rebuilt beside itself and moved into place, so a connection that
/// drops midway leaves the original `authorized_keys` intact rather than a
/// half-written one.
fn peer_ssh_key_install_script(key_line: &str) -> String {
    let blob = key_line.split_whitespace().nth(1).unwrap_or_default();
    let comment = key_line
        .split_whitespace()
        .skip(2)
        .collect::<Vec<_>>()
        .join(" ");

    format!(
        "set -eu\n\
         umask 077\n\
         mkdir -p \"$HOME/.ssh\"\n\
         chmod 700 \"$HOME/.ssh\"\n\
         auth=\"$HOME/.ssh/authorized_keys\"\n\
         tmp=\"$auth.herdr.$$\"\n\
         trap 'rm -f \"$tmp\"' EXIT\n\
         key={key}\n\
         blob={blob}\n\
         comment={comment}\n\
         legacy={legacy}\n\
         touch \"$auth\"\n\
         awk -v blob=\"$blob\" -v comment=\"$comment\" -v legacy=\"$legacy\" '\n\
         $1 !~ /^(ssh-|ecdsa-|sk-)/ {{ print; next }}\n\
         $2 == blob {{ next }}\n\
         {{\n\
         found = \"\";\n\
         for (i = 3; i <= NF; i++) found = found (i > 3 ? \" \" : \"\") $i;\n\
         if (found == comment || found == legacy) next;\n\
         print;\n\
         }}\n\
         ' \"$auth\" > \"$tmp\"\n\
         printf '%s\\n' \"$key\" >> \"$tmp\"\n\
         chmod 600 \"$tmp\"\n\
         mv \"$tmp\" \"$auth\"\n",
        key = shell_quote(key_line),
        blob = shell_quote(blob),
        comment = shell_quote(&comment),
        legacy = shell_quote(PEER_SSH_LEGACY_KEY_COMMENT),
    )
}

#[cfg(unix)]
/// Same shape as [`confirm_remote_install`]'s prompt, defaulting to yes.
fn confirm_peer_setup(prompt: &str) -> io::Result<bool> {
    eprint!("{prompt}");
    io::stderr().flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(!matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "n" | "no"
    ))
}

#[cfg(unix)]
/// A private directory removed once everything inside it is gone.
struct PrivateDir(PathBuf);

#[cfg(unix)]
impl PrivateDir {
    fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(unix)]
impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Creates a fresh user-only (`0700`) directory whose longest child will be
/// `longest_child`, returning its path.
///
/// Using a private directory created with fail-if-exists semantics — rather
/// than a predictable file in the world-writable temp dir — stops a local user
/// from pre-planting a symlink or world-writable file that herdr would write
/// and then read back (`ssh -F`) or connect to.
///
/// `longest_child` is what the socket-path length budget is checked against, so
/// a base directory that cannot fit the eventual socket is skipped before
/// anything is created.
#[cfg(unix)]
fn private_runtime_dir(prefix: &str, longest_child: &str) -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let mut bases = vec![std::env::temp_dir()];
    let short_tmp = PathBuf::from("/tmp");
    if bases.first() != Some(&short_tmp) {
        bases.push(short_tmp);
    }

    let mut last_error = None;
    for base in bases {
        for attempt in 0..100 {
            let dir = base.join(format!("{prefix}-{}-{attempt}", std::process::id()));
            if !fits_unix_socket_path(&dir.join(longest_child)) {
                continue;
            }
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(dir),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    last_error = Some(err);
                    break;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("failed to create private herdr directory under {prefix}"),
        )
    }))
}

/// Quotes a path for an ssh_config `Include` so a path containing spaces (or
/// glob metacharacters) is treated as one literal token instead of being split
/// or expanded by ssh — otherwise the user's config might not be Included and
/// herdr's fallback would wrongly take effect.
fn ssh_config_quote(path: &str) -> String {
    format!("\"{path}\"")
}

fn ssh_config_include_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '\\' {
        ssh_config_quote(&path.replace('\\', "/"))
    } else {
        ssh_config_quote(&path)
    }
}

/// Builds a temporary ssh config that includes the user's settings first, so
/// OpenSSH's first-value-wins behavior preserves explicit user keepalives.
fn write_managed_ssh_config() -> io::Result<ManagedSshConfig> {
    let paths = crate::platform::remote_ssh_config_paths();
    let dir = crate::platform::create_remote_ssh_config_dir(SSH_CONTROL_SOCKET_NAME)?;
    let path = dir.join("config");
    let control_path = paths
        .multiplexing
        .then(|| dir.join(SSH_CONTROL_SOCKET_NAME));

    let mut contents = String::new();
    if let Some(user_config) = paths.user_config.filter(|path| path.is_file()) {
        contents.push_str(&format!(
            "Include {}\n",
            ssh_config_include_path(&user_config)
        ));
    }
    if let Some(system_config) = paths.system_config.filter(|path| path.is_file()) {
        contents.push_str(&format!(
            "Include {}\n",
            ssh_config_include_path(&system_config)
        ));
    }
    contents.push_str("Host *\n");
    contents.push_str("  ServerAliveInterval 15\n");
    contents.push_str("  ServerAliveCountMax 4\n");

    let write_result = (|| {
        let mut file = crate::platform::create_remote_ssh_config_file(&path)?;
        file.write_all(contents.as_bytes())
    })();
    if let Err(err) = write_result {
        let _ = fs::remove_dir_all(&dir);
        return Err(err);
    }
    Ok(ManagedSshConfig {
        options: ManagedSshOptions {
            config_path: path,
            control_path,
        },
    })
}

#[cfg(unix)]
fn bridge_connection(
    stream: crate::ipc::LocalStream,
    target: &str,
    remote_herdr: &RemoteHerdr,
    session_name: &str,
    socket: BridgeSocket,
    ssh_options: Option<&ManagedSshOptions>,
    should_stop: &AtomicBool,
) -> io::Result<()> {
    let mut command = Command::new("ssh");
    apply_managed_ssh_options(&mut command, ssh_options);
    command
        .arg("-T")
        .arg(target)
        .arg(remote_bridge_command(remote_herdr, session_name, socket));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = command
        .spawn()
        .map_err(|err| io::Error::new(err.kind(), format!("failed to start ssh bridge: {err}")))?;
    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ssh bridge stdin missing"))?;
    let mut child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ssh bridge stdout missing"))?;
    let mut stream_to_child = stream.try_clone()?;
    let mut child_to_stream = stream.try_clone()?;

    let upload = thread::spawn(move || {
        let _ = copy_flush(&mut stream_to_child, &mut child_stdin);
    });
    let download = thread::spawn(move || {
        let _ = copy_flush(&mut child_stdout, &mut child_to_stream);
        let _ = crate::ipc::shutdown_local_stream_write(&child_to_stream);
    });

    // Polled rather than a blocking wait so a stopping bridge can cut this
    // connection short. A terminal attach streams for as long as its pane is
    // open, so waiting for the remote side to finish on its own would pin the
    // bridge's teardown — and whoever is joining it — to the pane's lifetime.
    let status = loop {
        if should_stop.load(Ordering::Acquire) {
            let _ = child.kill();
        }
        match child.try_wait()? {
            Some(status) => break status,
            None => thread::sleep(BRIDGE_ACCEPT_POLL),
        }
    };

    // Releases both copy threads. `upload` is parked reading the local socket
    // whenever the local peer is idle, so without this the joins below would
    // wait on traffic that is never coming.
    let _ = crate::ipc::shutdown_local_stream(&stream);
    let _ = upload.join();
    let _ = download.join();

    // A killed child reports failure; that is this bridge shutting down, not a
    // connection that broke.
    if status.success() || should_stop.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!("ssh bridge exited with {status}"),
        ))
    }
}

#[cfg(windows)]
fn bridge_connection(
    stream: crate::ipc::LocalStream,
    target: &str,
    remote_herdr: &RemoteHerdr,
    session_name: &str,
    socket: BridgeSocket,
    ssh_options: Option<&ManagedSshOptions>,
    bridge_stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    let mut command = Command::new("ssh");
    apply_managed_ssh_options(&mut command, ssh_options);
    command
        .arg("-T")
        .arg(target)
        .arg(remote_bridge_command(remote_herdr, session_name, socket))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = command
        .spawn()
        .map_err(|err| io::Error::new(err.kind(), format!("failed to start ssh bridge: {err}")))?;
    let mut child_stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => return terminate_bridge_child(child, "ssh bridge stdin missing"),
    };
    let mut child_stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => return terminate_bridge_child(child, "ssh bridge stdout missing"),
    };
    let stream_to_child = match stream.try_clone() {
        Ok(stream) => stream,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };
    if let Err(err) = stream.set_nonblocking(true) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }
    let mut child_to_stream = stream;

    let connection_stop = Arc::new(AtomicBool::new(false));
    let upload_stop = Arc::new(AtomicBool::new(false));
    let upload_failed = Arc::new(AtomicBool::new(false));
    let download_done = Arc::new(AtomicBool::new(false));
    let client_closed = Arc::new(AtomicBool::new(false));
    let upload_cancel = Arc::clone(&upload_stop);
    let upload_bridge_stop = Arc::clone(bridge_stop);
    let upload_failed_worker = Arc::clone(&upload_failed);
    let upload_client_closed = Arc::clone(&client_closed);
    let upload = thread::spawn(move || {
        let result = copy_local_stream_to_writer(
            stream_to_child,
            &mut child_stdin,
            &upload_cancel,
            &upload_bridge_stop,
            &upload_client_closed,
        );
        upload_failed_worker.store(result.is_err(), Ordering::Release);
        result
    });
    let download_stop = Arc::clone(&connection_stop);
    let download_bridge_stop = Arc::clone(bridge_stop);
    let download_done_worker = Arc::clone(&download_done);
    let download_upload_stop = Arc::clone(&upload_stop);
    let download = thread::spawn(move || {
        let result = copy_reader_to_local_stream(
            &mut child_stdout,
            &mut child_to_stream,
            &download_stop,
            &download_bridge_stop,
        );
        download_done_worker.store(true, Ordering::Release);
        download_upload_stop.store(true, Ordering::Release);
        result
    });

    let mut stopped_at = None;
    let (status_result, child_exited) = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                upload_stop.store(true, Ordering::Release);
                break (Ok(status), true);
            }
            Ok(None) => {}
            Err(err) => {
                connection_stop.store(true, Ordering::Release);
                upload_stop.store(true, Ordering::Release);
                let _ = child.kill();
                let _ = child.wait();
                break (Err(err), false);
            }
        }
        if bridge_stop.load(Ordering::Acquire) {
            connection_stop.store(true, Ordering::Release);
            upload_stop.store(true, Ordering::Release);
            let _ = child.kill();
            break (child.wait(), false);
        }
        if client_closed.load(Ordering::Acquire)
            || upload_failed.load(Ordering::Acquire)
            || download_done.load(Ordering::Acquire)
        {
            upload_stop.store(true, Ordering::Release);
            let stopped_at = stopped_at.get_or_insert_with(Instant::now);
            if stopped_at.elapsed() >= Duration::from_millis(250) {
                connection_stop.store(true, Ordering::Release);
                let _ = child.kill();
                break (child.wait(), false);
            }
        }
        thread::sleep(BRIDGE_ACCEPT_POLL);
    };
    upload_stop.store(true, Ordering::Release);
    if !child_exited {
        connection_stop.store(true, Ordering::Release);
    }
    let upload_result = upload
        .join()
        .map_err(|_| io::Error::other("remote bridge upload worker panicked"))?;
    let download_result = download
        .join()
        .map_err(|_| io::Error::other("remote bridge download worker panicked"))?;
    let status = status_result?;

    let stopping = bridge_stop.load(Ordering::Acquire);
    let client_closed = client_closed.load(Ordering::Acquire);
    if !stopping && !client_closed {
        upload_result.map_err(|err| {
            io::Error::new(err.kind(), format!("remote bridge upload failed: {err}"))
        })?;
        download_result.map_err(|err| {
            io::Error::new(err.kind(), format!("remote bridge download failed: {err}"))
        })?;
    }

    if status.success() || stopping || client_closed {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!("ssh bridge exited with {status}"),
        ))
    }
}

#[cfg(unix)]
fn copy_flush<R: io::Read, W: io::Write>(reader: &mut R, writer: &mut W) -> io::Result<u64> {
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0;

    loop {
        let bytes_read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(total),
            Ok(bytes_read) => bytes_read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };

        writer.write_all(&buffer[..bytes_read])?;
        writer.flush()?;
        total += bytes_read as u64;
    }
}

#[cfg(windows)]
fn terminate_bridge_child(mut child: std::process::Child, message: &'static str) -> io::Result<()> {
    let _ = child.kill();
    let _ = child.wait();
    Err(io::Error::new(io::ErrorKind::BrokenPipe, message))
}

#[cfg(windows)]
fn copy_reader_to_local_stream<R: io::Read>(
    reader: &mut R,
    stream: &mut crate::ipc::LocalStream,
    connection_stop: &AtomicBool,
    bridge_stop: &AtomicBool,
) -> io::Result<u64> {
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0;

    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(total),
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        let mut written = 0;
        while written < read {
            if connection_stop.load(Ordering::Acquire) || bridge_stop.load(Ordering::Acquire) {
                return Ok(total);
            }
            let chunk_len = (read - written).min(4 * 1024);
            match stream.write(&buffer[written..written + chunk_len]) {
                Ok(0) => thread::sleep(BRIDGE_IO_POLL),
                Ok(count) => written += count,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(BRIDGE_IO_POLL);
                }
                Err(err) => return Err(err),
            }
        }
        stream.flush()?;
        total += read as u64;
    }
}

#[cfg(windows)]
fn copy_local_stream_to_writer<W: io::Write>(
    mut stream: crate::ipc::LocalStream,
    writer: &mut W,
    connection_stop: &AtomicBool,
    bridge_stop: &AtomicBool,
    client_closed: &AtomicBool,
) -> io::Result<u64> {
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0;

    while !connection_stop.load(Ordering::Acquire) && !bridge_stop.load(Ordering::Acquire) {
        match crate::ipc::poll_local_stream_read_count(&mut stream, &mut buffer)? {
            crate::ipc::LocalStreamReadCount::Data(read) => {
                writer.write_all(&buffer[..read])?;
                writer.flush()?;
                total += read as u64;
            }
            crate::ipc::LocalStreamReadCount::Pending => thread::sleep(BRIDGE_IO_POLL),
            crate::ipc::LocalStreamReadCount::Closed => {
                client_closed.store(true, Ordering::Release);
                break;
            }
        }
    }

    Ok(total)
}

fn run_client_process(
    local_socket: &Path,
    reattach_command: &str,
    keybindings: RemoteKeybindings,
) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let status = Command::new(exe)
        .arg("client")
        .env(
            crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
            local_socket,
        )
        .env("HERDR_RENDER_ENCODING", "terminal-ansi")
        .env(REATTACH_COMMAND_ENV_VAR, reattach_command)
        .env(REMOTE_KEYBINDINGS_ENV_VAR, keybindings.as_str())
        .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            format!("remote client exited with {status}"),
        ))
    }
}

fn local_forward_socket_path(target: &str, session_name: &str) -> PathBuf {
    let pid = std::process::id();
    let target_clean = sanitize_path_component(target);
    let session_clean = sanitize_path_component(session_name);
    let readable_name = format!("herdr-remote-{pid}-{target_clean}-{session_clean}.sock");
    let target_prefix: String = target_clean.chars().take(8).collect();
    let hash = short_socket_hash(target, session_name);
    let short_name = format!("herdr-r-{pid}-{target_prefix}-{hash}.sock");
    crate::platform::remote_bridge_endpoint_path(&readable_name, &short_name)
}

#[cfg(unix)]
fn fits_unix_socket_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().len() <= 103
}

fn short_socket_hash(target: &str, session: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    target.hash(&mut hasher);
    0u8.hash(&mut hasher);
    session.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn sanitize_path_component(input: &str) -> String {
    let sanitized: String = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect();

    sanitized.trim_matches('-').chars().take(32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn ssh_output(code: i32, stderr: &str) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn ssh_permission_denied_is_an_auth_failure() {
        assert!(ssh_failed_to_authenticate(&ssh_output(
            255,
            "spark343@host: Permission denied (publickey,password)."
        )));
        assert!(ssh_failed_to_authenticate(&ssh_output(
            255,
            "Received disconnect from host: Too many authentication failures"
        )));
    }

    /// 255 is also what an unreachable host produces, and that one is worth
    /// retrying — so the exit status alone must not classify.
    #[test]
    fn ssh_unreachable_is_not_an_auth_failure() {
        assert!(!ssh_failed_to_authenticate(&ssh_output(
            255,
            "ssh: connect to host brainiac port 22: No route to host"
        )));
        assert!(!ssh_failed_to_authenticate(&ssh_output(
            255,
            "ssh: Could not resolve hostname nope: Name or service not known"
        )));
    }

    /// A remote command that fails on its own terms is not an ssh problem.
    #[test]
    fn a_failing_remote_command_is_not_an_auth_failure() {
        assert!(!ssh_failed_to_authenticate(&ssh_output(
            1,
            "uname: command not found"
        )));
    }

    /// The key text reaches the remote shell as one quoted word, so a key
    /// comment containing spaces or quotes cannot break out of the assignment.
    #[test]
    fn key_install_script_quotes_the_key() {
        let script = peer_ssh_key_install_script("ssh-ed25519 AAAA it's mine");
        assert!(script.contains(r#"key='ssh-ed25519 AAAA it'\''s mine'"#));
    }

    /// A client is named by where it runs, not by the key it currently holds,
    /// so the comment still matches after the key is regenerated.
    #[test]
    fn key_comment_names_the_client() {
        let comment = peer_ssh_key_comment();
        assert!(comment.starts_with("herdr-peer "));
        assert!(comment.contains(crate::config::app_dir_name()));
        assert!(comment.contains('@'));
    }

    /// `awk -v` reads a backslash in a value as an escape, and a space would
    /// split the comment across `authorized_keys` fields.
    #[test]
    fn identity_components_drop_characters_that_would_break_the_script() {
        assert_eq!(sanitized_identity_component("a b\\c'd", "fallback"), "abcd");
        assert_eq!(sanitized_identity_component("  ", "fallback"), "fallback");
        assert_eq!(
            sanitized_identity_component("host.local-1_x", "f"),
            "host.local-1_x"
        );
        assert_eq!(
            sanitized_identity_component(&"a".repeat(200), "f").len(),
            PEER_SSH_IDENTITY_COMPONENT_LIMIT
        );
    }

    /// Runs the install script against a throwaway `$HOME` and reports the
    /// `authorized_keys` it left behind.
    fn run_key_install_script(name: &str, existing: &str, key_line: &str) -> String {
        let home = std::env::temp_dir().join(format!(
            "herdr-authorized-keys-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        let ssh_dir = home.join(".ssh");
        fs::create_dir_all(&ssh_dir).expect("temp ssh dir");
        let authorized_keys = ssh_dir.join("authorized_keys");
        if !existing.is_empty() {
            fs::write(&authorized_keys, existing).expect("seed authorized_keys");
        }

        let output = Command::new("sh")
            .arg("-c")
            .arg(peer_ssh_key_install_script(key_line))
            .env("HOME", &home)
            .output()
            .expect("run install script");
        assert!(
            output.status.success(),
            "install script failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let installed = fs::read_to_string(&authorized_keys).expect("read authorized_keys");
        let _ = fs::remove_dir_all(&home);
        installed
    }

    /// The whole point of the rewrite: a client that regenerated its key takes
    /// the entry its previous key left with it instead of adding a second one.
    #[test]
    fn key_install_replaces_this_clients_previous_entry() {
        let comment = peer_ssh_key_comment();
        let installed = run_key_install_script(
            "replaces-own",
            &format!("ssh-ed25519 STALEKEY {comment}\n"),
            &format!("ssh-ed25519 FRESHKEY {comment}"),
        );

        assert_eq!(installed, format!("ssh-ed25519 FRESHKEY {comment}\n"));
    }

    /// Upgrading migrates the entry older herdr versions wrote rather than
    /// leaving it beside the new one with nothing able to remove it.
    #[test]
    fn key_install_claims_the_legacy_entry() {
        let comment = peer_ssh_key_comment();
        let installed = run_key_install_script(
            "claims-legacy",
            "ssh-ed25519 OLDHERDRKEY herdr-peer\n",
            &format!("ssh-ed25519 FRESHKEY {comment}"),
        );

        assert_eq!(installed, format!("ssh-ed25519 FRESHKEY {comment}\n"));
    }

    /// Another machine's herdr key names that machine, and a hand-edited line
    /// carries options herdr never writes. Neither is this client's to remove.
    #[test]
    fn key_install_leaves_entries_it_does_not_own() {
        let comment = peer_ssh_key_comment();
        let other_client = "ssh-ed25519 OTHERMACHINE herdr-peer herdr/someone@elsewhere";
        let plain = "ssh-rsa UNRELATED me@laptop";
        let restricted = "command=\"true\" ssh-ed25519 RESTRICTED herdr-peer";
        let installed = run_key_install_script(
            "keeps-others",
            &format!("{other_client}\n{plain}\n{restricted}\n"),
            &format!("ssh-ed25519 FRESHKEY {comment}"),
        );

        assert_eq!(
            installed,
            format!("{other_client}\n{plain}\n{restricted}\nssh-ed25519 FRESHKEY {comment}\n")
        );
    }

    /// Re-running setup with the key already installed is a no-op, including
    /// when an earlier run left it under a different comment.
    #[test]
    fn key_install_is_idempotent() {
        let comment = peer_ssh_key_comment();
        let key_line = format!("ssh-ed25519 FRESHKEY {comment}");

        let once = run_key_install_script("idempotent-a", "", &key_line);
        assert_eq!(once, format!("{key_line}\n"));

        let twice = run_key_install_script("idempotent-b", &once, &key_line);
        assert_eq!(twice, once);

        let renamed = run_key_install_script(
            "idempotent-c",
            "ssh-ed25519 FRESHKEY renamed by hand\n",
            &key_line,
        );
        assert_eq!(renamed, format!("{key_line}\n"));
    }

    /// A file that was never there is created rather than reported missing.
    #[test]
    fn key_install_creates_authorized_keys() {
        let comment = peer_ssh_key_comment();
        let installed =
            run_key_install_script("creates", "", &format!("ssh-ed25519 FRESHKEY {comment}"));

        assert_eq!(installed, format!("ssh-ed25519 FRESHKEY {comment}\n"));
    }

    /// The peer key rides as an explicit `-i` so it applies whether or not a
    /// managed ssh config was written.
    #[test]
    fn peer_identity_is_passed_on_the_command_line() {
        let mut ssh = RemoteSsh::new("host".to_string(), false);
        assert!(!command_args(&ssh).iter().any(|arg| arg == "-i"));

        ssh.identity = Some(PathBuf::from("/tmp/peer_id_ed25519"));
        let args = command_args(&ssh);
        let index = args
            .iter()
            .position(|arg| arg == "-i")
            .expect("identity flag");
        assert_eq!(args[index + 1], "/tmp/peer_id_ed25519");
    }

    /// The `--remote` path stays interactive: it runs in a terminal and a
    /// password prompt there is something a human can actually answer.
    #[test]
    fn only_the_peer_path_refuses_prompts() {
        let interactive = RemoteSsh::new("host".to_string(), false);
        assert!(!command_args(&interactive)
            .iter()
            .any(|arg| arg == "BatchMode=yes"));

        let mut peer = RemoteSsh::new("host".to_string(), false);
        peer.batch = true;
        assert!(command_args(&peer).iter().any(|arg| arg == "BatchMode=yes"));
    }

    /// Without this, ssh dialing a peer that has gone away waits out the OS
    /// TCP timeout — around two minutes on Linux — and the server's shutdown
    /// waits with it, because it joins the peer's thread.
    #[test]
    fn a_peer_connect_is_bounded() {
        let interactive = RemoteSsh::new("host".to_string(), false);
        assert!(!command_args(&interactive)
            .iter()
            .any(|arg| arg.starts_with("ConnectTimeout=")));

        let mut peer = RemoteSsh::new("host".to_string(), false);
        peer.batch = true;
        assert!(command_args(&peer)
            .iter()
            .any(|arg| arg == &format!("ConnectTimeout={PEER_SSH_CONNECT_TIMEOUT_SECS}")));
    }

    /// `ConnectTimeout` bounds the TCP connect and nothing after it, so a peer
    /// that answers and then stalls is held only by this. The wait has to end
    /// when the connection it serves does, or shutdown joins a thread that is
    /// parked on a host with no reason to ever reply.
    #[test]
    fn a_peer_ssh_wait_ends_when_the_connection_stops() {
        let running = Arc::new(AtomicBool::new(true));
        let ssh = RemoteSsh::new("host".to_string(), false).cancelled_with(Arc::clone(&running));

        // Stands in for ssh parked on an unresponsive peer: it will not exit on
        // its own inside this test's lifetime.
        let child = Command::new("sleep")
            .arg("120")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn stand-in child");

        let stopper = Arc::clone(&running);
        let stop = thread::spawn(move || {
            thread::sleep(PEER_SSH_CANCEL_POLL * 2);
            stopper.store(false, Ordering::Relaxed);
        });

        let started = Instant::now();
        let output = ssh.wait_for_output(child).expect("wait returns");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "wait outlived the connection it serves"
        );
        assert!(!output.status.success(), "the child was killed, not reaped");

        stop.join().expect("stopper finishes");
    }

    /// Everything that is not a peer keeps waiting the way it always has: a
    /// human at an interactive `--remote` prompt is the one deciding when to
    /// give up.
    #[test]
    fn an_uncancelled_ssh_wait_collects_output() {
        let ssh = RemoteSsh::new("host".to_string(), false);
        let child = Command::new("sh")
            .arg("-c")
            .arg("echo out; echo err >&2")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn stand-in child");

        let output = ssh.wait_for_output(child).expect("wait returns");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "out");
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "err");
    }

    /// The cancellable path drains both pipes rather than only polling for
    /// exit: a child that fills a pipe buffer blocks until it is read, so a
    /// poll that never read would hang on output instead of on the network.
    #[test]
    fn a_cancellable_wait_still_collects_output() {
        let running = Arc::new(AtomicBool::new(true));
        let ssh = RemoteSsh::new("host".to_string(), false).cancelled_with(running);
        let child = Command::new("sh")
            .arg("-c")
            // Comfortably past a pipe buffer, which a wait that only polled
            // would deadlock on.
            .arg("yes herdr | head -c 200000; echo err >&2")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn stand-in child");

        let output = ssh.wait_for_output(child).expect("wait returns");
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 200_000);
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "err");
    }

    fn command_args(ssh: &RemoteSsh) -> Vec<String> {
        ssh.base_command()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn bridge_socket_is_user_only() {
        use std::os::unix::fs::PermissionsExt;

        let socket = std::env::temp_dir().join(format!(
            "herdr-bridge-permissions-test-{}.sock",
            std::process::id()
        ));
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let bridge = SshStdioBridge::start(
            "example".to_string(),
            remote_herdr,
            socket.clone(),
            "default".to_string(),
            BridgeSocket::Client,
            None,
        )
        .expect("start bridge listener");

        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, BRIDGE_SOCKET_PERMISSION_MODE);

        drop(bridge);
        let _ = std::fs::remove_file(socket);
    }

    #[cfg(unix)]
    #[test]
    fn accepted_bridge_stream_is_reset_to_blocking() {
        use std::os::fd::AsRawFd as _;

        fn is_nonblocking(stream: &crate::ipc::LocalStream) -> bool {
            let fd = match stream {
                crate::ipc::LocalStream::UdSocket(stream) => stream.inner().as_raw_fd(),
            };
            // SAFETY: F_GETFL only reads flags from the live descriptor owned by `stream`.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert!(flags >= 0, "fcntl(F_GETFL): {}", io::Error::last_os_error());
            flags & libc::O_NONBLOCK != 0
        }

        let socket = std::env::temp_dir().join(format!(
            "herdr-bridge-blocking-test-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);
        let listener = crate::ipc::bind_private_local_listener(&socket).expect("bind listener");
        let client = crate::ipc::connect_local_stream(&socket).expect("connect client");
        let mut server = listener.accept().expect("accept client");

        crate::ipc::set_local_stream_polling(&mut server, true)
            .expect("force the macOS accepted-stream state");
        assert!(is_nonblocking(&server));
        let server = prepare_remote_bridge_stream(server).expect("prepare bridge stream");
        assert!(!is_nonblocking(&server));

        drop(server);
        drop(client);
        drop(listener);
        let _ = std::fs::remove_file(socket);
    }

    #[cfg(windows)]
    #[test]
    fn windows_bridge_drop_while_waiting_for_client_is_bounded() {
        let socket = local_forward_socket_path("drop-test", "default");
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let bridge = SshStdioBridge::start(
            "example".to_string(),
            remote_herdr,
            socket.clone(),
            "default".to_string(),
            None,
        )
        .expect("start bridge listener");
        let started = Instant::now();

        drop(bridge);

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!socket.exists());
    }

    #[cfg(unix)]
    #[test]
    fn managed_ssh_config_includes_user_config_then_fallback() {
        use std::os::unix::fs::PermissionsExt;

        let managed_config = write_managed_ssh_config().expect("write managed config");
        let path = managed_config.options.config_path.clone();
        let control_path = managed_config
            .options
            .control_path
            .clone()
            .expect("Unix managed config has a control path");
        let contents = std::fs::read_to_string(&path).expect("read keepalive config");

        // herdr's fallback transport settings are present...
        assert!(
            contents.contains("Host *"),
            "config should add a Host * fallback block: {contents}"
        );
        assert!(
            contents.contains("ServerAliveInterval 15"),
            "config should set the keepalive interval: {contents}"
        );
        assert!(
            contents.contains("ServerAliveCountMax 4"),
            "config should set the keepalive count: {contents}"
        );
        assert!(!contents.contains("ControlMaster"));
        assert!(!contents.contains("ControlPersist"));
        assert!(!contents.contains("ControlPath"));
        // ...and any user config is Included (quoted) BEFORE it so
        // first-value-wins keeps the user's own settings.
        if let Some(home) = std::env::var_os("HOME") {
            let user_config = PathBuf::from(home).join(".ssh").join("config");
            if user_config.is_file() {
                let include = format!(
                    "Include {}",
                    ssh_config_quote(&user_config.to_string_lossy())
                );
                let include_at = contents.find(&include).expect("user config Included");
                let fallback_at = contents.find("Host *").expect("fallback present");
                assert!(
                    include_at < fallback_at,
                    "user config must be Included before herdr's fallback: {contents}"
                );
            }
        }

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, BRIDGE_SOCKET_PERMISSION_MODE,
            "keepalive config must be user-only"
        );
        // The config lives in a private 0700 dir, not a predictable temp path.
        let dir = path.parent().expect("config has a parent dir");
        let dir_mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "ssh config dir must be user-only");
        assert!(
            fits_unix_socket_path(&control_path),
            "control socket path must fit portable Unix socket limits"
        );

        drop(managed_config);
    }

    #[test]
    fn ssh_config_quote_wraps_path_with_spaces() {
        assert_eq!(
            ssh_config_quote("/home/a b/.ssh/config"),
            "\"/home/a b/.ssh/config\""
        );
    }

    #[cfg(unix)]
    #[test]
    fn remote_ssh_command_uses_managed_config_when_present() {
        let managed_config = write_managed_ssh_config().expect("write managed config");
        let config_path = managed_config.options.config_path.clone();
        let control_path = managed_config
            .options
            .control_path
            .clone()
            .expect("Unix managed config has a control path");
        let ssh = RemoteSsh {
            target: "example".to_string(),
            managed_config: Some(managed_config),
            identity: None,
            batch: false,
            running: None,
        };

        let command = ssh.command();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            vec![
                "-F".to_string(),
                config_path.to_string_lossy().into_owned(),
                "-S".to_string(),
                control_path.to_string_lossy().into_owned(),
                "-o".to_string(),
                "ControlMaster=auto".to_string(),
                "-o".to_string(),
                "ControlPersist=yes".to_string(),
                "-T".to_string(),
                "example".to_string(),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_managed_ssh_config_uses_keepalives_without_control_socket() {
        let managed_config = write_managed_ssh_config().expect("write managed config");
        let config_path = managed_config.options.config_path.clone();
        assert!(managed_config.options.control_path.is_none());
        let contents = std::fs::read_to_string(&config_path).expect("read managed config");
        assert!(contents.contains("ServerAliveInterval 15"));
        assert!(contents.contains("ServerAliveCountMax 4"));

        let ssh = RemoteSsh {
            target: "example".to_string(),
            managed_config: Some(managed_config),
        };
        let args = ssh
            .command()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            vec![
                "-F".to_string(),
                config_path.to_string_lossy().into_owned(),
                "-T".to_string(),
                "example".to_string(),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_ssh_config_include_uses_forward_slashes() {
        assert_eq!(
            ssh_config_include_path(Path::new(r"C:\Users\A B\.ssh\config")),
            r#""C:/Users/A B/.ssh/config""#
        );
    }

    #[test]
    fn remote_ssh_command_is_plain_without_managed_config() {
        let ssh = RemoteSsh {
            target: "example".to_string(),
            managed_config: None,
            identity: None,
            batch: false,
            running: None,
        };

        let command = ssh.command();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(args, vec!["-T".to_string(), "example".to_string()]);
    }

    #[test]
    fn remote_install_stream_command_avoids_shell_c_wrapper() {
        let command = remote_install_stream_command("/home/a b/.local/bin/herdr.tmp.123");

        assert_eq!(command, "tee '/home/a b/.local/bin/herdr.tmp.123'");
    }

    #[test]
    fn remote_install_prepare_and_commit_scripts_quote_paths() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let prepare = remote_install_prepare_script(&remote_herdr);

        assert!(prepare.contains("mkdir -p \"$dir\""));
        assert!(prepare.contains("printf '%s\\0%s\\0' \"$tmp\" \"$dest\""));
        assert_eq!(
            parse_remote_install_paths(b"/home/a b/herdr.tmp.42\0/home/a b/herdr\0").unwrap(),
            (
                "/home/a b/herdr.tmp.42".to_string(),
                "/home/a b/herdr".to_string()
            )
        );
        assert_eq!(
            parse_remote_install_paths(b"/home/a b\n/herdr.tmp.42\0/home/a b\n/herdr\0").unwrap(),
            (
                "/home/a b\n/herdr.tmp.42".to_string(),
                "/home/a b\n/herdr".to_string()
            )
        );
        assert_eq!(
            remote_install_commit_script("/home/a b/herdr.tmp.42", "/home/a b/herdr"),
            "set -eu\nchmod 755 '/home/a b/herdr.tmp.42'\nmv '/home/a b/herdr.tmp.42' '/home/a b/herdr'\n"
        );
    }

    #[test]
    fn extract_remote_args_removes_space_form() {
        let args = vec![
            "herdr".into(),
            "--remote".into(),
            "dev".into(),
            "--help".into(),
        ];
        let (cleaned, remote) = extract_remote_args(&args).unwrap();
        assert_eq!(cleaned, vec!["herdr", "--help"]);
        let remote = remote.unwrap();
        assert_eq!(remote.target, "dev");
        assert_eq!(remote.keybindings, RemoteKeybindings::Local);
    }

    #[test]
    fn extract_remote_args_removes_equals_form() {
        let args = vec!["herdr".into(), "--remote=user@host".into()];
        let (cleaned, remote) = extract_remote_args(&args).unwrap();
        assert_eq!(cleaned, vec!["herdr"]);
        let remote = remote.unwrap();
        assert_eq!(remote.target, "user@host");
        assert_eq!(remote.keybindings, RemoteKeybindings::Local);
    }

    #[test]
    fn extract_remote_args_accepts_remote_keybindings_server() {
        let args = vec![
            "herdr".into(),
            "--remote".into(),
            "dev".into(),
            "--remote-keybindings=server".into(),
        ];
        let (cleaned, remote) = extract_remote_args(&args).unwrap();
        assert_eq!(cleaned, vec!["herdr"]);
        let remote = remote.unwrap();
        assert_eq!(remote.target, "dev");
        assert_eq!(remote.keybindings, RemoteKeybindings::Server);
    }

    #[test]
    fn extract_remote_args_accepts_remote_keybindings_space_form() {
        let args = vec![
            "herdr".into(),
            "--remote=dev".into(),
            "--remote-keybindings".into(),
            "server".into(),
        ];
        let (cleaned, remote) = extract_remote_args(&args).unwrap();
        assert_eq!(cleaned, vec!["herdr"]);
        assert_eq!(remote.unwrap().keybindings, RemoteKeybindings::Server);
    }

    #[test]
    fn extract_remote_args_accepts_explicit_handoff() {
        let args = vec!["herdr".into(), "--remote=dev".into(), "--handoff".into()];

        let (cleaned, remote) = extract_remote_args(&args).unwrap();

        assert_eq!(cleaned, vec!["herdr"]);
        let remote = remote.unwrap();
        assert_eq!(remote.target, "dev");
        assert!(remote.live_handoff);
    }

    #[test]
    fn extract_remote_args_preserves_child_remote_options_after_separator() {
        let args = vec![
            "herdr".into(),
            "agent".into(),
            "start".into(),
            "repro".into(),
            "--".into(),
            "child".into(),
            "--remote".into(),
            "dev".into(),
            "--remote-keybindings=server".into(),
            "--handoff".into(),
        ];

        let (cleaned, remote) = extract_remote_args(&args).unwrap();

        assert_eq!(cleaned, args);
        assert!(remote.is_none());
    }

    #[test]
    fn extract_remote_args_preserves_handoff_without_remote() {
        let args = vec!["herdr".into(), "update".into(), "--handoff".into()];

        let (cleaned, remote) = extract_remote_args(&args).unwrap();

        assert_eq!(cleaned, args);
        assert!(remote.is_none());
    }

    #[test]
    fn extract_remote_args_rejects_remote_keybindings_without_remote() {
        let args = vec!["herdr".into(), "--remote-keybindings=server".into()];
        let err = extract_remote_args(&args).unwrap_err();
        assert_eq!(err, "--remote-keybindings requires --remote");
    }

    #[test]
    fn extract_remote_args_rejects_duplicate_remote_keybindings() {
        let args = vec![
            "herdr".into(),
            "--remote=dev".into(),
            "--remote-keybindings=local".into(),
            "--remote-keybindings=server".into(),
        ];
        let err = extract_remote_args(&args).unwrap_err();
        assert_eq!(err, "--remote-keybindings can only be specified once");
    }

    #[test]
    fn extract_remote_args_requires_value() {
        let args = vec!["herdr".into(), "--remote".into()];
        let err = extract_remote_args(&args).unwrap_err();
        assert_eq!(err, "missing value for --remote");
    }

    #[test]
    fn extract_remote_args_rejects_empty_value() {
        let args = vec!["herdr".into(), "--remote=".into()];
        let err = extract_remote_args(&args).unwrap_err();
        assert_eq!(err, "missing value for --remote");
    }

    #[test]
    fn extract_remote_args_rejects_duplicate_values() {
        let args = vec![
            "herdr".into(),
            "--remote=dev".into(),
            "--remote=prod".into(),
        ];
        let err = extract_remote_args(&args).unwrap_err();
        assert_eq!(err, "--remote can only be specified once");
    }

    #[test]
    fn extract_remote_args_rejects_option_like_target() {
        let args = vec!["herdr".into(), "--remote".into(), "-oProxyCommand=x".into()];
        let err = extract_remote_args(&args).unwrap_err();
        assert_eq!(err, "--remote target must not start with '-'");
    }

    #[test]
    fn sanitize_path_component_removes_shell_sensitive_chars() {
        assert_eq!(sanitize_path_component("user@host:22"), "user-host-22");
    }

    #[test]
    fn remote_platform_maps_uname_values() {
        assert_eq!(
            RemotePlatform::from_uname("Linux", "amd64")
                .unwrap()
                .asset_key(),
            "linux-x86_64"
        );
        assert_eq!(
            RemotePlatform::from_uname("Darwin", "arm64")
                .unwrap()
                .asset_key(),
            "macos-aarch64"
        );
        assert!(RemotePlatform::from_uname("FreeBSD", "x86_64").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn reattach_command_includes_remote_and_session() {
        assert_eq!(
            reattach_command(
                "target/release/herdr",
                "user@host",
                "work",
                RemoteKeybindings::Local,
                false,
            ),
            "target/release/herdr --remote user@host --session work"
        );
        assert_eq!(
            reattach_command(
                "herdr",
                "host name",
                crate::session::DEFAULT_SESSION_NAME,
                RemoteKeybindings::Local,
                false,
            ),
            "herdr --remote 'host name'"
        );
        assert_eq!(
            reattach_command(
                "herdr",
                "host",
                crate::session::DEFAULT_SESSION_NAME,
                RemoteKeybindings::Server,
                false,
            ),
            "herdr --remote host --remote-keybindings server"
        );
        assert_eq!(
            reattach_command(
                "herdr",
                "host",
                crate::session::DEFAULT_SESSION_NAME,
                RemoteKeybindings::Local,
                true,
            ),
            "herdr --remote host --handoff"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_reattach_command_uses_current_executable() {
        let executable = std::env::current_exe().expect("current test executable");
        assert_eq!(
            reattach_command(
                r"C:\Program Files\Herdr\herdr.exe",
                "host'name",
                "work'name",
                RemoteKeybindings::Local,
                false,
            ),
            format!(
                "& '{}' --remote 'host''name' --session 'work''name'",
                executable.display().to_string().replace('\'', "''")
            )
        );
    }

    #[test]
    fn remote_bridge_command_uses_installed_binary() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        assert_eq!(
            remote_bridge_command(
                &remote_herdr,
                crate::session::DEFAULT_SESSION_NAME,
                BridgeSocket::Client
            ),
            "exec \"$HOME/.local/bin/herdr\" remote-client-bridge"
        );
    }

    #[test]
    fn remote_bridge_command_selects_the_api_subcommand() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        assert_eq!(
            remote_bridge_command(
                &remote_herdr,
                crate::session::DEFAULT_SESSION_NAME,
                BridgeSocket::Api
            ),
            "exec \"$HOME/.local/bin/herdr\" remote-api-bridge"
        );
    }

    #[test]
    fn remote_bridge_command_carries_a_named_session() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        assert_eq!(
            remote_bridge_command(&remote_herdr, "work", BridgeSocket::Api),
            "exec \"$HOME/.local/bin/herdr\" --session work remote-api-bridge"
        );
    }

    /// A peer's two bridged sockets have to sit in the same directory under the
    /// names the server's own derivation produces, or `resolve_peer_connection`
    /// would look for the client socket somewhere nothing is listening.
    #[test]
    fn peer_socket_names_match_the_server_derivation() {
        let dir = Path::new("/tmp/herdr-peer-1-0");
        let derived = crate::server::socket_paths::derive_client_socket_from_api_socket(
            &dir.join(PEER_API_SOCKET_NAME),
        );

        assert_eq!(derived, dir.join(PEER_CLIENT_SOCKET_NAME));
    }

    #[test]
    fn peer_socket_dir_fits_the_socket_path_budget() {
        let dir = PrivateDir(private_peer_socket_dir().expect("create peer socket dir"));

        for name in [PEER_API_SOCKET_NAME, PEER_CLIENT_SOCKET_NAME] {
            let socket = dir.path().join(name);
            assert!(
                fits_unix_socket_path(&socket),
                "{} does not fit sun_path",
                socket.display()
            );
        }
    }

    #[test]
    fn private_dir_is_removed_with_its_contents() {
        let path = {
            let dir = PrivateDir(private_peer_socket_dir().expect("create peer socket dir"));
            let path = dir.path().to_path_buf();
            fs::write(path.join("leftover"), b"x").expect("write leftover file");
            path
        };

        assert!(!path.exists(), "{} was not removed", path.display());
    }

    #[test]
    fn remote_path_discovery_uses_path_binary() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let remote_herdr = remote_herdr_from_path_discovery(&remote_herdr, "/usr/bin/herdr\n")
            .expect("path binary");

        assert_eq!(
            remote_bridge_command(
                &remote_herdr,
                crate::session::DEFAULT_SESSION_NAME,
                BridgeSocket::Client
            ),
            "exec /usr/bin/herdr remote-client-bridge"
        );
    }

    #[test]
    fn remote_path_discovery_quotes_discovered_binary() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let remote_herdr =
            remote_herdr_from_path_discovery(&remote_herdr, "/opt/herdr bin/herdr\n")
                .expect("path binary");

        assert_eq!(
            remote_bridge_command(
                &remote_herdr,
                crate::session::DEFAULT_SESSION_NAME,
                BridgeSocket::Client
            ),
            "exec '/opt/herdr bin/herdr' remote-client-bridge"
        );
    }

    #[test]
    fn remote_path_discovery_uses_macos_path_binary() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "macos",
            arch: "aarch64",
        });
        let remote_herdr =
            remote_herdr_from_path_discovery(&remote_herdr, "/opt/homebrew/bin/herdr\n")
                .expect("path binary");

        assert_eq!(
            remote_bridge_command(
                &remote_herdr,
                crate::session::DEFAULT_SESSION_NAME,
                BridgeSocket::Client
            ),
            "exec /opt/homebrew/bin/herdr remote-client-bridge"
        );
        assert_eq!(remote_herdr.platform.asset_key(), "macos-aarch64");
    }

    #[test]
    fn remote_path_discovery_reads_multiple_absolute_paths() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let candidates = remote_herdrs_from_path_discovery(
            &remote_herdr,
            "/usr/bin/herdr\nbin/herdr\n /opt/herdr bin/herdr\n",
        );

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].shell_path, "/usr/bin/herdr");
        assert_eq!(candidates[1].shell_path, "'/opt/herdr bin/herdr'");
    }

    #[test]
    fn remote_path_discovery_ignores_mise_shims() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let candidates = remote_herdrs_from_path_discovery(
            &remote_herdr,
            "/home/can/.local/share/mise/shims/herdr\n/home/can/.local/share/mise/installs/herdr/0.7.1/bin/herdr\n",
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].shell_path,
            "/home/can/.local/share/mise/installs/herdr/0.7.1/bin/herdr"
        );
    }

    #[test]
    fn known_remote_binary_candidate_script_includes_mise_and_nix_paths() {
        let script = known_remote_binary_candidate_script(&RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });

        assert!(script.contains("emit \"$home/.local/bin/herdr\""));
        assert!(!script.contains("mise/shims/herdr"));
        assert!(script.contains(&format!("version={}", shell_quote(&current_version()))));
        assert!(
            script.contains("emit \"$home/.local/share/mise/installs/herdr/$version/bin/herdr\"")
        );
        assert!(script.contains("emit \"$home/.local/share/mise/installs/herdr/$version/herdr\""));
        assert!(script.contains(
            "emit \"$home/.local/share/mise/installs/github-ogulcancelik-herdr/$version/herdr\""
        ));
        assert!(script.contains("emit \"$home/.nix-profile/bin/herdr\""));
        assert!(script.contains("emit \"/etc/profiles/per-user/$user/bin/herdr\""));
        assert!(script.contains("emit \"/run/current-system/sw/bin/herdr\""));
        assert!(script.contains("emit \"/home/linuxbrew/.linuxbrew/bin/herdr\""));
        assert!(!script.contains("emit \"/opt/homebrew/bin/herdr\""));
    }

    #[test]
    fn known_remote_binary_candidate_script_includes_macos_homebrew_paths() {
        let script = known_remote_binary_candidate_script(&RemotePlatform {
            os: "macos",
            arch: "aarch64",
        });

        assert!(script.contains("emit \"/opt/homebrew/bin/herdr\""));
        assert!(script.contains("emit \"/usr/local/bin/herdr\""));
        assert!(!script.contains("emit \"/home/linuxbrew/.linuxbrew/bin/herdr\""));
    }

    #[test]
    fn remote_path_discovery_quotes_single_quotes_in_discovered_binary() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let remote_herdr =
            remote_herdr_from_path_discovery(&remote_herdr, "/opt/herdr's/bin/herdr\n")
                .expect("path binary");

        assert_eq!(
            remote_bridge_command(
                &remote_herdr,
                crate::session::DEFAULT_SESSION_NAME,
                BridgeSocket::Client
            ),
            "exec '/opt/herdr'\\''s/bin/herdr' remote-client-bridge"
        );
    }

    #[test]
    fn remote_path_discovery_ignores_relative_paths() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let remote_herdr = remote_herdr_from_path_discovery(&remote_herdr, "bin/herdr\n");

        assert!(remote_herdr.is_none());
    }

    #[test]
    fn remote_path_discovery_ignores_empty_output() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let remote_herdr = remote_herdr_from_path_discovery(&remote_herdr, "\n");

        assert!(remote_herdr.is_none());
    }

    #[test]
    fn remote_shell_path_warning_accepts_managed_install() {
        assert!(remote_shell_resolves_managed_install(
            "/home/can/.local/bin/herdr\n"
        ));
        assert!(remote_shell_resolves_managed_install(
            "/Users/can/.local/bin/herdr\n"
        ));
        assert!(!remote_shell_resolves_managed_install(
            "/usr/local/bin/herdr\n"
        ));
        assert!(!remote_shell_resolves_managed_install(""));
    }

    #[test]
    fn parse_client_status_json_reads_protocol() {
        assert_eq!(
            parse_client_status_json(r#"{"version":"x","protocol":8,"binary":"/bin/herdr"}"#)
                .map(|status| status.protocol),
            Some(8)
        );
        assert!(parse_client_status_json(r#"{"protocol":"unknown"}"#).is_none());
    }

    #[test]
    fn parse_remote_server_status_json_reads_running_server() {
        assert_eq!(
            parse_remote_server_status_json(
                r#"{"status":"running","running":true,"version":"0.6.0","protocol":8,"capabilities":{"live_handoff":true,"detached_server_daemon":true}}"#
            )
            .unwrap(),
            RemoteServerStatus::Running {
                version: Some("0.6.0".into()),
                protocol: Some(8),
                live_handoff: true,
                detached_server_daemon: true
            }
        );
    }

    #[test]
    fn parse_remote_server_status_json_treats_missing_capability_as_old_server() {
        assert_eq!(
            parse_remote_server_status_json(
                r#"{"status":"running","running":true,"version":"0.6.0","protocol":8}"#
            )
            .unwrap(),
            RemoteServerStatus::Running {
                version: Some("0.6.0".into()),
                protocol: Some(8),
                live_handoff: false,
                detached_server_daemon: false
            }
        );
    }

    #[test]
    fn parse_remote_server_status_json_reads_stopped_server() {
        assert_eq!(
            parse_remote_server_status_json(
                r#"{"status":"not_running","running":false,"version":null,"protocol":null}"#
            )
            .unwrap(),
            RemoteServerStatus::NotRunning
        );
    }

    #[test]
    fn remote_update_manifest_uses_root_assets_for_latest_version() {
        let manifest: RemoteUpdateManifest = serde_json::from_str(
            r#"{
                "version": "1.2.3",
                "assets": {
                    "linux-x86_64": "https://example.com/latest"
                },
                "sha256": {
                    "linux-x86_64": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                },
                "releases": {
                    "1.2.3": {
                        "assets": {
                            "linux-x86_64": "https://example.com/archive"
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        let release = manifest.release_for_version("1.2.3").unwrap();
        assert_eq!(
            release.assets.get("linux-x86_64").map(RemoteAssetRef::url),
            Some("https://example.com/latest")
        );
        assert_eq!(
            release.sha256.get("linux-x86_64").map(String::as_str),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn remote_update_manifest_reads_archived_release_assets() {
        let manifest: RemoteUpdateManifest = serde_json::from_str(
            r#"{
                "version": "1.2.4",
                "assets": {
                    "linux-x86_64": "https://example.com/latest"
                },
                "releases": {
                    "1.2.3": {
                        "notes": "ignored",
                        "assets": {
                            "linux-x86_64": "https://example.com/archive"
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            manifest
                .release_for_version("1.2.3")
                .and_then(|release| release.assets.get("linux-x86_64"))
                .map(RemoteAssetRef::url),
            Some("https://example.com/archive")
        );
    }

    #[test]
    fn remote_update_manifest_uses_archived_release_protocol() {
        let manifest: RemoteUpdateManifest = serde_json::from_str(
            r#"{
                "version": "1.2.4",
                "protocol": 42,
                "assets": {
                    "linux-x86_64": "https://example.com/latest"
                },
                "releases": {
                    "1.2.3": {
                        "notes": "ignored",
                        "protocol": 41,
                        "assets": {
                            "linux-x86_64": "https://example.com/archive"
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            manifest
                .release_for_version("1.2.3")
                .and_then(|release| release.protocol),
            Some(41)
        );
    }

    #[test]
    fn remote_update_manifest_does_not_inherit_latest_protocol_for_archived_assets() {
        let manifest: RemoteUpdateManifest = serde_json::from_str(
            r#"{
                "version": "1.2.4",
                "protocol": 42,
                "assets": {
                    "linux-x86_64": "https://example.com/latest"
                },
                "releases": {
                    "1.2.3": {
                        "notes": "ignored",
                        "assets": {
                            "linux-x86_64": "https://example.com/archive"
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            manifest
                .release_for_version("1.2.3")
                .and_then(|release| release.protocol),
            None
        );
    }

    #[test]
    fn remote_preview_manifest_falls_back_to_archived_exact_build_assets() {
        let manifest: RemotePreviewManifest = serde_json::from_str(
            r#"{
                "build_id": "2026-06-06-new",
                "protocol": 12,
                "assets": {
                    "linux-x86_64": {
                        "url": "https://example.com/new",
                        "sha256": "new"
                    }
                },
                "builds": {
                    "2026-06-02-old": {
                        "protocol": 11,
                        "assets": {
                            "linux-x86_64": {
                                "url": "https://example.com/old",
                                "sha256": "old"
                            }
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        let (protocol, assets) =
            preview_assets_for_build(&manifest, "2026-06-02-old").expect("archived build");
        let asset = assets.get("linux-x86_64").expect("asset");
        assert_eq!(protocol, 11);
        assert_eq!(asset.url(), "https://example.com/old");
        assert_eq!(asset.sha256(), Some("old"));
    }

    #[test]
    fn remote_server_restart_reason_requires_stop_for_protocol_mismatch() {
        assert_eq!(
            remote_server_restart_reason(Some(&current_version()), Some(0), true, false),
            Some(RemoteServerRestartReason::ProtocolMismatch)
        );
    }

    #[test]
    fn remote_server_restart_reason_allows_unchanged_compatible_server() {
        assert_eq!(
            remote_server_restart_reason(
                Some(&current_version()),
                Some(CURRENT_PROTOCOL),
                true,
                false
            ),
            None
        );
    }

    #[test]
    fn remote_server_restart_reason_requires_restart_for_old_daemon() {
        assert_eq!(
            remote_server_restart_reason(
                Some(&current_version()),
                Some(CURRENT_PROTOCOL),
                false,
                false
            ),
            Some(RemoteServerRestartReason::DaemonDetachMissing)
        );
    }

    #[test]
    fn remote_server_restart_reason_requires_restart_after_helper_update() {
        assert_eq!(
            remote_server_restart_reason(
                Some(&current_version()),
                Some(CURRENT_PROTOCOL),
                true,
                true
            ),
            Some(RemoteServerRestartReason::BinaryUpdated)
        );
    }

    #[test]
    fn remote_server_restart_reason_offers_restart_for_version_mismatch() {
        assert_eq!(
            remote_server_restart_reason(Some("0.0.0"), Some(CURRENT_PROTOCOL), true, false),
            Some(RemoteServerRestartReason::VersionMismatch)
        );
        assert_eq!(
            remote_server_restart_reason(None, Some(CURRENT_PROTOCOL), true, false),
            Some(RemoteServerRestartReason::VersionMismatch)
        );
    }

    #[test]
    fn remote_server_restart_reason_allows_current_server() {
        assert_eq!(
            remote_server_restart_reason(
                Some(&current_version()),
                Some(CURRENT_PROTOCOL),
                true,
                false
            ),
            None
        );
    }

    #[test]
    fn remote_install_plan_keeps_compatible_running_server() {
        assert_eq!(
            remote_install_running_server_plan(
                Some(&current_version()),
                Some(CURRENT_PROTOCOL),
                true,
                false,
                false,
                false
            ),
            RemoteInstallRunningServerPlan::KeepRunning
        );
    }

    #[test]
    fn remote_install_plan_requires_stop_for_old_daemon() {
        assert_eq!(
            remote_install_running_server_plan(
                Some(&current_version()),
                Some(CURRENT_PROTOCOL),
                false,
                true,
                false,
                false
            ),
            RemoteInstallRunningServerPlan::StopRequired(
                RemoteServerRestartReason::DaemonDetachMissing
            )
        );
    }

    #[test]
    fn remote_install_plan_requires_stop_after_helper_update() {
        assert_eq!(
            remote_install_running_server_plan(
                Some(&current_version()),
                Some(CURRENT_PROTOCOL),
                true,
                true,
                false,
                false
            ),
            RemoteInstallRunningServerPlan::StopRequired(RemoteServerRestartReason::BinaryUpdated)
        );
    }

    #[test]
    fn remote_install_plan_requires_stop_for_incompatible_running_server() {
        assert_eq!(
            remote_install_running_server_plan(
                Some("0.0.0"),
                Some(CURRENT_PROTOCOL),
                true,
                true,
                false,
                false
            ),
            RemoteInstallRunningServerPlan::StopRequired(
                RemoteServerRestartReason::VersionMismatch
            )
        );
    }

    #[test]
    fn remote_install_plan_uses_live_handoff_for_incompatible_running_server() {
        assert_eq!(
            remote_install_running_server_plan(
                Some("0.0.0"),
                Some(CURRENT_PROTOCOL),
                true,
                true,
                true,
                true
            ),
            RemoteInstallRunningServerPlan::LiveHandoff
        );
    }

    #[test]
    fn install_source_description_uses_override_binary() {
        let platform = RemotePlatform {
            os: "linux",
            arch: "aarch64",
        };
        assert_eq!(
            install_source_description_for(&platform, Some(Path::new("/tmp/herdr-aarch64")), false),
            "HERDR_REMOTE_BINARY (/tmp/herdr-aarch64)"
        );
    }

    #[test]
    fn install_source_description_uses_local_binary_when_allowed() {
        let platform = RemotePlatform::local();

        assert_eq!(
            install_source_description_for(&platform, None, true),
            "the current local herdr binary"
        );
    }

    #[test]
    fn install_source_description_uses_release_asset_when_local_binary_cannot_seed_remote() {
        let platform = RemotePlatform::local();

        assert_eq!(
            install_source_description_for(&platform, None, false),
            format!(
                "the {} {} asset for {}",
                current_version(),
                current_channel(),
                platform.asset_key()
            )
        );
    }

    #[test]
    fn resolve_install_source_uses_override_binary_without_temporary_cleanup() {
        let platform = RemotePlatform {
            os: "linux",
            arch: "aarch64",
        };
        let source = resolve_install_source(&platform, Some(PathBuf::from("/tmp/herdr-aarch64")))
            .expect("override source");
        assert_eq!(source.path, PathBuf::from("/tmp/herdr-aarch64"));
        assert!(source.temporary_dir.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_local_forward_endpoint_uses_private_state_dir() {
        let path = local_forward_socket_path("user@example.com", "work");
        assert!(path.starts_with(crate::platform::remote_private_temp_base()));
        assert!(path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("herdr-r-")));
    }

    #[cfg(unix)]
    fn remote_env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[cfg(unix)]
    fn socket_path_byte_len(path: &Path) -> usize {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().len()
    }

    #[cfg(unix)]
    #[test]
    fn local_forward_socket_path_uses_readable_name_when_it_fits() {
        let _guard = remote_env_lock().lock().unwrap();
        // Short target + session leave plenty of room — keep the human-
        // readable form so the socket path stays grep-friendly.
        let path = local_forward_socket_path("dev", "default");
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        assert!(
            filename.starts_with("herdr-remote-"),
            "expected readable name, got {filename}"
        );
        assert!(filename.contains("-dev-default."), "got {filename}");
        assert!(
            fits_unix_socket_path(&path),
            "socket path too long: {} ({} bytes)",
            path.display(),
            socket_path_byte_len(&path)
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_forward_socket_path_fits_in_sun_path() {
        let _guard = remote_env_lock().lock().unwrap();
        // Worst case for the readable form: macOS-style 49-char TMPDIR +
        // max-length sanitized components. Should fall back to the hashed
        // short name, which fits under TMPDIR.
        let target = "longish-host.example.com";
        let session = "a-fairly-long-session-name-here";
        let path = local_forward_socket_path(target, session);
        assert!(
            fits_unix_socket_path(&path),
            "socket path too long for sun_path: {} ({} bytes)",
            path.display(),
            socket_path_byte_len(&path)
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_forward_socket_path_falls_back_to_tmp_when_dir_is_long() {
        let _guard = remote_env_lock().lock().unwrap();
        // Force a TMPDIR long enough that even the hashed short name cannot
        // fit inside it. The fallback should drop to /tmp.
        let prior = std::env::var_os("TMPDIR");
        let long_dir = std::env::temp_dir().join("a".repeat(80));
        let _ = fs::create_dir_all(&long_dir);
        std::env::set_var("TMPDIR", &long_dir);

        let path = local_forward_socket_path("longish-host.example.com", "default");
        let fits = fits_unix_socket_path(&path);
        let parent = path.parent().map(Path::to_path_buf);
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        match prior {
            Some(v) => std::env::set_var("TMPDIR", v),
            None => std::env::remove_var("TMPDIR"),
        }
        let _ = fs::remove_dir_all(&long_dir);

        assert!(fits, "fallback path still overflows: {}", path.display());
        assert_eq!(parent.as_deref(), Some(Path::new("/tmp")));
        assert!(
            filename.starts_with("herdr-r-"),
            "expected hashed fallback, got {filename}"
        );
    }

    #[test]
    fn install_source_cleanup_removes_temporary_directory() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-install-source-cleanup-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).expect("create temp dir");
        let path = dir.join("herdr.tmp");
        fs::write(&path, b"test").expect("write temp file");

        InstallSource::temporary(path, dir.clone()).cleanup();

        assert!(!dir.exists());
    }
}
