//! 命令参数的角色：哪些值会被命令当作文件打开，哪些只是匹配模式或字面量。
//!
//! 只认识下面列出的命令与选项。任何未识别的命令、选项、缺参或不完整表达式都
//! 返回 `None`，由调用方退回逐参数路径检查——角色表只能减少误报，不能放宽未知
//! 语法。读取类参数（模式文件、参考文件、搜索根）永远留在检查一侧；执行/写入类
//! 选项由 [`argv_is_readonly`] 提前排除。

use crate::shell::argv_is_readonly;

/// `argv` 里会被命令当作文件打开或遍历的值，可能是整 token，也可能是
/// `--opt=value` / `-Xvalue` 中 `=` 或短选项之后的那一段。
///
/// 返回 `None` 表示未识别：调用方必须检查 `argv[1..]` 的每个参数。
pub(super) fn path_arguments(argv: &[String]) -> Option<Vec<&str>> {
    let command = argv.first()?.rsplit('/').next()?;
    // echo/printf 的参数只是文本，也没有会打开文件的选项。
    if matches!(command, "echo" | "printf") {
        return Some(Vec::new());
    }
    if !argv_is_readonly(argv) {
        return None;
    }
    match command {
        "find" => parse_find(argv).map(|found| found.paths),
        "grep" => option_paths(argv, &GREP, Positionals::PatternThenPaths),
        "rg" => option_paths(argv, &RG, Positionals::PatternThenPaths),
        "du" => option_paths(argv, &DU, Positionals::AllPaths),
        "tree" => option_paths(argv, &TREE, Positionals::AllPaths),
        _ => None,
    }
}

/// 命令是否在参数角色表里。已建模命令解析失败时不能拿"元数据访问"替它担保：
/// 角色不明，路径可能被当成模式或读取目标。
pub(super) fn is_modeled_command(argv: &[String]) -> bool {
    let Some(command) = argv.first().and_then(|arg| arg.rsplit('/').next()) else {
        return false;
    };
    matches!(command, "find" | "grep" | "rg" | "du" | "tree")
}

/// grep/rg 是否在搜索根下递归。共用同一套选项簇解析：`-rn`、`-d recurse`、
/// `--directories=recurse`、`-.`、`-uu`、`--unrestricted` 都按各自语法判定，
/// 不再只认原样的 `-r`。选项簇解析失败时退回旧的字面形状。
pub(super) fn search_is_recursive(argv: &[String]) -> bool {
    let Some(command) = argv.first().and_then(|raw| raw.rsplit('/').next()) else {
        return false;
    };
    match command {
        "grep" => walk_options(argv, &GREP, Positionals::PatternThenPaths).map_or_else(
            || {
                argv[1..]
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "-r" | "-R" | "--recursive"))
            },
            |walked| {
                walked
                    .short_flags
                    .iter()
                    .any(|flag| matches!(flag, 'r' | 'R'))
                    || walked
                        .short_values
                        .iter()
                        .any(|(flag, value)| *flag == 'd' && *value == "recurse")
                    || walked
                        .long_flags
                        .iter()
                        .any(|name| matches!(*name, "recursive" | "dereference-recursive"))
                    || walked
                        .long_values
                        .iter()
                        .any(|(name, value)| *name == "directories" && *value == "recurse")
            },
        ),
        // ripgrep 默认递归，但只有 `--hidden` / `-u`（含 `-uu`）/ `-.` /
        // `--unrestricted` 才会读隐藏的点文件。`-u` 一次就够：它让被 gitignore
        // 的 `.env` 重新可见。
        "rg" => walk_options(argv, &RG, Positionals::PatternThenPaths).map_or_else(
            || {
                argv[1..].iter().any(|arg| {
                    arg == "--hidden"
                        || (arg.starts_with('-')
                            && !arg.starts_with("--")
                            && arg.chars().skip(1).any(|ch| ch == 'u'))
                })
            },
            |walked| {
                walked
                    .short_flags
                    .iter()
                    .any(|flag| matches!(flag, 'u' | '.'))
                    || walked
                        .long_flags
                        .iter()
                        .any(|name| matches!(*name, "hidden" | "unrestricted"))
            },
        ),
        _ => false,
    }
}

