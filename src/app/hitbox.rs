//! Hitbox dump: where the TUI thinks its controls are.
//!
//! A scenario that clicks a hand-counted column is guessing. A guess that misses
//! lands on blank space, which dismisses an open menu and dispatches nothing —
//! indistinguishable from a control that does not work, and that has already cost
//! one full false-bug investigation. This writes herdr's own hit rectangles to a
//! file so a test asks *where* a control is and clicks the point herdr itself
//! would resolve back to that control.
//!
//! Two things this deliberately is not:
//!
//! - **Not a cargo feature.** `HERDR_HITBOX_DUMP=<path>` gates it on the
//!   shipping binary, the way `HERDR_LOG` does. A `#[cfg(feature)]` build would
//!   mean testing a binary that is not the one users run.
//! - **Not an API method.** Hitboxes are TUI presentation state, so they go to a
//!   file and never onto the wire — see the runtime/client boundary guardrail in
//!   `CLAUDE.md`.
//!
//! It is written by whichever process computes the view. In the server/thin-client
//! split that is the *server*: it owns `AppState`, renders the frames, and
//! hit-tests the mouse events the client forwards. So the env var goes on the
//! process that renders — the headless server for an attached client, the TUI
//! process itself when it owns its own loop.
//!
//! Every rect here is read from the same helper the mouse handler hit-tests
//! against, never recomputed alongside it. `hitbox_click_resolves_*` tests hold
//! that line: a dump that disagreed with the hit test would be worse than none.

use std::path::{Path, PathBuf};

use ratatui::layout::Rect;
use serde::Serialize;

use crate::app::state::{AppState, ViewLayout};

/// Path to write the dump to, from the environment. Absent = feature off.
pub(crate) const DUMP_PATH_ENV: &str = "HERDR_HITBOX_DUMP";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct HitboxRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl From<Rect> for HitboxRect {
    fn from(rect: Rect) -> Self {
        Self {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        }
    }
}

/// The cell a caller should click to hit the control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct HitboxPoint {
    pub col: u16,
    pub row: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HitboxControl {
    /// Addressable name, e.g. `tab[1]`, `peer[b]`, `menu[0]`.
    pub name: String,
    pub rect: HitboxRect,
    pub click: HitboxPoint,
    /// Visible text, when the control has one. `menu[0]` carries the item label
    /// so a caller can address a menu entry by what it reads rather than by an
    /// index that shifts with the menu's contents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HitboxSnapshot {
    /// Bumped on every written frame, so a reader can tell a refresh from a
    /// stale file that happens to hold the same controls.
    pub seq: u64,
    pub layout: &'static str,
    pub mode: String,
    pub screen: HitboxRect,
    pub controls: Vec<HitboxControl>,
}

impl HitboxSnapshot {
    #[cfg(test)]
    pub(crate) fn control(&self, name: &str) -> Option<&HitboxControl> {
        self.controls.iter().find(|control| control.name == name)
    }
}

/// Centre of `rect`, clamped to stay inside it.
///
/// Odd widths land dead centre; even ones land left of it. Either is inside the
/// rect, which is the only property the hit tests care about.
fn click_point(rect: Rect) -> HitboxPoint {
    HitboxPoint {
        col: rect.x + rect.width.saturating_sub(1) / 2,
        row: rect.y + rect.height.saturating_sub(1) / 2,
    }
}

struct ControlSink {
    controls: Vec<HitboxControl>,
}

impl ControlSink {
    /// Records a control. Zero-sized rects are dropped: nothing can be clicked
    /// there, and emitting them would let a caller "find" a control that is not
    /// on screen.
    fn push(&mut self, name: impl Into<String>, rect: Rect, label: Option<String>) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.controls.push(HitboxControl {
            name: name.into(),
            rect: rect.into(),
            click: click_point(rect),
            label,
        });
    }
}

