//! shell — bash command analysis on a real parse tree (tree-sitter-bash),
//! ported from codex's `codex-shell-command` crate (Apache-2.0).
//!
//! 一棵树同时提供严格授权分析与局部风险证据。并发、只读、前缀与路径授权只消费
//! 完整分析；deny、敏感读取和危险判定还消费保留参数位置的局部事实。
//! The core rule is a *whitelist walk*: a script is only
//! [`BashAnalysis::Commands`] when every node in its parse tree is a plain
//! word-only command joined by `&&`/`||`/`;`/`|`/newline. Anything else —
//! subshells, command/process substitution, general expansions, variable assignments,
//! control flow, and input or dynamic output redirects — makes the
//! whole script [`BashAnalysis::Opaque`]: no classifier can vouch for what it
//! runs；manual 下不能靠内容分类自动放行，沙箱、bypass 与逐字会话批准另行决定。
//! Stream-only redirects (`2>&1`,
//! `>/dev/null`) are the exception: they add nothing the argv does not
//! already say ([`redirect_is_stream_only`]). 固定文件输出进入 `WithWrites`，
//! 同时保留 argv 与目标；只有 echo 参数的退出码展开例外地按纯数据分析。

mod facts;
mod word;
pub(crate) use facts::analyze as analyze_bash_facts;
pub(crate) use facts::{BashFacts, CommandFact, Context, Direction, WordValue};
pub(crate) use word::{ShellWord, WordEvaluation};

use tree_sitter::Node;
use tree_sitter::Parser;
use tree_sitter::Tree;
use tree_sitter_bash::LANGUAGE as BASH;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BashAnalysis {
    /// Every command in the script, as plain argv words, in source order.
    Commands(Vec<Vec<String>>),
    /// 固定输出重定向显式保留；它始终有写入效果，不能按 argv 当作只读。
    WithWrites {
        commands: Vec<Vec<String>>,
        paths: Vec<String>,
    },
    /// The script contains constructs the word-only parser cannot vouch for.
    Opaque,
}

impl BashAnalysis {
    pub fn commands(&self) -> Option<&[Vec<String>]> {
        match self {
            Self::Commands(commands) | Self::WithWrites { commands, .. } => Some(commands),
            Self::Opaque => None,
        }
    }

    pub fn writes(&self) -> &[String] {
        match self {
            Self::WithWrites { paths, .. } => paths,
            Self::Commands(_) | Self::Opaque => &[],
        }
    }

    pub fn is_readonly(&self) -> bool {
        matches!(self, Self::Commands(commands)
            if !commands.is_empty() && commands.iter().all(|argv| argv_is_readonly(argv)))
    }
}

/// How deep `bash -c "…"` nesting is unwrapped before giving up.
const MAX_UNWRAP_DEPTH: u8 = 2;

pub fn analyze_bash(script: &str) -> BashAnalysis {
    analyze_bash_facts(script).analysis
}

fn strict_analysis(
    commands: Vec<Vec<String>>,
    mut paths: Vec<String>,
    depth: u8,
    nested: &[(Vec<String>, BashAnalysis)],
) -> BashAnalysis {
    // 只用同一轮解析已采集的嵌套结论；未知 shell 选项或正文保持 opaque，
    // 不能退回 wrapper argv 后反而命中更宽的 allow。
    let mut flattened = Vec::with_capacity(commands.len());
    for argv in commands {
        if argv
            .first()
            .is_some_and(|name| matches!(executable_name(name), "bash" | "sh" | "zsh"))
            && argv.get(1).is_some_and(|arg| arg.starts_with('-'))
            && extract_wrapped_script(&argv).is_none()
        {
            return BashAnalysis::Opaque;
        }
        match extract_wrapped_script(&argv) {
            Some(_) if depth < MAX_UNWRAP_DEPTH => match nested
                .iter()
                .find(|(outer, _)| outer == &argv)
                .map(|(_, analysis)| analysis.clone())
                .unwrap_or(BashAnalysis::Opaque)
            {
                BashAnalysis::Commands(inner_cmds) => flattened.extend(inner_cmds),
                BashAnalysis::WithWrites {
                    commands,
                    paths: inner_paths,
                } => {
                    flattened.extend(commands);
                    paths.extend(inner_paths);
                }
                BashAnalysis::Opaque => return BashAnalysis::Opaque,
            },
            // A `bash -c` wrapper past the unwrap limit must NOT be kept as a
            // trusted plain command：其正文未经分析，危险命令与 deny 前缀都看不到它。
            // Opaque 让 manual 不会靠内容分类自动放行；沙箱与 bypass 的决定另行保留。
            Some(_) => return BashAnalysis::Opaque,
            None => flattened.push(argv),
        }
    }
    if paths.is_empty() {
        BashAnalysis::Commands(flattened)
    } else if flattened.is_empty()
        || (flattened.len() != 1 && paths.iter().any(|path| !path.starts_with('/')))
    {
        // 多命令中 cwd 可能已经变化；不把相对目标错误地锚定到工具初始 cwd。
        BashAnalysis::Opaque
    } else {
        BashAnalysis::WithWrites {
            commands: flattened,
            paths,
        }
    }
}

fn extract_wrapped_script(argv: &[String]) -> Option<&str> {
    let [shell, flag, script] = argv else {
        return None;
    };
    let shell_name = executable_name(shell);
    if matches!(shell_name, "bash" | "sh" | "zsh") && matches!(flag.as_str(), "-c" | "-lc") {
        Some(script)
    } else {
        None
    }
}

fn parse_bash(script: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&BASH.into())
        .expect("bash grammar always loads");
    parser.parse(script, None)
}

/// The whitelist walk (codex `try_parse_word_only_commands_sequence`): any
/// named node outside ALLOWED_KINDS, or any operator token outside
/// `&& || ; |` (plus quotes/whitespace), rejects the whole script.
fn word_only_commands_sequence(tree: &Tree, src: &str) -> Option<(Vec<Vec<String>>, Vec<String>)> {
    if tree.root_node().has_error() {
        return None;
    }
    const ALLOWED_KINDS: &[&str] = &[
        "program",
        "list",
        "pipeline",
        "command",
        "command_name",
        "word",
        "string",
        "string_content",
        "raw_string",
        "number",
        "concatenation",
        "comment",
        // 重定向独立解析并保留固定输出目标；heredoc/herestring 不在白名单。
        "redirected_statement",
    ];
    const ALLOWED_PUNCT_TOKENS: &[&str] = &["&&", "||", ";", "|", "\"", "'"];

    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut stack = vec![root];
    let mut command_nodes = Vec::new();
    let mut redirect_nodes = Vec::new();
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if node.is_named() {
            // A redirect is judged whole and never descended into: its operator
            // tokens (`>`, `>&`, …) are not punctuation the rest of the walk
            // should learn to accept, and its destination is not an argument.
            if kind == "file_redirect" {
                redirect_nodes.push(node);
                continue;
            }
            if echo_status_expansion(node, src) {
                continue;
            }
            if !ALLOWED_KINDS.contains(&kind) {
                return None;
            }
            if kind == "command" {
                command_nodes.push(node);
            }
        } else {
            // One rule covers the operators too: a token containing `&;|` is never
            // whitespace, so anything not on the punctuation allowlist rejects here.
            if !(ALLOWED_PUNCT_TOKENS.contains(&kind) || kind.trim().is_empty()) {
                return None;
            }
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }

    // The stack walk is LIFO; restore source order.
    command_nodes.sort_by_key(Node::start_byte);

    let mut commands = Vec::with_capacity(command_nodes.len());
    for node in command_nodes {
        commands.push(plain_command_argv(node, src)?);
    }
    redirect_nodes.sort_by_key(Node::start_byte);
    let mut paths = Vec::new();
    for node in redirect_nodes {
        paths.extend(output_redirect_paths(node, src)?);
    }
    Some((commands, paths))
}

