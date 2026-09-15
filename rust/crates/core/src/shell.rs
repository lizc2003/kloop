//! shell — bash command analysis on a real parse tree (tree-sitter-bash),
//! ported from codex's `codex-shell-command` crate.
//!
//! One analysis feeds three verdicts: concurrency safety (tools.rs), the
//! read-only/allowlist layers, and the dangerous-command safety check
//! (permissions.rs). The core rule is a *whitelist walk*: a script is only
//! [`BashAnalysis::Commands`] when every node in its parse tree is a plain
//! word-only command joined by `&&`/`||`/`;`/`|`/newline. Anything else —
//! subshells, command/process substitution, expansions, variable assignments,
//! control flow, and any redirect that reads or writes a file — makes the
//! whole script [`BashAnalysis::Opaque`]: no classifier can vouch for what it
//! runs, so it can never be auto-approved. Stream-only redirects (`2>&1`,
//! `>/dev/null`) are the exception: they add nothing the argv does not
//! already say ([`redirect_is_stream_only`]).

use tree_sitter::Node;
use tree_sitter::Parser;
use tree_sitter::Tree;
use tree_sitter_bash::LANGUAGE as BASH;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BashAnalysis {
    /// Every command in the script, as plain argv words, in source order.
    Commands(Vec<Vec<String>>),
    /// The script contains constructs the word-only parser cannot vouch for.
    Opaque,
}

/// How deep `bash -c "…"` nesting is unwrapped before giving up.
const MAX_UNWRAP_DEPTH: u8 = 2;

pub fn analyze_bash(script: &str) -> BashAnalysis {
    analyze_at_depth(script, 0)
}