/// The controls the client would hit-test right now.
///
/// Pure: it reads state and touches no files, so the agreement tests can call it
/// directly.
pub(crate) fn snapshot(state: &AppState) -> HitboxSnapshot {
    let mut sink = ControlSink {
        controls: Vec::new(),
    };

    sink.push("sidebar", state.view.sidebar_rect, None);
    sink.push("sidebar.workspace_list", state.workspace_list_rect(), None);
    sink.push("sidebar.agent_panel", state.agent_panel_rect(), None);
    sink.push("sidebar.footer", state.sidebar_footer_rect(), None);
    sink.push("sidebar.new", state.sidebar_new_button_rect(), None);
    sink.push("sidebar.launcher", state.global_launcher_rect(), None);
    sink.push("terminal", state.view.terminal_area, None);
    sink.push("tab_bar", state.view.tab_bar_rect, None);
    sink.push("tab_scroll_left", state.view.tab_scroll_left_hit_area, None);
    sink.push(
        "tab_scroll_right",
        state.view.tab_scroll_right_hit_area,
        None,
    );
    sink.push("tab_new", state.view.new_tab_hit_area, None);
    sink.push("toast", state.view.toast_hit_area, None);
    sink.push("mobile_header", state.view.mobile_header_rect, None);
    sink.push("mobile_menu", state.view.mobile_menu_hit_area, None);

    for card in &state.view.workspace_card_areas {
        // The runtime-aware label needs the terminal registry, which this has no
        // reason to reach for: the label is a hint for a human reading the dump,
        // and `name` is what a caller addresses.
        let label = state
            .workspaces
            .get(card.ws_idx)
            .map(|ws| ws.display_name_from_terminals(&state.terminals));
        sink.push(format!("workspace[{}]", card.ws_idx), card.rect, label);
    }

    for header in &state.view.peer_header_areas {
        sink.push(
            format!("peer[{}]", header.peer),
            header.rect,
            Some(header.peer.clone()),
        );
    }

    let active_ws = state.active.and_then(|idx| state.workspaces.get(idx));
    for (idx, rect) in state.view.tab_hit_areas.iter().enumerate() {
        let label = active_ws.and_then(|ws| ws.tab_display_name(idx));
        sink.push(format!("tab[{idx}]"), *rect, label);
    }

    if let Some(menu_rect) = state.context_menu_rect() {
        sink.push("menu", menu_rect, None);
        let items = state
            .context_menu
            .as_ref()
            .map(|menu| menu.items())
            .unwrap_or_default();
        for (idx, item) in items.iter().enumerate() {
            if let Some(rect) = state.context_menu_item_rect(idx) {
                sink.push(format!("menu[{idx}]"), rect, Some((*item).to_string()));
            }
        }
    }

    if state.mode == crate::app::state::Mode::GlobalMenu {
        sink.push("global_menu", state.global_menu_rect(), None);
        for (idx, label) in state.global_menu_labels().iter().enumerate() {
            if let Some(rect) = state.global_menu_item_rect(idx) {
                sink.push(
                    format!("global_menu[{idx}]"),
                    rect,
                    Some((*label).to_string()),
                );
            }
        }
    }

    HitboxSnapshot {
        seq: 0,
        layout: match state.view.layout {
            ViewLayout::Desktop => "desktop",
            ViewLayout::Mobile => "mobile",
        },
        mode: format!("{:?}", state.mode),
        screen: state.screen_rect().into(),
        controls: sink.controls,
    }
}

/// Writes [`snapshot`] to the path named by `HERDR_CLIENT_HITBOX_DUMP`.
pub(crate) struct HitboxDump {
    path: PathBuf,
    seq: u64,
    /// Last written controls, always with `seq` still 0 — see `write`.
    last: Option<HitboxSnapshot>,
    warned: bool,
}

impl HitboxDump {
    /// `None` when the environment does not ask for a dump, which is every
    /// normal run.
    pub(crate) fn from_env() -> Option<Self> {
        let path = std::env::var_os(DUMP_PATH_ENV)?;
        if path.is_empty() {
            return None;
        }
        Some(Self::to_path(PathBuf::from(path)))
    }

