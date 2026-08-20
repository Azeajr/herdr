use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
    Frame,
};

use super::text::{display_width_u16, truncate_end};
use super::widgets::{
    action_button_row_rects, centered_popup_rect, panel_contrast_fg, render_action_button,
    render_modal_header, render_modal_shell, render_panel_shell, ActionButtonSpec,
};
use crate::app::{
    state::{AddPeerField, PeerWorkspaceOpenState, WorktreeOpenState},
    AppState, Mode,
};
use crate::terminal::TerminalRuntimeRegistry;

const NEW_LINKED_WORKTREE_POPUP_WIDTH: u16 = 68;
const ADD_PEER_POPUP_WIDTH: u16 = 60;
const ADD_PEER_POPUP_HEIGHT: u16 = 13;
const NEW_LINKED_WORKTREE_POPUP_HEIGHT: u16 = 12;

pub(crate) fn rename_button_rects(inner: Rect) -> (Rect, Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "save",
            },
            ActionButtonSpec {
                hint: Some("^c"),
                label: "clear",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        3,
    );
    (rects[0], rects[1], rects[2])
}

/// Draws the shared `name_input` field and puts the host cursor on its caret.
///
/// IMEs draw their composition preview at the host terminal cursor. Without an
/// explicit cursor the frame carries none, the client keeps the position last
/// reported by the focused pane, and composition lands behind the dialog.
fn render_name_input_field(app: &AppState, frame: &mut Frame, input_rect: Rect) {
    frame.render_widget(Clear, input_rect);

    // The text stops one column short of the field so the clamped caret always
    // lands on a blank cell: a host terminal inverts the cell under its cursor,
    // and an IME composes there.
    let text_rect = Rect {
        width: input_rect.width.saturating_sub(1),
        ..input_rect
    };
    frame.render_widget(
        Paragraph::new(format!(" {}", app.name_input)).style(
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0),
        ),
        text_rect,
    );

    if input_rect.width == 0 {
        return;
    }
    let caret_x = input_rect
        .x
        .saturating_add(1)
        .saturating_add(display_width_u16(&app.name_input))
        .min(input_rect.right().saturating_sub(1));
    frame.set_cursor_position((caret_x, input_rect.y));
}

pub(super) fn render_rename_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    super::dim_background(frame, area);

    let title = match app.mode {
        Mode::RenameWorkspace if app.pending_workspace_create_cwd.is_some() => "new workspace",
        Mode::RenameWorkspace => "rename workspace",
        Mode::RenameTab if app.creating_new_tab => "new tab",
        Mode::RenameTab => "rename tab",
        Mode::RenamePane => "rename pane",
        _ => return,
    };

    let Some(inner) = render_modal_shell(frame, area, 56, 7, &app.palette) else {
        return;
    };
    if inner.height < 4 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<5>(inner);

    render_modal_header(frame, rows[0], title, &app.palette);

    let input_rect = Rect::new(rows[2].x, rows[2].y, rows[2].width, 1);
    render_name_input_field(app, frame, input_rect);

    let (save_rect, clear_rect, cancel_rect) = rename_button_rects(inner);

    render_action_button(
        frame,
        save_rect,
        Some("↵"),
        "save",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        clear_rect,
        Some("^c"),
        "clear",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

pub(crate) fn new_linked_worktree_inner_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(
        area,
        NEW_LINKED_WORKTREE_POPUP_WIDTH,
        NEW_LINKED_WORKTREE_POPUP_HEIGHT,
    )
    .map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

pub(crate) fn new_linked_worktree_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "create and open",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

pub(crate) fn remove_worktree_popup_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(area, 72, 10)
}

