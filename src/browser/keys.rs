//! Translates herdr key events into `agent-browser` keyboard actions.
//!
//! Two shapes exist on the CLI side (verified against agent-browser 0.33.1):
//! `keyboard type <text>` types printable text into whatever has focus, and
//! `press <key>` sends a named key or modifier combination. Printable
//! characters go through the former so they can be batched; everything else
//! becomes a `press`.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::BrowserCommand;

/// Returns the command a focused Browser pane should run for `key`, or `None`
/// when the key carries nothing for the page (modifier-only presses, key
/// releases).
pub(crate) fn command_for_key(key: &KeyEvent) -> Option<BrowserCommand> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let mods = key.modifiers;
    // Shift is deliberately excluded: crossterm has already folded it into
    // the character, so `Shift+A` would double-apply it.
    let is_chord = mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);

    if let KeyCode::Char(c) = key.code {
        if !is_chord {
            return Some(BrowserCommand::TypeText(c.to_string()));
        }
    }

    let (name, force_shift) = key_name(key.code)?;
    Some(BrowserCommand::PressKey(with_modifiers(
        &name,
        mods,
        force_shift,
    )))
}

/// Playwright-style key name for `code`, plus whether the name implies Shift.
fn key_name(code: KeyCode) -> Option<(String, bool)> {
    let name = match code {
        KeyCode::Enter => "Enter",
        KeyCode::Tab => "Tab",
        // Back-tab is Shift+Tab arriving as its own code.
        KeyCode::BackTab => return Some(("Tab".to_string(), true)),
        KeyCode::Backspace => "Backspace",
        KeyCode::Delete => "Delete",
        KeyCode::Esc => "Escape",
        KeyCode::Left => "ArrowLeft",
        KeyCode::Right => "ArrowRight",
        KeyCode::Up => "ArrowUp",
        KeyCode::Down => "ArrowDown",
        KeyCode::Home => "Home",
        KeyCode::End => "End",
        KeyCode::PageUp => "PageUp",
        KeyCode::PageDown => "PageDown",
        KeyCode::Insert => "Insert",
        KeyCode::F(n) if (1..=12).contains(&n) => return Some((format!("F{n}"), false)),
        KeyCode::Char(c) => return Some((c.to_string(), false)),
        _ => return None,
    };
    Some((name.to_string(), false))
}

fn with_modifiers(name: &str, mods: KeyModifiers, force_shift: bool) -> String {
    let mut out = String::new();
    if mods.contains(KeyModifiers::CONTROL) {
        out.push_str("Control+");
    }
    if mods.contains(KeyModifiers::ALT) {
        out.push_str("Alt+");
    }
    if force_shift || mods.contains(KeyModifiers::SHIFT) {
        out.push_str("Shift+");
    }
    if mods.contains(KeyModifiers::SUPER) {
        out.push_str("Meta+");
    }
    out.push_str(name);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn typed(code: KeyCode, mods: KeyModifiers) -> Option<String> {
        match command_for_key(&key(code, mods)) {
            Some(BrowserCommand::TypeText(text)) => Some(text),
            _ => None,
        }
    }

    fn pressed(code: KeyCode, mods: KeyModifiers) -> Option<String> {
        match command_for_key(&key(code, mods)) {
            Some(BrowserCommand::PressKey(name)) => Some(name),
            _ => None,
        }
    }

    #[test]
    fn printable_characters_are_typed_not_pressed() {
        assert_eq!(
            typed(KeyCode::Char('a'), KeyModifiers::NONE).as_deref(),
            Some("a")
        );
        assert_eq!(
            typed(KeyCode::Char(' '), KeyModifiers::NONE).as_deref(),
            Some(" ")
        );
        // Shift is already folded into the character by crossterm; emitting
        // Shift+A as a chord would double-apply it.
        assert_eq!(
            typed(KeyCode::Char('A'), KeyModifiers::SHIFT).as_deref(),
            Some("A")
        );
    }

    #[test]
    fn chords_become_named_presses() {
        assert_eq!(
            pressed(KeyCode::Char('a'), KeyModifiers::CONTROL).as_deref(),
            Some("Control+a")
        );
        assert_eq!(
            pressed(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )
            .as_deref(),
            Some("Control+Shift+c")
        );
    }

    #[test]
    fn special_keys_use_playwright_names() {
        assert_eq!(
            pressed(KeyCode::Enter, KeyModifiers::NONE).as_deref(),
            Some("Enter")
        );
        assert_eq!(
            pressed(KeyCode::Esc, KeyModifiers::NONE).as_deref(),
            Some("Escape")
        );
        assert_eq!(
            pressed(KeyCode::Left, KeyModifiers::NONE).as_deref(),
            Some("ArrowLeft")
        );
        assert_eq!(
            pressed(KeyCode::F(5), KeyModifiers::NONE).as_deref(),
            Some("F5")
        );
        // BackTab arrives as its own code but means Shift+Tab.
        assert_eq!(
            pressed(KeyCode::BackTab, KeyModifiers::NONE).as_deref(),
            Some("Shift+Tab")
        );
    }

    #[test]
    fn releases_and_unmapped_keys_produce_nothing() {
        let mut release = key(KeyCode::Char('a'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert!(command_for_key(&release).is_none());
        assert!(command_for_key(&key(KeyCode::Null, KeyModifiers::NONE)).is_none());
    }
}