/// Whether one redirect only moves streams around, leaving the argv a complete
/// account of the call. Two forms qualify — duplicating onto another descriptor
/// (`2>&1`, `>&2`) and discarding into `/dev/null` — and both are how a shell
/// command says "I do not want this output", not how it touches the filesystem.
///
/// 文件输出由 `output_redirect_paths` 显式保留；输入重定向和动态目标仍 opaque。
/// `foo > 1` 与 `2>&1` 的目标节点同为 number，必须同时看操作符，不能只看树形。
fn redirect_is_stream_only(node: Node, src: &str) -> bool {
    /// `2>&1`, `>&2`: the destination must be a bare descriptor number.
    const DUP_OPS: &[&str] = &[">&", "<&"];
    /// `>/dev/null`, `2>/dev/null`, `&>/dev/null`: nothing is kept.
    const DISCARD_OPS: &[&str] = &[">", ">>", "&>", "&>>"];
    const DISCARD_TARGET: &str = "/dev/null";

    let mut cursor = node.walk();
    let mut pending_op: Option<&str> = None;
    for child in node.children(&mut cursor) {
        // An anonymous token is the operator; its kind is its own text.
        if !child.is_named() {
            if pending_op.replace(child.kind()).is_some() {
                return false;
            }
            continue;
        }
        // The leading `2` of `2>&1`, before any operator has been seen.
        if child.kind() == "file_descriptor" {
            if pending_op.is_some() {
                return false;
            }
            continue;
        }
        let Some(op) = pending_op.take() else {
            return false;
        };
        let Ok(target) = child.utf8_text(src.as_bytes()) else {
            return false;
        };
        let ok = if DUP_OPS.contains(&op) {
            !target.is_empty() && target.chars().all(|c| c.is_ascii_digit())
        } else {
            DISCARD_OPS.contains(&op) && target == DISCARD_TARGET
        };
        if !ok {
            return false;
        }
    }
    // A trailing operator with nothing to pair it with (`3>&-` closes a
    // descriptor, and the `-` is not a destination node).
    pending_op.is_none()
}

/// 只认固定文件输出；目标独立于 argv 进入权限门，不能静默丢掉。
fn output_redirect_paths(node: Node, src: &str) -> Option<Vec<String>> {
    if redirect_is_stream_only(node, src) {
        return Some(Vec::new());
    }
    let mut paths = Vec::new();
    let mut pending_op = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            if pending_op.replace(child.kind()).is_some() {
                return None;
            }
        } else if child.kind() == "file_descriptor" {
            if pending_op.is_some() {
                return None;
            }
        } else {
            let op = pending_op.take()?;
            let target = fixed_output_target(child, src)?;
            if matches!(op, ">&" | "<&")
                && !target.is_empty()
                && target.chars().all(|ch| ch.is_ascii_digit())
            {
                continue;
            }
            if !matches!(op, ">" | ">>" | ">|" | "&>" | "&>>")
                || target.is_empty()
                || target.contains(['\\', '\n', '\r'])
            {
                return None;
            }
            if target != "/dev/null" {
                paths.push(target);
            }
        }
    }
    pending_op.is_none().then_some(paths)
}

fn fixed_output_target(node: Node, src: &str) -> Option<String> {
    match node.kind() {
        "word" | "number" => {
            let raw = node.utf8_text(src.as_bytes()).ok()?;
            (!raw.contains(['*', '?', '[', ']', '{', '}', '~', '$', '`', '\\']))
                .then(|| raw.to_string())
        }
        "string" | "raw_string" => plain_word(node, src, /*status_allowed*/ false),
        "concatenation" => {
            let mut target = String::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                target.push_str(&fixed_output_target(child, src)?);
            }
            Some(target)
        }
        _ => None,
    }
}

/// $? 只在 echo 的参数中降为稳定占位符；命令名和重定向目标不参与此例外。
fn echo_status_expansion(node: Node, src: &str) -> bool {
    if !matches!(node.kind(), "simple_expansion" | "expansion")
        || !matches!(node.utf8_text(src.as_bytes()), Ok("$?" | "${?}"))
    {
        return false;
    }
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        match parent.kind() {
            "file_redirect" | "command_name" => return false,
            "command" => {
                return parent
                    .child_by_field_name("name")
                    .is_some_and(|name| name.utf8_text(src.as_bytes()) == Ok("echo"));
            }
            _ => ancestor = parent.parent(),
        }
    }
    false
}

fn plain_command_argv(cmd: Node, src: &str) -> Option<Vec<String>> {
    let status_allowed = cmd
        .child_by_field_name("name")
        .is_some_and(|name| name.utf8_text(src.as_bytes()) == Ok("echo"));
    let mut words = Vec::new();
    let mut cursor = cmd.walk();
    for child in cmd.named_children(&mut cursor) {
        match child.kind() {
            "command_name" => {
                let word = child.named_child(0)?;
                if word.kind() != "word" {
                    return None;
                }
                words.push(word.utf8_text(src.as_bytes()).ok()?.to_owned());
            }
            "comment" => {}
            _ => words.push(plain_word(child, src, status_allowed)?),
        }
    }
    (!words.is_empty()).then_some(words)
}

fn plain_word(node: Node, src: &str, status_allowed: bool) -> Option<String> {
    match node.kind() {
        "word" | "number" => {
            let raw = node.utf8_text(src.as_bytes()).ok()?;
            Some(raw.to_string())
        }
        "raw_string" => raw_string_content(node, src),
        "string" => {
            // 原始切片保留换行和转义；只替换已经认可的退出码节点。
            let mut value = String::new();
            let mut start = node.start_byte() + 1;
            let mut cursor = node.walk();
            for part in node.named_children(&mut cursor) {
                if part.kind() == "string_content" {
                    continue;
                }
                if !status_allowed || !echo_status_expansion(part, src) {
                    return None;
                }
                value.push_str(&src[start..part.start_byte()]);
                value.push('0');
                start = part.end_byte();
            }
            value.push_str(&src[start..node.end_byte() - 1]);
            Some(value)
        }
        "concatenation" => {
            let mut value = String::new();
            let mut cursor = node.walk();
            for part in node.named_children(&mut cursor) {
                value.push_str(&plain_word(part, src, status_allowed)?);
            }
            (!value.is_empty()).then_some(value)
        }
        "simple_expansion" | "expansion" if status_allowed && echo_status_expansion(node, src) => {
            Some("0".to_string())
        }
        _ => None,
    }
}