pub(crate) fn remove_worktree_button_rects(inner: Rect, force_confirmation: bool) -> (Rect, Rect) {
    let primary_label = if force_confirmation {
        "delete anyway"
    } else {
        "remove"
    };
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: primary_label,
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

pub(crate) fn open_existing_worktree_inner_rect(area: Rect, entry_count: usize) -> Option<Rect> {
    let height = (entry_count as u16)
        .saturating_mul(2)
        .saturating_add(7)
        .clamp(12, 26);
    centered_popup_rect(area, 96, height).map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

pub(crate) fn open_existing_worktree_max_visible_rows(inner: Rect) -> usize {
    usize::from(inner.height.saturating_sub(5) / 2)
}

pub(crate) fn open_existing_worktree_visible_start(
    open: &WorktreeOpenState,
    max_rows: usize,
) -> usize {
    let filtered = open.filtered_indices();
    let selected = open.selected_entry_index().unwrap_or(open.selected);
    let selected_pos = filtered
        .iter()
        .position(|idx| *idx == selected)
        .unwrap_or(0);
    selected_pos.saturating_sub(max_rows.saturating_sub(1))
}

pub(crate) fn open_existing_worktree_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "open",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

const PEER_WORKSPACE_OPEN_POPUP_WIDTH: u16 = 76;

pub(crate) fn peer_workspace_open_inner_rect(area: Rect, entry_count: usize) -> Option<Rect> {
    centered_popup_rect(
        area,
        PEER_WORKSPACE_OPEN_POPUP_WIDTH,
        peer_workspace_open_popup_height(entry_count),
    )
    .map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

fn peer_workspace_open_popup_height(entry_count: usize) -> u16 {
    (entry_count as u16)
        .saturating_mul(2)
        .saturating_add(7)
        .clamp(12, 26)
}

pub(crate) fn peer_workspace_open_max_visible_rows(inner: Rect) -> usize {
    usize::from(inner.height.saturating_sub(5) / 2)
}

pub(crate) fn peer_workspace_open_visible_start(
    open: &PeerWorkspaceOpenState,
    max_rows: usize,
) -> usize {
    let filtered = open.filtered_indices();
    let selected = open.selected_entry_index().unwrap_or(open.selected);
    let selected_pos = filtered
        .iter()
        .position(|idx| *idx == selected)
        .unwrap_or(0);
    selected_pos.saturating_sub(max_rows.saturating_sub(1))
}

pub(crate) fn peer_workspace_open_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "open",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

pub(super) fn render_new_linked_worktree_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(create) = app.worktree_create.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let Some(inner) = render_modal_shell(
        frame,
        area,
        NEW_LINKED_WORKTREE_POPUP_WIDTH,
        NEW_LINKED_WORKTREE_POPUP_HEIGHT,
        &app.palette,
    ) else {
        return;
    };
    if inner.height < 9 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<8>(inner);

    let header = match create.peer.as_deref() {
        Some(peer) => format!("new worktree on {peer}"),
        None => "new worktree".to_string(),
    };
    render_modal_header(frame, rows[0], &header, &app.palette);

    frame.render_widget(
        Paragraph::new(" branch").style(Style::default().fg(app.palette.overlay0)),
        rows[1],
    );
    let input_rect = Rect::new(rows[2].x, rows[2].y, rows[2].width, 1);
    render_name_input_field(app, frame, input_rect);

    // A peer chooses its own checkout directory, so there is no path to preview:
    // showing one derived from this server's worktree directory would name a
    // path on the wrong machine. The repo it will be made in is what can be
    // said, and the answer reports where it landed.
    // The repo name arrives from the peer, so it is empty until it answers.
    let (checkout_label, checkout) = match create.peer.as_deref() {
        Some(peer) if create.repo_name.is_empty() => (" repo", format!("on {peer}")),
        Some(peer) => (" repo", format!("{} on {peer}", create.repo_name)),
        None => (" checkout", create.checkout_path.display().to_string()),
    };
    frame.render_widget(
        Paragraph::new(checkout_label).style(Style::default().fg(app.palette.overlay0)),
        rows[3],
    );
    frame.render_widget(
        Paragraph::new(format!(" {checkout}")).style(Style::default().fg(app.palette.subtext0)),
        rows[4],
    );

    if create.creating {
        frame.render_widget(
            Paragraph::new(" creating…").style(Style::default().fg(app.palette.overlay0)),
            rows[5],
        );
    } else if let Some(error) = &create.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}"))
                .style(Style::default().fg(app.palette.red))
                .wrap(Wrap { trim: false }),
            rows[5],
        );
    }

    let (create_rect, cancel_rect) = new_linked_worktree_button_rects(inner);
    render_action_button(
        frame,
        create_rect,
        Some("↵"),
        "create and open",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

/// Recent-peer rows the dialog offers, capped so the popup stays small.
pub(crate) const ADD_PEER_MAX_RECENT_ROWS: usize = 5;

/// ssh-recappable history entries, in the order the dialog lists them.
pub(crate) fn add_peer_recent_entries(app: &AppState) -> Vec<&crate::config::PeerHistoryEntry> {
    app.peer_history
        .iter()
        .filter(|entry| entry.target.starts_with("ssh://"))
        .take(ADD_PEER_MAX_RECENT_ROWS)
        .collect()
}

/// Rows the recent list occupies: a label row plus one per entry, or none.
pub(crate) fn add_peer_recent_rows(app: &AppState) -> u16 {
    let count = add_peer_recent_entries(app).len();
    if count == 0 {
        0
    } else {
        1 + count as u16
    }
}

pub(crate) fn add_peer_inner_rect(app: &AppState, area: Rect) -> Option<Rect> {
    let height = ADD_PEER_POPUP_HEIGHT + add_peer_recent_rows(app);
    centered_popup_rect(area, ADD_PEER_POPUP_WIDTH, height).map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

/// The recent-list row rects, one per offered entry, below their label row.
pub(crate) fn add_peer_recent_row_rects(app: &AppState, inner: Rect) -> Vec<Rect> {
    let count = add_peer_recent_entries(app).len();
    (0..count)
        .map(|idx| {
            Rect::new(
                inner.x,
                inner.y.saturating_add(2 + idx as u16),
                inner.width,
                1,
            )
        })
        .collect()
}

/// The two input rows, so a click can move the caret between fields.
pub(crate) fn add_peer_field_rects(app: &AppState, inner: Rect) -> (Rect, Rect) {
    let recent_rows = add_peer_recent_rows(app);
    let destination = Rect::new(
        inner.x,
        inner.y.saturating_add(2 + recent_rows),
        inner.width,
        1,
    );
    let name = Rect::new(
        inner.x,
        inner.y.saturating_add(4 + recent_rows),
        inner.width,
        1,
    );
    (destination, name)
}

pub(crate) fn add_peer_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "connect",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

pub(super) fn render_add_peer_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(add) = app.add_peer.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let recent_rows = add_peer_recent_rows(app);
    let Some(inner) = render_modal_shell(
        frame,
        area,
        ADD_PEER_POPUP_WIDTH,
        ADD_PEER_POPUP_HEIGHT + recent_rows,
        &app.palette,
    ) else {
        return;
    };
    if inner.height < 10 + recent_rows {
        return;
    }

    render_modal_header(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        "add peer",
        &app.palette,
    );

    if recent_rows > 0 {
        let entries = add_peer_recent_entries(app);
        let label = Rect::new(inner.x, inner.y + 1, inner.width, 1);
        frame.render_widget(
            Paragraph::new(" recent").style(Style::default().fg(app.palette.overlay0)),
            label,
        );
        for (idx, (entry, row)) in entries
            .iter()
            .zip(add_peer_recent_row_rects(app, inner))
            .enumerate()
        {
            let selected = add.field == AddPeerField::Recent && add.recent_selected == idx;
            let style = if selected {
                Style::default()
                    .fg(panel_contrast_fg(&app.palette))
                    .bg(app.palette.accent)
            } else {
                Style::default().fg(app.palette.text)
            };
            frame.render_widget(
                Paragraph::new(format!(" {} — {}", entry.name, entry.target)).style(style),
                row,
            );
        }
    }

    let fields_y = inner.y.saturating_add(recent_rows);
    let field_rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<9>(Rect::new(
        inner.x,
        fields_y,
        inner.width,
        inner.height.saturating_sub(recent_rows),
    ));

    render_add_peer_field(
        app,
        frame,
        field_rows[1],
        field_rows[2],
        " ssh destination",
        &add.destination,
        add.field == AddPeerField::Destination,
    );
    render_add_peer_field(
        app,
        frame,
        field_rows[3],
        field_rows[4],
        " name (optional)",
        &add.name,
        add.field == AddPeerField::Name,
    );

    // Said here rather than discovered by surprise: submitting hands off to a
    // pane, and the user may have to answer something in it.
    let note = match &add.error {
        Some(error) => Paragraph::new(format!(" {error}"))
            .style(Style::default().fg(app.palette.red))
            .wrap(Wrap { trim: false }),
        None => Paragraph::new(
            " opens a pane running `herdr peer connect`, so ssh can ask\n for a password the first time",
        )
        .style(Style::default().fg(app.palette.overlay0))
        .wrap(Wrap { trim: false }),
    };
    frame.render_widget(note, field_rows[6]);

    let (connect_rect, cancel_rect) = add_peer_button_rects(inner);
    render_action_button(
        frame,
        connect_rect,
        Some("↵"),
        "connect",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

/// Only the focused field carries the host cursor, so which one a keystroke
/// lands in is visible rather than remembered.
///
/// The cursor is the terminal's own rather than a drawn glyph, for the reason
/// [`render_name_input_field`] gives: an IME composes at the host cursor, and a
/// frame that carries none leaves composition behind the dialog.
fn render_add_peer_field(
    app: &AppState,
    frame: &mut Frame,
    label_row: Rect,
    input_row: Rect,
    label: &str,
    value: &str,
    focused: bool,
) {
    frame.render_widget(
        Paragraph::new(label).style(Style::default().fg(app.palette.overlay0)),
        label_row,
    );
    let input_rect = Rect::new(input_row.x, input_row.y, input_row.width, 1);
    frame.render_widget(Clear, input_rect);
    let background = if focused {
        app.palette.surface1
    } else {
        app.palette.surface0
    };
    let style = Style::default().fg(app.palette.text).bg(background);
    // Paint the field, then lay the text over all but its last column: a
    // clamped caret then always lands on a blank cell, which is the cell the
    // host terminal inverts, without leaving the field a column short.
    frame.render_widget(Paragraph::new("").style(style), input_rect);
    frame.render_widget(
        Paragraph::new(format!(" {value}")).style(style),
        Rect {
            width: input_rect.width.saturating_sub(1),
            ..input_rect
        },
    );

    if !focused || input_rect.width == 0 {
        return;
    }
    let caret_x = input_rect
        .x
        .saturating_add(1)
        .saturating_add(display_width_u16(value))
        .min(input_rect.right().saturating_sub(1));
    frame.set_cursor_position((caret_x, input_rect.y));
}

pub(super) fn render_remove_worktree_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(remove) = app.worktree_remove.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let Some(popup) = remove_worktree_popup_rect(area) else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, popup, app.palette.red, app.palette.panel_bg)
    else {
        return;
    };

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<8>(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            " delete worktree checkout?",
            Style::default()
                .fg(app.palette.red)
                .add_modifier(Modifier::BOLD),
        )])),
        rows[0],
    );
    let removes_label = match remove.peer.as_deref() {
        Some(peer) => format!(" This removes the checkout folder on '{peer}':"),
        None => " This removes the checkout folder:".to_string(),
    };
    frame.render_widget(
        Paragraph::new(removes_label).style(Style::default().fg(app.palette.overlay0)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(format!(" {}", remove.path.display()))
            .style(Style::default().fg(app.palette.text)),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(" The branch is not deleted. The Herdr workspace will close.")
            .style(Style::default().fg(app.palette.overlay0)),
        rows[3],
    );
    if remove.force_confirmation {
        frame.render_widget(
            Paragraph::new(" Dirty or untracked files will be permanently deleted.")
                .style(Style::default().fg(app.palette.red)),
            rows[4],
        );
    }
    if remove.removing {
        frame.render_widget(
            Paragraph::new(" removing…").style(Style::default().fg(app.palette.overlay0)),
            rows[5],
        );
    } else if let Some(error) = &remove.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}")).style(Style::default().fg(app.palette.red)),
            rows[5],
        );
    }

    let (remove_rect, cancel_rect) = remove_worktree_button_rects(inner, remove.force_confirmation);
    let remove_label = if remove.force_confirmation {
        "delete anyway"
    } else {
        "remove"
    };
    render_action_button(
        frame,
        remove_rect,
        Some("↵"),
        remove_label,
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.red)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

