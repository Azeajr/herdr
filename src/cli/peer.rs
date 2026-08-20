//! `herdr peer` — configure federated peer servers from the command line.
//!
//! `peer.*` is otherwise socket-only, so adding a peer meant hand-writing JSON.
//! `list`, `add`, and `remove` all answer with the whole peer list, which is too
//! verbose to be a useful default on a terminal, so they print a table and keep
//! the raw response behind `--json`.

use crate::api::schema::{
    EmptyParams, Method, PeerAddParams, PeerConnectionKind, PeerInfo, PeerRef, PeerTargetSpec,
    PeerWorkspaceCreateParams, PeerWorkspaceOpenParams, Request,
};

/// How long `peer connect` waits for a peer to come up before giving up.
///
/// An ssh peer has to open a bridge, boot a server on the far side and identify
/// it, so this is generous next to the 5s a single peer request gets.
const PEER_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
const PEER_CONNECT_POLL: std::time::Duration = std::time::Duration::from_millis(200);

pub(super) fn run_peer_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_peer_help();
        return Ok(2);
    };

    match subcommand {
        "list" => peer_list(&args[1..]),
        "add" => peer_add(&args[1..]),
        "remove" => peer_remove(&args[1..]),
        "open" => peer_open(&args[1..]),
        "connect" => peer_connect(&args[1..]),
        "setup-ssh" => peer_setup_ssh(&args[1..]),
        "help" | "--help" | "-h" => {
            print_peer_help();
            Ok(0)
        }
        _ => {
            print_peer_help();
            Ok(2)
        }
    }
}

fn peer_list(args: &[String]) -> std::io::Result<i32> {
    let json = match parse_json_flag(args, "usage: herdr peer list [--json]") {
        Ok(json) => json,
        Err(code) => return Ok(code),
    };

    send_peer_list_request(
        "cli:peer:list",
        Method::PeerList(EmptyParams::default()),
        json,
    )
}

