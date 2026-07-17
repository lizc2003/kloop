//! Completion popups for the composer (plan 38 slice 4): the `/` slash-command
//! menu and the `@` file menu. A single flat menu backs both — pure trigger
//! detection, slash filtering, and cursor state, all unit-tested without a TTY;
//! the file list is filled by the event loop (an I/O search, [`crate::App`]).
//! At most one popup is open at a time (the App holds `Option<Popup>`), and the
//! rendering lives in [`crate::render`] beside the other draw code.

/// How many file candidates the `@` search returns (and thus the tallest the
/// file menu can be before its window scrolls).
pub const FILE_MENU_MAX: usize = 50;

/// The most menu rows shown at once; a longer list windows around the cursor.
pub const MENU_ROWS: usize = 8;

/// One entry in the slash-command catalog, built once at startup from the
/// built-ins and loaded skills/commands. `name` has no leading `/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
}

/// A row in an open menu. `label` and `detail` are what the user sees; `insert`
/// is the full token (prefix included) that replaces the typed one on accept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItem {
    pub label: String,
    pub detail: String,
    pub insert: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupKind {
    Slash,
    File,
}

/// An open completion popup: which trigger opened it, the query typed after the
/// trigger char, the candidate rows, and the highlighted one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Popup {
    pub kind: PopupKind,
    pub query: String,
    pub items: Vec<MenuItem>,
    pub cursor: usize,
}

impl Popup {
    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if !self.items.is_empty() {
            self.cursor = (self.cursor + 1).min(self.items.len() - 1);
        }
    }

    pub fn selected(&self) -> Option<&MenuItem> {
        self.items.get(self.cursor)
    }
}

/// Which completion the composer's current token asks for, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// `/` at the very start of the input: the string after the slash.
    Slash(String),
    /// `@` anywhere: the string after the at-sign.
    File(String),
}

/// Inspect the composer text and cursor for an active completion trigger. The
/// current token is the run of non-whitespace chars ending at the cursor (the
/// same span [`crate::composer::Composer::replace_token`] would replace). A `/`
/// token counts only at the very start of the input (a command line); an `@`
/// token counts anywhere (a file mention). `allow_slash` is false while a turn
/// runs — a `/` line is then steering text, not a command.
pub fn detect_trigger(text: &str, cursor: usize, allow_slash: bool) -> Option<Trigger> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut start = cursor;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let token: String = chars[start..cursor].iter().collect();
    if let Some(rest) = token.strip_prefix('@') {
        return Some(Trigger::File(rest.to_string()));
    }
    if allow_slash && start == 0 {
        if let Some(rest) = token.strip_prefix('/') {
            return Some(Trigger::Slash(rest.to_string()));
        }
    }
    None
}

/// The slash-menu rows for `query`: every command whose name starts with it
/// (case-insensitive), in catalog order (built-ins first). Empty when nothing
/// matches — the caller then shows no popup, so an unknown `/name` still runs
/// and reports itself.
pub fn slash_items(commands: &[CommandInfo], query: &str) -> Vec<MenuItem> {
    let q = query.to_lowercase();
    commands
        .iter()
        .filter(|c| c.name.to_lowercase().starts_with(&q))
        .map(|c| MenuItem {
            label: format!("/{}", c.name),
            detail: c.description.clone(),
            insert: format!("/{}", c.name),
        })
        .collect()
}

/// The file-menu rows for a set of found paths.
pub fn file_items(paths: Vec<String>) -> Vec<MenuItem> {
    paths
        .into_iter()
        .map(|p| MenuItem {
            insert: format!("@{p}"),
            detail: String::new(),
            label: p,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Vec<CommandInfo> {
        ["help", "cost", "compact", "clear", "exit"]
            .iter()
            .map(|n| CommandInfo {
                name: n.to_string(),
                description: format!("the {n} command"),
            })
            .collect()
    }

    #[test]
    fn slash_trigger_only_at_start_of_input() {
        assert_eq!(
            detect_trigger("/co", 3, true),
            Some(Trigger::Slash("co".into()))
        );
        // Bare slash: empty query, still a trigger (shows the full menu).
        assert_eq!(
            detect_trigger("/", 1, true),
            Some(Trigger::Slash(String::new()))
        );
        // A slash mid-line is not a command.
        assert_eq!(detect_trigger("go /co", 6, true), None);
        // Suppressed while running.
        assert_eq!(detect_trigger("/co", 3, false), None);
    }

    #[test]
    fn file_trigger_anywhere() {
        assert_eq!(
            detect_trigger("review @src/ma", 14, true),
            Some(Trigger::File("src/ma".into()))
        );
        // At the very start too.
        assert_eq!(
            detect_trigger("@a", 2, true),
            Some(Trigger::File("a".into()))
        );
        // Bare at-sign: empty query.
        assert_eq!(
            detect_trigger("@", 1, true),
            Some(Trigger::File(String::new()))
        );
    }

    #[test]
    fn no_trigger_for_plain_text_or_after_whitespace() {
        assert_eq!(detect_trigger("hello", 5, true), None);
        // Cursor after the token's trailing space: the token is empty.
        assert_eq!(detect_trigger("@a ", 3, true), None);
    }

    #[test]
    fn cursor_mid_token_uses_only_the_prefix() {
        // "@src/main" with the cursor after "@src" → query "src".
        assert_eq!(
            detect_trigger("@src/main", 4, true),
            Some(Trigger::File("src".into()))
        );
    }

    #[test]
    fn slash_items_filter_by_prefix_in_catalog_order() {
        let items = slash_items(&catalog(), "c");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["/cost", "/compact", "/clear"]);
        assert_eq!(items[0].insert, "/cost");
        // Case-insensitive, exact-prefix.
        assert_eq!(slash_items(&catalog(), "COMP").len(), 1);
        assert!(slash_items(&catalog(), "zzz").is_empty());
    }

    #[test]
    fn cursor_moves_and_clamps() {
        let mut p = Popup {
            kind: PopupKind::Slash,
            query: "c".into(),
            items: slash_items(&catalog(), "c"),
            cursor: 0,
        };
        p.move_up(); // clamps at 0
        assert_eq!(p.cursor, 0);
        p.move_down();
        p.move_down();
        p.move_down(); // clamps at len-1 (3 items)
        assert_eq!(p.cursor, 2);
        assert_eq!(p.selected().unwrap().label, "/clear");
    }
}