pub(super) fn render_open_existing_worktree_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(open) = app.worktree_open.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let height = (open.entries.len() as u16)
        .saturating_mul(2)
        .saturating_add(7)
        .clamp(12, 26);
    let Some(inner) = render_modal_shell(frame, area, 96, height, &app.palette) else {
        return;
    };
    if inner.height < 8 {
        return;
    }

    let header = match open.peer.as_deref() {
        Some(peer) => format!("open worktree on {peer}"),
        None => "open worktree".to_string(),
    };
    render_modal_header(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        &header,
        &app.palette,
    );
    render_open_worktree_search(
        app,
        frame,
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
        open,
    );
    frame.render_widget(
        Paragraph::new("─".repeat(inner.width as usize))
            .style(Style::default().fg(app.palette.surface1)),
        Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1),
    );

    let filtered = open.filtered_indices();
    let max_rows = open_existing_worktree_max_visible_rows(inner);
    let start = open_existing_worktree_visible_start(open, max_rows);
    for (visible_idx, entry_idx) in filtered.iter().skip(start).take(max_rows).enumerate() {
        let Some(entry) = open.entries.get(*entry_idx) else {
            continue;
        };
        let selected = Some(*entry_idx) == open.selected_entry_index();
        let y = inner.y.saturating_add(3 + (visible_idx as u16 * 2));
        let marker = if selected { "›" } else { " " };
        let row_style = if selected {
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.palette.subtext0)
        };
        let path_style = if selected {
            Style::default()
                .fg(app.palette.subtext0)
                .bg(app.palette.surface0)
        } else {
            Style::default().fg(app.palette.overlay0)
        };
        let status = entry.status_label();
        let title_width = inner
            .width
            .saturating_sub(display_width_u16(status))
            .saturating_sub(4) as usize;
        let mut title = format!(
            "{marker} {}",
            truncate_end(&entry.display_name(), title_width)
        );
        if !status.is_empty() {
            let pad = inner
                .width
                .saturating_sub(display_width_u16(&title))
                .saturating_sub(display_width_u16(status))
                .max(1);
            title.push_str(&" ".repeat(pad as usize));
            title.push_str(status);
        }
        frame.render_widget(
            Paragraph::new(truncate_end(&title, inner.width as usize)).style(row_style),
            Rect::new(inner.x, y, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(truncate_end(
                &format!("  {}", entry.path.display()),
                inner.width as usize,
            ))
            .style(path_style),
            Rect::new(inner.x, y.saturating_add(1), inner.width, 1),
        );
    }

    if filtered.is_empty() {
        // A peer's list is a round trip, so an empty dialog is "not answered
        // yet" until it says otherwise. Saying "no matching worktrees" while
        // the peer is still being asked would report a result nobody has.
        let empty = if open.loading {
            " asking the peer…"
        } else {
            " no matching worktrees"
        };
        frame.render_widget(
            Paragraph::new(empty).style(Style::default().fg(app.palette.overlay0)),
            Rect::new(inner.x, inner.y.saturating_add(3), inner.width, 1),
        );
    }

    if let Some(error) = &open.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}")).style(Style::default().fg(app.palette.red)),
            Rect::new(
                inner.x,
                inner.y + inner.height.saturating_sub(2),
                inner.width,
                1,
            ),
        );
    }

    let (open_rect, cancel_rect) = open_existing_worktree_button_rects(inner);
    render_action_button(
        frame,
        open_rect,
        Some("↵"),
        "open",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

/// Picker over one peer's enumerated workspaces.
///
/// Mirrors [`render_open_existing_worktree_overlay`] row for row, because it is
/// the same job: choose one candidate to open, with the ones already open
/// marked rather than hidden.
pub(super) fn render_open_peer_workspace_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(open) = app.peer_workspace_open.as_ref() else {
        return;
    };

    // The peer's connection state is read live rather than snapshotted with the
    // entries, so a peer that reconnects while the picker is open stops reading
    // as stale without the list being rebuilt.
    let unavailable = app
        .peers
        .get(&crate::app::peers::PeerHandle::new(open.peer.as_str()))
        .and_then(|peer| crate::app::peer_unavailable_reason(&peer.connection));

    super::dim_background(frame, area);
    let Some(inner) = render_modal_shell(
        frame,
        area,
        PEER_WORKSPACE_OPEN_POPUP_WIDTH,
        peer_workspace_open_popup_height(open.entries.len()),
        &app.palette,
    ) else {
        return;
    };
    if inner.height < 8 {
        return;
    }

    render_modal_header(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        &format!(
            "open workspace on {}",
            // The peer's handle, matching the sidebar header and the unhide picker.
            // Truncated anyway: a handle is user-chosen and nothing bounds its length.
            truncate_end(&open.peer, inner.width.saturating_sub(20).max(8) as usize)
        ),
        &app.palette,
    );
    render_peer_workspace_search(
        app,
        frame,
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
        open,
    );
    frame.render_widget(
        Paragraph::new("─".repeat(inner.width as usize))
            .style(Style::default().fg(app.palette.surface1)),
        Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1),
    );

    let filtered = open.filtered_indices();
    let max_rows = peer_workspace_open_max_visible_rows(inner);
    let start = peer_workspace_open_visible_start(open, max_rows);
    for (visible_idx, entry_idx) in filtered.iter().skip(start).take(max_rows).enumerate() {
        let Some(entry) = open.entries.get(*entry_idx) else {
            continue;
        };
        let selected = Some(*entry_idx) == open.selected_entry_index();
        let y = inner.y.saturating_add(3 + (visible_idx as u16 * 2));
        let marker = if selected { "›" } else { " " };
        // Entries from a peer that is not up are last-known rather than live,
        // so they render dim the same way stale peer content does.
        let stale = unavailable.is_some();
        let row_style = if selected {
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0)
                .add_modifier(Modifier::BOLD)
        } else if stale {
            Style::default().fg(app.palette.overlay0)
        } else {
            Style::default().fg(app.palette.subtext0)
        };
        let detail_style = if selected {
            Style::default()
                .fg(app.palette.subtext0)
                .bg(app.palette.surface0)
        } else {
            Style::default().fg(app.palette.overlay0)
        };
        let status = entry.status_label();
        let title_width = inner
            .width
            .saturating_sub(display_width_u16(status))
            .saturating_sub(4) as usize;
        let mut title = format!(
            "{marker} {}. {}",
            entry.number,
            truncate_end(&entry.label, title_width)
        );
        if !status.is_empty() {
            let pad = inner
                .width
                .saturating_sub(display_width_u16(&title))
                .saturating_sub(display_width_u16(status))
                .max(1);
            title.push_str(&" ".repeat(pad as usize));
            title.push_str(status);
        }
        frame.render_widget(
            Paragraph::new(truncate_end(&title, inner.width as usize)).style(row_style),
            Rect::new(inner.x, y, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(truncate_end(
                &format!("  {}", entry.detail()),
                inner.width as usize,
            ))
            .style(detail_style),
            Rect::new(inner.x, y.saturating_add(1), inner.width, 1),
        );
    }

    if filtered.is_empty() {
        frame.render_widget(
            Paragraph::new(" no matching workspaces")
                .style(Style::default().fg(app.palette.overlay0)),
            Rect::new(inner.x, inner.y.saturating_add(3), inner.width, 1),
        );
    }

    // A failed open is an error; a peer that is merely not up yet is not, so
    // the two share the line but not the colour.
    let notice = open
        .error
        .as_ref()
        .map(|error| (error.clone(), app.palette.red))
        .or_else(|| {
            unavailable.as_ref().map(|reason| {
                (
                    format!("{reason} — showing last known list"),
                    app.palette.yellow,
                )
            })
        });
    if let Some((text, color)) = notice {
        frame.render_widget(
            Paragraph::new(truncate_end(&format!(" {text}"), inner.width as usize))
                .style(Style::default().fg(color)),
            Rect::new(
                inner.x,
                inner.y + inner.height.saturating_sub(2),
                inner.width,
                1,
            ),
        );
    }

    let (open_rect, cancel_rect) = peer_workspace_open_button_rects(inner);
    render_action_button(
        frame,
        open_rect,
        Some("↵"),
        "open",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

fn render_peer_workspace_search(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
    open: &PeerWorkspaceOpenState,
) {
    let focus_style = if open.search_focused {
        Style::default()
            .fg(app.palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.palette.overlay0)
    };
    let filtered_count = open.filtered_indices().len();
    let count = if open.query.trim().is_empty() {
        format!("{} workspaces", open.entries.len())
    } else {
        format!("{filtered_count}/{} workspaces", open.entries.len())
    };
    let mut spans = vec![Span::styled(" / ", focus_style)];
    if open.query.trim().is_empty() {
        spans.push(Span::styled(
            "filter workspaces",
            Style::default().fg(app.palette.overlay0),
        ));
    } else {
        spans.push(Span::styled(
            open.query.clone(),
            Style::default().fg(app.palette.text),
        ));
    }
    spans.push(Span::styled(
        format!(
            "{count:>width$}",
            width = area.width.saturating_sub(20) as usize
        ),
        Style::default().fg(app.palette.overlay0),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_open_worktree_search(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
    open: &WorktreeOpenState,
) {
    let focus_style = if open.search_focused {
        Style::default()
            .fg(app.palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.palette.overlay0)
    };
    let filtered_count = open.filtered_indices().len();
    let count = if open.query.trim().is_empty() {
        format!("{} checkouts", open.entries.len())
    } else {
        format!("{filtered_count}/{} checkouts", open.entries.len())
    };
    let mut spans = vec![Span::styled(" / ", focus_style)];
    if open.query.trim().is_empty() {
        spans.push(Span::styled(
            "filter worktrees",
            Style::default().fg(app.palette.overlay0),
        ));
    } else {
        spans.push(Span::styled(
            open.query.clone(),
            Style::default().fg(app.palette.text),
        ));
    }
    spans.push(Span::styled(
        format!(
            "{count:>width$}",
            width = area.width.saturating_sub(18) as usize
        ),
        Style::default().fg(app.palette.overlay0),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn confirm_close_overlay_text(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> (String, String) {
    let ws_name = app
        .workspaces
        .get(app.selected)
        .map(|ws| ws.display_name_from(&app.terminals, terminal_runtimes))
        .unwrap_or_else(|| "?".to_string());
    let selected_space = app
        .workspaces
        .get(app.selected)
        .and_then(|ws| ws.worktree_space());
    let group_member_indices = selected_space
        .filter(|space| !space.is_linked_worktree)
        .map(|space| {
            app.workspaces
                .iter()
                .enumerate()
                .filter_map(|(idx, ws)| {
                    ws.worktree_space()
                        .is_some_and(|member| member.key == space.key)
                        .then_some(idx)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let closes_group = group_member_indices.len() > 1;
    let pane_count = if closes_group {
        group_member_indices
            .iter()
            .filter_map(|idx| app.workspaces.get(*idx))
            .map(|ws| ws.layout.pane_count())
            .sum()
    } else {
        app.workspaces
            .get(app.selected)
            .map(|ws| ws.layout.pane_count())
            .unwrap_or(0)
    };

    let pane_text = if pane_count == 1 {
        "1 pane".to_string()
    } else {
        format!("{pane_count} panes")
    };
    let workspace_text = if closes_group {
        let count = group_member_indices.len();
        if count == 1 {
            "1 workspace, ".to_string()
        } else {
            format!("{count} workspaces, ")
        }
    } else {
        String::new()
    };

    let title = if closes_group {
        "Close worktree group?"
    } else {
        "Close workspace?"
    };
    let detail = format!("{ws_name} — {workspace_text}{pane_text}");
    (title.to_string(), detail)
}

pub(super) fn render_confirm_close_overlay(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
    area: Rect,
) {
    let (title, detail) = confirm_close_overlay_text(app, terminal_runtimes);

    super::dim_background(frame, area);

    let Some(popup) = confirm_close_popup_rect(area) else {
        return;
    };

    let warn = Style::default()
        .fg(app.palette.red)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(app.palette.overlay0);

    let title_line = Line::from(vec![Span::styled(format!(" {title}"), warn)]);

    let detail_line = Line::from(vec![
        Span::styled(
            format!(" {}", detail.split(" — ").next().unwrap_or(&detail)),
            Style::default()
                .fg(app.palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            detail
                .split_once(" — ")
                .map(|(_, rest)| format!(" — {rest}"))
                .unwrap_or_default(),
            dim,
        ),
    ]);

    let Some(inner) = render_panel_shell(frame, popup, app.palette.red, app.palette.panel_bg)
    else {
        return;
    };

    if inner.height >= 3 {
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas::<4>(inner);

        frame.render_widget(Paragraph::new(title_line), rows[0]);
        frame.render_widget(Paragraph::new(detail_line), rows[1]);

        let (confirm_rect, cancel_rect) = confirm_close_button_rects(inner);
        render_action_button(
            frame,
            confirm_rect,
            Some("↵"),
            "confirm",
            Style::default()
                .fg(panel_contrast_fg(&app.palette))
                .bg(app.palette.red)
                .add_modifier(Modifier::BOLD),
        );
        render_action_button(
            frame,
            cancel_rect,
            Some("esc"),
            "cancel",
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0)
                .add_modifier(Modifier::BOLD),
        );
    }
}

const HIDDEN_PEERS_POPUP_WIDTH: u16 = 44;

fn hidden_peers_popup_height(entry_count: usize) -> u16 {
    // header + divider + rows + hint + borders
    6 + entry_count.max(1) as u16
}

pub(crate) fn hidden_peers_inner_rect(app: &AppState, area: Rect) -> Option<Rect> {
    let entries = app
        .hidden_peers_picker
        .as_ref()
        .map(|picker| picker.entries.len())
        .unwrap_or(0);
    centered_popup_rect(
        area,
        HIDDEN_PEERS_POPUP_WIDTH,
        hidden_peers_popup_height(entries),
    )
    .map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

/// One rect per listed hidden peer, so a click can unhide it directly.
pub(crate) fn hidden_peers_row_rects(app: &AppState, inner: Rect) -> Vec<Rect> {
    let count = app
        .hidden_peers_picker
        .as_ref()
        .map(|picker| picker.entries.len())
        .unwrap_or(0);
    (0..count)
        .map(|idx| {
            Rect::new(
                inner.x,
                inner.y.saturating_add(2 + idx as u16),
                inner.width,
                1,
            )
        })
        .collect()
}

pub(super) fn render_hidden_peers_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(picker) = app.hidden_peers_picker.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let Some(inner) = render_modal_shell(
        frame,
        area,
        HIDDEN_PEERS_POPUP_WIDTH,
        hidden_peers_popup_height(picker.entries.len()),
        &app.palette,
    ) else {
        return;
    };

    render_modal_header(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        "hidden peers",
        &app.palette,
    );
    frame.render_widget(
        Paragraph::new("─".repeat(inner.width as usize))
            .style(Style::default().fg(app.palette.surface1)),
        Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1),
    );

    for (idx, (entry, row)) in picker
        .entries
        .iter()
        .zip(hidden_peers_row_rects(app, inner))
        .enumerate()
    {
        let selected = picker.selected == idx;
        let marker = if selected { "›" } else { " " };
        let scope = if entry.permanent {
            "permanent"
        } else {
            "session"
        };
        let style = if selected {
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.palette.subtext0)
        };
        frame.render_widget(
            Paragraph::new(truncate_end(
                &format!("{marker} {} ({scope})", entry.peer),
                inner.width as usize,
            ))
            .style(style),
            row,
        );
    }

    let footer_y = inner.y + inner.height.saturating_sub(1);
    let (text, color) = match &picker.error {
        Some(error) => (format!(" {error}"), app.palette.red),
        None => (" ↵ unhide   esc close".to_string(), app.palette.overlay0),
    };
    frame.render_widget(
        Paragraph::new(truncate_end(&text, inner.width as usize)).style(Style::default().fg(color)),
        Rect::new(inner.x, footer_y, inner.width, 1),
    );
}

pub(crate) fn confirm_close_popup_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(area, 64, 6)
}

pub(crate) fn confirm_close_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "confirm",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        3,
    );
    (rects[0], rects[1])
}

#[cfg(test)]
mod tests {
    use crate::{
        app::{state::WorktreeCreateState, AppState, Mode},
        workspace::Workspace,
    };
    use ratatui::{
        backend::TestBackend,
        buffer::Buffer,
        layout::{Position, Rect},
        Terminal,
    };

    use super::{
        confirm_close_overlay_text, render_new_linked_worktree_overlay, render_rename_overlay,
    };

    #[test]
    fn confirm_close_text_uses_live_workspace_cwd_label() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("initial");
        workspace.custom_name = None;
        workspace.identity_cwd = "/projects/original".into();
        let root_pane = workspace.tabs[0].root_pane;
        let terminal_id = workspace.tabs[0].panes[&root_pane]
            .attached_terminal_id
            .clone();
        app.workspaces = vec![workspace];
        app.ensure_test_terminals();
        app.terminals.get_mut(&terminal_id).unwrap().cwd = "/projects/current".into();
        app.selected = 0;

        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let (title, detail) = confirm_close_overlay_text(&app, &terminal_runtimes);

        assert_eq!(title, "Close workspace?");
        assert_eq!(detail, "current — 1 pane");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confirm_close_text_prefers_live_runtime_cwd_over_stale_terminal_cwd() {
        let root = std::env::temp_dir().join(format!(
            "herdr-confirm-close-runtime-cwd-{}",
            std::process::id()
        ));
        let stale_cwd = root.join("original");
        let live_cwd = root.join("current");
        std::fs::create_dir_all(&live_cwd).unwrap();

        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("initial");
        workspace.custom_name = None;
        workspace.identity_cwd = stale_cwd.clone();
        let root_pane = workspace.tabs[0].root_pane;
        let terminal_id = workspace.tabs[0].panes[&root_pane]
            .attached_terminal_id
            .clone();
        app.workspaces = vec![workspace];
        app.ensure_test_terminals();
        app.selected = 0;

        let (events, _) = tokio::sync::mpsc::channel(4);
        let runtime = crate::terminal::TerminalRuntime::spawn(
            root_pane,
            24,
            80,
            live_cwd,
            0,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            crate::pane::PaneShellConfig::new("/bin/sh", crate::config::ShellModeConfig::NonLogin),
            &crate::pane::PaneLaunchEnv::default(),
            events,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            std::sync::Arc::new(crate::render_signal::RenderSignal::new()),
        )
        .unwrap();
        let mut terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        terminal_runtimes.insert(terminal_id, runtime);

        let (_, detail) = confirm_close_overlay_text(&app, &terminal_runtimes);

        assert_eq!(detail, "current — 1 pane");

        drop(terminal_runtimes);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn confirm_close_text_uses_selected_custom_name_instead_of_active_workspace_cwd() {
        let mut app = AppState::test_new();
        let active = Workspace::test_new("active");
        let selected = Workspace::test_new("selected");
        let selected_root = selected.tabs[0].root_pane;
        let selected_terminal_id = selected.tabs[0].panes[&selected_root]
            .attached_terminal_id
            .clone();
        app.workspaces = vec![active, selected];
        app.ensure_test_terminals();
        app.terminals.get_mut(&selected_terminal_id).unwrap().cwd = "/projects/current".into();
        app.active = Some(0);
        app.selected = 1;

        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let (_, detail) = confirm_close_overlay_text(&app, &terminal_runtimes);

        assert_eq!(detail, "selected — 1 pane");
    }

    #[test]
    fn confirm_close_text_reports_parent_group_scope() {
        let mut app = AppState::test_new();
        let mut parent = Workspace::test_new("main");
        parent.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr".into(),
            is_linked_worktree: false,
        });
        let mut child = Workspace::test_new("issue");
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr-issue".into(),
            is_linked_worktree: true,
        });
        app.workspaces = vec![parent, child];
        app.selected = 0;

        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let (title, detail) = confirm_close_overlay_text(&app, &terminal_runtimes);

        assert_eq!(title, "Close worktree group?");
        assert_eq!(detail, "main — 2 workspaces, 2 panes");
    }

    #[test]
    fn new_worktree_error_renders_fatal_stderr_line() {
        let mut app = AppState::test_new();
        app.name_input = "foo".into();
        app.worktree_create = Some(WorktreeCreateState {
            source_workspace_id: "source".into(),
            source_checkout_path: "/repo/herdr".into(),
            source_existing_membership: None,
            source_repo_root: "/repo/herdr".into(),
            peer: None,
            repo_key: "repo-key".into(),
            repo_name: "herdr".into(),
            branch: "foo".into(),
            checkout_path: "/repo/.worktrees/herdr/foo".into(),
            error: Some(
                "Preparing worktree (new branch 'foo')\nfatal: a branch named 'foo' already exists"
                    .into(),
            ),
            creating: false,
        });

        let mut terminal =
            Terminal::new(TestBackend::new(100, 30)).expect("test terminal should initialize");
        terminal
            .draw(|frame| render_new_linked_worktree_overlay(&app, frame, Rect::new(0, 0, 100, 30)))
            .expect("new worktree overlay should render");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("fatal: a branch named 'foo' already exists"));
    }

    #[test]
    fn new_worktree_hit_test_geometry_matches_modal_size() {
        let area = Rect::new(0, 0, 100, 30);
        let inner = super::new_linked_worktree_inner_rect(area).unwrap();
        let (create, cancel) = super::new_linked_worktree_button_rects(inner);

        assert_eq!(inner.width, super::NEW_LINKED_WORKTREE_POPUP_WIDTH - 2);
        assert_eq!(inner.height, super::NEW_LINKED_WORKTREE_POPUP_HEIGHT - 2);
        assert_eq!(create.y, inner.y + inner.height - 1);
        assert_eq!(cancel.y, inner.y + inner.height - 1);
    }

    const RENAME_AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 20,
    };
    const WORKTREE_AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 30,
    };

    /// Reproduces the input row that `render_rename_overlay` lays out: the
    /// centred popup, the border inset, then the third row of the vertical
    /// split.
    fn rename_input_rect(area: Rect) -> Rect {
        let popup = super::centered_popup_rect(area, 56, 7).expect("popup fits");
        let inner = Rect::new(popup.x + 1, popup.y + 1, popup.width - 2, popup.height - 2);
        Rect::new(inner.x, inner.y + 2, inner.width, 1)
    }

    fn rename_overlay_caret_in(mode: Mode, name: &str) -> (Position, Buffer) {
        let mut app = AppState::test_new();
        app.mode = mode;
        app.name_input = name.into();

        let mut terminal = Terminal::new(TestBackend::new(RENAME_AREA.width, RENAME_AREA.height))
            .expect("test terminal");
        terminal
            .draw(|frame| render_rename_overlay(&app, frame, RENAME_AREA))
            .expect("rename overlay should render");
        let caret = terminal.get_cursor_position().expect("cursor position");
        (caret, terminal.backend().buffer().clone())
    }

    fn rename_overlay_caret(name: &str) -> Position {
        rename_overlay_caret_in(Mode::RenameWorkspace, name).0
    }

    fn worktree_overlay_caret(branch: &str) -> Position {
        let mut app = AppState::test_new();
        app.name_input = branch.into();
        app.worktree_create = Some(WorktreeCreateState {
            source_workspace_id: "source".into(),
            source_checkout_path: "/repo/herdr".into(),
            source_existing_membership: None,
            source_repo_root: "/repo/herdr".into(),
            peer: None,
            repo_key: "repo-key".into(),
            repo_name: "herdr".into(),
            branch: branch.into(),
            checkout_path: "/repo/.worktrees/herdr/foo".into(),
            error: None,
            creating: false,
        });

        let mut terminal =
            Terminal::new(TestBackend::new(WORKTREE_AREA.width, WORKTREE_AREA.height))
                .expect("test terminal");
        terminal
            .draw(|frame| render_new_linked_worktree_overlay(&app, frame, WORKTREE_AREA))
            .expect("new worktree overlay should render");
        terminal.get_cursor_position().expect("cursor position")
    }

    #[test]
    fn rename_overlay_anchors_the_host_cursor_to_the_input_caret() {
        let input = rename_input_rect(RENAME_AREA);

        // Without an explicit cursor the frame carries none, the client parks the
        // host cursor where the focused pane last reported it, and the IME
        // composes there instead of in the dialog.
        assert_eq!(
            rename_overlay_caret(""),
            Position::new(input.x + 1, input.y),
            "empty input should put the caret past the one-column left padding"
        );
        assert_eq!(
            rename_overlay_caret("abcd"),
            Position::new(input.x + 5, input.y)
        );

        // The cell under the caret has to be blank: a host terminal draws its
        // cursor by inverting that cell, so a glyph there would swallow it.
        let (caret, buffer) = rename_overlay_caret_in(Mode::RenameWorkspace, "ab");
        assert_eq!(caret, Position::new(input.x + 3, input.y));
        assert_eq!(buffer[(caret.x, caret.y)].symbol(), " ");
        assert_eq!(buffer[(caret.x - 1, caret.y)].symbol(), "b");
    }

    #[test]
    fn rename_overlay_anchors_the_cursor_in_every_rename_mode() {
        let input = rename_input_rect(RENAME_AREA);
        let expected = Position::new(input.x + 3, input.y);

        for mode in [Mode::RenameWorkspace, Mode::RenameTab, Mode::RenamePane] {
            assert_eq!(
                rename_overlay_caret_in(mode, "ab").0,
                expected,
                "{mode:?} should anchor the caret like the other rename modes"
            );
        }
    }

    #[test]
    fn rename_overlay_caret_counts_wide_characters_as_two_columns() {
        let input = rename_input_rect(RENAME_AREA);

        // "あい" is two columns per character, so the caret sits two cells further
        // right than the two-column "ab".
        assert_eq!(
            rename_overlay_caret("あい"),
            Position::new(input.x + 5, input.y)
        );
        assert_eq!(
            rename_overlay_caret("aあ"),
            Position::new(input.x + 4, input.y)
        );
    }

    #[test]
    fn rename_overlay_caret_stays_inside_the_input_when_the_name_overflows() {
        let input = rename_input_rect(RENAME_AREA);
        let last_column = input.right() - 1;

        // The field is 54 columns wide. 51 characters is the last name whose
        // caret still lands strictly inside it; from 52 on the unclamped column
        // would leave the field and gets pinned to the final cell.
        assert_eq!(
            rename_overlay_caret(&"a".repeat(51)),
            Position::new(input.x + 52, input.y)
        );
        assert_eq!(
            rename_overlay_caret(&"a".repeat(53)),
            Position::new(last_column, input.y)
        );
        assert_eq!(
            rename_overlay_caret(&"a".repeat(200)),
            Position::new(last_column, input.y)
        );

        // The clamped cell has to stay blank as well, or the host cursor would
        // sit on a glyph and the IME would compose over it.
        let (caret, buffer) = rename_overlay_caret_in(Mode::RenameWorkspace, &"a".repeat(200));
        assert_eq!(caret, Position::new(last_column, input.y));
        assert_eq!(buffer[(caret.x, caret.y)].symbol(), " ");
        assert_eq!(buffer[(caret.x - 1, caret.y)].symbol(), "a");
    }

    #[test]
    fn rename_overlay_caret_reaches_the_frame_the_server_sends() {
        let input = rename_input_rect(RENAME_AREA);
        let mut app = AppState::test_new();
        app.mode = Mode::RenameWorkspace;
        app.name_input = "ab".into();

        // The widget tests above stop at the ratatui frame. This one goes through
        // the server's cursor resolution, which is where the bug lived: the frame
        // used to leave here with `cursor: None`.
        let (_, cursor) =
            crate::server::render_stream::render_virtual(&mut app, RENAME_AREA, false);
        let cursor = cursor.expect("the modal caret should survive cursor resolution");

        assert_eq!((cursor.x, cursor.y), (input.x + 3, input.y));
        assert!(cursor.visible);
    }

    #[test]
    fn new_worktree_overlay_anchors_the_host_cursor_to_the_input_caret() {
        let popup = super::new_linked_worktree_inner_rect(WORKTREE_AREA).expect("popup fits");
        let input = Rect::new(popup.x, popup.y + 2, popup.width, 1);

        assert_eq!(
            worktree_overlay_caret(""),
            Position::new(input.x + 1, input.y)
        );
        assert_eq!(
            worktree_overlay_caret("ab"),
            Position::new(input.x + 3, input.y)
        );
        assert_eq!(
            worktree_overlay_caret("あい"),
            Position::new(input.x + 5, input.y)
        );
    }
}