fn raw_string_content(node: Node, src: &str) -> Option<String> {
    let raw = node.utf8_text(src.as_bytes()).ok()?;
    Some(raw.strip_prefix('\'')?.strip_suffix('\'')?.to_string())
}

fn executable_name(raw: &str) -> &str {
    raw.rsplit('/').next().unwrap_or(raw)
}

/// Read-only classifier over one parsed argv (codex
/// `is_safe_to_call_with_exec`): a safelisted NAME is necessary but not
/// sufficient — tools whose options can execute or write are vetted
/// per-option.
pub fn argv_is_readonly(argv: &[String]) -> bool {
    let Some(cmd0) = argv.first() else {
        return false;
    };
    let rest = &argv[1..];
    match executable_name(cmd0) {
        "cat" | "cd" | "cut" | "du" | "echo" | "expr" | "false" | "file" | "head" | "id" | "ls"
        | "nl" | "paste" | "pwd" | "rev" | "seq" | "stat" | "tail" | "tr" | "true" | "uname"
        | "uniq" | "wc" | "which" | "whoami" => true,

        "base64" => !rest
            .iter()
            .any(|a| a.starts_with("-o") || a == "--output" || a.starts_with("--output=")),

        // Options that execute commands or write files.
        "find" => !rest.iter().any(|a| {
            matches!(
                a.as_str(),
                "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete" | "-fls"
            ) || a.starts_with("-fprint")
        }),

        // --pre / --hostname-bin run arbitrary commands; -z shells out to
        // decompressors.
        "rg" | "grep" => !rest.iter().any(|a| {
            a == "--pre"
                || a.starts_with("--pre=")
                || a.starts_with("--hostname-bin")
                || matches!(a.as_str(), "-z" | "--search-zip")
        }),

        // -o writes the listing; -R re-runs tree with -H and writes
        // 00Tree.html at each depth boundary. Both may sit inside a short
        // cluster (`-ao`, `-aR`), but operands are skipped: `-I -old*` is a
        // pattern, not an output file.
        "tree" => !tree_writes(argv),

        "git" => git_is_readonly(argv),

        // Only the `sed -n {N|M,N}p FILE` query form.
        "sed" => {
            argv.len() <= 4
                && rest.first().map(String::as_str) == Some("-n")
                && is_valid_sed_n_arg(rest.get(1).map(String::as_str))
        }

        _ => false,
    }
}

/// `tree` writes when `-o` (output file) or `-R` (re-runs with -H and writes
/// 00Tree.html at each depth boundary) is an option letter. Operand-taking
/// options consume following argv tokens, so `-I -old*` is a pattern, not an
/// output file, and `--` ends option parsing.
fn tree_writes(argv: &[String]) -> bool {
    let mut pending_operands = 0_usize;
    let mut options_done = false;
    let mut index = 1;
    while let Some(raw) = argv.get(index) {
        let arg = raw.as_str();
        if pending_operands > 0 {
            pending_operands -= 1;
            index += 1;
            continue;
        }
        if options_done || !arg.starts_with('-') || arg == "-" {
            index += 1;
            continue;
        }
        if arg == "--" {
            options_done = true;
            index += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            if name == "outfile" {
                return true;
            }
            let takes_operand = matches!(
                name,
                "charset"
                    | "timefmt"
                    | "filelimit"
                    | "sort"
                    | "gitfile"
                    | "infofile"
                    | "hintro"
                    | "houtro"
                    | "scheme"
                    | "authority"
            );
            // 未识别的长选项可能带操作数，把后面的 `--` 或 `-o` 吞进去；
            // 无法担保时按非只读处理。
            if !takes_operand
                && !matches!(
                    name,
                    "gitignore"
                        | "ignore-case"
                        | "matchdirs"
                        | "metafirst"
                        | "prune"
                        | "info"
                        | "noreport"
                        | "si"
                        | "du"
                        | "inodes"
                        | "device"
                        | "dirsfirst"
                        | "filesfirst"
                        | "nolinks"
                        | "hyperlink"
                        | "fromfile"
                        | "fromtabfile"
                        | "fflinks"
                        | "opt-toggle"
                        | "help"
                        | "version"
                )
            {
                return true;
            }
            if takes_operand && attached.is_none() {
                pending_operands += 1;
            }
            index += 1;
            continue;
        }
        let body = &arg[1..];
        for (offset, ch) in body.char_indices() {
            match ch {
                'o' | 'R' => return true,
                'I' | 'P' | 'T' | 'H' => pending_operands += 1,
                'L' => {
                    // `-L<digits>` carries its operand; bare `-L` takes the
                    // next token.
                    let rest = &body[offset + ch.len_utf8()..];
                    if !rest.starts_with(|c: char| c.is_ascii_digit()) {
                        pending_operands += 1;
                    }
                }
                // 已知无参开关。其余字母保守按非只读处理：未知短选项也可能
                // 吞掉后面的 token。
                'a' | 'c' | 'd' | 'f' | 'g' | 'h' | 'i' | 'l' | 'n' | 'p' | 'q' | 'r' | 's'
                | 't' | 'u' | 'v' | 'x' | 'A' | 'C' | 'D' | 'F' | 'J' | 'N' | 'Q' | 'S' | 'U'
                | 'X' => {}
                '0'..='9' => {}
                _ => return true,
            }
        }
        index += 1;
    }
    false
}

/// Dangerous classifier (codex `is_dangerous_to_call_with_exec`),
/// deliberately independent from the read-only one: a command can be
/// neither, and the middle ground is where "just ask" lives. Checked on the
/// wrapper-stripped argv too, so `sudo rm -rf` and `env rm -rf` both hit.
///
/// This is a blocklist, and a blocklist is never complete — so what it holds is
/// chosen by one rule rather than by how alarming a command looks: **the damage
/// is irreversible and git is not the way back**. That admits `rm -rf`, `dd`
/// writing to a destination, `mkfs*`, and `shred`; it deliberately leaves out
/// `git clean -fdx` and `git reset --hard`, which inside a repository are the
/// recovery path rather than the threat. 固定输出设备目标（`… > /dev/sda`）
/// 由权限门独立检查；未知目标仍使整个脚本 [`BashAnalysis::Opaque`]。
///
/// 这是 manual 下需要强制审批的短名单，不提供 OS containment 保证；bypass 跳过它。
pub fn argv_is_dangerous(argv: &[String]) -> bool {
    let Some(cmd0) = argv.first() else {
        return false;
    };
    let name = executable_name(cmd0);
    // mkfs, mkfs.ext4, mkfs.xfs, …: every spelling formats, whatever the flags.
    if name.starts_with("mkfs") {
        return true;
    }
    match name {
        "rm" => argv[1..].iter().any(|a| {
            matches!(a.as_str(), "--force" | "--recursive")
                || (a.starts_with('-')
                    && !a.starts_with("--")
                    && a.chars().any(|c| c == 'r' || c == 'R' || c == 'f'))
        }),
        // `of=` is where the bytes land, and what was there is gone. Without it
        // dd only reads (to stdout or a pipe), which is no more dangerous than
        // `cat`.
        "dd" => argv[1..].iter().any(|a| a.starts_with("of=")),
        // Overwrites in place by design; that is the whole point of the tool.
        "shred" => true,
        "sudo" => argv.len() > 1 && argv_is_dangerous(&argv[1..]),
        _ => false,
    }
}