fn peer_add(args: &[String]) -> std::io::Result<i32> {
    // The name is optional, so a leading non-flag argument is the only thing
    // that can be one.
    let (name, rest) = match args.first() {
        Some(first) if !first.starts_with('-') => (Some(first.clone()), &args[1..]),
        _ => (None, args),
    };

    let mut socket = None;
    let mut ssh = None;
    let mut session = None;
    let mut json = false;
    let mut assume_yes = false;
    let mut skip_ssh_check = false;

    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--socket" => {
                let Some(value) = rest.get(index + 1) else {
                    eprintln!("missing value for --socket");
                    return Ok(2);
                };
                socket = Some(value.clone());
                index += 2;
            }
            "--ssh" => {
                let Some(value) = rest.get(index + 1) else {
                    eprintln!("missing value for --ssh");
                    return Ok(2);
                };
                ssh = Some(value.clone());
                index += 2;
            }
            // Not `--session`: herdr strips that from argv before a subcommand
            // ever sees it, to pick which local server the CLI talks to.
            "--peer-session" => {
                let Some(value) = rest.get(index + 1) else {
                    eprintln!("missing value for --peer-session");
                    return Ok(2);
                };
                session = Some(value.clone());
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            "--yes" | "-y" => {
                assume_yes = true;
                index += 1;
            }
            "--skip-ssh-check" => {
                skip_ssh_check = true;
                index += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    let destination = ssh.clone();
    let target = match peer_target_from_args(socket, ssh, session) {
        Ok(target) => target,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let name = match name.or_else(|| destination.as_deref().map(peer_name_from_destination)) {
        Some(name) => name,
        None => {
            print_peer_add_usage();
            return Ok(2);
        }
    };

    // Both preflights run here, in the CLI, because this is the only herdr
    // process with a terminal for ssh to prompt against. The server dials the
    // peer with no terminal at all, so anything interactive has to be settled
    // before the peer is handed over to it.
    if let Some(destination) = &destination {
        if !skip_ssh_check {
            if let Err(err) = prepare_ssh_peer(destination, assume_yes) {
                eprintln!("{err}");
                return Ok(1);
            }
        }
    }

    send_peer_list_request(
        "cli:peer:add",
        Method::PeerAdd(PeerAddParams { name, target }),
        json,
    )
}

/// Settles everything an ssh peer needs a human for: key auth, then a matching
/// remote binary.
fn prepare_ssh_peer(destination: &str, assume_yes: bool) -> std::io::Result<()> {
    crate::remote::ensure_peer_ssh_ready(destination, assume_yes)?;
    crate::remote::ensure_peer_remote_binary(destination)
}

/// `herdr peer connect <destination>` — everything between "I have an ssh
/// destination" and "I am looking at a shell on it".
///
/// Adding a peer and opening a view onto it were always two commands with a
/// connection wait in between, and the wait had no command at all.
fn peer_connect(args: &[String]) -> std::io::Result<i32> {
    let Some(destination) = args.first().filter(|arg| !arg.starts_with('-')) else {
        print_peer_connect_usage();
        return Ok(2);
    };

    let mut name = None;
    let mut session = None;
    let mut assume_yes = false;
    let mut skip_ssh_check = false;
    let mut new_workspace = false;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--name" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --name");
                    return Ok(2);
                };
                name = Some(value.clone());
                index += 2;
            }
            "--peer-session" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --peer-session");
                    return Ok(2);
                };
                session = Some(value.clone());
                index += 2;
            }
            "--yes" | "-y" => {
                assume_yes = true;
                index += 1;
            }
            "--skip-ssh-check" => {
                skip_ssh_check = true;
                index += 1;
            }
            "--new" => {
                new_workspace = true;
                index += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    let name = name.unwrap_or_else(|| peer_name_from_destination(destination));

    if !skip_ssh_check {
        if let Err(err) = prepare_ssh_peer(destination, assume_yes) {
            eprintln!("{err}");
            return Ok(1);
        }
    }

    // Adding a peer that is already configured is not an error worth stopping
    // for: connect is the command someone runs again after a reboot.
    if !peer_is_configured(&name)? {
        let response = super::send_request(&Request {
            id: "cli:peer:connect:add".into(),
            method: Method::PeerAdd(PeerAddParams {
                name: name.clone(),
                target: PeerTargetSpec::Ssh {
                    destination: destination.clone(),
                    session,
                },
            }),
        })?;
        if let Some(error) = response.get("error") {
            eprintln!("{error}");
            return Ok(1);
        }
    }

    let peer = match wait_for_connected_peer(&name, PEER_CONNECT_TIMEOUT)? {
        Ok(peer) => peer,
        Err(message) => {
            eprintln!("{message}");
            return Ok(1);
        }
    };

    // A peer with workspaces already has work on it to land in; only a bare one
    // needs something made. `--new` asks for a fresh workspace regardless.
    let focused = peer
        .workspaces
        .iter()
        .find(|workspace| workspace.focused)
        .or_else(|| peer.workspaces.first());
    match focused.filter(|_| !new_workspace) {
        Some(workspace) => super::runtime::peer_workspace_open(PeerWorkspaceOpenParams {
            target: workspace.workspace_id.clone(),
            name: Some(name),
            label: None,
            focus: true,
            takeover: false,
        }),
        None => super::runtime::peer_workspace_create(PeerWorkspaceCreateParams {
            name,
            cwd: None,
            label: None,
            focus: true,
        }),
    }
}

fn peer_is_configured(name: &str) -> std::io::Result<bool> {
    let response = super::send_request(&Request {
        id: "cli:peer:connect:list".into(),
        method: Method::PeerList(EmptyParams::default()),
    })?;
    Ok(peers_from_response(&response)
        .unwrap_or_default()
        .iter()
        .any(|peer| peer.name == name))
}

