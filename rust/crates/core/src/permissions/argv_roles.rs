//! 命令参数的角色：哪些值会被命令当作文件打开，哪些只是匹配模式或字面量。
//!
//! 只认识下面列出的命令与选项。元数据命令的未知选项仍逐值检查元数据路径；其它
//! 未识别的命令、选项、缺参或不完整表达式返回 `None`，由调用方逐参数保守检查。
//! 路径分别保留内容读取、元数据访问和写入方向；只读授权另行判定。

use crate::shell::argv_is_readonly;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PathAccess {
    Read,
    Metadata,
    MetadataWrite,
    Write,
}

impl PathAccess {
    pub fn is_metadata(self) -> bool {
        matches!(self, Self::Metadata | Self::MetadataWrite)
    }

    pub fn writes(self) -> bool {
        matches!(self, Self::Write | Self::MetadataWrite)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PathArgument<'a> {
    pub value: &'a str,
    pub access: PathAccess,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct UnknownEffects {
    pub unresolved_access: bool,
    pub dangerous: bool,
}

impl UnknownEffects {
    const UNKNOWN: Self = Self {
        unresolved_access: true,
        dangerous: true,
    };
}

/// 与字面参数共用角色表。NUL 是词事实的未知片段，不是可能传给进程的字节。
pub(super) fn unknown_effects(argv: &[String]) -> UnknownEffects {
    if !argv.iter().any(|word| word.contains('\0')) {
        return UnknownEffects::default();
    }
    let Some(name) = argv.first().filter(|word| !word.contains('\0')) else {
        return UnknownEffects::UNKNOWN;
    };
    let paths = match name.rsplit('/').next().unwrap_or(name) {
        "rm" => {
            return UnknownEffects {
                unresolved_access: false,
                dangerous: true,
            };
        }
        _ => path_arguments(argv),
    };
    match paths {
        Some(paths) => UnknownEffects {
            // 凭据与 agent state 的名称也受保护，未知元数据目标不能排除这些路径。
            unresolved_access: paths
                .iter()
                .any(|path| path.access != PathAccess::Write && path.value.contains('\0')),
            dangerous: false,
        },
        None => UnknownEffects::UNKNOWN,
    }
}

/// `argv` 里会被命令当作文件打开或遍历的值，可能是整 token，也可能是
/// `--opt=value` / `-Xvalue` 中 `=` 或短选项之后的那一段。
///
/// 返回 `None` 表示未识别：调用方必须检查 `argv[1..]` 的每个参数。
pub(super) fn path_arguments(argv: &[String]) -> Option<Vec<PathArgument<'_>>> {
    let command = argv.first()?.rsplit('/').next()?;
    // echo/printf 的参数只是文本，也没有会打开文件的选项。
    if matches!(command, "echo" | "printf") {
        return Some(Vec::new());
    }
    match command {
        "find" => parse_find(argv).map(|found| found.paths),
        "grep" if argv_is_readonly(argv) => {
            option_paths(argv, &GREP, Positionals::PatternThenPaths)
        }
        "rg" if argv_is_readonly(argv) => option_paths(argv, &RG, Positionals::PatternThenPaths),
        "du" => option_paths(argv, &DU, Positionals::AllPaths),
        "tree" => {
            let mut walked = walk_options(argv, &TREE, Positionals::AllPaths)?;
            if walked.short_flags.contains(&'R') {
                return None;
            }
            if walked
                .long_flags
                .iter()
                .any(|flag| matches!(*flag, "fromfile" | "fromtabfile" | "gitignore"))
            {
                for path in &mut walked.paths {
                    if path.access == PathAccess::Metadata {
                        path.access = PathAccess::Read;
                    }
                }
            }
            Some(walked.paths)
        }
        "ls" => Some(metadata_paths(argv, &LS, Positionals::AllPaths)),
        "stat" => Some(metadata_paths(argv, &STAT, Positionals::AllPaths)),
        "git" => git_paths(argv),
        "touch" => Some(metadata_paths(argv, &TOUCH, Positionals::AllPaths)),
        "mkdir" => Some(metadata_paths(argv, &MKDIR, Positionals::AllPaths)),
        "chmod" => Some(metadata_paths(argv, &CHMOD, Positionals::PatternThenPaths)),
        _ => None,
    }
}

fn metadata_paths<'a>(
    argv: &'a [String],
    spec: &Spec,
    positionals: Positionals,
) -> Vec<PathArgument<'a>> {
    // 这些命令没有打开目标正文的选项；角色不明时仍检查所有值的元数据访问。
    option_paths(argv, spec, positionals).unwrap_or_else(|| {
        argv.iter()
            .skip(1)
            .map(|value| PathArgument {
                value,
                access: spec.positionals,
            })
            .collect()
    })
}

