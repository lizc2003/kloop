//! 从语法树保存词的组成。权限层消费求值结果，不再从去引号的字符串猜展开。

use std::path::Path;

use tree_sitter::Node;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Quoting {
    Bare,
    Single,
    Double,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PartKind {
    Text(String),
    Home,
    Variable(String),
    Pattern(String),
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WordPart {
    pub(super) kind: PartKind,
    pub(super) quoting: Quoting,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ShellWord {
    pub(super) parts: Vec<WordPart>,
}

/// 部分拼写中的 NUL 代表未知片段，永远不能成为真实路径或授权证据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WordEvaluation {
    Resolved(String),
    Partial(String),
    Unknown,
}

impl ShellWord {
    pub(super) fn from_node(node: Node<'_>, src: &str) -> Self {
        let mut word = Self::default();
        word.collect(node, src, Quoting::Bare, node.start_byte(), node.end_byte());
        word
    }

    pub fn unknown() -> Self {
        Self {
            parts: vec![WordPart {
                kind: PartKind::Unknown,
                quoting: Quoting::Bare,
            }],
        }
    }

    pub fn evaluate(&self, home: Option<&Path>) -> WordEvaluation {
        let mut text = String::new();
        let mut partial = false;
        let mut evidence = false;
        for part in &self.parts {
            match &part.kind {
                PartKind::Text(value) => {
                    evidence |= !value.is_empty();
                    text.push_str(value);
                }
                PartKind::Pattern(value) => {
                    partial = true;
                    evidence |= !value.is_empty();
                    text.push_str(value);
                }
                PartKind::Home | PartKind::Variable(_) => {
                    let value = match &part.kind {
                        PartKind::Home => home.and_then(Path::to_str),
                        PartKind::Variable(name) if name == "HOME" => home.and_then(Path::to_str),
                        _ => None,
                    };
                    if let Some(value) = value {
                        // 展开是字符串拼接；`$HOME.bak` 并不是 home 下的 `.bak`。
                        text.push_str(value);
                        evidence = true;
                        // 未引用变量还会分词/glob；不能把多参数当成单一路径。
                        partial |= part.quoting == Quoting::Bare
                            && matches!(part.kind, PartKind::Variable(_))
                            && (value.is_empty()
                                || value.contains([' ', '\t', '\n', '*', '?', '[']));
                    } else {
                        text.push('\0');
                        partial = true;
                    }
                }
                PartKind::Unknown => {
                    text.push('\0');
                    partial = true;
                }
            }
        }
        if !partial {
            WordEvaluation::Resolved(text)
        } else if evidence {
            WordEvaluation::Partial(text)
        } else {
            WordEvaluation::Unknown
        }
    }

    pub(super) fn append(&mut self, mut suffix: Self) {
        if self.parts.len() == 1 && self.parts[0].kind == PartKind::Home {
            let slash = suffix.parts.first().is_some_and(|part| {
                part.quoting == Quoting::Bare
                    && matches!(&part.kind, PartKind::Text(text) if text.starts_with('/'))
            });
            if !slash {
                self.parts[0].kind = if suffix
                    .parts
                    .first()
                    .is_some_and(|part| part.quoting != Quoting::Bare)
                {
                    PartKind::Text("~".into())
                } else {
                    PartKind::Unknown
                };
            }
        }
        // 换行续接后的片段不是词首，`a\<newline>~/b` 中的 ~ 不展开。
        for part in &mut suffix.parts {
            if part.kind == PartKind::Home {
                part.kind = PartKind::Text("~".into());
            }
        }
        self.parts.extend(suffix.parts);
    }

    fn push(&mut self, kind: PartKind, quoting: Quoting) {
        if matches!(&kind, PartKind::Text(text) if text.is_empty()) {
            return;
        }
        self.parts.push(WordPart { kind, quoting });
    }

    fn collect(&mut self, node: Node<'_>, src: &str, quoting: Quoting, start: usize, end: usize) {
        let raw = node.utf8_text(src.as_bytes()).unwrap_or_default();
        match node.kind() {
            "concatenation" => {
                let mut offset = node.start_byte();
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.text(&src[offset..child.start_byte()], quoting);
                    self.collect(child, src, quoting, start, end);
                    offset = child.end_byte();
                }
                self.text(&src[offset..node.end_byte()], quoting);
            }
            "string" => {
                if raw == "\"\"" {
                    self.parts.push(WordPart {
                        kind: PartKind::Text(String::new()),
                        quoting: Quoting::Double,
                    });
                    return;
                }
                let mut offset = node.start_byte() + 1;
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.text(&src[offset..child.start_byte()], Quoting::Double);
                    self.collect(child, src, Quoting::Double, start, end);
                    offset = child.end_byte();
                }
                self.text(&src[offset..node.end_byte() - 1], Quoting::Double);
            }
            "raw_string" => self.parts.push(WordPart {
                kind: PartKind::Text(raw[1..raw.len() - 1].into()),
                quoting: Quoting::Single,
            }),
            "simple_expansion" | "expansion" => {
                let mut cursor = node.walk();
                let children: Vec<_> = node.named_children(&mut cursor).collect();
                let name = children
                    .first()
                    .filter(|child| child.kind() == "variable_name")
                    .and_then(|child| child.utf8_text(src.as_bytes()).ok());
                if let Some(name) = name.filter(|name| {
                    children.len() == 1
                        && (raw == format!("${name}") || raw == format!("${{{name}}}"))
                }) {
                    self.push(PartKind::Variable(name.into()), quoting);
                } else {
                    self.push(PartKind::Unknown, quoting);
                }
            }
            "word" | "number" | "string_content" if node.named_child_count() == 0 => {
                if quoting == Quoting::Bare
                    && node.start_byte() == start
                    && raw.starts_with('~')
                    && (node.end_byte() == end || raw.contains('/'))
                    && !raw.split('/').next().unwrap_or_default().contains('\\')
                {
                    let prefix_end = raw.find('/').unwrap_or(raw.len());
                    self.push(
                        if &raw[..prefix_end] == "~" {
                            PartKind::Home
                        } else {
                            PartKind::Unknown
                        },
                        quoting,
                    );
                    self.text(&raw[prefix_end..], quoting);
                } else {
                    self.text(raw, quoting);
                }
            }
            _ => self.push(PartKind::Unknown, quoting),
        }
    }

    fn text(&mut self, raw: &str, quoting: Quoting) {
        let mut chars = raw.chars();
        let mut text = String::new();
        let mut pattern = false;
        while let Some(ch) = chars.next() {
            if ch == '\\' && quoting != Quoting::Single {
                if let Some(escaped) = chars.next() {
                    if escaped == '\n' {
                        continue;
                    }
                    if quoting == Quoting::Double && !matches!(escaped, '"' | '\\' | '$' | '`') {
                        text.push('\\');
                    }
                    text.push(escaped);
                } else {
                    self.push(PartKind::Unknown, quoting);
                }
            } else {
                pattern |= quoting == Quoting::Bare && matches!(ch, '*' | '?' | '[' | '{');
                text.push(ch);
            }
        }
        self.push(
            if pattern {
                PartKind::Pattern(text)
            } else {
                PartKind::Text(text)
            },
            quoting,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::analyze_bash_facts;

    fn word(src: &str) -> ShellWord {
        let facts = analyze_bash_facts(&format!("printf %s {src}"));
        assert!(facts.reliable, "{src}: {facts:#?}");
        facts
            .commands
            .iter()
            .find(|command| command.range.start == 0)
            .unwrap()
            .words[2]
            .syntax
            .clone()
    }

    /// 与真实 shell 的参数字节对照，覆盖等价拼写及必须保持不等价的反例。
    #[cfg(unix)]
    #[test]
    fn resolved_words_agree_with_bash() {
        let home = Path::new("/tmp/kloop-word-home");
        for src in [
            "~",
            "~/notes.txt",
            "~/'notes.txt'",
            "~/\"notes.txt\"",
            "$HOME/notes.txt",
            "${HOME}/notes.txt",
            "\"$HOME/notes.txt\"",
            "\"${HOME}/notes.txt\"",
            "\"\"$HOME/notes.txt",
            "''${HOME}/notes.txt",
            "$HOME/'notes.txt'",
            "\"$HOME\"/'notes.txt'",
            "\"\"\"$HOME\"/notes.txt",
            "\"$HOME.notes.txt\"",
            "${HOME}.notes.txt",
            "\"$HOME/O'Brien.txt\"",
            "\"$HOME\"/'a b.txt'",
            "\"~/.bashrc\"",
            "'~/.bashrc'",
            "''~/notes.txt",
            "\\~/notes.txt",
            "'${HOME}/notes.txt'",
            "\"\\$HOME/notes.txt\"",
            "a\\ b.txt",
            "\"a\\q.txt\"",
            "'a\\b.txt'",
            "\"a\\\"b.txt\"",
            "\"a\\\\b.txt\"",
            "a\\\nb.txt",
            "\"a\\\nb.txt\"",
            "'中文.txt'",
            "'*.txt'",
            "\\*.txt",
            "\"\"",
            "''",
            "123",
            "\"a'b\"",
            "~\"/notes.txt\"",
            "~''/notes.txt",
            "~\\/notes.txt",
            "a\\\n~/notes.txt",
            "~\\\n/notes.txt",
            "~\\\n\"/notes.txt\"",
        ] {
            let actual = std::process::Command::new("/bin/bash")
                .args([
                    "--noprofile",
                    "--norc",
                    "-c",
                    &format!("printf '%s\\0' {src}"),
                ])
                .env_clear()
                .env("HOME", home)
                .output()
                .unwrap();
            assert!(actual.status.success(), "{src}: {actual:?}");
            let WordEvaluation::Resolved(expected) = word(src).evaluate(Some(home)) else {
                panic!("expected resolved word: {src}: {:?}", word(src));
            };
            let mut expected = expected.into_bytes();
            expected.push(0);
            assert_eq!(actual.stdout, expected, "{src}");
        }
    }

    #[test]
    fn variable_boundaries_and_unknown_parts_stay_explicit() {
        let home = Some(Path::new("/tmp/home"));
        assert_eq!(
            word("\"$HOMEBREW_PREFIX/notes\"").evaluate(home),
            WordEvaluation::Partial("\0/notes".into())
        );
        assert_eq!(
            word("\"$HOME_SUFFIX\"").evaluate(home),
            WordEvaluation::Unknown
        );
        assert_eq!(
            word("${HOME:-/tmp}/notes").evaluate(home),
            WordEvaluation::Partial("\0/notes".into())
        );
        assert_eq!(
            word("\"$(printf x)/.bashrc\"").evaluate(home),
            WordEvaluation::Partial("\0/.bashrc".into())
        );
        assert_eq!(
            word(".en?").evaluate(home),
            WordEvaluation::Partial(".en?".into())
        );
        assert_eq!(
            word(".env{a,b}").evaluate(home),
            WordEvaluation::Partial(".env{a,b}".into())
        );
        assert_eq!(
            word("$HOME/.bashrc").evaluate(None),
            WordEvaluation::Partial("\0/.bashrc".into())
        );
        assert_eq!(
            word("$HOME/notes").evaluate(Some(Path::new("/tmp/a b"))),
            WordEvaluation::Partial("/tmp/a b/notes".into())
        );
        assert_eq!(
            word("\"$HOME/notes\"").evaluate(Some(Path::new("/tmp/a b"))),
            WordEvaluation::Resolved("/tmp/a b/notes".into())
        );
        assert_eq!(
            word("~other/notes").evaluate(home),
            WordEvaluation::Partial("\0/notes".into())
        );
    }

    #[test]
    fn quotation_belongs_to_each_part() {
        assert_eq!(
            word("$HOME/'.bashrc'"),
            ShellWord {
                parts: vec![
                    WordPart {
                        kind: PartKind::Variable("HOME".into()),
                        quoting: Quoting::Bare
                    },
                    WordPart {
                        kind: PartKind::Text("/".into()),
                        quoting: Quoting::Bare
                    },
                    WordPart {
                        kind: PartKind::Text(".bashrc".into()),
                        quoting: Quoting::Single
                    },
                ]
            }
        );
        assert_eq!(
            word("\"$HOME/O'Brien.txt\""),
            ShellWord {
                parts: vec![
                    WordPart {
                        kind: PartKind::Variable("HOME".into()),
                        quoting: Quoting::Double
                    },
                    WordPart {
                        kind: PartKind::Text("/O'Brien.txt".into()),
                        quoting: Quoting::Double
                    },
                ]
            }
        );
    }
}