/// 只 stat 或列目录、不打开文件内容的调用。未知命令一律按内容访问处理，
/// 所以这里只放行明确确认过"没有读取内容选项"的命令。
pub(super) fn is_metadata_access(argv: &[String]) -> bool {
    let Some(command) = argv.first().and_then(|arg| arg.rsplit('/').next()) else {
        return false;
    };
    match command {
        "ls" | "stat" => true,
        // du 只统计目录；带 Path 操作数的选项（-X、--exclude-from、
        // --files0-from，含 -sX 这类组合）会打开文件。
        "du" => option_opens_file(argv, &DU) == Some(false),
        // tree 只 stat；-o/-R 写文件，--from* / --gitignore / --gitfile /
        // --infofile / --h* 读文件。只看实际作为选项消费的开关，
        // `-I --gitignore` 里的 `--gitignore` 是模式，不是开关。
        "tree" => match walk_options(argv, &TREE, Positionals::AllPaths) {
            Some(walked) => {
                !walked.opens_file
                    && !walked.short_flags.contains(&'R')
                    && !walked.long_flags.iter().any(|name| {
                        matches!(
                            *name,
                            "outfile"
                                | "fromfile"
                                | "fromtabfile"
                                | "gitignore"
                                | "gitfile"
                                | "infofile"
                                | "hintro"
                                | "houtro"
                        )
                    })
            }
            None => false,
        },
        // 普通 find 谓词只 stat / 输出名字；`-files0-from` 会读取根列表文件。
        "find" => parse_find(argv).is_some_and(|found| !found.opens_file),
        _ => false,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operand {
    /// 没有操作数。
    None,
    /// 只接受附着值（`--color=always`），值是字面量。
    Optional,
    /// 操作数是匹配模式或字面量，不是路径。
    Literal,
    /// 操作数会被命令打开。
    Path,
    /// 操作数是模式，且出现后位置参数全是路径（`-e` / `--regexp`）。
    Pattern,
    /// 操作数是模式文件，且出现后位置参数全是路径（`-f` / `--file`）。
    PatternPath,
    /// 没有操作数，但命令进入“没有模式”的模式（`rg --files`）。
    NoPattern,
}

enum ShortOperands {
    AttachedOrNext,
    /// tree 的 -L 数字后只接受已知无参开关；其它带操作数的组合退回全参数检查。
    Tree,
}

struct Spec {
    short: fn(char) -> Option<Operand>,
    long: fn(&str) -> Option<Operand>,
    short_operands: ShortOperands,
    /// grep 的 `-5` 是 `-C 5` 的简写：纯数字短参数不消耗操作数。
    digits_are_context: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Positionals {
    AllPaths,
    PatternThenPaths,
}

struct Walked<'a> {
    paths: Vec<&'a str>,
    /// 消费了某个选项的 Path 类操作数（会打开或写入文件）；位置参数不算。
    opens_file: bool,
    /// 实际作为选项消费的长选项名（`--name=value` 取 `name`）。
    long_flags: Vec<&'a str>,
    /// 实际作为无操作数开关消费的短选项字母。
    short_flags: Vec<char>,
    /// 短选项及其操作数（`-d recurse`、`--directories=recurse` 这类语义住在
    /// 值里的开关靠它判定）。
    short_values: Vec<(char, &'a str)>,
    /// 长选项及其操作数。
    long_values: Vec<(&'a str, &'a str)>,
}

fn option_paths<'a>(
    argv: &'a [String],
    spec: &Spec,
    positionals: Positionals,
) -> Option<Vec<&'a str>> {
    walk_options(argv, spec, positionals).map(|walked| walked.paths)
}

/// 命令是否消费了带 Path 角色的选项操作数。用来区分“只 stat 位置参数”的
/// 元数据调用和会打开文件的调用（`du -sX file`、`tree -so file`）。
fn option_opens_file(argv: &[String], spec: &Spec) -> Option<bool> {
    walk_options(argv, spec, Positionals::AllPaths).map(|walked| walked.opens_file)
}

fn walk_options<'a>(
    argv: &'a [String],
    spec: &Spec,
    positionals: Positionals,
) -> Option<Walked<'a>> {
    let mut walked = Walked {
        paths: Vec::new(),
        opens_file: false,
        long_flags: Vec::new(),
        short_flags: Vec::new(),
        short_values: Vec::new(),
        long_values: Vec::new(),
    };
    let mut pattern_source = false;
    // 第一个位置参数可能是模式，也可能是路径：`-f`/`--file` 可以出现在它后面
    // （getopt 会把选项换到前面），所以推迟到解析结束再决定。
    let mut first_positional: Option<&'a str> = None;
    let mut options_done = false;
    let mut index = 1;
    while let Some(raw) = argv.get(index) {
        let arg = raw.as_str();
        if options_done || !arg.starts_with('-') || arg == "-" {
            match positionals {
                Positionals::AllPaths => walked.paths.push(arg),
                Positionals::PatternThenPaths => {
                    if first_positional.is_none() && !pattern_source {
                        first_positional = Some(arg);
                    } else {
                        walked.paths.push(arg);
                    }
                }
            }
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
            let role = (spec.long)(name)?;
            walked.long_flags.push(name);
            match (role, attached) {
                (Operand::None | Operand::Optional, _) => index += 1,
                (Operand::NoPattern, _) => {
                    pattern_source = true;
                    index += 1;
                }
                (Operand::Literal | Operand::Pattern, Some(value)) => {
                    pattern_source |= role == Operand::Pattern;
                    walked.long_values.push((name, value));
                    index += 1;
                }
                (Operand::Literal | Operand::Pattern, None) => {
                    let value = argv.get(index + 1)?.as_str();
                    pattern_source |= role == Operand::Pattern;
                    walked.long_values.push((name, value));
                    index += 2;
                }
                (Operand::Path | Operand::PatternPath, Some(value)) => {
                    pattern_source |= role == Operand::PatternPath;
                    walked.opens_file = true;
                    walked.paths.push(value);
                    walked.long_values.push((name, value));
                    index += 1;
                }
                (Operand::Path | Operand::PatternPath, None) => {
                    let value = argv.get(index + 1)?.as_str();
                    pattern_source |= role == Operand::PatternPath;
                    walked.opens_file = true;
                    walked.paths.push(value);
                    walked.long_values.push((name, value));
                    index += 2;
                }
            }
            continue;
        }
        let body = &arg[1..];
        if spec.digits_are_context && !body.is_empty() && body.chars().all(|c| c.is_ascii_digit()) {
            index += 1;
            continue;
        }
        let mut consumed_next = false;
        for (offset, ch) in body.char_indices() {
            let role = (spec.short)(ch)?;
            match role {
                Operand::None | Operand::Optional => {
                    walked.short_flags.push(ch);
                    continue;
                }
                Operand::NoPattern => {
                    walked.short_flags.push(ch);
                    pattern_source = true;
                    continue;
                }
                _ => {}
            }
            let rest = &arg[1 + offset + ch.len_utf8()..];
            let value = if rest.is_empty() {
                consumed_next = true;
                argv.get(index + 1)?.as_str()
            } else {
                match spec.short_operands {
                    ShortOperands::AttachedOrNext => rest,
                    ShortOperands::Tree if ch == 'L' => tree_depth_operand(rest)?,
                    ShortOperands::Tree => return None,
                }
            };
            pattern_source |= matches!(role, Operand::Pattern | Operand::PatternPath);
            walked.short_values.push((ch, value));
            if matches!(role, Operand::Path | Operand::PatternPath) {
                walked.opens_file = true;
                walked.paths.push(value);
            }
            break;
        }
        index += if consumed_next { 2 } else { 1 };
    }
    if positionals == Positionals::PatternThenPaths
        && pattern_source
        && let Some(first) = first_positional
    {
        walked.paths.push(first);
    }
    Some(walked)
}