/// grep/rg 实际遍历的搜索根：位置参数才是根，`-f patterns.txt` 这类选项读取
/// 的文件不算。返回 `None` 表示语法未识别。
pub(super) fn search_roots(argv: &[String]) -> Option<Vec<&str>> {
    let command = argv.first()?.rsplit('/').next()?;
    match command {
        "grep" => walk_options(argv, &GREP, Positionals::PatternThenPaths)
            .map(|walked| walked.positional_paths),
        "rg" => walk_options(argv, &RG, Positionals::PatternThenPaths)
            .map(|walked| walked.positional_paths),
        _ => None,
    }
}

/// grep/rg 是否在搜索根下递归。共用同一套选项簇解析：`-rn`、`-d recurse`、
/// `--directories=recurse`、`-.`、`-uu`、`--unrestricted` 都按各自语法判定，
/// 不再只认原样的 `-r`。选项簇解析失败（夹了未收录的选项）时仍按同一组拼写
/// 复核：`rg --no-config -.` 这类调用不该因为多一个选项就丢掉递归证据。
pub(super) fn search_is_recursive(argv: &[String]) -> bool {
    let Some(command) = argv.first().and_then(|raw| raw.rsplit('/').next()) else {
        return false;
    };
    match command {
        "grep" => walk_options(argv, &GREP, Positionals::PatternThenPaths).map_or_else(
            || grep_recursion_fallback(argv),
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
            || rg_recursion_fallback(argv),
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

/// 选项簇解析失败时的 grep 递归证据。只认无歧义的拼写：簇里的 `r`/`R`、
/// 递归长选项，以及 `-d`/`--directories` 的 `recurse` 值。
fn grep_recursion_fallback(argv: &[String]) -> bool {
    let recurse_value = |value: &str| value == "recurse";
    argv[1..].iter().any(|arg| match arg.strip_prefix("--") {
        Some(long) => match long.split_once('=') {
            Some((name, value)) => {
                matches!(name, "recursive" | "dereference-recursive")
                    || (name == "directories" && recurse_value(value))
            }
            None => matches!(long, "recursive" | "dereference-recursive"),
        },
        None => {
            arg.len() > 1
                && arg.starts_with('-')
                && arg[1..].chars().any(|ch| matches!(ch, 'r' | 'R'))
        }
    }) || argv
        .windows(2)
        .any(|pair| matches!(pair[0].as_str(), "-d" | "--directories") && recurse_value(&pair[1]))
}

/// 选项簇解析失败时的 rg 递归证据：`--hidden`、`--unrestricted`，以及簇里的
/// `u`/`.`（`-uu`、`-.`）。
fn rg_recursion_fallback(argv: &[String]) -> bool {
    argv[1..].iter().any(|arg| match arg.strip_prefix("--") {
        Some(long) => matches!(
            long.split_once('=').map_or(long, |(name, _)| name),
            "hidden" | "unrestricted"
        ),
        None => {
            arg.len() > 1
                && arg.starts_with('-')
                && arg[1..].chars().any(|ch| matches!(ch, 'u' | '.'))
        }
    })
}

/// 只 stat 或列目录、不打开文件内容的调用。未知命令一律按内容访问处理，
/// 所以这里只放行明确确认过"没有读取内容选项"的命令。
#[cfg(test)]
fn is_metadata_access(argv: &[String]) -> bool {
    path_arguments(argv)
        .is_some_and(|paths| paths.iter().all(|path| path.access == PathAccess::Metadata))
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
    MetadataPath,
    WritePath,
    /// 操作数是模式，且出现后位置参数全是路径（`-e` / `--regexp`）。
    Pattern,
    /// 操作数是模式文件，且出现后位置参数全是路径（`-f` / `--file`）。
    PatternPath,
    MetadataReference,
    /// 没有操作数，但命令进入“没有模式”的模式（`rg --files`）。
    NoPattern,
}

impl Operand {
    fn access(self) -> Option<PathAccess> {
        match self {
            Self::Path | Self::PatternPath => Some(PathAccess::Read),
            Self::MetadataPath | Self::MetadataReference => Some(PathAccess::Metadata),
            Self::WritePath => Some(PathAccess::Write),
            Self::None | Self::Optional | Self::Literal | Self::Pattern | Self::NoPattern => None,
        }
    }

    fn supplies_pattern(self) -> bool {
        matches!(
            self,
            Self::Pattern | Self::PatternPath | Self::MetadataReference
        )
    }
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
    positionals: PathAccess,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Positionals {
    AllPaths,
    PatternThenPaths,
}

struct Walked<'a> {
    paths: Vec<PathArgument<'a>>,
    /// 位置参数里会被遍历/打开的路径。与 `paths` 的区别是它不含选项读取的
    /// 文件（`-f patterns.txt` 是模式来源，不是搜索根）。
    positional_paths: Vec<&'a str>,
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
) -> Option<Vec<PathArgument<'a>>> {
    walk_options(argv, spec, positionals).map(|walked| walked.paths)
}

fn walk_options<'a>(
    argv: &'a [String],
    spec: &Spec,
    positionals: Positionals,
) -> Option<Walked<'a>> {
    let mut walked = Walked {
        paths: Vec::new(),
        positional_paths: Vec::new(),
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
        // 未知首字节可能生成新选项。只有已被已知选项消费的值或 -- 后的路径才能
        // 按固定角色处理；它们不会经过此处分支。
        if !options_done && arg.starts_with('\0') {
            return None;
        }
        if options_done || !arg.starts_with('-') || arg == "-" {
            match positionals {
                Positionals::AllPaths => {
                    walked.paths.push(PathArgument {
                        value: arg,
                        access: spec.positionals,
                    });
                    walked.positional_paths.push(arg);
                }
                Positionals::PatternThenPaths => {
                    if first_positional.is_none() && !pattern_source {
                        first_positional = Some(arg);
                    } else {
                        walked.paths.push(PathArgument {
                            value: arg,
                            access: spec.positionals,
                        });
                        walked.positional_paths.push(arg);
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
                (
                    Operand::Literal
                    | Operand::Pattern
                    | Operand::Path
                    | Operand::PatternPath
                    | Operand::MetadataPath
                    | Operand::MetadataReference
                    | Operand::WritePath,
                    Some(value),
                ) => {
                    pattern_source |= role.supplies_pattern();
                    if let Some(access) = role.access() {
                        walked.paths.push(PathArgument { value, access });
                    }
                    walked.long_values.push((name, value));
                    index += 1;
                }
                (
                    Operand::Literal
                    | Operand::Pattern
                    | Operand::Path
                    | Operand::PatternPath
                    | Operand::MetadataPath
                    | Operand::MetadataReference
                    | Operand::WritePath,
                    None,
                ) => {
                    let value = argv.get(index + 1)?.as_str();
                    pattern_source |= role.supplies_pattern();
                    if let Some(access) = role.access() {
                        walked.paths.push(PathArgument { value, access });
                    }
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
            pattern_source |= role.supplies_pattern();
            walked.short_values.push((ch, value));
            if let Some(access) = role.access() {
                walked.paths.push(PathArgument { value, access });
            }
            break;
        }
        index += if consumed_next { 2 } else { 1 };
    }
    if positionals == Positionals::PatternThenPaths
        && pattern_source
        && let Some(first) = first_positional
    {
        walked.paths.push(PathArgument {
            value: first,
            access: spec.positionals,
        });
        walked.positional_paths.push(first);
    }
    Some(walked)
}

struct Found<'a> {
    paths: Vec<PathArgument<'a>>,
}

fn parse_find(argv: &[String]) -> Option<Found<'_>> {
    let mut found = Found { paths: Vec::new() };
    let mut index = 1;
    // GNU 的前置选项与 BSD 的 `-f` 根：它们把“表达式从哪开始”往后推。
    while let Some(arg) = argv.get(index) {
        match arg.as_str() {
            "-H" | "-L" | "-P" | "-E" | "-X" | "-d" | "-s" | "-x" => index += 1,
            "-f" => {
                // BSD `-f` 后面是搜索根，即使它恰好叫 `-path` 也不是谓词。
                found.paths.push(PathArgument {
                    value: argv.get(index + 1)?,
                    access: PathAccess::Metadata,
                });
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
        if arg.starts_with('\0') {
            return None;
        }
        if arg.starts_with('-') || matches!(arg.as_str(), "(" | r"\(" | "!" | r"\!") {
            break;
        }
        found.paths.push(PathArgument {
            value: arg,
            access: PathAccess::Metadata,
        });
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
                found.paths.push(PathArgument {
                    value: argv.get(index)?,
                    access: PathAccess::Metadata,
                });
                needs_operand = false;
            }
            _ if arg.starts_with("-newer") => {
                index += 1;
                found.paths.push(PathArgument {
                    value: argv.get(index)?,
                    access: PathAccess::Metadata,
                });
                needs_operand = false;
            }
            // 根列表文件：读取内容，按内容访问处理。
            "-files0-from" => {
                index += 1;
                found.paths.push(PathArgument {
                    value: argv.get(index)?,
                    access: PathAccess::Read,
                });
                needs_operand = false;
            }
            "-fprint" | "-fprint0" | "-fls" | "-fprintf" => {
                let formatted = arg == "-fprintf";
                index += 1;
                found.paths.push(PathArgument {
                    value: argv.get(index)?,
                    access: PathAccess::Write,
                });
                if formatted {
                    index += 1;
                    argv.get(index)?;
                }
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

fn git_paths(argv: &[String]) -> Option<Vec<PathArgument<'_>>> {
    let mut paths = Vec::new();
    let mut index = 1;
    while let Some(arg) = argv.get(index).filter(|arg| arg.starts_with('-')) {
        let value = match arg.as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" => {
                index += 1;
                argv.get(index)?.as_str()
            }
            "--no-pager"
            | "--paginate"
            | "-P"
            | "-p"
            | "--bare"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs"
            | "--no-optional-locks" => {
                index += 1;
                continue;
            }
            _ if arg.starts_with("-C") || arg.starts_with("-c") => &arg[2..],
            _ if arg.starts_with("--git-dir=") || arg.starts_with("--work-tree=") => {
                arg.split_once('=')?.1
            }
            _ => return None,
        };
        if value.contains('\0') {
            return None;
        }
        // 配置值也可能是 include.path 一类读取来源，不能把整个 -c 操作数当消息。
        let value = if arg.starts_with("-c") {
            value.split_once('=').map_or(value, |(_, value)| value)
        } else {
            value
        };
        paths.push(PathArgument {
            value,
            access: PathAccess::Read,
        });
        index += 1;
    }
    if argv.get(index)? != "commit" {
        return None;
    }
    paths.extend(option_paths(
        &argv[index..],
        &GIT_COMMIT,
        Positionals::AllPaths,
    )?);
    Some(paths)
}

const LS: Spec = Spec {
    short: |flag| match flag {
        'I' | 'T' | 'w' if !cfg!(target_os = "macos") => Some(Operand::Literal),
        '1' | 'A' | 'B' | 'C' | 'F' | 'G' | 'H' | 'L' | 'N' | 'Q' | 'R' | 'S' | 'U' | 'X' | 'Z'
        | 'a' | 'b' | 'c' | 'd' | 'f' | 'g' | 'h' | 'i' | 'k' | 'l' | 'm' | 'n' | 'o' | 'p'
        | 'q' | 'r' | 's' | 't' | 'u' | 'v' | 'x' | 'I' | 'T' | 'w' => Some(Operand::None),
        _ => None,
    },
    long: |flag| match flag {
        "color" | "hyperlink" => Some(Operand::Optional),
        "block-size" | "format" | "hide" | "ignore" | "indicator-style" | "quoting-style"
        | "sort" | "tabsize" | "time" | "time-style" | "width" => Some(Operand::Literal),
        "all"
        | "almost-all"
        | "author"
        | "classify"
        | "context"
        | "directory"
        | "escape"
        | "file-type"
        | "full-time"
        | "group-directories-first"
        | "human-readable"
        | "ignore-backups"
        | "inode"
        | "literal"
        | "no-group"
        | "numeric-uid-gid"
        | "quote-name"
        | "recursive"
        | "reverse"
        | "si"
        | "size"
        | "dereference"
        | "dereference-command-line"
        | "help"
        | "version" => Some(Operand::None),
        _ => None,
    },
    short_operands: ShortOperands::AttachedOrNext,
    digits_are_context: false,
    positionals: PathAccess::Metadata,
};

const STAT: Spec = Spec {
    short: |flag| match flag {
        'c' => Some(Operand::Literal),
        'f' | 't' if cfg!(target_os = "macos") => Some(Operand::Literal),
        'f' | 't' => Some(Operand::None),
        'F' | 'H' | 'L' | 'n' | 'q' | 'r' | 's' | 'x' => Some(Operand::None),
        _ => None,
    },
    long: |flag| match flag {
        "format" | "printf" => Some(Operand::Literal),
        "cached" => Some(Operand::Optional),
        "dereference" | "file-system" | "terse" | "help" | "version" => Some(Operand::None),
        _ => None,
    },
    short_operands: ShortOperands::AttachedOrNext,
    digits_are_context: false,
    positionals: PathAccess::Metadata,
};

const TOUCH: Spec = Spec {
    short: |flag| match flag {
        'r' => Some(Operand::MetadataPath),
        'd' | 't' => Some(Operand::Literal),
        'a' | 'c' | 'f' | 'h' | 'm' => Some(Operand::None),
        _ => None,
    },
    long: |flag| match flag {
        "reference" => Some(Operand::MetadataPath),
        "date" | "time" => Some(Operand::Literal),
        "no-create" | "no-dereference" | "help" | "version" => Some(Operand::None),
        _ => None,
    },
    short_operands: ShortOperands::AttachedOrNext,
    digits_are_context: false,
    positionals: PathAccess::MetadataWrite,
};

const MKDIR: Spec = Spec {
    short: |flag| match flag {
        'm' => Some(Operand::Literal),
        'p' | 'v' => Some(Operand::None),
        _ => None,
    },
    long: |flag| match flag {
        "mode" => Some(Operand::Literal),
        "context" => Some(Operand::Optional),
        "parents" | "verbose" | "help" | "version" => Some(Operand::None),
        _ => None,
    },
    short_operands: ShortOperands::AttachedOrNext,
    digits_are_context: false,
    positionals: PathAccess::MetadataWrite,
};

const CHMOD: Spec = Spec {
    short: |flag| match flag {
        'R' | 'c' | 'f' | 'v' | 'H' | 'L' | 'P' | 'h' => Some(Operand::None),
        _ => None,
    },
    long: |flag| match flag {
        "reference" => Some(Operand::MetadataReference),
        "recursive" | "changes" | "silent" | "quiet" | "verbose" | "preserve-root"
        | "no-preserve-root" | "help" | "version" => Some(Operand::None),
        _ => None,
    },
    short_operands: ShortOperands::AttachedOrNext,
    digits_are_context: false,
    positionals: PathAccess::MetadataWrite,
};

const GIT_COMMIT: Spec = Spec {
    short: |flag| match flag {
        'm' => Some(Operand::Literal),
        'F' => Some(Operand::Path),
        'a' | 'q' | 'v' | 's' => Some(Operand::None),
        _ => None,
    },
    long: |flag| match flag {
        "message" | "author" | "date" | "cleanup" => Some(Operand::Literal),
        "file" | "pathspec-from-file" => Some(Operand::Path),
        "all"
        | "quiet"
        | "verbose"
        | "signoff"
        | "amend"
        | "no-edit"
        | "allow-empty"
        | "allow-empty-message"
        | "no-verify"
        | "pathspec-file-nul" => Some(Operand::None),
        _ => None,
    },
    short_operands: ShortOperands::AttachedOrNext,
    digits_are_context: false,
    positionals: PathAccess::Read,
};

const GREP: Spec = Spec {
    short: grep_short,
    long: grep_long,
    short_operands: ShortOperands::AttachedOrNext,
    digits_are_context: true,
    positionals: PathAccess::Read,
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
    positionals: PathAccess::Read,
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
    positionals: PathAccess::Metadata,
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
    positionals: PathAccess::Metadata,
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

/// `tree` 的选项角色只此一份，读 `shell` 里同一张表；`-R` 无操作数但会写，
/// 这里仍返回 `None`，由 `path_arguments` 对 `-R` 整体退回逐参数检查。
fn tree_operand(option: crate::shell::TreeOption) -> Operand {
    match option {
        crate::shell::TreeOption::Flag | crate::shell::TreeOption::WriteFlag => Operand::None,
        crate::shell::TreeOption::Value => Operand::Literal,
        crate::shell::TreeOption::Path => Operand::Path,
        crate::shell::TreeOption::Write => Operand::WritePath,
    }
}

fn tree_short(ch: char) -> Option<Operand> {
    crate::shell::tree_short_option(ch).map(tree_operand)
}

fn tree_long(name: &str) -> Option<Operand> {
    crate::shell::tree_long_option(name).map(tree_operand)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_paths(words: &[&str], expected: Option<&[&str]>) {
        let argv: Vec<String> = words.iter().map(|word| (*word).to_string()).collect();
        let actual = path_arguments(&argv).map(|paths| {
            paths
                .into_iter()
                .map(|path| path.value.to_string())
                .collect::<Vec<_>>()
        });
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
        assert_paths(&["tree", "-o", "out.txt", "."], Some(&["out.txt", "."]));
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
        assert!(!metadata(&["tree", "--fromtabfile", ".git/config"]));
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