    pub(crate) fn to_path(path: PathBuf) -> Self {
        Self {
            path,
            seq: 0,
            last: None,
            warned: false,
        }
    }

    /// Writes the current controls unless they are byte-identical to the last
    /// write. Herdr redraws far more often than its chrome moves, so most frames
    /// cost one serialization and no I/O.
    ///
    /// Never fails the render: a dump is a debugging aid, and an unwritable path
    /// must not take the client down. The first failure is logged, the rest are
    /// silent.
    pub(crate) fn write(&mut self, state: &AppState) {
        // Compared before `seq` is stamped: `seq` changes by construction on
        // every frame, so comparing after it would make every frame a rewrite.
        let mut snapshot = snapshot(state);
        if self.last.as_ref() == Some(&snapshot) {
            return;
        }
        let controls = snapshot.clone();
        snapshot.seq = self.seq.saturating_add(1);
        let json = match serde_json::to_string_pretty(&snapshot) {
            Ok(json) => json,
            Err(err) => {
                self.warn_once(format_args!("failed to serialize hitbox dump: {err}"));
                return;
            }
        };
        if let Err(err) = write_atomic(&self.path, &json) {
            let path = self.path.display().to_string();
            self.warn_once(format_args!("failed to write hitbox dump to {path}: {err}"));
            return;
        }
        self.seq = snapshot.seq;
        self.last = Some(controls);
    }

    fn warn_once(&mut self, args: std::fmt::Arguments<'_>) {
        if self.warned {
            return;
        }
        self.warned = true;
        tracing::warn!("{args}");
    }
}