struct Found<'a> {
    paths: Vec<&'a str>,
    /// `-files0-from` 读取文件内容（根列表），不是元数据访问。
    opens_file: bool,
}

fn parse_find(argv: &[String]) -> Option<Found<'_>> {
    let mut found = Found {
        paths: Vec::new(),
        opens_file: false,
    };
    let mut index = 1;
    // GNU 的前置选项与 BSD 的 `-f` 根：它们把“表达式从哪开始”往后推。
    while let Some(arg) = argv.get(index) {
        match arg.as_str() {
            "-H" | "-L" | "-P" | "-E" | "-X" | "-d" | "-s" | "-x" => index += 1,
            "-f" => {
                // BSD `-f` 后面是搜索根，即使它恰好叫 `-path` 也不是谓词。
                found.paths.push(argv.get(index + 1)?.as_str());
                index += 2;
            }
            "-D" => {
                argv.get(index + 1)?;
                index += 2;
            }
            _ if is_find_optimization_level(arg) => index += 1,
            _ => break,
        }
    }
    while let Some(arg) = argv.get(index) {
        if arg.starts_with('-') || matches!(arg.as_str(), "(" | r"\(" | "!" | r"\!") {
            break;
        }
        found.paths.push(arg.as_str());
        index += 1;
    }
    // 没有表达式也是合法调用（`find .git` 只列目录），此时只有搜索根是路径。
    if argv.get(index).is_none() {
        return Some(found);
    }

    let mut depth = 0_usize;
    let mut needs_operand = true;
    while let Some(arg) = argv.get(index) {
        match arg.as_str() {
            "(" | r"\(" => {
                depth += 1;
                needs_operand = true;
            }
            ")" | r"\)" => {
                if needs_operand || depth == 0 {
                    return None;
                }
                depth -= 1;
            }
            "!" | r"\!" | "-not" => needs_operand = true,
            "-a" | "-and" | "-o" | "-or" | "," => {
                if needs_operand {
                    return None;
                }
                needs_operand = true;
            }
            // 匹配模式：不打开文件。
            "-name" | "-iname" | "-path" | "-ipath" | "-wholename" | "-iwholename" | "-regex"
            | "-iregex" | "-lname" | "-ilname" => {
                index += 1;
                argv.get(index)?;
                needs_operand = false;
            }
            // 参考文件：只 stat，路径仍要检查。
            "-anewer" | "-cnewer" | "-mnewer" | "-Bnewer" | "-samefile" => {
                index += 1;
                found.paths.push(argv.get(index)?.as_str());
                needs_operand = false;
            }
            _ if arg.starts_with("-newer") => {
                index += 1;
                found.paths.push(argv.get(index)?.as_str());
                needs_operand = false;
            }
            // 根列表文件：读取内容，按内容访问处理。
            "-files0-from" => {
                index += 1;
                found.paths.push(argv.get(index)?.as_str());
                found.opens_file = true;
                needs_operand = false;
            }
            // 字面量操作数：类型、大小、模式、格式串……都不是路径。
            "-type" | "-xtype" | "-size" | "-perm" | "-flags" | "-fstype" | "-user" | "-group"
            | "-uid" | "-gid" | "-inum" | "-links" | "-amin" | "-atime" | "-cmin" | "-ctime"
            | "-mmin" | "-mtime" | "-Bmin" | "-Btime" | "-used" | "-maxdepth" | "-mindepth"
            | "-regextype" | "-xattrname" | "-printf" => {
                index += 1;
                argv.get(index)?;
                needs_operand = false;
            }
            "-true"
            | "-false"
            | "-empty"
            | "-nouser"
            | "-nogroup"
            | "-readable"
            | "-writable"
            | "-executable"
            | "-acl"
            | "-sparse"
            | "-xattr"
            | "-depth"
            | "-d"
            | "-xdev"
            | "-mount"
            | "-follow"
            | "-daystart"
            | "-noleaf"
            | "-ignore_readdir_race"
            | "-noignore_readdir_race"
            | "-print"
            | "-print0"
            | "-ls"
            | "-prune"
            | "-quit" => needs_operand = false,
            _ => return None,
        }
        index += 1;
    }
    (depth == 0 && !needs_operand).then_some(found)
}

