//! How big a terminal is, as a value whose halves cannot be swapped.
//!
//! This exists because `(u16, u16)` did not say which half was which, and the
//! two runtimes behind [`crate::terminal::TerminalRuntime`] disagreed: the
//! remote one answered `(cols, rows)` while the local one answered
//! `(rows, cols)`. Both are reached through one method, so a caller that did
//! not know which kind of pane it held — the shell respawn in `src/app/api.rs`
//! looks a runtime up by terminal id and cannot know — read the dimensions
//! swapped for a peer-backed pane.
//!
//! A test now pins the two variants against each other, which catches the same
//! defect after it is written. Naming the fields stops it being writable: there
//! is no order left to get wrong, and a third variant cannot reintroduce one.

/// The size of a terminal in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalSize {
    /// Height in character cells.
    pub rows: u16,
    /// Width in character cells.
    pub cols: u16,
}

impl TerminalSize {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self { rows, cols }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the type: two sizes that differ only in which
    /// dimension is which are different values, and no call site can reorder
    /// them by accident because there is no order to state.
    #[test]
    fn rows_and_cols_are_not_interchangeable() {
        assert_ne!(TerminalSize::new(24, 80), TerminalSize::new(80, 24));
        let size = TerminalSize::new(24, 80);
        assert_eq!(size.rows, 24);
        assert_eq!(size.cols, 80);
    }
}
