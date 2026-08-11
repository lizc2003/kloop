//! Completion popups for the composer (plan 38 slice 4, hardened in plan 76).
//!
//! Trigger detection returns an exact UTF-8 byte range and cursor identity. The
//! file-search result and popup acceptance both carry that target, so an equal
//! query at another document position cannot edit the wrong token.

use crate::text_layout::is_grapheme_boundary;
use crate::text_layout::ByteOffset;
use crate::text_layout::TextRange;

pub const FILE_MENU_MAX: usize = 50;
pub const MENU_ROWS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
}

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionTarget {
    pub kind: PopupKind,
    pub query: String,
    pub range: TextRange,
    pub cursor: ByteOffset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Popup {
    pub target: CompletionTarget,
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

pub fn detect_trigger(
    text: &str,
    cursor: ByteOffset,
    allow_slash: bool,
) -> Option<CompletionTarget> {
    if cursor.get() > text.len() || !is_grapheme_boundary(text, cursor) {
        return None;
    }
    let prefix = &text[..cursor.get()];
    let start = prefix
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(0);
    let token = &text[start..cursor.get()];
    let range = TextRange::new(ByteOffset::new(start), cursor);
    if let Some(query) = token.strip_prefix('@') {
        return Some(CompletionTarget {
            kind: PopupKind::File,
            query: query.to_string(),
            range,
            cursor,
        });
    }
    if allow_slash && start == 0 {
        if let Some(query) = token.strip_prefix('/') {
            return Some(CompletionTarget {
                kind: PopupKind::Slash,
                query: query.to_string(),
                range,
                cursor,
            });
        }
    }
    None
}

pub fn slash_items(commands: &[CommandInfo], query: &str) -> Vec<MenuItem> {
    let query = query.to_lowercase();
    commands
        .iter()
        .filter(|command| command.name.to_lowercase().starts_with(&query))
        .map(|command| MenuItem {
            label: format!("/{}", command.name),
            detail: command.description.clone(),
            insert: format!("/{}", command.name),
        })
        .collect()
}

pub fn file_items(paths: Vec<String>) -> Vec<MenuItem> {
    paths
        .into_iter()
        .map(|path| MenuItem {
            insert: format!("@{path}"),
            detail: String::new(),
            label: path,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Vec<CommandInfo> {
        ["help", "cost", "compact", "clear", "exit"]
            .iter()
            .map(|name| CommandInfo {
                name: name.to_string(),
                description: format!("the {name} command"),
            })
            .collect()
    }

    fn target(kind: PopupKind, query: &str, start: usize, end: usize) -> CompletionTarget {
        CompletionTarget {
            kind,
            query: query.into(),
            range: TextRange::new(ByteOffset::new(start), ByteOffset::new(end)),
            cursor: ByteOffset::new(end),
        }
    }

    #[test]
    fn slash_trigger_only_at_start_of_input() {
        assert_eq!(
            detect_trigger("/co", ByteOffset::new(3), true),
            Some(target(PopupKind::Slash, "co", 0, 3))
        );
        assert_eq!(
            detect_trigger("/", ByteOffset::new(1), true),
            Some(target(PopupKind::Slash, "", 0, 1))
        );
        assert_eq!(detect_trigger("go /co", ByteOffset::new(6), true), None);
        assert_eq!(detect_trigger("/co", ByteOffset::new(3), false), None);
    }

    #[test]
    fn file_trigger_carries_unicode_byte_range() {
        let text = "审阅 @src/界";
        let start = "审阅 ".len();
        assert_eq!(
            detect_trigger(text, ByteOffset::new(text.len()), true),
            Some(target(PopupKind::File, "src/界", start, text.len()))
        );
        assert_eq!(
            detect_trigger("@a", ByteOffset::new(2), true),
            Some(target(PopupKind::File, "a", 0, 2))
        );
        assert_eq!(
            detect_trigger("@", ByteOffset::new(1), true),
            Some(target(PopupKind::File, "", 0, 1))
        );
    }

    #[test]
    fn no_trigger_for_plain_text_whitespace_or_non_boundary_cursor() {
        assert_eq!(detect_trigger("hello", ByteOffset::new(5), true), None);
        assert_eq!(detect_trigger("@a ", ByteOffset::new(3), true), None);
        assert_eq!(detect_trigger("@界", ByteOffset::new(2), true), None);
    }

    #[test]
    fn cursor_mid_token_targets_only_the_prefix() {
        assert_eq!(
            detect_trigger("@src/main", ByteOffset::new(4), true),
            Some(target(PopupKind::File, "src", 0, 4))
        );
        let text = "前 @界/后";
        let cursor = ByteOffset::new("前 @界".len());
        assert_eq!(
            detect_trigger(text, cursor, true),
            Some(target(PopupKind::File, "界", "前 ".len(), cursor.get()))
        );
    }

    #[test]
    fn identical_queries_at_different_ranges_are_distinct_targets() {
        let first = detect_trigger("@src", ByteOffset::new(4), true).unwrap();
        let second_text = "see @src";
        let second = detect_trigger(second_text, ByteOffset::new(second_text.len()), true).unwrap();
        assert_eq!(first.query, second.query);
        assert_ne!(first, second);
    }

    #[test]
    fn slash_items_filter_by_prefix_in_catalog_order() {
        let items = slash_items(&catalog(), "c");
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["/cost", "/compact", "/clear"]);
        assert_eq!(items[0].insert, "/cost");
        assert_eq!(slash_items(&catalog(), "COMP").len(), 1);
        assert!(slash_items(&catalog(), "zzz").is_empty());
    }

    #[test]
    fn file_items_preserve_path_and_add_trigger() {
        assert_eq!(
            file_items(vec!["src/main.rs".into()]),
            vec![MenuItem {
                label: "src/main.rs".into(),
                detail: String::new(),
                insert: "@src/main.rs".into(),
            }]
        );
    }

    #[test]
    fn cursor_moves_and_clamps() {
        let mut popup = Popup {
            target: target(PopupKind::Slash, "c", 0, 2),
            items: slash_items(&catalog(), "c"),
            cursor: 0,
        };
        popup.move_up();
        assert_eq!(popup.cursor, 0);
        popup.move_down();
        popup.move_down();
        popup.move_down();
        assert_eq!(popup.cursor, 2);
        assert_eq!(popup.selected().unwrap().label, "/clear");
    }
}