fn is_find_optimization_level(arg: &str) -> bool {
    arg.strip_prefix("-O")
        .is_some_and(|level| !level.is_empty() && level.chars().all(|c| c.is_ascii_digit()))
}

const GREP: Spec = Spec {
    short: grep_short,
    long: grep_long,
    short_operands: ShortOperands::AttachedOrNext,
    digits_are_context: true,
};

fn grep_short(ch: char) -> Option<Operand> {
    match ch {
        'e' => Some(Operand::Pattern),
        'f' => Some(Operand::PatternPath),
        'A' | 'B' | 'C' | 'D' | 'd' | 'm' => Some(Operand::Literal),
        'E' | 'F' | 'G' | 'H' | 'I' | 'L' | 'P' | 'R' | 'T' | 'U' | 'Z' | 'a' | 'b' | 'c' | 'h'
        | 'i' | 'l' | 'n' | 'o' | 'q' | 'r' | 's' | 'u' | 'v' | 'w' | 'x' | 'y' | 'z' => {
            Some(Operand::None)
        }
        _ => None,
    }
}

fn grep_long(name: &str) -> Option<Operand> {
    match name {
        "regexp" => Some(Operand::Pattern),
        "file" => Some(Operand::PatternPath),
        "include" | "exclude" | "exclude-dir" => Some(Operand::Literal),
        "exclude-from" => Some(Operand::Path),
        "max-count" | "after-context" | "before-context" | "context" | "binary-files"
        | "directories" | "devices" | "label" | "group-separator" => Some(Operand::Literal),
        "color" | "colour" => Some(Operand::Optional),
        "extended-regexp"
        | "fixed-strings"
        | "basic-regexp"
        | "perl-regexp"
        | "ignore-case"
        | "no-ignore-case"
        | "invert-match"
        | "word-regexp"
        | "line-regexp"
        | "files-with-matches"
        | "files-without-match"
        | "count"
        | "line-number"
        | "with-filename"
        | "no-filename"
        | "only-matching"
        | "quiet"
        | "silent"
        | "no-messages"
        | "byte-offset"
        | "text"
        | "binary"
        | "unix-byte-offsets"
        | "recursive"
        | "dereference-recursive"
        | "line-buffered"
        | "null"
        | "null-data"
        | "initial-tab"
        | "no-group-separator"
        | "no-mmap"
        | "mmap"
        | "help"
        | "version" => Some(Operand::None),
        _ => None,
    }
}