fn analyze_at_depth(script: &str, depth: u8) -> BashAnalysis {
    let Some(commands) = parse_word_only_commands(script) else {
        return BashAnalysis::Opaque;
    };
    // Unwrap nested `bash|sh|zsh -c|-lc "<script>"` one level at a time so
    // the inner commands are classified, not the opaque wrapper words. An
    // unparseable inner script keeps the wrapper argv as-is: `bash` is in no
    // safe list and matches no sane allow rule, so it falls through to ask.
    let mut flattened = Vec::with_capacity(commands.len());
    for argv in commands {
        match extract_wrapped_script(&argv) {
            Some(inner) if depth < MAX_UNWRAP_DEPTH => match analyze_at_depth(inner, depth + 1) {
                BashAnalysis::Commands(inner_cmds) => flattened.extend(inner_cmds),
                BashAnalysis::Opaque => return BashAnalysis::Opaque,
            },
            // A `bash -c` wrapper past the unwrap limit must NOT be kept as a
            // trusted plain command: its inner script stays unvetted, so
            // `argv_is_dangerous(["bash", …])` and deny prefixes never see the
            // `rm -rf` inside it, and bypass mode would auto-run it. Opaque
            // keeps it out of every auto-allow path (falls through to ask).
            Some(_) => return BashAnalysis::Opaque,
            None => flattened.push(argv),
        }
    }
    BashAnalysis::Commands(flattened)
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

fn parse_word_only_commands(script: &str) -> Option<Vec<Vec<String>>> {
    let tree = parse_bash(script)?;
    word_only_commands_sequence(&tree, script)
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
fn word_only_commands_sequence(tree: &Tree, src: &str) -> Option<Vec<Vec<String>>> {
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
        // Only the wrapper: each `file_redirect` under it is vetted separately
        // by `redirect_is_stream_only`, and a heredoc/herestring redirect is
        // not on this list at all.
        "redirected_statement",
    ];
    const ALLOWED_PUNCT_TOKENS: &[&str] = &["&&", "||", ";", "|", "\"", "'"];

    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut stack = vec![root];
    let mut command_nodes = Vec::new();
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if node.is_named() {
            // A redirect is judged whole and never descended into: its operator
            // tokens (`>`, `>&`, …) are not punctuation the rest of the walk
            // should learn to accept, and its destination is not an argument.
            if kind == "file_redirect" {
                if !redirect_is_stream_only(node, src) {
                    return None;
                }
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
    Some(commands)
}

/// Whether one redirect only moves streams around, leaving the argv a complete
/// account of the call. Two forms qualify — duplicating onto another descriptor
/// (`2>&1`, `>&2`) and discarding into `/dev/null` — and both are how a shell
/// command says "I do not want this output", not how it touches the filesystem.
///
/// Everything else sinks the whole script: `> f` and `>> f` write a file the
/// argv never names, `< f` feeds one in, and a heredoc carries content no
/// classifier reads. The shape alone cannot tell them apart — `foo > 1`
/// parses with the same `destination: (number)` as `2>&1` — so the operator
/// text is what decides.
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

fn plain_command_argv(cmd: Node, src: &str) -> Option<Vec<String>> {
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
            "word" | "number" => {
                words.push(child.utf8_text(src.as_bytes()).ok()?.to_owned());
            }
            "string" => words.push(double_quoted_content(child, src)?),
            "raw_string" => words.push(raw_string_content(child, src)?),
            "concatenation" => {
                // e.g. -g"*.py": flag glued to a quoted value.
                let mut joined = String::new();
                let mut concat_cursor = child.walk();
                for part in child.named_children(&mut concat_cursor) {
                    match part.kind() {
                        "word" | "number" => {
                            joined.push_str(part.utf8_text(src.as_bytes()).ok()?);
                        }
                        "string" => joined.push_str(&double_quoted_content(part, src)?),
                        "raw_string" => joined.push_str(&raw_string_content(part, src)?),
                        _ => return None,
                    }
                }
                if joined.is_empty() {
                    return None;
                }
                words.push(joined);
            }
            "comment" => {}
            _ => return None,
        }
    }
    if words.is_empty() {
        return None;
    }
    Some(words)
}

/// A double-quoted string is literal only if every named child is plain
/// content — any expansion/substitution node inside rejects it.
fn double_quoted_content(node: Node, src: &str) -> Option<String> {
    let mut cursor = node.walk();
    for part in node.named_children(&mut cursor) {
        if part.kind() != "string_content" {
            return None;
        }
    }
    let raw = node.utf8_text(src.as_bytes()).ok()?;
    Some(raw.strip_prefix('"')?.strip_suffix('"')?.to_string())
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

        "tree" => !rest.iter().any(|a| a == "-o"),

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
/// recovery path rather than the threat. Device clobbering through a redirect
/// (`… > /dev/sda`) needs no entry either: a redirect onto a file makes the
/// whole script [`BashAnalysis::Opaque`], which never gets an automatic verdict
/// anywhere.
///
/// Because it is a blocklist, it is not the containment story — see the bypass
/// layer in [`crate::permissions`]. It is the short list of things worth one
/// question even when the user has said "stop asking".
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

/// Strip wrapper prefixes (`sudo`, `env FOO=x`, `timeout 5`, `nice -n 10`,
/// `nohup`, `time`, `xargs`) so DENY rules match the wrapped command.
/// Only used for deny matching and the danger check — allow/read-only
/// verdicts never strip, erring conservative.
pub fn strip_wrappers(argv: &[String]) -> Vec<String> {
    let mut rest: &[String] = argv;
    loop {
        let Some(first) = rest.first() else {
            return rest.to_vec();
        };
        match executable_name(first) {
            "sudo" | "env" | "nohup" | "nice" | "time" | "timeout" | "xargs" => {
                rest = &rest[1..];
                // Drop the wrapper's own leading options / assignments /
                // durations, e.g. `env FOO=1`, `timeout 5s`, `nice -n 10`.
                while let Some(tok) = rest.first() {
                    let is_assignment = tok.split_once('=').is_some_and(|(name, _)| {
                        !name.is_empty()
                            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    });
                    let is_duration = !tok.is_empty()
                        && tok.chars().all(|c| {
                            c.is_ascii_digit() || matches!(c, '.' | 's' | 'm' | 'h' | 'd')
                        })
                        && tok.chars().any(|c| c.is_ascii_digit());
                    if tok.starts_with('-') || is_assignment || is_duration {
                        rest = &rest[1..];
                    } else {
                        break;
                    }
                }
            }
            _ => return rest.to_vec(),
        }
    }
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
            BashAnalysis::Opaque => None,
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
            "ls > out.txt",               // redirection onto a file
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
    fn a_redirect_that_touches_a_file_still_sinks_the_script() {
        for script in [
            "ls >> out.txt",          // append
            "cat < in.txt",           // a file read the argv never names
            "foo &> log",             // both streams, real file
            "foo > 1",                // same parse shape as `2>&1`, a file named 1
            "go test ./x 2>&1 > out", // one bad redirect among good ones
            "foo 3>&-",               // operator with no destination
            "cat <<EOF\nhi\nEOF",     // heredoc
            "cat <<< hi",             // herestring
        ] {
            assert_eq!(analyze_bash(script), BashAnalysis::Opaque, "{script}");
        }
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
        // would escape the danger/deny checks and bypass mode would auto-run it.
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
        assert!(!argv_is_readonly(&argv(&["tree", "-o", "out.txt"])));
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

        // A redirect onto a device needs no entry: it makes the whole script
        // opaque, and an opaque script never gets an automatic verdict.
        assert_eq!(analyze_bash("cat x > /dev/sda"), BashAnalysis::Opaque);
    }

    #[test]
    fn wrapper_stripping_reaches_the_wrapped_command() {
        let cases: &[(&[&str], &[&str])] = &[
            (&["sudo", "rm", "-rf", "x"], &["rm", "-rf", "x"]),
            (&["env", "FOO=1", "BAR=2", "rm", "x"], &["rm", "x"]),
            (&["timeout", "5s", "make", "clean"], &["make", "clean"]),
            (&["nice", "-n", "10", "git", "push"], &["git", "push"]),
            (&["xargs", "rm"], &["rm"]),
            (&["cargo", "build"], &["cargo", "build"]),
        ];
        for (input, expected) in cases {
            assert_eq!(strip_wrappers(&argv(input)), argv(expected), "{input:?}");
        }
    }
}