/// Blocks until the peer reports `connected`, or says why it never will.
///
/// The CLI has no event subscription, so this polls, the same way
/// `herdr agent start` waits for an agent to come up. A peer that lands in
/// `error` is reported immediately rather than waited out: that state means the
/// server has stopped retrying, so the deadline would only add delay.
fn wait_for_connected_peer(
    name: &str,
    timeout: std::time::Duration,
) -> std::io::Result<Result<PeerInfo, String>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let response = super::send_request(&Request {
            id: "cli:peer:connect:wait".into(),
            method: Method::PeerList(EmptyParams::default()),
        })?;
        let peers = peers_from_response(&response).unwrap_or_default();
        match peers.into_iter().find(|peer| peer.name == name) {
            Some(peer) if peer.connection == PeerConnectionKind::Connected => return Ok(Ok(peer)),
            Some(peer) if peer.connection == PeerConnectionKind::Error => {
                return Ok(Err(peer
                    .error
                    .unwrap_or_else(|| format!("peer '{name}' could not be reached"))))
            }
            // Still connecting or retrying. Its last error, if it has one, is
            // the most useful thing to report should the deadline arrive.
            Some(peer) if std::time::Instant::now() >= deadline => {
                let detail = peer
                    .error
                    .map(|error| format!(": {error}"))
                    .unwrap_or_default();
                return Ok(Err(format!(
                    "peer '{name}' did not connect within {}s{detail}",
                    timeout.as_secs()
                )));
            }
            Some(_) => {}
            None => return Ok(Err(format!("peer '{name}' is no longer configured"))),
        }

        std::thread::sleep(PEER_CONNECT_POLL);
    }
}

fn peers_from_response(response: &serde_json::Value) -> Option<Vec<PeerInfo>> {
    serde_json::from_value(response.get("result")?.get("peers")?.clone()).ok()
}

fn peer_setup_ssh(args: &[String]) -> std::io::Result<i32> {
    let Some(destination) = args.first().filter(|arg| !arg.starts_with('-')) else {
        eprintln!("usage: herdr peer setup-ssh <destination> [--yes]");
        return Ok(2);
    };

    let mut assume_yes = false;
    for arg in &args[1..] {
        match arg.as_str() {
            "--yes" | "-y" => assume_yes = true,
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    if let Err(err) = prepare_ssh_peer(destination, assume_yes) {
        eprintln!("{err}");
        return Ok(1);
    }

    println!("{destination} is ready to be added as a peer");
    Ok(0)
}

/// A peer added without a name is named after where it lives.
///
/// The user part is dropped because it identifies an account, not a machine,
/// and two peers on one host would collide on it anyway.
fn peer_name_from_destination(destination: &str) -> String {
    let host = destination
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(destination);
    let host = host.split(':').next().unwrap_or(host);
    let host = host.trim_matches(|ch| ch == '[' || ch == ']');
    if host.is_empty() {
        destination.to_string()
    } else {
        host.to_string()
    }
}

fn peer_remove(args: &[String]) -> std::io::Result<i32> {
    let Some(name) = args.first().filter(|name| !name.starts_with('-')) else {
        eprintln!("usage: herdr peer remove <name> [--json]");
        return Ok(2);
    };

    let json = match parse_json_flag(&args[1..], "usage: herdr peer remove <name> [--json]") {
        Ok(json) => json,
        Err(code) => return Ok(code),
    };

    send_peer_list_request(
        "cli:peer:remove",
        Method::PeerRemove(PeerRef { name: name.clone() }),
        json,
    )
}

fn peer_open(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first().filter(|target| !target.starts_with('-')) else {
        print_peer_open_usage();
        return Ok(2);
    };

    let mut name = None;
    let mut label = None;
    let mut focus = false;
    let mut takeover = false;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--peer" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --peer");
                    return Ok(2);
                };
                name = Some(value.clone());
                index += 2;
            }
            "--label" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --label");
                    return Ok(2);
                };
                label = Some(value.clone());
                index += 2;
            }
            "--focus" => {
                focus = true;
                index += 1;
            }
            "--no-focus" => {
                focus = false;
                index += 1;
            }
            "--takeover" => {
                takeover = true;
                index += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    super::runtime::peer_workspace_open(PeerWorkspaceOpenParams {
        target: target.clone(),
        name,
        label,
        focus,
        takeover,
    })
}