const RG: Spec = Spec {
    short: rg_short,
    long: rg_long,
    short_operands: ShortOperands::AttachedOrNext,
    digits_are_context: false,
};

fn rg_short(ch: char) -> Option<Operand> {
    match ch {
        'e' => Some(Operand::Pattern),
        'f' => Some(Operand::PatternPath),
        'A' | 'B' | 'C' | 'E' | 'M' | 'T' | 'd' | 'g' | 'j' | 'm' | 'r' | 't' => {
            Some(Operand::Literal)
        }
        'F' | 'H' | 'I' | 'L' | 'N' | 'P' | 'S' | 'U' | 'V' | 'a' | 'b' | 'c' | 'h' | 'i' | 'l'
        | 'n' | 'o' | 'p' | 'q' | 's' | 'u' | 'v' | 'w' | 'x' | 'z' | '0' | '.' => {
            Some(Operand::None)
        }
        _ => None,
    }
}

fn rg_long(name: &str) -> Option<Operand> {
    match name {
        "regexp" => Some(Operand::Pattern),
        "file" => Some(Operand::PatternPath),
        "glob" | "iglob" | "pre-glob" => Some(Operand::Literal),
        "ignore-file" | "pre" | "hostname-bin" => Some(Operand::Path),
        "after-context"
        | "before-context"
        | "context"
        | "max-count"
        | "max-depth"
        | "threads"
        | "max-filesize"
        | "max-columns"
        | "type"
        | "type-not"
        | "type-add"
        | "type-clear"
        | "sort"
        | "sortr"
        | "replace"
        | "encoding"
        | "path-separator"
        | "field-match-separator"
        | "field-context-separator"
        | "context-separator"
        | "engine"
        | "dfa-size-limit"
        | "regex-size-limit" => Some(Operand::Literal),
        "color" | "colour" | "colors" => Some(Operand::Optional),
        "files" => Some(Operand::NoPattern),
        "hidden"
        | "no-hidden"
        | "no-ignore"
        | "no-ignore-vcs"
        | "no-ignore-parent"
        | "no-ignore-global"
        | "no-ignore-dot"
        | "no-ignore-files"
        | "no-require-git"
        | "no-messages"
        | "no-mmap"
        | "mmap"
        | "heading"
        | "no-heading"
        | "line-number"
        | "no-line-number"
        | "column"
        | "no-column"
        | "with-filename"
        | "no-filename"
        | "only-matching"
        | "count"
        | "count-matches"
        | "files-with-matches"
        | "files-without-match"
        | "json"
        | "quiet"
        | "text"
        | "binary"
        | "null"
        | "null-data"
        | "crlf"
        | "trim"
        | "passthru"
        | "stats"
        | "no-stats"
        | "sort-files"
        | "follow"
        | "no-follow"
        | "one-file-system"
        | "no-one-file-system"
        | "no-unicode"
        | "unicode"
        | "pcre2"
        | "no-pcre2"
        | "smart-case"
        | "ignore-case"
        | "case-sensitive"
        | "invert-match"
        | "word-regexp"
        | "line-regexp"
        | "multiline"
        | "no-multiline"
        | "multiline-dotall"
        | "glob-case-insensitive"
        | "no-glob-case-insensitive"
        | "no-ignore-messages"
        | "no-context-separator"
        | "no-group-separator"
        | "no-max-columns"
        | "debug"
        | "trace"
        | "no-color"
        | "unrestricted"
        | "help"
        | "version"
        | "generate"
        | "complete" => Some(Operand::None),
        _ => None,
    }
}

