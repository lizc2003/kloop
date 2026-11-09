//! 严格授权之外的局部证据；未知词保留位置，不能拿这些证据证明整条脚本安全。

use std::ops::Range;

use super::*;

const MAX_SCRIPT_BYTES: usize = 1_048_576;
const MAX_NODES: usize = 20_000;
const MAX_TREE_DEPTH: usize = 128;
/// 循环值个数 × 正文引用次数。
const MAX_LOOP_BINDINGS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WordValue {
    Literal(String),
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WordFact {
    pub value: WordValue,
    pub range: Range<usize>,
    pub syntax: ShellWord,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Context {
    pub pipeline: bool,
    pub substitution: bool,
    pub compound: bool,
    pub cwd_unknown: bool,
    /// 有环境赋值或未建模的 shell 状态变更时，不拿启动环境推算展开。
    pub home_unknown: bool,
    /// xargs 在执行前改写了词；未知位置的命令、参数不能按字面风险排除。
    pub argument_rewrite: bool,
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
    fn rewritten_execution(&self) -> Self {
        let mut command = self.clone();
        command.words.truncate(1);
        command.words[0].value = WordValue::Unknown;
        command.words[0].syntax = ShellWord::unknown();
        command.context.argument_rewrite = true;
        command.context.script_depth += 1;
        command
    }

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

    fn wrapper_prefix(&self) -> Vec<String> {
        let mut argv = Vec::new();
        for word in &self.words {
            match word.syntax.evaluate(None) {
                WordEvaluation::Resolved(value) => argv.push(value),
                // 单独的 {} 不发生 brace expansion；它也是 xargs 的默认替换串。
                WordEvaluation::Partial(value) if value == "{}" => argv.push(value),
                WordEvaluation::Partial(_) => {
                    let Some(value) = word.syntax.fixed_assignment() else {
                        break;
                    };
                    argv.push(value);
                    let wrappers = analyze_wrappers(&argv);
                    // 只越过 env 控制区中的固定赋值，不能把 -c 正文或其它参数猜成字面词。
                    let fixed_assignment = wrappers.complete
                        && wrappers.argv.is_empty()
                        && wrappers
                            .wrapper_commands
                            .last()
                            .is_some_and(|wrapper| executable_name(&wrapper[0]) == "env");
                    if !fixed_assignment {
                        argv.pop();
                        break;
                    }
                }
                WordEvaluation::Unknown => break,
            }
        }
        argv
    }

    /// 未知词保留成不可执行占位符，后面的已知词仍在原位置。只供 deny 与危险判定
    /// 这类 blocklist 使用，不能进入 allow、只读或记忆。
    pub(crate) fn risk_argv(&self) -> Vec<String> {
        self.words
            .iter()
            .map(|word| match &word.value {
                WordValue::Literal(value) => value.clone(),
                WordValue::Unknown => word
                    .syntax
                    .fixed_assignment()
                    .unwrap_or_else(|| "\0unknown".into()),
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
    pub syntax: ShellWord,
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
    /// 解析树和各层局部事实是否完整；语法错误或节点上限会丢掉整段证据，
    /// 未知包装器效果只是不可严格授权，两者不能混为一谈。
    pub facts_complete: bool,
    /// 语法仍是完整的字面命令，但包装器带未知效果：升级只可逐字记忆，不能退到通配。
    pub verbatim_escalation: bool,
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
        facts_complete: false,
        verbatim_escalation: false,
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
            facts.facts_complete = false;
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
    // 只在顶层细化：绑定值可能再作为嵌套脚本分析，逐层展开会相乘。
    let binding = (depth == 0).then(|| literal_loop(&nodes, src)).flatten();
    let cwd_unknown = nodes.iter().any(|node| {
        node.kind() == "command"
            && node
                .child_by_field_name("name")
                .is_some_and(|name| name.utf8_text(src.as_bytes()).is_ok_and(changes_directory))
    });
    let home_unknown = nodes.iter().any(|node| {
        matches!(
            node.kind(),
            "variable_assignment"
                | "function_definition"
                | "arithmetic_expansion"
                | "arithmetic_command"
        ) || (node.kind() == "expansion" && node.named_child_count() > 1)
            || (node.kind() == "command"
                && node.child_by_field_name("name").is_some_and(|name| {
                    name.utf8_text(src.as_bytes())
                        .is_ok_and(changes_environment)
                }))
    });
    facts.reliable = true;
    facts.facts_complete = true;
    let strict = word_only_commands_sequence(&tree, src);
    let mut nested_analyses = Vec::new();
    let mut joined_words = false;
    let mut wrapper_effects_unknown = false;
    for node in nodes {
        let context = context(node, depth, cwd_unknown, home_unknown);
        match node.kind() {
            "command" => {
                let (words, joined) = command_words(node, src);
                joined_words |= joined;
                // 每个循环值各走一遍完整分析，与把值写在原处一致（含嵌套 -c 正文）。
                for words in bind_loop(binding.as_ref(), node, words, |words, variable, value| {
                    // WordValue 保留 Unknown：deny/ask 仍按未知词保守匹配，不能产生授权。
                    words.iter_mut().fold(false, |bound, word| {
                        word.syntax.bind_quoted_variable(variable, value) | bound
                    })
                }) {
                    let command = CommandFact {
                        words,
                        range: node.byte_range(),
                        context: context.clone(),
                    };
                    let argv = command.wrapper_prefix();
                    let dynamic_argv = argv.len() != command.words.len();
                    if dynamic_argv {
                        facts.unknown.push("dynamic argument".into());
                    }
                    let wrappers = analyze_wrappers(&argv);
                    // 剥离成功后仍有命令名，普通动态参数不等于未知执行体。
                    if !wrappers.complete || (dynamic_argv && wrappers.argv.is_empty()) {
                        facts.reliable = false;
                        facts.unknown.push(
                            if dynamic_argv {
                                "dynamic wrapper argument"
                            } else {
                                "unsupported wrapper"
                            }
                            .into(),
                        );
                        // 包装器没能定位被包命令时，完整路径会递归分析的 `sh -c` 正文在这里
                        // 无人检查：`sudo -X sh -c 'cat .env'` 曾在 bypass 下放行。近似剥离后
                        // 任一 shell 可能进入 -c 模式，就按未知执行体处理。
                        let shell_script = wrappers.argv.iter().enumerate().any(|(index, word)| {
                            matches!(executable_name(word), "sh" | "bash" | "zsh")
                                && !matches!(
                                    shell_command_string(&wrappers.argv[index + 1..]),
                                    Ok(None)
                                )
                        });
                        if wrappers.arguments_unknown
                            || shell_script
                            || (dynamic_argv && !wrappers.wrapper_commands.is_empty())
                        {
                            facts.commands.push(command.rewritten_execution());
                        }
                        facts.commands.push(command);
                        continue;
                    }
                    let unknown_effects = wrappers.arguments_unknown || wrappers.writes_unknown;
                    wrapper_effects_unknown |= unknown_effects;
                    facts.reliable &= !unknown_effects;
                    let stripped = wrappers.argv;
                    let rewritten_words = wrappers.arguments_unknown
                        && stripped.iter().any(|word| wrappers.rewrites_argument(word));
                    let shell = stripped
                        .first()
                        .is_some_and(|name| matches!(executable_name(name), "sh" | "bash" | "zsh"));
                    if wrappers.rewrites_wrapper()
                        || (wrappers.arguments_unknown && dynamic_argv)
                        || (shell && rewritten_words)
                    {
                        // 替换能改变包装器选项或给 -c 注入语法，整个执行体均未知。
                        // 不能继续把替换前的脚本解析成“实际会执行”的字面命令。
                        facts.commands.push(command.rewritten_execution());
                        facts.commands.push(command);
                        continue;
                    }
                    if rewritten_words {
                        let mut child = command.clone();
                        child.words.drain(..argv.len() - stripped.len());
                        for (word, argument) in child.words.iter_mut().zip(stripped) {
                            if wrappers.rewrites_argument(argument) {
                                word.value = WordValue::Unknown;
                                word.syntax = match wrappers.replacements.as_slice() {
                                    [replacement] => {
                                        ShellWord::replaced(argument, replacement.marker)
                                    }
                                    // 多层替换可能跨越未知片段与已知文本的边界。
                                    _ => ShellWord::unknown(),
                                };
                            } else {
                                word.value = WordValue::Literal(argument.clone());
                            }
                        }
                        child.context.argument_rewrite = true;
                        child.context.script_depth += 1;
                        facts.commands.push(child);
                    }
                    // 风险事实可分析已知脚本及其后的位置参数；严格授权仍要求完整 argv。
                    let inner = match stripped.split_first() {
                        Some((_, args)) if shell => shell_command_string(args),
                        _ => Ok(None),
                    };
                    if shell && !matches!(inner, Ok(Some(_))) {
                        facts.reliable &= !dynamic_argv && inner.is_ok();
                        facts.unknown.push("unsupported shell wrapper".into());
                    }
                    if inner.is_err() {
                        // 命令串位置不明：正文按未知执行体处理，不能当普通参数逐个查路径。
                        facts.commands.push(command.rewritten_execution());
                    }
                    if let Ok(Some((inner, startup))) = inner {
                        if depth < MAX_UNWRAP_DEPTH {
                            let mut nested = analyze_depth(inner, depth + 1);
                            // -e、-u、-o pipefail 不改启动文件；登录、交互 shell 会读 profile/rc。
                            let startup_unknown = startup
                                || stripped
                                    .first()
                                    .is_some_and(|shell| executable_name(shell) == "zsh");
                            let nested_home_unknown = home_unknown
                                || startup_unknown
                                || wrappers.environment_unknown
                                || wrappers.arguments_unknown;
                            let nested_cwd_unknown = cwd_unknown
                                || startup_unknown
                                || wrappers.cwd_unknown
                                || wrappers.arguments_unknown;
                            if nested_home_unknown || nested_cwd_unknown {
                                for command in &mut nested.commands {
                                    command.context.home_unknown |= nested_home_unknown;
                                    command.context.cwd_unknown |= nested_cwd_unknown;
                                }
                                for redirect in &mut nested.redirects {
                                    redirect.context.home_unknown |= nested_home_unknown;
                                    redirect.context.cwd_unknown |= nested_cwd_unknown;
                                }
                            }
                            nested_analyses.push((argv.clone(), nested.analysis.clone()));
                            wrapper_effects_unknown |= nested.verbatim_escalation;
                            facts.reliable &= nested.reliable;
                            facts.facts_complete &= nested.facts_complete;
                            facts.commands.extend(nested.commands);
                            facts.redirects.extend(nested.redirects);
                            facts.unknown.extend(nested.unknown);
                            // 嵌套文本经过引号解码；本轮不从它生成外层正文豁免。
                        } else {
                            facts.reliable = false;
                            facts.facts_complete = false;
                            facts.unknown.push("wrapper limit".into());
                        }
                    }
                    facts.commands.push(command);
                }
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
                let mut targets = Vec::new();
                for target in node.children_by_field_name("destination", &mut cursor) {
                    joined_words |= push_word(&mut targets, target, target.byte_range(), src);
                }
                let mut targets: Vec<_> = targets.into_iter().map(|word| word.syntax).collect();
                if targets.is_empty() {
                    targets.push(ShellWord::unknown());
                }
                for syntax in targets.into_iter().flat_map(|syntax| {
                    bind_loop(
                        binding.as_ref(),
                        node,
                        syntax,
                        ShellWord::bind_quoted_variable,
                    )
                }) {
                    facts.redirects.push(RedirectFact {
                        direction,
                        fd: fd.clone(),
                        syntax,
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
    // 命令名也可能由引用或换行续接构成；最终按保留位置的字面事实复核状态变更。
    let names: Vec<_> = facts
        .commands
        .iter()
        .filter(|command| command.context.script_depth == depth)
        .filter_map(|command| command.words.first())
        .map(|word| word.syntax.evaluate(None))
        .collect();
    let state_unknown = names
        .iter()
        .any(|word| !matches!(word, WordEvaluation::Resolved(_)))
        || facts
            .commands
            .iter()
            .any(|command| command.context.compound);
    let cwd_unknown = cwd_unknown
        || state_unknown
        || names
            .iter()
            .any(|word| matches!(word, WordEvaluation::Resolved(name) if changes_directory(name)));
    let home_unknown = home_unknown
        || state_unknown
        || names.iter().any(
            |word| matches!(word, WordEvaluation::Resolved(name) if changes_environment(name)),
        )
        || facts.commands.iter().any(|command| {
            let prefix = command.literal_prefix();
            prefix.first().is_some_and(|name| name == "printf")
                && prefix.get(1).is_some_and(|flag| flag.starts_with("-v"))
        });
    for command in &mut facts.commands {
        command.context.cwd_unknown |= cwd_unknown;
        command.context.home_unknown |= home_unknown;
    }
    for redirect in &mut facts.redirects {
        redirect.context.cwd_unknown |= cwd_unknown;
        redirect.context.home_unknown |= home_unknown;
    }
    // 严格白名单没有建模跨节点续接；局部事实可以合并，但不能因此扩大授权。
    facts.verbatim_escalation = wrapper_effects_unknown && !joined_words && strict.is_some();
    if facts.reliable
        && !joined_words
        && let Some((commands, paths)) = strict
    {
        facts.analysis = strict_analysis(commands, paths, depth, &nested_analyses);
    }
    facts
}

struct LiteralLoop {
    body: Range<usize>,
    variable: String,
    values: Vec<String>,
}

fn ordinary_loop_variable(name: &str) -> bool {
    // Bash/Zsh 的大写及下划线特殊参数不推定为固定标量；Zsh 另有小写特殊参数。
    const SPECIAL: &str = "aliases argv builtins cdpath commands dirstack dis_aliases
        dis_builtins dis_functions dis_functions_source dis_galiases dis_patchars dis_reswords
        dis_saliases errnos fignore fpath funcfiletrace funcsourcetrace funcstack functrace
        functions functions_source galiases histchars history historywords jobdirs jobstates
        jobtexts keymaps mailpath manpath module_path modules nameddirs options parameters
        patchars path pipestatus prompt psvar reswords saliases signals status sysparams
        termcap terminfo userdirs usergroups watch widgets zsh_eval_context zsh_scheduled_events";
    name.starts_with(|ch: char| ch.is_ascii_lowercase())
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        && !SPECIAL
            .split_ascii_whitespace()
            .any(|special| special == name)
}

/// 只认识单个有限 for；不做通用变量传播，也不跨状态变更猜值。
fn literal_loop(nodes: &[Node<'_>], src: &str) -> Option<LiteralLoop> {
    let mut loops = nodes.iter().filter(|node| node.kind() == "for_statement");
    let node = loops.next()?;
    if loops.next().is_some() || node.child(0)?.kind() != "for" {
        return None;
    }
    let text = |node: Node| node.utf8_text(src.as_bytes()).ok();
    let variable = text(node.child_by_field_name("variable")?)?;
    let body = node.child_by_field_name("body")?.byte_range();
    if !ordinary_loop_variable(variable) {
        return None;
    }
    let mut references = 0;
    for node in nodes.iter().filter(|node| node.is_named()) {
        match node.kind() {
            "program"
            | "list"
            | "for_statement"
            | "do_group"
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
            | "file_redirect"
            | "file_descriptor"
            | "variable_name" => {}
            "simple_expansion" | "expansion" => {
                let name = node.named_child(0)?;
                if node.kind() == "expansion"
                    && (node.named_child_count() != 1
                        || name.kind() != "variable_name"
                        || text(*node)? != format!("${{{}}}", text(name)?))
                {
                    return None;
                }
                references +=
                    usize::from(text(name)? == variable && body.contains(&node.start_byte()));
            }
            "command" => {
                let (words, _) = command_words(*node, src);
                let mut argv = words.iter().map(|word| word.syntax.evaluate(None));
                if !matches!(argv.next(),
                    Some(WordEvaluation::Resolved(name)) if preserves_variables(&name, argv.next()))
                {
                    return None;
                }
            }
            _ => return None,
        }
    }
    let mut cursor = node.walk();
    let values = node
        .children_by_field_name("value", &mut cursor)
        .map(|value| match literal(value, src) {
            WordValue::Literal(value) => Some(value),
            WordValue::Unknown => None,
        })
        .collect::<Option<Vec<_>>>()?;
    // 每个绑定值都可能作为嵌套脚本再分析一遍，展开总数须有上限。
    // 不让未知词变成新选项；未建模命令的选项操作数仍须保守检查。
    if values.is_empty()
        || values.len() * references > MAX_LOOP_BINDINGS
        || values.iter().any(|value| value.starts_with('-'))
    {
        return None;
    }
    Some(LiteralLoop {
        body,
        variable: variable.into(),
        values,
    })
}

/// 循环正文里的事实按每个值各出一份；没有可绑定的引用就原样一份。
fn bind_loop<T: Clone>(
    binding: Option<&LiteralLoop>,
    node: Node,
    item: T,
    bind: impl Fn(&mut T, &str, &str) -> bool,
) -> Vec<T> {
    let Some(binding) = binding.filter(|binding| binding.body.contains(&node.start_byte())) else {
        return vec![item];
    };
    let mut alternatives = Vec::new();
    for value in &binding.values {
        let mut alternative = item.clone();
        if !bind(&mut alternative, &binding.variable, value) {
            return vec![item];
        }
        alternatives.push(alternative);
    }
    alternatives
}

fn changes_directory(name: &str) -> bool {
    matches!(
        name,
        "cd" | "pushd"
            | "popd"
            | "eval"
            | "source"
            | "."
            | "builtin"
            | "command"
            | "trap"
            | "set"
            | "shopt"
            | "alias"
            | "unalias"
            | "enable"
            | "fc"
    )
}

fn changes_environment(name: &str) -> bool {
    matches!(
        name,
        "eval"
            | "source"
            | "."
            | "builtin"
            | "command"
            | "read"
            | "readarray"
            | "mapfile"
            | "unset"
            | "export"
            | "declare"
            | "typeset"
            | "local"
            | "let"
            | "getopts"
            | "trap"
            | "set"
            | "shopt"
            | "alias"
            | "unalias"
            | "enable"
            | "fc"
    )
}

/// 不会改父 shell 变量的命令：外部进程改不了，内建只收已知的这几个。不能反过来
/// 列会改变量的命令——Zsh 的 zstyle -s、vared 等也会赋值。
fn preserves_variables(name: &str, first_argument: Option<WordEvaluation>) -> bool {
    match name {
        "echo" | "cd" | "pwd" | ":" | "true" | "false" | "cargo" | "cat" | "grep" | "rg"
        | "head" | "tail" | "wc" | "env" | "bash" | "sh" | "zsh" => true,
        "printf" => matches!(first_argument,
            Some(WordEvaluation::Resolved(format)) if !format.starts_with("-v")),
        _ => std::path::Path::new(name).is_absolute(),
    }
}

/// sh/bash/zsh 的 -c 命令串，以及是否读启动文件（-l、-i、--login）。三者都在选项结束后
/// 取第一个操作数；只越过不带值的短选项簇、簇尾 `o` 加下一个词、`--` 与几个无值长选项。
/// Ok(None) 是不在 -c 模式或命令串尚未给出（动态参数）；Err 是选项形状不认识、
/// 又可能进入 -c 模式。
fn shell_command_string(args: &[String]) -> Result<Option<(&str, bool)>, ()> {
    let mut command = false;
    let mut startup = false;
    let mut rest = args;
    while let Some((arg, tail)) = rest.split_first() {
        if matches!(
            arg.as_str(),
            "--login" | "--noprofile" | "--norc" | "--posix"
        ) {
            startup |= arg == "--login";
            rest = tail;
            continue;
        }
        // bash 的 -O 取值而 zsh 的不取；o 后还有字符时 bash 取下一个词、zsh 取簇里余下的
        // 部分（`-oshwordsplit`）。只认三者一致的形状：没有 O，o 至多一个且在簇尾。
        let Some(flags) = arg.strip_prefix('-').filter(|flags| {
            !flags.is_empty()
                && flags
                    .chars()
                    .all(|ch| ch.is_ascii_alphabetic() && ch != 'O')
                && flags.find('o').is_none_or(|index| index == flags.len() - 1)
        }) else {
            break;
        };
        let values = usize::from(flags.ends_with('o'));
        if tail
            .get(..values)
            .is_none_or(|values| values.iter().any(|value| value.starts_with(['-', '+'])))
        {
            break;
        }
        command |= flags.contains('c');
        startup |= flags.contains(['l', 'i']);
        rest = &tail[values..];
    }
    let ended = rest.first().is_some_and(|arg| arg == "--");
    if ended {
        rest = &rest[1..];
    }
    match rest.first() {
        Some(arg) if !ended && arg.starts_with(['-', '+']) => {
            let maybe_command = command
                || rest.iter().any(|arg| {
                    arg.starts_with(['-', '+']) && !arg.starts_with("--") && arg.contains('c')
                });
            if maybe_command { Err(()) } else { Ok(None) }
        }
        Some(script) if command => Ok(Some((script, startup))),
        _ => Ok(None),
    }
}

fn command_words(node: Node, src: &str) -> (Vec<WordFact>, bool) {
    let mut words = Vec::new();
    let mut joined = false;
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
        joined |= push_word(&mut words, word, child.byte_range(), src);
    }
    (words, joined)
}

fn push_word(words: &mut Vec<WordFact>, node: Node, range: Range<usize>, src: &str) -> bool {
    let syntax = ShellWord::from_node(node, src);
    if let Some(previous) = words.last_mut() {
        let gap = &src[previous.range.end..range.start];
        if !gap.is_empty() && gap.replace("\\\n", "").is_empty() {
            previous.syntax.append(syntax);
            previous.range.end = range.end;
            previous.value = match previous.syntax.evaluate(None) {
                WordEvaluation::Resolved(text) => WordValue::Literal(text),
                WordEvaluation::Partial(_) | WordEvaluation::Unknown => WordValue::Unknown,
            };
            return true;
        }
    }
    words.push(WordFact {
        value: literal(node, src),
        range,
        syntax,
    });
    false
}

fn context(node: Node, depth: u8, cwd_unknown: bool, home_unknown: bool) -> Context {
    let mut context = Context {
        script_depth: depth,
        cwd_unknown,
        home_unknown,
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
    use std::path::Path;

    use super::super::word::{PartKind, Quoting, WordPart};
    use super::*;

    fn text(value: &str) -> ShellWord {
        ShellWord {
            parts: vec![WordPart {
                kind: PartKind::Text(value.into()),
                quoting: Quoting::Bare,
            }],
        }
    }

    #[test]
    fn continued_words_keep_one_argument_and_one_redirect_target() {
        let home = Path::new("/tmp/home");
        for src in [
            "cat ~/.bas\\\nhrc",
            "cat < ~/.bas\\\nhrc",
            "echo x > ~/.bas\\\nhrc",
        ] {
            let facts = analyze(src);
            let word = if facts.redirects.is_empty() {
                assert_eq!(facts.commands[0].words.len(), 2, "{src}: {facts:#?}");
                &facts.commands[0].words[1].syntax
            } else {
                assert_eq!(facts.redirects.len(), 1, "{src}: {facts:#?}");
                &facts.redirects[0].syntax
            };
            assert_eq!(
                word.evaluate(Some(home)),
                WordEvaluation::Resolved("/tmp/home/.bashrc".into()),
                "{src}: {facts:#?}"
            );
            assert_eq!(facts.analysis, BashAnalysis::Opaque, "{src}");
        }
        let facts = analyze("c\\\nd /tmp; echo x > notes.txt");
        assert!(facts.redirects[0].context.cwd_unknown, "{facts:#?}");
    }

    #[test]
    fn literal_loop_refines_risk_values_without_authorizing_the_script() {
        let facts = analyze("for t in first second; do cargo test -- \"${t}\"; done");
        assert_eq!(facts.analysis, BashAnalysis::Opaque);
        let values: Vec<_> = facts
            .commands
            .iter()
            .map(|command| {
                assert_eq!(command.words[3].value, WordValue::Unknown);
                assert_eq!(command.risk_argv(), ["cargo", "test", "--", "\0unknown"]);
                command.words[3].syntax.evaluate(None)
            })
            .collect();
        assert_eq!(
            values,
            [
                WordEvaluation::Resolved("first".into()),
                WordEvaluation::Resolved("second".into())
            ]
        );

        // 上限按值数 × 引用数计；超出时整段不细化。
        for (count, body) in [
            (MAX_LOOP_BINDINGS + 1, "cat \"$t\""),
            (MAX_LOOP_BINDINGS / 2 + 1, "cat \"$t\"; wc \"$t\""),
        ] {
            let values = (0..count)
                .map(|index| format!("t{index}"))
                .collect::<Vec<_>>()
                .join(" ");
            let facts = analyze(&format!("for t in {values}; do {body}; done"));
            assert!(
                facts.commands.iter().all(|command| {
                    command.words[1].syntax.evaluate(None) == WordEvaluation::Unknown
                }),
                "{facts:#?}"
            );
        }
        // 只在顶层细化：绑定值会再作为嵌套脚本分析，逐层展开会相乘。
        let facts = analyze("bash -c 'for t in a b; do cat \"$t\"; done'");
        assert!(
            facts
                .commands
                .iter()
                .filter(|command| command.context.script_depth == 1)
                .all(|command| command.words[1].syntax.evaluate(None) == WordEvaluation::Unknown),
            "{facts:#?}"
        );
    }

    #[test]
    fn shell_command_string_follows_option_parsing() {
        for (args, expected) in [
            ("-c x n", Ok(Some(("x", false)))),
            ("-e -c x", Ok(Some(("x", false)))),
            ("-c -e x", Ok(Some(("x", false)))),
            ("-euo pipefail -c x", Ok(Some(("x", false)))),
            ("-c -- x", Ok(Some(("x", false)))),
            ("--noprofile --norc -c x", Ok(Some(("x", false)))),
            ("-lc x", Ok(Some(("x", true)))),
            ("-c -i x", Ok(Some(("x", true)))),
            ("--login -c x", Ok(Some(("x", true)))),
            ("-c", Ok(None)),
            ("-e script.sh", Ok(None)),
            ("script.sh -c x", Ok(None)),
            ("-O extglob -c x", Err(())),
            ("--rcfile rc -c x", Err(())),
            ("-o -c x", Err(())),
            // zsh 把 o 后的字符当选项名，bash 取下一个词，两边的正文位置不同。
            ("-c -oshwordsplit x n", Err(())),
            ("-oe pipefail -c x", Err(())),
        ] {
            let args: Vec<_> = args.split(' ').map(String::from).collect();
            assert_eq!(shell_command_string(&args), expected, "{args:?}");
        }
    }

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
                        syntax: text("rg")
                    },
                    WordFact {
                        value: WordValue::Unknown,
                        range: 3..13,
                        syntax: ShellWord {
                            parts: vec![WordPart {
                                kind: PartKind::Variable("PATTERN".into()),
                                quoting: Quoting::Double
                            },]
                        }
                    },
                    WordFact {
                        value: WordValue::Literal("--file".into()),
                        range: 14..20,
                        syntax: text("--file")
                    },
                    WordFact {
                        value: WordValue::Literal(".env".into()),
                        range: 21..25,
                        syntax: text(".env")
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
                            syntax: text("/tmp/probe"),
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
        // -c 前后的选项照样定位正文；认不出的形状把执行体当未知，而非当普通参数。
        let facts = analyze("bash -x -c 'cat .env'");
        assert_eq!(facts.analysis, BashAnalysis::Opaque);
        assert!(
            facts.commands.iter().any(|command| {
                command.context.script_depth == 1
                    && command.argv() == Some(vec!["cat".into(), ".env".into()])
            }),
            "{facts:#?}"
        );
        let facts = analyze("bash -O extglob -c 'cat .env'");
        assert!(!facts.reliable);
        assert!(
            facts.commands.iter().any(|command| {
                command.context.argument_rewrite
                    && command.words[0].syntax.evaluate(None) == WordEvaluation::Unknown
            }),
            "{facts:#?}"
        );
    }
}