/// 包装器剥离与执行上下文共用一份分析；未知选项不能被猜成无操作数开关。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WrapperAnalysis<'a> {
    pub argv: &'a [String],
    /// 选项语法完整，被包命令的位置已知；不代表参数或写效果没有变化。
    pub complete: bool,
    pub environment_unknown: bool,
    pub cwd_unknown: bool,
    pub arguments_unknown: bool,
    pub writes_unknown: bool,
}

pub(crate) fn analyze_wrappers(argv: &[String]) -> WrapperAnalysis<'_> {
    let mut analysis = WrapperAnalysis {
        argv,
        complete: true,
        environment_unknown: false,
        cwd_unknown: false,
        arguments_unknown: false,
        writes_unknown: false,
    };
    let mut rest = argv;
    while let Some(first) = rest.first() {
        let wrapper = executable_name(first);
        let next = match wrapper {
            "env" => env_command(&rest[1..], &mut analysis),
            "sudo" => sudo_command(&rest[1..], &mut analysis),
            "nohup" | "nice" | "time" | "timeout" | "xargs" => {
                transparent_wrapper_command(wrapper, &rest[1..], &mut analysis)
            }
            _ => break,
        };
        rest = match next {
            Some(next) => next,
            None => {
                analysis.complete = false;
                // 保留原有 deny/危险前缀的近似剥离；事实层不能把它当成完整命令。
                risk_wrapper_command(&rest[1..], &mut analysis)
            }
        };
    }
    analysis.argv = rest;
    analysis
}

/// Options that consume the token after them. A wrapper's operand is not the
/// wrapped command, so stripping has to walk past it: `xargs -I {} rm -rf {}`
/// must leave `rm -rf {}`, not `{} rm -rf {}`.
fn wrapper_option_takes_value(wrapper: &str, token: &str) -> bool {
    match wrapper {
        "nice" => matches!(token, "-n" | "--adjustment"),
        "timeout" => matches!(token, "-s" | "--signal" | "-k" | "--kill-after"),
        "time" => matches!(token, "-o" | "--output" | "-f" | "--format"),
        // xargs' long optional-argument forms (`--replace`, `--eof`,
        // `--max-lines`) only take an operand when it is attached with `=`.
        "xargs" => matches!(
            token,
            "-I" | "--max-args"
                | "-n"
                | "--max-procs"
                | "-P"
                | "--max-chars"
                | "-s"
                | "--delimiter"
                | "-d"
                | "--arg-file"
                | "-a"
                | "-E"
        ),
        _ => false,
    }
}

fn wrapper_option_is_attached(wrapper: &str, token: &str) -> bool {
    match wrapper {
        "nice" => {
            token.starts_with("--adjustment=")
                || (token.starts_with("-n") && token.len() > 2)
                || token[1..].chars().all(|ch| ch.is_ascii_digit())
        }
        "timeout" => {
            token.starts_with("--signal=")
                || token.starts_with("--kill-after=")
                || (token.len() > 2 && (token.starts_with("-s") || token.starts_with("-k")))
        }
        "time" => {
            token.starts_with("--output=")
                || token.starts_with("--format=")
                || (token.len() > 2 && (token.starts_with("-o") || token.starts_with("-f")))
        }
        "xargs" => {
            token.starts_with("--replace=")
                || token.starts_with("--eof=")
                || token.starts_with("--max-lines=")
                || token.starts_with("--max-args=")
                || token.starts_with("--max-procs=")
                || token.starts_with("--max-chars=")
                || token.starts_with("--delimiter=")
                || token.starts_with("--arg-file=")
                || token.starts_with("--process-slot-var=")
                || (token.len() > 2
                    && token.starts_with('-')
                    && !token.starts_with("--")
                    && matches!(
                        token.as_bytes()[1],
                        b'I' | b'L' | b'n' | b'P' | b's' | b'd' | b'E' | b'a' | b'i'
                    ))
        }
        _ => false,
    }
}

fn wrapper_option_takes_no_value(wrapper: &str, token: &str) -> bool {
    match wrapper {
        "time" => matches!(
            token,
            "-p" | "--portability"
                | "-a"
                | "--append"
                | "-v"
                | "--verbose"
                | "-q"
                | "--quiet"
                | "--help"
                | "--version"
        ),
        "timeout" => matches!(
            token,
            "--foreground" | "--preserve-status" | "-v" | "--verbose"
        ),
        "xargs" => matches!(
            token,
            "-0" | "--null"
                | "-r"
                | "--no-run-if-empty"
                | "-t"
                | "--verbose"
                | "-p"
                | "--interactive"
                | "-x"
                | "--exit"
                | "-o"
                | "--open-tty"
                | "--show-limits"
                | "--replace"
                | "--eof"
                | "--max-lines"
                | "-i"
                | "--help"
                | "--version"
        ),
        "nohup" => matches!(token, "--help" | "--version"),
        _ => false,
    }
}

fn xargs_replaces_arguments(token: &str) -> bool {
    matches!(token, "-I" | "-i" | "--replace")
        || token.starts_with("--replace=")
        || (token.len() > 2 && (token.starts_with("-I") || token.starts_with("-i")))
}

fn time_writes_a_report(token: &str) -> bool {
    matches!(token, "-o" | "--output")
        || token.starts_with("--output=")
        || (token.len() > 2 && token.starts_with("-o"))
}

fn transparent_wrapper_command<'a>(
    wrapper: &str,
    mut rest: &'a [String],
    analysis: &mut WrapperAnalysis<'_>,
) -> Option<&'a [String]> {
    while let Some(token) = rest.first().filter(|token| token.starts_with('-')) {
        let token = token.as_str();
        if token == "--" {
            rest = &rest[1..];
            break;
        }
        if wrapper == "xargs" && xargs_replaces_arguments(token) {
            // 替换会改变内层脚本；仍定位并采集其风险，但不能拿字面词生成授权。
            analysis.arguments_unknown = true;
        }
        if wrapper == "xargs" && token.starts_with("--process-slot-var=") {
            // GNU xargs sets this variable per child, overriding whatever the
            // parent had (including HOME).
            analysis.environment_unknown = true;
        }
        if wrapper == "time" && time_writes_a_report(token) {
            // 报告由包装器自身写入，不在内层脚本的重定向里。
            analysis.writes_unknown = true;
        }
        if wrapper_option_takes_value(wrapper, token) {
            rest = rest.get(1..)?;
            rest.first()?;
        } else if !wrapper_option_is_attached(wrapper, token)
            && !wrapper_option_takes_no_value(wrapper, token)
        {
            return None;
        }
        rest = &rest[1..];
    }
    if wrapper == "timeout" {
        let duration = rest.first()?;
        if !duration
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 's' | 'm' | 'h' | 'd'))
            || !duration.chars().any(|c| c.is_ascii_digit())
        {
            return None;
        }
        rest = &rest[1..];
    }
    Some(rest)
}