const DU: Spec = Spec {
    short: du_short,
    long: du_long,
    short_operands: ShortOperands::AttachedOrNext,
    digits_are_context: false,
};

fn du_short(ch: char) -> Option<Operand> {
    match ch {
        'B' | 'd' => Some(Operand::Literal),
        'X' => Some(Operand::Path),
        '0' | 'H' | 'L' | 'P' | 'a' | 'b' | 'c' | 'h' | 'k' | 'm' | 's' | 'x' => {
            Some(Operand::None)
        }
        _ => None,
    }
}

fn du_long(name: &str) -> Option<Operand> {
    match name {
        "exclude" => Some(Operand::Literal),
        "exclude-from" | "files0-from" => Some(Operand::Path),
        "block-size" | "max-depth" | "threshold" | "time-style" => Some(Operand::Literal),
        "all" | "apparent-size" | "bytes" | "count-links" | "dereference" | "no-dereference"
        | "human-readable" | "inodes" | "null" | "one-file-system" | "separate-dirs" | "si"
        | "summarize" | "sync" | "time" | "total" | "help" | "version" => Some(Operand::None),
        _ => None,
    }
}

const TREE: Spec = Spec {
    short: tree_short,
    long: tree_long,
    short_operands: ShortOperands::Tree,
    digits_are_context: false,
};

fn tree_depth_operand(rest: &str) -> Option<&str> {
    let depth_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (depth, flags) = rest.split_at(depth_len);
    // 后缀仍按开关解释；只有全部不消耗操作数，后续 token 才能保持原角色。
    (!depth.is_empty()
        && flags
            .chars()
            .all(|ch| matches!(tree_short(ch), Some(Operand::None))))
    .then_some(depth)
}

fn tree_short(ch: char) -> Option<Operand> {
    match ch {
        'H' | 'I' | 'L' | 'P' | 'T' => Some(Operand::Literal),
        'o' => Some(Operand::Path),
        'A' | 'C' | 'F' | 'J' | 'N' | 'Q' | 'R' | 'S' | 'U' | 'X' | 'a' | 'd' | 'f' | 'h' | 'i'
        | 'l' | 'n' | 'p' | 'q' | 'r' | 's' | 't' | 'u' | 'v' | 'x' => Some(Operand::None),
        _ => None,
    }
}

