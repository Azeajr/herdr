mod attach;
#[cfg(unix)]
mod host_unix;

pub(crate) use attach::*;
#[cfg(unix)]
pub(crate) use host_unix::{run_remote_api_bridge, run_remote_client_bridge};

#[cfg(windows)]
pub(crate) fn run_remote_client_bridge() -> std::io::Result<()> {
    Err(std::io::Error::other(
        "remote Windows hosts are not supported yet",
    ))
}

#[cfg(windows)]
pub(crate) fn run_remote_api_bridge() -> std::io::Result<()> {
    Err(std::io::Error::other(
        "remote API bridge is not supported on Windows yet",
    ))
}

/// Why an ssh peer bridge could not be stood up.
#[cfg(windows)]
pub(crate) enum PeerSshBridgeError {
    /// The remote could not be reached. Retrying may work.
    #[allow(dead_code)] // Constructed only by the Unix implementation.
    Unreachable(String),
    /// The remote answered but cannot be federated with as configured.
    Unsupported(String),
}

/// Uninhabited on Windows: ssh peers cannot be bridged there, so no value of
/// this type can exist and its accessor is unreachable rather than a stub that
/// would report a socket path that does not work.
#[cfg(windows)]
pub(crate) enum PeerSshBridge {}

#[cfg(windows)]
impl PeerSshBridge {
    pub(crate) fn api_socket(&self) -> &std::path::Path {
        match *self {}
    }
}

#[cfg(windows)]
pub(crate) fn start_peer_ssh_bridge(
    _destination: &str,
    _session: Option<&str>,
    _running: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<PeerSshBridge, PeerSshBridgeError> {
    Err(PeerSshBridgeError::Unsupported(
        "ssh peers are not supported on Windows yet".to_string(),
    ))
}

#[cfg(windows)]
pub(crate) fn ensure_peer_ssh_ready(_destination: &str, _assume_yes: bool) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "ssh peers are not supported on Windows yet",
    ))
}

#[cfg(windows)]
pub(crate) fn ensure_peer_remote_binary(_destination: &str) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "ssh peers are not supported on Windows yet",
    ))
}

pub(crate) fn print_remote_error_hint(err: &std::io::Error, target: &str) {
    if is_remote_auth_error(err) {
        eprintln!(
            "hint: verify SSH access first with `{}`.",
            ssh_check_command(target)
        );
        eprintln!(
            "hint: if your SSH key has a passphrase, load it into ssh-agent with `ssh-add` before running `herdr --remote`."
        );
    }
}

fn is_remote_auth_error(err: &std::io::Error) -> bool {
    let message = err.to_string();
    message.contains("Permission denied")
        && (message.contains("(publickey")
            || message.contains("(keyboard-interactive")
            || message.contains("(password"))
}

fn ssh_check_command(target: &str) -> String {
    format!("ssh {}", shell_quote(target))
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_auth_error_matches_ssh_auth_denied() {
        let err = std::io::Error::other(
            "remote platform detection failed: user@host: Permission denied (publickey).",
        );

        assert!(is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_matches_keyboard_interactive_denied() {
        let err = std::io::Error::other(
            "remote server status failed: user@host: Permission denied (keyboard-interactive).",
        );

        assert!(is_remote_auth_error(&err));
    }

    #[test]
    fn remote_auth_error_ignores_non_auth_errors() {
        let err = std::io::Error::other("remote platform detection failed: unsupported platform");

        assert!(!is_remote_auth_error(&err));
    }

    #[test]
    fn ssh_check_command_quotes_remote_target() {
        assert_eq!(ssh_check_command("host name"), "ssh 'host name'");
    }
}