fn risk_wrapper_command<'a>(
    mut rest: &'a [String],
    analysis: &mut WrapperAnalysis<'_>,
) -> &'a [String] {
    while let Some(tok) = rest.first() {
        let is_duration = !tok.is_empty()
            && tok
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '.' | 's' | 'm' | 'h' | 'd'))
            && tok.chars().any(|c| c.is_ascii_digit());
        if tok.starts_with('-') || environment_assignment(tok) || is_duration {
            analysis.environment_unknown |= environment_assignment(tok);
            rest = &rest[1..];
        } else {
            break;
        }
    }
    rest
}

fn environment_assignment(token: &str) -> bool {
    token
        .split_once('=')
        .is_some_and(|(name, _)| !name.is_empty())
}

fn env_command<'a>(
    mut rest: &'a [String],
    analysis: &mut WrapperAnalysis<'_>,
) -> Option<&'a [String]> {
    while let Some(token) = rest.first().filter(|token| token.starts_with('-')) {
        let token = token.as_str();
        match token {
            "--" => {
                rest = &rest[1..];
                break;
            }
            "-" | "-i" | "--ignore-environment" => analysis.environment_unknown = true,
            "-u" | "--unset" | "-C" | "--chdir" => {
                analysis.environment_unknown = true;
                analysis.cwd_unknown |= matches!(token, "-C" | "--chdir");
                rest = rest.get(1..)?;
                rest.first()?;
            }
            _ if token.starts_with("--unset=") || (token.starts_with("-u") && token.len() > 2) => {
                analysis.environment_unknown = true;
            }
            _ if token.starts_with("--chdir=") || (token.starts_with("-C") && token.len() > 2) => {
                analysis.environment_unknown = true;
                analysis.cwd_unknown = true;
            }
            _ => return None,
        }
        rest = &rest[1..];
    }
    while rest
        .first()
        .is_some_and(|token| environment_assignment(token))
    {
        analysis.environment_unknown = true;
        rest = &rest[1..];
    }
    Some(rest)
}

fn sudo_command<'a>(
    mut rest: &'a [String],
    analysis: &mut WrapperAnalysis<'_>,
) -> Option<&'a [String]> {
    // sudo 的环境重置由系统策略决定，连不带 -E/-H 的形式也不能沿用父 HOME。
    analysis.environment_unknown = true;
    const VALUE_OPTIONS: &[&str] = &[
        "-u",
        "--user",
        "-g",
        "--group",
        "-h",
        "--host",
        "-p",
        "--prompt",
        "-C",
        "--close-from",
        "-T",
        "--command-timeout",
        "-D",
        "--chdir",
        "-r",
        "--role",
        "-t",
        "--type",
    ];
    while let Some(token) = rest.first().filter(|token| token.starts_with('-')) {
        let token = token.as_str();
        let option = token.split_once('=').map_or(token, |(name, _)| name);
        if token == "--" {
            rest = &rest[1..];
            break;
        }
        if VALUE_OPTIONS.contains(&option) {
            analysis.cwd_unknown |= matches!(option, "-D" | "--chdir");
            if !token.contains('=') {
                rest = rest.get(1..)?;
                rest.first()?;
            }
        } else if let Some(option) = VALUE_OPTIONS
            .iter()
            .find(|option| option.len() == 2 && token.starts_with(**option) && token.len() > 2)
        {
            analysis.cwd_unknown |= *option == "-D";
        } else if matches!(token, "-i" | "--login" | "-s" | "--shell") {
            analysis.cwd_unknown = true;
        } else if !matches!(
            option,
            "--preserve-env"
                | "--set-home"
                | "--non-interactive"
                | "--stdin"
                | "--background"
                | "--reset-timestamp"
                | "--remove-timestamp"
        ) && !(token.starts_with('-')
            && !token.starts_with("--")
            && token.len() > 1
            && token[1..].chars().all(|ch| "nEHkKSb".contains(ch)))
        {
            return None;
        }
        rest = &rest[1..];
    }
    while rest
        .first()
        .is_some_and(|token| environment_assignment(token))
    {
        rest = &rest[1..];
    }
    Some(rest)
}

/// 仅供 deny/危险判断；allow 与只读判定不消费剥离后的命令。
pub fn strip_wrappers(argv: &[String]) -> Vec<String> {
    analyze_wrappers(argv).argv.to_vec()
}

/// deny 与危险判定的剥离视图。语法完整且参数不改写时只给一条；未知选项或参数
/// 替换时把剩余 argv 的每个起点都试一次。这里只增加风险证据，不生成授权。
pub(crate) fn risk_wrapper_views(argv: &[String]) -> Vec<Vec<String>> {
    let analysis = analyze_wrappers(argv);
    let mut views = Vec::with_capacity(analysis.argv.len());
    if !analysis.complete || analysis.arguments_unknown {
        for start in 1..analysis.argv.len() {
            views.push(analysis.argv[start..].to_vec());
        }
    }
    views.push(analysis.argv.to_vec());
    views
}

/// 两词前缀只在两个词都描述命令本身时才稳：首词是包装器时前缀描述的是包装器，
/// 第二词是选项或赋值时前缀描述的是这一次调用的参数。
pub(crate) fn argv_prefix_is_unstable(argv: &[String]) -> bool {
    const WRAPPERS: &[&str] = &[
        "env", "sudo", "nohup", "nice", "time", "timeout", "xargs", "command", "builtin", "exec",
    ];
    let Some(first) = argv.first() else {
        return true;
    };
    WRAPPERS.contains(&executable_name(first))
        || argv
            .get(1)
            .is_some_and(|second| second.starts_with('-') || second.contains('='))
}

/// Find the git subcommand while skipping global options, and refuse the
/// ones (`-C`, `-c`, `--git-dir`, `--exec-path`, …) that can redirect or
/// reconfigure git into arbitrary execution (codex's git-global-option
/// bypass fix).
fn git_is_readonly(argv: &[String]) -> bool {
    const READONLY_SUBCOMMANDS: &[&str] = &["status", "log", "diff", "show", "branch"];
    const UNSAFE_GLOBAL_EXACT: &[&str] = &[
        "-C",
        "-c",
        "-p",
        "--config-env",
        "--exec-path",
        "--git-dir",
        "--namespace",
        "--paginate",
        "--super-prefix",
        "--work-tree",
    ];
    const UNSAFE_GLOBAL_PREFIX: &[&str] = &[
        "-C",
        "-c",
        "--config-env=",
        "--exec-path=",
        "--git-dir=",
        "--namespace=",
        "--super-prefix=",
        "--work-tree=",
    ];
    // Subcommand options that write or execute (`git log --output=…`,
    // `git diff --ext-diff`, …).
    const UNSAFE_SUBCOMMAND_ARGS: &[&str] = &["--output", "--ext-diff", "--textconv", "--exec"];

    let mut subcommand: Option<(usize, &str)> = None;
    for (idx, arg) in argv.iter().enumerate().skip(1) {
        let arg = arg.as_str();
        if UNSAFE_GLOBAL_EXACT.contains(&arg)
            || UNSAFE_GLOBAL_PREFIX
                .iter()
                .any(|p| arg.starts_with(p) && arg.len() > p.len())
        {
            return false;
        }
        if arg == "--" || arg.starts_with('-') {
            continue;
        }
        if READONLY_SUBCOMMANDS.contains(&arg) {
            subcommand = Some((idx, arg));
        }
        // First non-option token is the subcommand; stop either way so a
        // later positional (e.g. a branch name) is never misread as one.
        break;
    }
    let Some((idx, subcommand)) = subcommand else {
        return false;
    };

    let sub_args = &argv[idx + 1..];
    if sub_args.iter().any(|a| {
        UNSAFE_SUBCOMMAND_ARGS
            .iter()
            .any(|u| a == u || a.starts_with(&format!("{u}=")))
    }) {
        return false;
    }
    match subcommand {
        "branch" => git_branch_is_readonly(sub_args),
        _ => true,
    }
}