fn tree_long(name: &str) -> Option<Operand> {
    match name {
        "ignore" | "match" | "level" | "title" | "baseHREF" | "charset" | "filelimit"
        | "timefmt" | "sort" => Some(Operand::Literal),
        "outfile" => Some(Operand::Path),
        "all" | "ansi" | "color" | "device" | "dirs-only" | "dirsfirst" | "du" | "full-path"
        | "fromfile" | "gitignore" | "help" | "inodes" | "json" | "no-indent" | "nocolor"
        | "noreport" | "prune" | "quote" | "version" | "xml" => Some(Operand::None),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_paths(words: &[&str], expected: Option<&[&str]>) {
        let argv: Vec<String> = words.iter().map(|word| (*word).to_string()).collect();
        let actual = path_arguments(&argv)
            .map(|paths| paths.into_iter().map(str::to_string).collect::<Vec<_>>());
        let expected = expected.map(|values| {
            values
                .iter()
                .map(|value| (*value).to_string())
                .collect::<Vec<_>>()
        });
        assert_eq!(actual, expected, "{words:?}");
    }

    #[test]
    fn find_collects_roots_and_reference_files_only() {
        assert_paths(
            &[
                "find", ".", "src", "-path", "./.git", "-prune", "-o", "-print",
            ],
            Some(&[".", "src"]),
        );
        assert_paths(
            &["find", "-f", "src", "tests", "-name", "*.rs"],
            Some(&["src", "tests"]),
        );
        assert_paths(
            &["find", ".", "-name", "x", "-newer", "./.git/config"],
            Some(&[".", "./.git/config"]),
        );
        assert_paths(
            &["find", ".", "-newermt", "2024-01-01"],
            Some(&[".", "2024-01-01"]),
        );
        assert_paths(
            &["find", ".", "-files0-from", "roots"],
            Some(&[".", "roots"]),
        );
        assert_paths(&["find", ".", "-printf", "%p\n"], Some(&["."]));
    }

    #[test]
    fn find_accepts_known_options_and_rejects_unknown_syntax() {
        assert_paths(
            &["find", "-O2", ".", "-path", "./.git", "-print"],
            Some(&["."]),
        );
        assert_paths(&["find", "-D", "tree", ".", "-print"], Some(&["."]));
        assert_paths(&["find", ".", "-unknown"], None);
        assert_paths(&["find", ".", "-name"], None);
        assert_paths(&["find", ".", "-exec", "cat", "{}", ";"], None);
        assert_paths(&["find", ".", "-prune", "-o"], None);
        assert_paths(&["find", ".", "-newer", "-path", "./.git"], None);
    }

    #[test]
    fn grep_patterns_are_not_paths() {
        assert_paths(&["grep", "-rn", ".env", "src"], Some(&["src"]));
        assert_paths(&["grep", "-e", "./.git/config", "src"], Some(&["src"]));
        assert_paths(&["grep", "-rn", "--", ".env", "src"], Some(&["src"]));
        assert_paths(&["grep", "-A", "3", ".env", "src"], Some(&["src"]));
        assert_paths(&["grep", "-5", ".env", "src"], Some(&["src"]));
        assert_paths(
            &["grep", "--include=*.rs", "-rn", ".env", "."],
            Some(&["."]),
        );
        assert_paths(
            &["grep", "-f", "./.git/config", "src"],
            Some(&["./.git/config", "src"]),
        );
        assert_paths(
            &["grep", "--exclude-from=./.git/config", ".env", "src"],
            Some(&["./.git/config", "src"]),
        );
        assert_paths(&["grep", "--color=always", ".env", "."], Some(&["."]));
        assert_paths(
            &["grep", "src", "-f", "./.git/config"],
            Some(&["./.git/config", "src"]),
        );
        assert_paths(&["grep", "--future-flag", ".env"], None);
    }

    #[test]
    fn ripgrep_globs_are_not_paths() {
        assert_paths(&["rg", "-g", "!**/.git/*", "TODO", "."], Some(&["."]));
        assert_paths(&["rg", "--glob=!**/.git/*", "TODO", "."], Some(&["."]));
        assert_paths(&["rg", "-n", ".env", "."], Some(&["."]));
        assert_paths(
            &["rg", "--ignore-file", "./.git/config", "TODO", "."],
            Some(&["./.git/config", "."]),
        );
        assert_paths(&["rg", "--type-add", "ts:*.ts", "TODO", "."], Some(&["."]));
        assert_paths(&["rg", "--files", ".env"], Some(&[".env"]));
        assert_paths(&["rg", "--pre", "cat", "TODO", "."], None);
    }

    #[test]
    fn du_and_tree_match_values_are_not_paths() {
        assert_paths(&["du", "-sh", "--exclude=./.git", "."], Some(&["."]));
        assert_paths(&["du", "--exclude", "./.git", "."], Some(&["."]));
        assert_paths(
            &["du", "-X", "./.git/config", "."],
            Some(&["./.git/config", "."]),
        );
        assert_paths(
            &["du", "--files0-from=./.git/config"],
            Some(&["./.git/config"]),
        );
        assert_paths(&["tree", "-I", ".git", "."], Some(&["."]));
        assert_paths(
            &["tree", "--fromfile", "./.git/config"],
            Some(&["./.git/config"]),
        );
        assert_paths(&["tree", "-o", "out.txt", "."], None);
    }

    #[test]
    fn tree_fromfile_preserves_input_paths_and_option_boundaries() {
        assert_paths(&["tree", "--fromfile"], Some(&[]));
        assert_paths(&["tree", "--fromfile", "-I", "-P", ".env"], Some(&[".env"]));
        assert_paths(
            &["tree", "--fromfile", "--", "-P", ".env"],
            Some(&["-P", ".env"]),
        );
        assert_paths(
            &["tree", "--fromfile", "-I", ".git", "paths.txt", ".env"],
            Some(&["paths.txt", ".env"]),
        );
        assert_paths(&["tree", "--fromfile", "-I"], None);
    }

    #[test]
    fn tree_short_operands_do_not_use_getopt_attachment_rules() {
        assert_paths(&["tree", "-aI", ".git", "src"], Some(&["src"]));
        assert_paths(&["tree", "-L2", "-I", ".git", "src"], Some(&["src"]));
        for words in [
            vec!["tree", "-Ia", "-P", ".git"],
            vec!["tree", "--fromfile", "paths.txt", "-Ia", "-P", ".env"],
            vec!["tree", "-La", "1", "-I", "-P", ".env"],
            vec!["tree", "-I.git", "src"],
        ] {
            assert_paths(&words, None);
        }
    }

    #[test]
    fn tree_depth_suffixes_preserve_patterns_and_input_paths() {
        for flags in ["-L2a", "-L2h", "-L2U", "-L12ahU", "-aL12hU"] {
            assert_paths(&["tree", flags, "-I", ".git", "src"], Some(&["src"]));
            assert_paths(&["tree", flags, ".env"], Some(&[".env"]));
        }
        for flags in ["-L2P", "-L2aP", "-L2ao", "-L2aL3", "-L2Z", "-L2a3", "-La2"] {
            assert_paths(&["tree", flags, ".git", "src"], None);
        }
    }

    #[test]
    fn text_commands_have_no_paths() {
        assert_paths(&["echo", ".env", "./.git/config"], Some(&[]));
        assert_paths(&["printf", "%s\n", ".env"], Some(&[]));
    }

    #[test]
    fn unmodeled_commands_and_wrappers_fall_back() {
        assert_paths(&["cat", "./.git/config"], None);
        assert_paths(&["sudo", "find", ".", "-path", "./.git"], None);
        assert_paths(&["xargs", "find", ".", "-path", "./.git"], None);
        assert_paths(&["grep", "-z", ".env"], None);
    }

    #[test]
    fn metadata_access_requires_a_command_that_only_stats() {
        let metadata = |words: &[&str]| {
            let argv = words
                .iter()
                .map(|word| (*word).to_string())
                .collect::<Vec<_>>();
            is_metadata_access(&argv)
        };
        assert!(metadata(&["ls", "-l", ".git/config"]));
        assert!(metadata(&["stat", ".git/config"]));
        assert!(metadata(&["du", "-sh", ".git"]));
        assert!(metadata(&["tree", "-L1", ".git"]));
        assert!(metadata(&["find", ".git", "-maxdepth", "1", "-print"]));
        assert!(metadata(&["find", ".git", "-name", "--files0-from"]));
        assert!(metadata(&["tree", "-I", "--gitignore", ".git"]));
        assert!(metadata(&["tree", "-I", "-old*", "-P", ".git", "."]));
        assert!(!metadata(&["du", "--files0-from=.git/config"]));
        assert!(!metadata(&["du", "-X", ".git/config", "."]));
        assert!(!metadata(&["du", "-sX", ".git/config", "."]));
        assert!(!metadata(&["du", "-hX", "./.git/config"]));
        assert!(!metadata(&["tree", "--fromfile", ".git/config"]));
        assert!(!metadata(&["tree", "--gitignore", ".git"]));
        assert!(!metadata(&["tree", "-R", "-L", "1", ".git"]));
        assert!(!metadata(&["tree", "-aR", ".git"]));
        assert!(!metadata(&["tree", "-o", "out.txt", "."]));
        assert!(!metadata(&["tree", "-so", ".git/config", "."]));
        assert!(!metadata(&["find", ".git", "-files0-from", ".git/config"]));
        assert!(!metadata(&["find", ".git", "-exec", "cat", "{}", ";"]));
        assert!(!metadata(&["cat", ".git/config"]));
        assert!(!metadata(&["grep", "-rn", "x", ".git"]));
        assert!(!metadata(&["future-tool", ".git/config"]));
    }
}
