//! Dialog for adding a peer server.
//!
//! Everything an ssh peer needs before it can be added — a key the far side
//! trusts, and a matching herdr binary over there — may have to ask the user
//! something. This process cannot ask: the server has no controlling terminal,
//! which is exactly why the peer path fails so opaquely without help.
//!
//! So this dialog does not add the peer. It collects a destination and opens a
//! pane running `herdr peer connect`, which runs under a real PTY and can
//! therefore prompt for an ssh password, an install approval, or anything else
//! ssh decides to ask. The pane doubles as the progress display, which is why
//! there is no in-flight flag or spinner here: the work is visible where it
//! happens. A successful connect exits the shell, so the workspace closes
//! itself through the ordinary pane-death path; a failed one leaves the pane
//! on screen for the user to read or retry.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::state::{AddPeerField, AddPeerState};
use super::{App, Mode};

impl App {
    pub(crate) fn open_add_peer_dialog(&mut self) {
        self.state.add_peer = Some(AddPeerState::default());
        self.state.mode = Mode::AddPeer;
    }

    pub(crate) fn close_add_peer_dialog(&mut self) {
        self.state.add_peer = None;
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
    }

    pub(crate) fn handle_add_peer_key(&mut self, key: KeyEvent) {
        let recent_count = self
            .state
            .peer_history
            .iter()
            .filter(|entry| entry.target.starts_with("ssh://"))
            .count();
        match key.code {
            KeyCode::Esc => self.close_add_peer_dialog(),
            KeyCode::Tab => {
                if let Some(add) = &mut self.state.add_peer {
                    add.toggle_field(recent_count > 0);
                }
            }
            KeyCode::Up | KeyCode::Down => {
                if let Some(add) = &mut self.state.add_peer {
                    if add.field == AddPeerField::Recent && recent_count > 0 {
                        if key.code == KeyCode::Up {
                            add.recent_selected = add.recent_selected.saturating_sub(1);
                        } else {
                            add.recent_selected =
                                (add.recent_selected + 1).min(recent_count.saturating_sub(1));
                        }
                    } else {
                        add.toggle_field(recent_count > 0);
                    }
                }
            }
            KeyCode::Enter => {
                let fill = self
                    .state
                    .add_peer
                    .as_ref()
                    .filter(|add| add.field == AddPeerField::Recent)
                    .and_then(|add| {
                        self.state
                            .peer_history
                            .iter()
                            .filter(|entry| entry.target.starts_with("ssh://"))
                            .nth(add.recent_selected)
                            .cloned()
                    });
                match fill {
                    Some(entry) => {
                        if let Some(add) = &mut self.state.add_peer {
                            add.fill_from_history(&entry);
                        }
                    }
                    None => self.submit_add_peer(),
                }
            }
            KeyCode::Backspace => {
                if let Some(add) = &mut self.state.add_peer {
                    if add.field == AddPeerField::Recent {
                        add.field = AddPeerField::Destination;
                    }
                    add.active_input_mut().pop();
                }
            }
            KeyCode::Char(ch)
                if (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
                    && !ch.is_control() =>
            {
                if let Some(add) = &mut self.state.add_peer {
                    if add.field == AddPeerField::Recent {
                        add.field = AddPeerField::Destination;
                    }
                }
                self.insert_add_peer_text(&ch.to_string());
            }
            _ => {}
        }
    }

    pub(crate) fn insert_add_peer_text(&mut self, text: &str) {
        if let Some(add) = &mut self.state.add_peer {
            add.active_input_mut().push_str(text);
        }
    }

    /// Opens a pane running `herdr peer connect` for the typed destination.
    pub(crate) fn submit_add_peer(&mut self) {
        let Some(add) = &self.state.add_peer else {
            return;
        };
        let destination = add.destination.trim().to_string();
        if destination.is_empty() {
            if let Some(add) = &mut self.state.add_peer {
                add.error = Some("enter an ssh destination".to_string());
            }
            return;
        }
        let name = add.name.trim().to_string();
        let command = peer_connect_command(&destination, &name);

        // The pane starts in the user's own home rather than wherever the
        // focused workspace happens to point: this command is about another
        // machine, so a working directory here means nothing to it.
        let cwd = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        let ws_idx = match self.create_workspace_with_launch_env(cwd, true, Vec::new()) {
            Ok(ws_idx) => ws_idx,
            Err(err) => {
                if let Some(add) = &mut self.state.add_peer {
                    add.error = Some(format!("could not open a pane: {err}"));
                }
                return;
            }
        };

        if let Some(workspace) = self.state.workspaces.get_mut(ws_idx) {
            workspace.set_custom_name(format!("connect {destination}"));
        }
        self.run_command_in_workspace(ws_idx, &command);
        self.close_add_peer_dialog();
    }

    /// Types `command` into a freshly created workspace's pane.
    ///
    /// Sent as text to the shell rather than launched as the pane's program.
    /// The command ends in `&& exit`, so a successful connect closes the shell
    /// and with it the workspace — the connect already opened the peer's
    /// workspace, and an idle prompt named after it would be leftovers. On
    /// failure the shell outlives the command: the user is left at a prompt in
    /// front of the error instead of watching the pane close over it.
    fn run_command_in_workspace(&mut self, ws_idx: usize, command: &str) {
        let Some(workspace) = self.state.workspaces.get(ws_idx) else {
            return;
        };
        let Some(pane_id) = workspace.focused_pane_id() else {
            return;
        };
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return;
        };
        let _ = runtime.try_send_bytes(bytes::Bytes::from(format!("{command}\n")));
    }
}