/// Every peer mutation answers with the full peer list, so they all report the
/// same way.
fn send_peer_list_request(id: &'static str, method: Method, json: bool) -> std::io::Result<i32> {
    let response = super::send_request(&Request {
        id: id.into(),
        method,
    })?;
    if json {
        return super::print_response(&response);
    }
    if response.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(&response).unwrap_or_default());
        return Ok(1);
    }

    match serde_json::from_value::<Vec<PeerInfo>>(response["result"]["peers"].clone()) {
        Ok(peers) => {
            print_peer_table(&peers);
            Ok(0)
        }
        // The request succeeded; only the pretty-printing could not read it.
        Err(_) => super::print_response(&response),
    }
}

fn peer_target_from_args(
    socket: Option<String>,
    ssh: Option<String>,
    session: Option<String>,
) -> Result<PeerTargetSpec, String> {
    match (socket, ssh) {
        (Some(_), Some(_)) => Err("use either --socket or --ssh, not both".into()),
        (None, None) => Err("herdr peer add needs --socket PATH or --ssh DESTINATION".into()),
        (Some(_), None) if session.is_some() => {
            Err("--peer-session applies to --ssh peers only".into())
        }
        (Some(path), None) => Ok(PeerTargetSpec::SocketPath { path }),
        (None, Some(destination)) => Ok(PeerTargetSpec::Ssh {
            destination,
            session,
        }),
    }
}

fn parse_json_flag(args: &[String], usage: &str) -> Result<bool, i32> {
    match args {
        [] => Ok(false),
        [flag] if flag == "--json" => Ok(true),
        _ => {
            eprintln!("{usage}");
            Err(2)
        }
    }
}

fn print_peer_table(peers: &[PeerInfo]) {
    println!(
        "{:<20} {:<16} {:<10} target",
        "name", "connection", "workspaces"
    );
    for peer in peers {
        println!(
            "{:<20} {:<16} {:<10} {}",
            peer.name,
            connection_summary(peer),
            peer.workspaces.len(),
            target_summary(&peer.target)
        );
        if let Some(error) = &peer.error {
            println!("{:<20} {error}", "");
        }
    }
}

/// `reconnecting` is not `error`: the attempt count is the difference a reader
/// needs, so it rides along with the state rather than in the error column.
fn connection_summary(peer: &PeerInfo) -> String {
    match peer.attempt {
        Some(attempt) => format!("{} #{attempt}", peer.connection),
        None => peer.connection.to_string(),
    }
}

fn target_summary(target: &PeerTargetSpec) -> String {
    match target {
        PeerTargetSpec::SocketPath { path } => path.clone(),
        PeerTargetSpec::Ssh {
            destination,
            session: Some(session),
        } => format!("ssh {destination} (session {session})"),
        PeerTargetSpec::Ssh {
            destination,
            session: None,
        } => format!("ssh {destination}"),
    }
}

fn print_peer_add_usage() {
    eprintln!(
        "usage: herdr peer add [name] --socket PATH | --ssh DESTINATION [--peer-session NAME] [--yes] [--skip-ssh-check] [--json]"
    );
    eprintln!("       name defaults to the ssh destination's host");
}

fn print_peer_connect_usage() {
    eprintln!(
        "usage: herdr peer connect <destination> [--name NAME] [--peer-session NAME] [--new] [--yes] [--skip-ssh-check]"
    );
    eprintln!("       sets up ssh, adds the peer, waits for it, and opens a workspace");
}

fn print_peer_open_usage() {
    eprintln!(
        "usage: herdr peer open <target> [--peer NAME] [--label TEXT] [--focus] [--no-focus] [--takeover]"
    );
}

