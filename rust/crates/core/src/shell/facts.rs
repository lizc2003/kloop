//! 严格授权之外的局部证据；未知词保留位置，不能拿这些证据证明整条脚本安全。

use std::ops::Range;

use super::*;

const MAX_SCRIPT_BYTES: usize = 1_048_576;
const MAX_NODES: usize = 20_000;
const MAX_TREE_DEPTH: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WordValue {
    Literal(String),
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WordFact {
    pub value: WordValue,
    pub range: Range<usize>,
    /// 源文本。`value` 只保留可静态确定的字面量；glob、`~`、`$HOME` 这类
    /// 展开拼写落进 `Unknown` 后仍要能复核，不能只剩一个“未知”标记。
    pub raw: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Context {
    pub pipeline: bool,
    pub substitution: bool,
    pub compound: bool,
    pub cwd_unknown: bool,
    /// 嵌套 -c 的范围属于它自己的源字符串，不能拿来裁外层原文。
    pub script_depth: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandFact {
    pub words: Vec<WordFact>,
    pub range: Range<usize>,
    pub context: Context,
}

impl CommandFact {
    pub fn argv(&self) -> Option<Vec<String>> {
        self.words
            .iter()
            .map(|word| match &word.value {
                WordValue::Literal(value) => Some(value.clone()),
                WordValue::Unknown => None,
            })
            .collect()
    }

    /// 只取连续的已知前缀；遇到未知词立即停止，绝不拼接后面的参数。
    pub fn literal_prefix(&self) -> Vec<String> {
        self.words
            .iter()
            .map_while(|word| match &word.value {
                WordValue::Literal(value) => Some(value.clone()),
                WordValue::Unknown => None,
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Read,
    Write,
    Descriptor,
    HereDoc,
    HereString,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RedirectFact {
    pub direction: Direction,
    pub fd: Option<String>,
    pub target: WordValue,
    /// 目标节点的源文本；动态目标（`~`、`$HOME`、glob）靠它复核落点。
    pub raw_target: Option<String>,
    pub range: Range<usize>,
    pub context: Context,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiteralInput {
    pub body: Range<usize>,
    pub delimiter: String,
    pub quoted: bool,
    pub consumer: Option<Range<usize>>,
    pub context: Context,
    pub eligible: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BashFacts {
    pub analysis: BashAnalysis,
    pub commands: Vec<CommandFact>,
    pub redirects: Vec<RedirectFact>,
    pub literal_inputs: Vec<LiteralInput>,
    pub unknown: Vec<String>,
    pub reliable: bool,
}

pub(crate) fn analyze(script: &str) -> BashFacts {
    analyze_depth(script, 0)
}

fn analyze_depth(src: &str, depth: u8) -> BashFacts {
    let mut facts = BashFacts {
        analysis: BashAnalysis::Opaque,
        commands: Vec::new(),
        redirects: Vec::new(),
        literal_inputs: Vec::new(),
        unknown: Vec::new(),
        reliable: false,
    };
    if src.len() > MAX_SCRIPT_BYTES {
        facts.unknown.push("script limit".into());
        return facts;
    }
    let Some(tree) = parse_bash(src) else {
        facts.unknown.push("parse failure".into());
        return facts;
    };
    if tree.root_node().has_error() {
        facts.unknown.push("parse error".into());
        return facts;
    }
    let mut stack = vec![(tree.root_node(), 0)];
    let mut nodes = Vec::new();
    while let Some((node, tree_depth)) = stack.pop() {
        if nodes.len() == MAX_NODES || tree_depth > MAX_TREE_DEPTH || node.is_missing() {
            facts.unknown.push("node limit or missing node".into());
            return facts;
        }
        nodes.push(node);
        let mut cursor = node.walk();
        stack.extend(
            node.children(&mut cursor)
                .map(|child| (child, tree_depth + 1)),
        );
    }
    nodes.sort_by_key(Node::start_byte);
    let cwd_unknown = nodes.iter().any(|node| {
        node.kind() == "command"
            && node.child_by_field_name("name").is_some_and(|name| {
                matches!(name.utf8_text(src.as_bytes()), Ok("cd" | "pushd" | "popd"))
            })
    });
    facts.reliable = true;
    let strict = word_only_commands_sequence(&tree, src);
    let mut nested_analyses = Vec::new();
    for node in nodes {
        let context = context(node, depth, cwd_unknown);
        match node.kind() {
            "command" => {
                let mut words = Vec::new();
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if matches!(
                        child.kind(),
                        "comment" | "file_redirect" | "herestring_redirect" | "variable_assignment"
                    ) {
                        continue;
                    }
                    let word = if child.kind() == "command_name" {
                        child.named_child(0).unwrap_or(child)
                    } else {
                        child
                    };
                    words.push(WordFact {
                        value: literal(word, src),
                        range: child.byte_range(),
                        raw: word
                            .utf8_text(src.as_bytes())
                            .unwrap_or_default()
                            .to_string(),
                    });
                }
                let command = CommandFact {
                    words,
                    range: node.byte_range(),
                    context,
                };
                if command.argv().is_none() {
                    facts.unknown.push("dynamic argument".into());
                }
                if let Some(argv) = command.argv() {
                    let stripped = strip_wrappers(&argv);
                    if stripped
                        .first()
                        .is_some_and(|name| matches!(executable_name(name), "sh" | "bash" | "zsh"))
                        && extract_wrapped_script(&stripped).is_none()
                    {
                        facts.unknown.push("unsupported shell wrapper".into());
                    }
                    if let Some(inner) = extract_wrapped_script(&stripped) {
                        if depth < MAX_UNWRAP_DEPTH {
                            let nested = analyze_depth(inner, depth + 1);
                            nested_analyses.push((argv.clone(), nested.analysis.clone()));
                            facts.reliable &= nested.reliable;
                            facts.commands.extend(nested.commands);
                            facts.redirects.extend(nested.redirects);
                            facts.unknown.extend(nested.unknown);
                            // 嵌套文本经过引号解码；本轮不从它生成外层正文豁免。
                        } else {
                            facts.reliable = false;
                            facts.unknown.push("wrapper limit".into());
                        }
                    }
                }
                facts.commands.push(command);
            }
            "file_redirect" | "herestring_redirect" | "heredoc_redirect" => {
                let direction = match node.kind() {
                    "herestring_redirect" => Direction::HereString,
                    "heredoc_redirect" => Direction::HereDoc,
                    _ => {
                        let mut cursor = node.walk();
                        let op = node
                            .children(&mut cursor)
                            .find(|child| !child.is_named())
                            .map(|child| child.kind());
                        match op {
                            Some("<") => Direction::Read,
                            Some(">" | ">>" | ">|" | "&>" | "&>>") => Direction::Write,
                            _ => Direction::Descriptor,
                        }
                    }
                };
                let fd = node
                    .child_by_field_name("descriptor")
                    .and_then(|fd| fd.utf8_text(src.as_bytes()).ok())
                    .map(str::to_string);
                let mut cursor = node.walk();
                let mut targets: Vec<_> = node
                    .children_by_field_name("destination", &mut cursor)
                    .map(|target| {
                        let raw = target.utf8_text(src.as_bytes()).ok().map(str::to_string);
                        (literal(target, src), raw)
                    })
                    .collect();
                if targets.is_empty() {
                    targets.push((WordValue::Unknown, None));
                }
                for (target, raw_target) in targets {
                    facts.redirects.push(RedirectFact {
                        direction,
                        fd: fd.clone(),
                        target,
                        raw_target,
                        range: node.byte_range(),
                        context: context.clone(),
                    });
                }
                if node.kind() == "heredoc_redirect" {
                    facts.literal_inputs.push(heredoc(node, src, context));
                }
            }
            "program"
            | "list"
            | "pipeline"
            | "redirected_statement"
            | "command_name"
            | "word"
            | "number"
            | "string"
            | "raw_string"
            | "string_content"
            | "concatenation"
            | "comment"
            | "file_descriptor"
            | "heredoc_start"
            | "heredoc_body"
            | "heredoc_end" => {}
            kind if node.is_named() && !facts.unknown.iter().any(|unknown| unknown == kind) => {
                facts.unknown.push(kind.into());
            }
            _ => {}
        }
    }
    if facts.reliable
        && let Some((commands, paths)) = strict
    {
        facts.analysis = strict_analysis(commands, paths, depth, &nested_analyses);
    }
    facts
}

fn context(node: Node, depth: u8, cwd_unknown: bool) -> Context {
    let mut context = Context {
        script_depth: depth,
        cwd_unknown,
        ..Context::default()
    };
    let mut parent = node.parent();
    while let Some(node) = parent {
        match node.kind() {
            "pipeline" => context.pipeline = true,
            "command_substitution" | "process_substitution" => context.substitution = true,
            "program" | "list" | "redirected_statement" | "command" | "heredoc_redirect" => {}
            _ => context.compound = true,
        }
        parent = node.parent();
    }
    context
}

fn literal(node: Node, src: &str) -> WordValue {
    if node.kind() == "word"
        && let Ok(raw @ (r"\(" | r"\)" | r"\!")) = node.utf8_text(src.as_bytes())
    {
        return WordValue::Literal(raw[1..].to_string());
    }
    // word 也可能含 glob/转义；不把它当成固定参数数量或实际路径。
    fixed_output_target(node, src).map_or(WordValue::Unknown, WordValue::Literal)
}

fn delimiter(raw: &str) -> Option<String> {
    if let Some(value) = raw
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    {
        return (!value.is_empty() && !value.contains('\'')).then(|| value.to_string());
    }
    if let Some(value) = raw
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        return (!value.is_empty() && !value.contains(['"', '\\', '$', '`']))
            .then(|| value.to_string());
    }
    if let Some(value) = raw.strip_prefix('\\') {
        return (!value.is_empty()
            && value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'))
        .then(|| value.to_string());
    }
    None
}

fn heredoc(node: Node, src: &str, context: Context) -> LiteralInput {
    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    let start = children
        .iter()
        .find(|child| child.kind() == "heredoc_start");
    let end = children.iter().find(|child| child.kind() == "heredoc_end");
    let body = children.iter().find(|child| child.kind() == "heredoc_body");
    let raw = start
        .and_then(|start| start.utf8_text(src.as_bytes()).ok())
        .unwrap_or("");
    let decoded = delimiter(raw);
    let mut input = LiteralInput {
        body: body.map_or(node.end_byte()..node.end_byte(), |body| body.byte_range()),
        delimiter: decoded.clone().unwrap_or_else(|| raw.to_string()),
        quoted: decoded.is_some(),
        consumer: None,
        context: context.clone(),
        eligible: false,
    };
    if !input.quoted
        || context.pipeline
        || context.substitution
        || context.compound
        || context.script_depth != 0
    {
        return input;
    }
    let Some(parent) = node
        .parent()
        .filter(|parent| parent.kind() == "redirected_statement")
    else {
        return input;
    };
    let Some(command) = parent
        .child_by_field_name("body")
        .filter(|body| body.kind() == "command")
    else {
        return input;
    };
    input.consumer = Some(command.byte_range());
    if !matches!(plain_command_argv(command, src).as_deref(), Some([name]) if matches!(name.as_str(), "cat" | "/bin/cat" | "/usr/bin/cat"))
    {
        return input;
    }
    let Some(end) = end else {
        return input;
    };
    let Some(body) = body else {
        return input;
    };
    if end.utf8_text(src.as_bytes()).ok() != decoded.as_deref()
        || body.end_byte() > end.start_byte()
    {
        return input;
    }
    // 同一行的管道/条件右项可能藏在 heredoc 节点里；只认可正文与文件重定向。
    if children.iter().any(|child| {
        !matches!(
            child.kind(),
            "heredoc_start" | "heredoc_body" | "heredoc_end" | "file_redirect"
        )
    }) {
        return input;
    }
    let mut redirects = Vec::new();
    let mut cursor = parent.walk();
    for child in parent.named_children(&mut cursor) {
        if child.id() == command.id() || child.id() == node.id() {
            continue;
        }
        if child.kind() != "file_redirect" {
            return input;
        }
        redirects.push(child);
    }
    redirects.extend(
        children
            .iter()
            .copied()
            .filter(|child| child.kind() == "file_redirect"),
    );
    // 一个 stdin heredoc + 一个 stdout 文件；不接受 fd、附加重定向或重复目标。
    if node.child_by_field_name("descriptor").is_some() || redirects.len() != 1 {
        return input;
    }
    let output = redirects[0];
    if output.child_by_field_name("descriptor").is_some() {
        return input;
    }
    let mut cursor = output.walk();
    let operators: Vec<_> = output
        .children(&mut cursor)
        .filter(|child| !child.is_named())
        .map(|child| child.kind())
        .collect();
    if operators != [">"]
        || !matches!(output_redirect_paths(output, src), Some(paths) if paths.len() == 1)
    {
        return input;
    }
    input.eligible = true;
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan239_unknown_words_keep_positions_and_ranges() {
        let src = "rg \"$PATTERN\" --file .env";
        let facts = analyze(src);
        assert_eq!(
            facts.commands,
            vec![CommandFact {
                words: vec![
                    WordFact {
                        value: WordValue::Literal("rg".into()),
                        range: 0..2,
                        raw: "rg".into()
                    },
                    WordFact {
                        value: WordValue::Unknown,
                        range: 3..13,
                        raw: "\"$PATTERN\"".into()
                    },
                    WordFact {
                        value: WordValue::Literal("--file".into()),
                        range: 14..20,
                        raw: "--file".into()
                    },
                    WordFact {
                        value: WordValue::Literal(".env".into()),
                        range: 21..25,
                        raw: ".env".into()
                    },
                ],
                range: 0..25,
                context: Context::default(),
            }]
        );
        assert_eq!(facts.analysis, BashAnalysis::Opaque);
        assert_eq!(facts.commands[0].literal_prefix(), vec!["rg"]);
    }

    #[test]
    fn plan239_heredoc_boundaries_and_variants() {
        for delimiter in ["'EOF'", "\"EOF\"", "\\EOF"] {
            for operator in ["<<", "<<-"] {
                for prefix in [true, false] {
                    let command = if prefix {
                        format!("cat > /tmp/probe {operator}{delimiter}")
                    } else {
                        format!("cat {operator}{delimiter} > /tmp/probe")
                    };
                    let body = "~/.ssh/id_rsa $(echo data) `rm -rf data`; 中文\n";
                    let src = format!("{command}\n{body}EOF\necho done");
                    let facts = analyze(&src);
                    assert!(facts.reliable, "{src}: {facts:#?}");
                    assert_eq!(
                        facts.literal_inputs,
                        vec![LiteralInput {
                            body: command.len() + 1..command.len() + 1 + body.len(),
                            delimiter: "EOF".into(),
                            quoted: true,
                            consumer: Some(0..3),
                            context: Context::default(),
                            eligible: true,
                        }],
                        "{src}: {facts:#?}"
                    );
                    assert_eq!(facts.analysis, BashAnalysis::Opaque);
                    assert_eq!(facts.commands.len(), 2, "body is data: {facts:#?}");
                    let output = facts
                        .redirects
                        .iter()
                        .find(|redirect| redirect.direction == Direction::Write)
                        .unwrap();
                    let output_start = src.find("> /tmp/probe").unwrap();
                    assert_eq!(
                        output,
                        &RedirectFact {
                            direction: Direction::Write,
                            fd: None,
                            target: WordValue::Literal("/tmp/probe".into()),
                            raw_target: Some("/tmp/probe".into()),
                            range: output_start..output_start + "> /tmp/probe".len(),
                            context: Context::default(),
                        }
                    );
                }
            }
        }
        let src = "cat > /tmp/probe <<-'EOF'\n\t~/.ssh/id\n\tEOF\n";
        let facts = analyze(src);
        assert!(facts.literal_inputs[0].eligible, "{facts:#?}");
    }

    #[test]
    fn plan239_heredoc_rejects_ambiguous_flows() {
        for src in [
            "cat >/tmp/x <<EOF\n~/.ssh/id\nEOF",
            "sh >/tmp/x <<'EOF'\n~/.ssh/id\nEOF",
            "cat file >/tmp/x <<'EOF'\n~/.ssh/id\nEOF",
            "cat >/tmp/x <<'EOF' | sh\n~/.ssh/id\nEOF",
            "cat >/tmp/x <<'EOF' && cat .env\n~/.ssh/id\nEOF",
            "cat >/tmp/x 2>/tmp/err <<'EOF'\n~/.ssh/id\nEOF",
            "cat >/tmp/x 0<<'EOF'\n~/.ssh/id\nEOF",
            "cat 1>/tmp/x <<'EOF'\n~/.ssh/id\nEOF",
            "cat >/tmp/x <<'A' <<'B'\n~/.ssh/id\nA\n~/.ssh/id\nB",
            "(cat >/tmp/x <<'EOF'\n~/.ssh/id\nEOF\n)",
            "echo $(cat >/tmp/x <<'EOF'\n~/.ssh/id\nEOF\n)",
        ] {
            let facts = analyze(src);
            assert!(
                facts.literal_inputs.iter().all(|input| !input.eligible),
                "{src}: {facts:#?}"
            );
            assert_eq!(facts.analysis, BashAnalysis::Opaque);
        }
    }

    #[test]
    fn plan239_partial_context_and_limits() {
        let facts = analyze("cd next; if true; then cat .env; fi; echo $(cat /tmp/file)");
        assert!(
            facts
                .commands
                .iter()
                .all(|command| command.context.cwd_unknown)
        );
        assert!(
            facts
                .commands
                .iter()
                .any(|command| command.context.compound)
        );
        assert!(
            facts
                .commands
                .iter()
                .any(|command| command.context.substitution)
        );
        for src in [
            "cat <<'EOF'\n~/.ssh/id",
            "echo $(",
            "bash -c 'sh -c \"zsh -c true\"'",
        ] {
            let facts = analyze(src);
            assert!(!facts.reliable, "{src}: {facts:#?}");
            assert_eq!(facts.analysis, BashAnalysis::Opaque);
        }
        let facts = analyze(&"x".repeat(MAX_SCRIPT_BYTES + 1));
        assert!(!facts.reliable);
        assert_eq!(facts.analysis, BashAnalysis::Opaque);
        let facts = analyze(&"true;".repeat(MAX_NODES));
        assert!(!facts.reliable);
        assert_eq!(facts.analysis, BashAnalysis::Opaque);
        let src = format!(
            "{}true{}",
            "( ".repeat(MAX_TREE_DEPTH),
            " )".repeat(MAX_TREE_DEPTH)
        );
        let facts = analyze(&src);
        assert!(!facts.reliable);
        assert_eq!(facts.analysis, BashAnalysis::Opaque);
        let facts = analyze("bash -x -c 'cat .env'");
        assert_eq!(facts.analysis, BashAnalysis::Opaque);
        assert!(facts.unknown.contains(&"unsupported shell wrapper".into()));
    }
}