/// Builds the command line the pane runs.
///
/// Uses this executable's own path so a debug or otherwise non-installed build
/// drives its own CLI rather than whichever `herdr` happens to be on `PATH`.
///
/// The trailing `&& exit` is what retires the workspace: the pane's program is
/// a shell, not the CLI, so without it a successful connect would leave an idle
/// prompt behind. The exit code already distinguishes the cases — `0` closes
/// the shell, anything else keeps the error on screen.
fn peer_connect_command(destination: &str, name: &str) -> String {
    let program = std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "herdr".to_string());

    let mut command = format!(
        "{} peer connect {}",
        shell_quote(&program),
        shell_quote(destination)
    );
    if !name.is_empty() {
        command.push_str(" --name ");
        command.push_str(&shell_quote(name));
    }
    command.push_str(" && exit");
    command
}

/// A destination is user input heading for a shell, so it is quoted rather
/// than interpolated.
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
    use crate::app::state::AddPeerField;

    #[test]
    fn command_names_the_destination() {
        let command = peer_connect_command("spark343@brainiac", "");
        assert!(command.ends_with(" peer connect spark343@brainiac && exit"));
        assert!(!command.contains("--name"));
    }

    #[test]
    fn an_explicit_name_is_passed_through() {
        let command = peer_connect_command("brainiac", "big-box");
        assert!(command.ends_with(" peer connect brainiac --name big-box && exit"));
    }

    /// A destination is typed by a user and lands in a shell, so it must not be
    /// able to run anything of its own — and the `&& exit` must stay outside
    /// the quoting, or a failure would take the shell with it.
    #[test]
    fn a_hostile_destination_cannot_break_out() {
        let command = peer_connect_command("host; rm -rf ~", "");
        assert!(command.ends_with(r#" peer connect 'host; rm -rf ~' && exit"#));

        let command = peer_connect_command("it's", "");
        assert!(command.ends_with(r#" peer connect 'it'\''s' && exit"#));
    }

    #[test]
    fn tab_moves_between_the_two_fields() {
        let mut state = AddPeerState::default();
        assert_eq!(state.field, AddPeerField::Destination);
        state.active_input_mut().push_str("brainiac");
        state.toggle_field(false);
        assert_eq!(state.field, AddPeerField::Name);
        state.active_input_mut().push_str("big-box");
        assert_eq!(state.destination, "brainiac");
        assert_eq!(state.name, "big-box");
    }

    #[test]
    fn tab_reaches_the_recent_list_only_when_one_is_offered() {
        let mut state = AddPeerState::default();
        state.toggle_field(true);
        assert_eq!(state.field, AddPeerField::Name);
        state.toggle_field(true);
        assert_eq!(state.field, AddPeerField::Recent);
        state.toggle_field(true);
        assert_eq!(state.field, AddPeerField::Destination);

        let mut state = AddPeerState::default();
        state.toggle_field(false);
        state.toggle_field(false);
        assert_eq!(state.field, AddPeerField::Destination);
    }

    #[test]
    fn history_targets_strip_back_to_destinations() {
        use crate::app::state::peer_history_destination;
        assert_eq!(peer_history_destination("ssh://me@box"), "me@box");
        assert_eq!(peer_history_destination("ssh://me@box#work"), "me@box");
    }
}