/// `git branch` mutates unless its args are clearly a listing query.
fn git_branch_is_readonly(args: &[String]) -> bool {
    if args.is_empty() {
        return true;
    }
    let mut saw_readonly_flag = false;
    for arg in args.iter().map(String::as_str) {
        match arg {
            "--list" | "-l" | "--show-current" | "-a" | "--all" | "-r" | "--remotes" | "-v"
            | "-vv" | "--verbose" => saw_readonly_flag = true,
            _ if arg.starts_with("--format=") => saw_readonly_flag = true,
            _ => return false,
        }
    }
    saw_readonly_flag
}

/// Matches /^(\d+,)?\d+p$/ — the read-only `sed -n` line-query form.
fn is_valid_sed_n_arg(arg: Option<&str>) -> bool {
    let Some(core) = arg.and_then(|s| s.strip_suffix('p')) else {
        return false;
    };
    let parts: Vec<&str> = core.split(',').collect();
    matches!(parts.as_slice(), [a] | [a, _] if !a.is_empty())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn commands(script: &str) -> Option<Vec<Vec<String>>> {
        match analyze_bash(script) {
            BashAnalysis::Commands(c) => Some(c),
            BashAnalysis::WithWrites { .. } | BashAnalysis::Opaque => None,
        }
    }

    #[test]
    fn parses_plain_commands_with_safe_operators() {
        assert_eq!(commands("ls -1").unwrap(), vec![argv(&["ls", "-1"])]);
        assert_eq!(
            commands("ls && pwd; echo 'hi there' | wc -l").unwrap(),
            vec![
                argv(&["ls"]),
                argv(&["pwd"]),
                argv(&["echo", "hi there"]),
                argv(&["wc", "-l"]),
            ]
        );
        // Newlines separate commands like `;` does.
        assert_eq!(
            commands("ls\nrm -rf x").unwrap(),
            vec![argv(&["ls"]), argv(&["rm", "-rf", "x"])]
        );
        // Quoted strings and concatenations are unwrapped literally.
        assert_eq!(
            commands(r#"rg -n "foo" -g"*.py""#).unwrap(),
            vec![argv(&["rg", "-n", "foo", "-g*.py"])]
        );
        assert_eq!(
            commands("git commit -m \"line1\nline2\"").unwrap(),
            vec![argv(&["git", "commit", "-m", "line1\nline2"])]
        );
    }

    #[test]
    fn rejects_constructs_the_walk_cannot_vouch_for() {
        for script in [
            "echo $(pwd)",                // command substitution
            "echo `pwd`",                 // backtick substitution
            "cat <(ls)",                  // process substitution
            "echo $HOME",                 // expansion
            "echo \"hi ${USER}\"",        // expansion inside quotes
            "rg -g\"$(pwd)\" pattern",    // substitution inside concatenation
            "(ls)",                       // subshell
            "ls > \"$LOG\"",              // dynamic redirection target
            "echo hi & echo bye",         // background chaining
            "sleep 60 >/dev/null 2>&1 &", // background `&` (the redirects are fine)
            "FOO=bar ls",                 // assignment prefix
            "for f in *; do rm $f; done", // control flow
            "ls &&",                      // parse error
        ] {
            assert_eq!(analyze_bash(script), BashAnalysis::Opaque, "{script}");
        }
    }

    #[test]
    fn stream_only_redirects_leave_the_script_parseable() {
        // The shape every test run in this repo has: one `2>&1` used to sink the
        // whole script, and with it a `go test` prefix rule nobody could write.
        assert_eq!(
            commands("cd sub && go test ./pkg -count=1 2>&1 | tail -3").unwrap(),
            vec![
                argv(&["cd", "sub"]),
                argv(&["go", "test", "./pkg", "-count=1"]),
                argv(&["tail", "-3"]),
            ]
        );
        // The redirect is a sibling of the command, so it never reaches the argv.
        assert_eq!(
            commands("make >/dev/null 2>&1").unwrap(),
            vec![argv(&["make"])]
        );
        assert_eq!(
            commands("cd /tmp/x 2>/dev/null").unwrap(),
            vec![argv(&["cd", "/tmp/x"])]
        );
        assert_eq!(
            commands("echo hi >&2").unwrap(),
            vec![argv(&["echo", "hi"])]
        );
        assert_eq!(
            commands("cat log &>/dev/null").unwrap(),
            vec![argv(&["cat", "log"])]
        );
    }

    #[test]
    fn dynamic_and_input_redirects_still_sink_the_script() {
        for script in [
            "cat < in.txt",       // a file read the argv never names
            "foo 3>&-",           // operator with no destination
            "cat <<EOF\nhi\nEOF", // heredoc
            "cat <<< hi",         // herestring
        ] {
            assert_eq!(analyze_bash(script), BashAnalysis::Opaque, "{script}");
        }
    }

    #[test]
    fn fixed_output_redirects_keep_every_write_and_stay_mutating() {
        for (script, paths) in [
            ("make check > out.log 2>&1", vec!["out.log"]),
            ("echo result >> out.log", vec!["out.log"]),
            ("echo result >| out.log", vec!["out.log"]),
            ("make check &> /tmp/out.log", vec!["/tmp/out.log"]),
            ("make check &>> /tmp/out.log", vec!["/tmp/out.log"]),
            (
                "make check > /tmp/one 2> /tmp/two",
                vec!["/tmp/one", "/tmp/two"],
            ),
            (
                "make check > '/tmp/log dir'/result.log",
                vec!["/tmp/log dir/result.log"],
            ),
            ("make check > 1", vec!["1"]),
        ] {
            let analysis = analyze_bash(script);
            assert_eq!(analysis.writes(), paths, "{script}");
            assert!(analysis.commands().is_some(), "{script}");
            assert!(!analysis.is_readonly(), "{script}");
        }
        assert_eq!(
            analyze_bash(
                "cd /tmp && make check > /tmp/check.log; echo \"EXIT:$?\" >> /tmp/check.log"
            ),
            BashAnalysis::WithWrites {
                commands: vec![
                    argv(&["cd", "/tmp"]),
                    argv(&["make", "check"]),
                    argv(&["echo", "EXIT:0"])
                ],
                paths: vec!["/tmp/check.log".into(), "/tmp/check.log".into()],
            }
        );
    }

    #[test]
    fn status_expansion_is_only_echo_data() {
        for script in ["echo $?", "echo ${?}", "echo \"EXIT:$?\"", "echo EXIT:${?}"] {
            assert!(analyze_bash(script).is_readonly(), "{script}");
        }
        for script in [
            "$?",
            "make $?",
            "echo > /tmp/$?",
            "echo > \"/tmp/${?}\"",
            "echo ${?:-fallback}",
            "echo $(touch /tmp/status)",
            "echo $((1 + $?))",
        ] {
            assert_eq!(analyze_bash(script), BashAnalysis::Opaque, "{script}");
        }
    }

    #[test]
    fn output_target_expansions_and_ambiguous_working_directories_stay_opaque() {
        for script in [
            "> /tmp/out.log",
            "make > /tmp/*.log",
            "make > /tmp/'logs'/*.log",
            "make > ~/out.log",
            "make > /tmp/{one,two}",
            "make > \"$LOG\"",
            "make > $(pwd)/out.log",
            "cd sub && make check > out.log",
            "make > out.log; echo done",
            "sh -c 'cd sub && make > out.log'",
            "cd sub && sh -c 'make > out.log'",
        ] {
            assert_eq!(analyze_bash(script), BashAnalysis::Opaque, "{script}");
        }
        assert_eq!(
            analyze_bash("sh -c 'make check > /tmp/nested.log'"),
            BashAnalysis::WithWrites {
                commands: vec![argv(&["make", "check"])],
                paths: vec!["/tmp/nested.log".into()],
            }
        );
    }

    #[test]
    fn unwraps_nested_shell_dash_c() {
        assert_eq!(
            commands("bash -c 'ls && pwd'").unwrap(),
            vec![argv(&["ls"]), argv(&["pwd"])]
        );
        // An opaque inner script poisons the whole analysis.
        assert_eq!(analyze_bash("sh -c 'echo $(pwd)'"), BashAnalysis::Opaque);
        // Non -c shapes are kept as plain argv, not unwrapped.
        assert_eq!(
            commands("bash script.sh").unwrap(),
            vec![argv(&["bash", "script.sh"])]
        );
        // Nesting past the unwrap limit stays a `bash -c` wrapper: it must be
        // Opaque, not a trusted plain command. `whoami` stands in for the real
        // payload (`rm -rf /`); if this returned Commands, the inner command
        // would escape the danger/deny checks and manual could trust a whole-tool rule.
        assert_eq!(
            analyze_bash("bash -c \"bash -c 'bash -c whoami'\""),
            BashAnalysis::Opaque
        );
    }

    #[test]
    fn readonly_classifier_vets_options_not_just_names() {
        assert!(argv_is_readonly(&argv(&["ls", "-la"])));
        assert!(argv_is_readonly(&argv(&["sed", "-n", "1,20p", "f.txt"])));
        assert!(argv_is_readonly(&argv(&["find", ".", "-name", "*.rs"])));
        assert!(argv_is_readonly(&argv(&["rg", "-n", "foo"])));

        assert!(!argv_is_readonly(&argv(&["find", ".", "-delete"])));
        assert!(!argv_is_readonly(&argv(&[
            "find", ".", "-exec", "rm", "{}", ";"
        ])));
        assert!(!argv_is_readonly(&argv(&["rg", "--pre", "evil", "x"])));
        assert!(!argv_is_readonly(&argv(&["rg", "-z", "x"])));
        assert!(!argv_is_readonly(&argv(&["base64", "-o", "out", "f"])));
        assert!(argv_is_readonly(&argv(&[
            "tree",
            "-I",
            "node_modules",
            "."
        ])));
        assert!(!argv_is_readonly(&argv(&["tree", "-o", "out.txt"])));
        assert!(!argv_is_readonly(&argv(&["tree", "-R", "-L", "1", "."])));
        assert!(!argv_is_readonly(&argv(&["tree", "-aR", "."])));
        assert!(!argv_is_readonly(&argv(&["tree", "-ao", "out.txt"])));
        // Operands are skipped: `-I -old*` / `-I -R` are patterns, not writes,
        // and `--` ends option parsing.
        assert!(argv_is_readonly(&argv(&[
            "tree", "-I", "-old*", "-P", ".git", "."
        ])));
        assert!(argv_is_readonly(&argv(&["tree", "-I", "-R", "."])));
        assert!(argv_is_readonly(&argv(&["tree", "--", "-R"])));
        assert!(argv_is_readonly(&argv(&["tree", "--gitignore", "."])));
        // Unknown options may take an operand and swallow a `--`, so they
        // cannot be vouched for.
        assert!(!argv_is_readonly(&argv(&[
            "tree",
            "--compress",
            "--",
            "-o",
            "out.txt",
            "."
        ])));
        assert!(!argv_is_readonly(&argv(&["tree", "--future-flag", "."])));
        assert!(!argv_is_readonly(&argv(&["tree", "-Z", "."])));
        assert!(!argv_is_readonly(&argv(&["sed", "-i", "s/a/b/", "f"])));
        assert!(!argv_is_readonly(&argv(&["rm", "-rf", "x"])));
        assert!(!argv_is_readonly(&argv(&["make", "build"])));
    }

    #[test]
    fn git_readonly_blocks_global_and_subcommand_injection() {
        assert!(argv_is_readonly(&argv(&["git", "status"])));
        assert!(argv_is_readonly(&argv(&["git", "log", "-5"])));
        assert!(argv_is_readonly(&argv(&["git", "branch", "--list"])));
        assert!(argv_is_readonly(&argv(&["git", "branch"])));

        assert!(!argv_is_readonly(&argv(&["git", "push"])));
        assert!(!argv_is_readonly(&argv(&["git", "-C", "/x", "status"])));
        assert!(!argv_is_readonly(&argv(&[
            "git",
            "-c",
            "core.pager=evil",
            "log"
        ])));
        assert!(!argv_is_readonly(&argv(&["git", "--git-dir=/x", "status"])));
        assert!(!argv_is_readonly(&argv(&["git", "log", "--output=/tmp/f"])));
        assert!(!argv_is_readonly(&argv(&["git", "diff", "--ext-diff"])));
        assert!(!argv_is_readonly(&argv(&["git", "branch", "-D", "main"])));
        assert!(!argv_is_readonly(&argv(&["git", "branch", "new-branch"])));
    }

    #[test]
    fn dangerous_classifier_sees_through_wrappers() {
        assert!(argv_is_dangerous(&argv(&["rm", "-rf", "x"])));
        assert!(argv_is_dangerous(&argv(&["rm", "-fr", "x"])));
        assert!(argv_is_dangerous(&argv(&["rm", "-r", "dir"])));
        assert!(argv_is_dangerous(&argv(&["rm", "--force", "x"])));
        assert!(argv_is_dangerous(&argv(&["sudo", "rm", "-rf", "x"])));
        assert!(argv_is_dangerous(&strip_wrappers(&argv(&[
            "env", "FOO=1", "rm", "-rf", "x"
        ]))));

        assert!(!argv_is_dangerous(&argv(&["rm", "x"])));
        assert!(!argv_is_dangerous(&argv(&["cargo", "build"])));
    }

    /// The list is chosen by "irreversible, and git is not the way back" — not
    /// by how alarming a command reads. Both halves of that rule are asserted
    /// here, because the tempting failure is to keep adding words until the
    /// blocklist feels complete, which it can never be.
    #[test]
    fn the_danger_list_holds_only_irreversible_loss_git_cannot_undo() {
        // Destination given: the bytes that were there are gone.
        assert!(argv_is_dangerous(&argv(&[
            "dd",
            "if=/dev/zero",
            "of=/dev/sda"
        ])));
        assert!(argv_is_dangerous(&argv(&["dd", "of=disk.img"])));
        assert!(argv_is_dangerous(&argv(&["sudo", "dd", "of=/dev/sda"])));
        // No destination: dd is reading, which is no worse than `cat`.
        assert!(!argv_is_dangerous(&argv(&["dd", "if=/dev/urandom"])));

        // Every mkfs spelling formats, whatever the flags.
        assert!(argv_is_dangerous(&argv(&["mkfs", "/dev/sdb1"])));
        assert!(argv_is_dangerous(&argv(&["mkfs.ext4", "-F", "/dev/sdb1"])));
        assert!(argv_is_dangerous(&argv(&["/sbin/mkfs.xfs", "/dev/sdb1"])));

        // Overwriting in place is the tool's purpose.
        assert!(argv_is_dangerous(&argv(&["shred", "secret.txt"])));

        // Inside a repository these are the recovery path, not the threat —
        // kloop reaches for them itself. Keeping them out is the rule working,
        // not an omission.
        assert!(!argv_is_dangerous(&argv(&["git", "clean", "-fdx"])));
        assert!(!argv_is_dangerous(&argv(&["git", "reset", "--hard"])));

        assert_eq!(
            analyze_bash("cat x > /dev/sda"),
            BashAnalysis::WithWrites {
                commands: vec![argv(&["cat", "x"])],
                paths: vec!["/dev/sda".into()],
            }
        );
    }

    #[test]
    fn wrapper_stripping_reaches_the_wrapped_command() {
        let cases: &[(&[&str], &[&str])] = &[
            (&["sudo", "rm", "-rf", "x"], &["rm", "-rf", "x"]),
            (&["env", "FOO=1", "BAR=2", "rm", "x"], &["rm", "x"]),
            (&["env", "-u", "HOME", "rm", "x"], &["rm", "x"]),
            (
                &["sudo", "-u", "root", "env", "-i", "rm", "x"],
                &["rm", "x"],
            ),
            (&["timeout", "5s", "make", "clean"], &["make", "clean"]),
            (&["nice", "-n", "10", "git", "push"], &["git", "push"]),
            (&["xargs", "rm"], &["rm"]),
            (
                &["timeout", "-s", "KILL", "60", "rm", "-rf", "build"],
                &["rm", "-rf", "build"],
            ),
            (&["env", "-u", "CI", "git", "push"], &["git", "push"]),
            (&["cargo", "build"], &["cargo", "build"]),
        ];
        for (input, expected) in cases {
            assert_eq!(strip_wrappers(&argv(input)), argv(expected), "{input:?}");
        }
        for input in [
            &["xargs", "-z", "{}", "rm", "-rf", "build"][..],
            &["xargs", "-I"][..],
        ] {
            assert!(!analyze_wrappers(&argv(input)).complete, "{input:?}");
        }
        for (input, skipped, arguments_unknown, writes_unknown) in [
            (
                &["xargs", "-I", "{}", "rm", "-rf", "{}"][..],
                3,
                true,
                false,
            ),
            (
                &["xargs", "--replace", "-I", "{}", "rm", "{}"][..],
                4,
                true,
                false,
            ),
            (
                &["time", "-o", "t.log", "cargo", "build"][..],
                3,
                false,
                true,
            ),
        ] {
            let input = argv(input);
            assert_eq!(
                analyze_wrappers(&input),
                WrapperAnalysis {
                    argv: &input[skipped..],
                    complete: true,
                    environment_unknown: false,
                    cwd_unknown: false,
                    arguments_unknown,
                    writes_unknown,
                }
            );
        }
        for (input, expected) in [
            (
                &["xargs", "-I", "{}", "rm", "-rf", "{}"][..],
                &["rm", "-rf", "{}"][..],
            ),
            (
                &["time", "-o", "t.log", "cargo", "build"][..],
                &["cargo", "build"][..],
            ),
        ] {
            let views = risk_wrapper_views(&argv(input));
            assert!(
                views
                    .iter()
                    .any(|view| view.as_slice() == argv(expected).as_slice()),
                "{input:?}: {views:?}"
            );
        }
        assert!(
            analyze_wrappers(&argv(&["xargs", "--process-slot-var=HOME", "printenv"]))
                .environment_unknown,
            "the child environment no longer comes from the parent"
        );
    }

    #[test]
    fn two_word_prefixes_are_only_stable_for_plain_commands() {
        for stable in [
            &["git", "commit", "-m", "x"][..],
            &["cargo", "test", "--workspace"][..],
            &["make", "check"][..],
            &["make"][..],
        ] {
            assert!(!argv_prefix_is_unstable(&argv(stable)), "{stable:?}");
        }
        for unstable in [
            &["timeout", "600", "cargo", "test"][..],
            &["env", "RUST_LOG=1", "cargo", "run"][..],
            &["time", "cargo", "build"][..],
            &["git", "-C", "../lib", "status"][..],
            &["python3", "-c", "print(1)"][..],
            &["./scripts/run", "--fast"][..],
            &["make", "VAR=1", "check"][..],
        ] {
            assert!(argv_prefix_is_unstable(&argv(unstable)), "{unstable:?}");
        }
    }

    #[test]
    fn wrapper_analysis_keeps_environment_effects() {
        let prefixes: &[&[&str]] = &[
            &["env", "HOME=/tmp"],
            &["env", "-u", "HOME"],
            &["env", "-uHOME"],
            &["env", "--unset=HOME"],
            &["env", "-i"],
            &["sudo"],
            &["sudo", "-E"],
            &["sudo", "--preserve-env=HOME"],
            &["sudo", "-u", "root", "env", "HOME=/tmp"],
            &["timeout", "5", "/usr/bin/env", "HOME=/tmp"],
        ];
        for prefix in prefixes {
            let mut input = argv(prefix);
            input.extend(argv(&["bash", "-c", "printf x"]));
            assert_eq!(
                analyze_wrappers(&input),
                WrapperAnalysis {
                    argv: &input[prefix.len()..],
                    complete: true,
                    environment_unknown: true,
                    cwd_unknown: false,
                    arguments_unknown: false,
                    writes_unknown: false,
                },
                "{prefix:?}"
            );
        }
        let input = argv(&["env", "bash", "-c", "printf x"]);
        assert_eq!(
            analyze_wrappers(&input),
            WrapperAnalysis {
                argv: &input[1..],
                complete: true,
                environment_unknown: false,
                cwd_unknown: false,
                arguments_unknown: false,
                writes_unknown: false,
            }
        );
        let input = argv(&["env", "-C", "/tmp", "bash", "-c", "printf x"]);
        assert_eq!(
            analyze_wrappers(&input),
            WrapperAnalysis {
                argv: &input[3..],
                complete: true,
                environment_unknown: true,
                cwd_unknown: true,
                arguments_unknown: false,
                writes_unknown: false,
            }
        );
        for input in [
            argv(&["env", "-u"]),
            argv(&["sudo", "-u"]),
            argv(&["env", "-S", "bash -c x"]),
        ] {
            assert!(!analyze_wrappers(&input).complete, "{input:?}");
        }
    }
}