fn print_peer_help() {
    eprintln!("herdr peer commands:");
    eprintln!(
        "  herdr peer connect <destination> [--name NAME] [--peer-session NAME] [--new] [--yes]"
    );
    eprintln!("  herdr peer list [--json]");
    eprintln!(
        "  herdr peer add [name] --socket PATH | --ssh DESTINATION [--peer-session NAME] [--yes] [--skip-ssh-check] [--json]"
    );
    eprintln!("  herdr peer remove <name> [--json]");
    eprintln!(
        "  herdr peer open <target> [--peer NAME] [--label TEXT] [--focus] [--no-focus] [--takeover]"
    );
    eprintln!("  herdr peer setup-ssh <destination> [--yes]");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_target_needs_exactly_one_transport() {
        assert!(peer_target_from_args(None, None, None).is_err());
        assert!(
            peer_target_from_args(Some("/tmp/a.sock".into()), Some("host".into()), None).is_err()
        );
    }

    #[test]
    fn socket_peer_rejects_session() {
        assert!(
            peer_target_from_args(Some("/tmp/a.sock".into()), None, Some("work".into())).is_err()
        );
        assert_eq!(
            peer_target_from_args(Some("/tmp/a.sock".into()), None, None),
            Ok(PeerTargetSpec::SocketPath {
                path: "/tmp/a.sock".into()
            })
        );
    }

    #[test]
    fn ssh_peer_carries_its_session() {
        assert_eq!(
            peer_target_from_args(None, Some("host".into()), Some("work".into())),
            Ok(PeerTargetSpec::Ssh {
                destination: "host".into(),
                session: Some("work".into())
            })
        );
    }

    #[test]
    fn peer_name_defaults_to_the_destination_host() {
        assert_eq!(peer_name_from_destination("spark343@brainiac"), "brainiac");
        assert_eq!(peer_name_from_destination("brainiac"), "brainiac");
        assert_eq!(
            peer_name_from_destination("spark343@192.168.0.2"),
            "192.168.0.2"
        );
        // A Host alias is already the name a user thinks in.
        assert_eq!(
            peer_name_from_destination("herdr-selftest"),
            "herdr-selftest"
        );
    }

    #[test]
    fn peer_name_drops_a_port_and_brackets() {
        assert_eq!(peer_name_from_destination("me@host:2222"), "host");
        assert_eq!(peer_name_from_destination("me@[fe80::1]"), "fe80");
    }

    /// A destination that is nothing but a user part has no host to fall back
    /// to, so the whole string stays rather than producing an empty peer name.
    #[test]
    fn peer_name_never_ends_up_empty() {
        assert_eq!(peer_name_from_destination("me@"), "me@");
    }

    #[test]
    fn json_flag_rejects_stray_arguments() {
        assert_eq!(parse_json_flag(&[], "usage"), Ok(false));
        assert_eq!(parse_json_flag(&["--json".to_string()], "usage"), Ok(true));
        assert_eq!(parse_json_flag(&["--wat".to_string()], "usage"), Err(2));
    }

    #[test]
    fn reconnecting_peers_show_their_attempt() {
        let peer = PeerInfo {
            name: "beta".into(),
            label: "beta".into(),
            target: PeerTargetSpec::SocketPath {
                path: "/tmp/b.sock".into(),
            },
            connection: PeerConnectionKind::Reconnecting,
            attempt: Some(3),
            error: None,
            instance_id: None,
            version: None,
            protocol: None,
            stale: true,
            failed_pane_cleanups: 0,
            workspaces: Vec::new(),
        };
        assert_eq!(connection_summary(&peer), "reconnecting #3");

        let connected = PeerInfo {
            connection: PeerConnectionKind::Connected,
            attempt: None,
            ..peer
        };
        assert_eq!(connection_summary(&connected), "connected");
    }

    #[test]
    fn ssh_targets_read_as_destinations() {
        assert_eq!(
            target_summary(&PeerTargetSpec::Ssh {
                destination: "box".into(),
                session: None
            }),
            "ssh box"
        );
        assert_eq!(
            target_summary(&PeerTargetSpec::Ssh {
                destination: "box".into(),
                session: Some("work".into())
            }),
            "ssh box (session work)"
        );
    }
}