fn write_atomic(path: &Path, json: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A reader polls this file while the client rewrites it, so it is renamed
    // into place rather than truncated: a partial dump would parse as a control
    // that is missing rather than as an error.
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, json)?;
    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::app::state::{ContextMenuKind, ContextMenuState, MenuListState, Mode};
    use crate::workspace::Workspace;

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("herdr-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn state_with(names: &[&str], area: Rect) -> AppState {
        let mut state = AppState::test_new();
        state.workspaces = names.iter().map(|name| Workspace::test_new(name)).collect();
        state.ensure_test_terminals();
        state.active = Some(0);
        state.selected = 0;
        state.mode = Mode::Navigate;
        crate::ui::compute_view(&mut state, area);
        state
    }

    #[test]
    fn hitbox_click_resolves_back_to_the_same_context_menu_item() {
        let mut state = state_with(&["one"], Rect::new(0, 0, 100, 30));
        state.context_menu = Some(ContextMenuState {
            kind: ContextMenuKind::Workspace { ws_idx: 0 },
            x: 4,
            y: 3,
            list: MenuListState::new(0),
        });
        state.mode = Mode::ContextMenu;

        let dump = snapshot(&state);
        let items = state
            .context_menu
            .as_ref()
            .expect("context menu")
            .items()
            .len();
        assert!(items > 1, "the test needs a menu with more than one row");

        for idx in 0..items {
            let control = dump
                .control(&format!("menu[{idx}]"))
                .unwrap_or_else(|| panic!("menu[{idx}] missing from the dump"));
            assert_eq!(
                state.context_menu_item_at(control.click.col, control.click.row),
                Some(idx),
                "clicking the dumped point for menu[{idx}] must hit that item",
            );
        }
    }

    #[test]
    fn hitbox_click_resolves_back_to_the_same_tab_and_workspace_row() {
        let mut state = state_with(&["one", "two"], Rect::new(0, 0, 100, 30));
        state.workspaces[0].test_add_tab(Some("second"));
        state.workspaces[0].test_add_tab(Some("third"));
        state.ensure_test_terminals();
        crate::ui::compute_view(&mut state, Rect::new(0, 0, 100, 30));

        let dump = snapshot(&state);
        for idx in 0..state.workspaces[0].tabs.len() {
            let control = dump
                .control(&format!("tab[{idx}]"))
                .unwrap_or_else(|| panic!("tab[{idx}] missing from the dump"));
            assert_eq!(
                state.tab_at(control.click.col, control.click.row),
                Some(idx),
                "clicking the dumped point for tab[{idx}] must hit that tab",
            );
        }
        for idx in 0..state.workspaces.len() {
            let control = dump
                .control(&format!("workspace[{idx}]"))
                .unwrap_or_else(|| panic!("workspace[{idx}] missing from the dump"));
            assert_eq!(
                state.workspace_at_row(control.click.row),
                Some(idx),
                "clicking the dumped point for workspace[{idx}] must hit that row",
            );
        }
    }

    #[test]
    fn hitbox_click_resolves_back_to_the_same_global_menu_item() {
        let mut state = state_with(&["one"], Rect::new(0, 0, 100, 30));
        state.mode = Mode::GlobalMenu;

        let dump = snapshot(&state);
        for (idx, label) in state.global_menu_labels().iter().enumerate() {
            let control = dump
                .control(&format!("global_menu[{idx}]"))
                .unwrap_or_else(|| panic!("global_menu[{idx}] missing from the dump"));
            assert_eq!(control.label.as_deref(), Some(*label));
            assert!(
                state
                    .global_menu_item_at(control.click.col, control.click.row)
                    .is_some(),
                "clicking the dumped point for global_menu[{idx}] must hit an item",
            );
        }
    }

    #[test]
    fn a_closed_menu_contributes_no_controls() {
        let state = state_with(&["one"], Rect::new(0, 0, 100, 30));
        assert!(state.context_menu.is_none());
        assert!(snapshot(&state)
            .controls
            .iter()
            .all(|control| !control.name.starts_with("menu")
                && !control.name.starts_with("global_menu")));
    }

    #[test]
    fn offscreen_controls_are_not_offered() {
        // A 1-column terminal leaves the tab bar and most chrome with no room.
        // Emitting a zero-width rect would let a caller "find" a control that is
        // not on screen and click a cell that belongs to something else.
        let state = state_with(&["one"], Rect::new(0, 0, 1, 1));
        for control in snapshot(&state).controls {
            assert!(
                control.rect.width > 0 && control.rect.height > 0,
                "{} was emitted with an empty rect",
                control.name,
            );
        }
    }

    #[test]
    fn a_dump_is_rewritten_only_when_the_controls_move() {
        let dir = unique_temp_dir("hitbox-rewrite");
        let path = dir.join("hitbox.json");
        let mut dump = HitboxDump::to_path(path.clone());
        let mut state = state_with(&["one"], Rect::new(0, 0, 100, 30));

        dump.write(&state);
        let first = std::fs::read_to_string(&path).expect("first dump");
        assert!(first.contains("\"seq\": 1"));

        dump.write(&state);
        assert_eq!(
            std::fs::read_to_string(&path).expect("unchanged dump"),
            first,
            "an unchanged frame must not rewrite the file",
        );

        state.mode = Mode::ContextMenu;
        state.context_menu = Some(ContextMenuState {
            kind: ContextMenuKind::Workspace { ws_idx: 0 },
            x: 4,
            y: 3,
            list: MenuListState::new(0),
        });
        dump.write(&state);
        let second = std::fs::read_to_string(&path).expect("second dump");
        assert!(second.contains("\"seq\": 2"));
        assert!(second.contains("menu[0]"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwritable_path_does_not_fail_the_render() {
        let dir = unique_temp_dir("hitbox-unwritable");
        let blocker = dir.join("blocked");
        std::fs::write(&blocker, "not a directory").expect("write blocker");
        let mut dump = HitboxDump::to_path(blocker.join("hitbox.json"));
        let state = state_with(&["one"], Rect::new(0, 0, 100, 30));

        dump.write(&state);
        dump.write(&state);

        assert_eq!(
            std::fs::read_to_string(&blocker).expect("blocker"),
            "not a directory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
