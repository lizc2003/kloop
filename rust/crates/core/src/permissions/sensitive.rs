//! 敏感路径的唯一名单：凭据、agent state、VCS 内部与 shell rc。
//!
//! 原文扫描、路径判定、递归搜索、glob 拼写与沙箱凭据根都从这里派生。名单分家
//! 就会漏项——递归搜索漏掉 `.env*` 就是两份名单各自演化出来的。

/// 仓库内部：元数据访问不算读取，内容访问仍是硬边界。
pub(super) const VCS_DIR: &str = ".git";

/// 凭据与 agent state 目录：元数据访问也按读取处理。
pub(super) const SECRET_DIRS: &[&str] = &[".kloop", ".ssh", ".gnupg", ".aws"];

/// shell / git 的启动文件：进程一启动就会执行里面的内容。
pub(super) const SECRET_FILES: &[&str] = &[
    ".bashrc",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".bash_profile",
    ".profile",
    ".gitconfig",
    ".envrc",
];

/// `.env` 及其派生文件（`.env.local`、`.env.production`）按文件名前缀匹配。
pub(super) const ENV_PREFIX: &str = ".env";

/// home 下的凭据目录。`.kloop` 是 kloop 自己的 state，按配置的绝对路径而不是
/// home 下的名字定位，所以沙箱的凭据根不包含它。
pub(crate) const HOME_CREDENTIAL_DIRS: &[&str] = &[".ssh", ".gnupg", ".aws"];

/// 敏感路径的类别，决定元数据访问是否算读取。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Sensitivity {
    /// 仓库内部（`.git`）：元数据访问不是读取，内容访问仍是硬边界。
    Vcs,
    /// 凭据或 agent state：元数据也按读取处理。
    Secret,
}

/// Case-folding filesystems (macOS/Windows default): `.GIT` and `.git` are
/// the same inode, so a cased alias would slip a write past a case-sensitive
/// sensitive-path or deny check and land on the real `.git/hooks`. Fold case
/// there; compare verbatim where distinct names are distinct files.
pub(super) const FS_FOLDS_CASE: bool = cfg!(any(target_os = "macos", target_os = "windows"));

pub(super) fn fs_fold(name: &str) -> std::borrow::Cow<'_, str> {
    if FS_FOLDS_CASE {
        std::borrow::Cow::Owned(name.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

/// 目录名是否属于凭据/agent state。
pub(super) fn dir_name_is_secret(name: &str) -> bool {
    let folded = fs_fold(name);
    SECRET_DIRS.contains(&folded.as_ref())
}

/// 文件名是否属于凭据/状态：rc 文件或 `.env*`。
pub(super) fn file_name_is_secret(name: &str) -> bool {
    let folded = fs_fold(name);
    let name = folded.as_ref();
    SECRET_FILES.contains(&name) || name.starts_with(ENV_PREFIX)
}

/// 名字（不区分最后一段是不是目录）是否属于凭据/状态。
pub(super) fn name_is_secret(name: &str) -> bool {
    dir_name_is_secret(name) || file_name_is_secret(name)
}

/// 一个未引用的 glob 分量是否会命中敏感名字。不访问文件系统：只问“这个模式
/// 能不能拼出名单里的名字”。拼不出的形状（`.c*`）照旧放行，常见写法不误拦。
///
/// 只看以 `.` 开头的分量：shell 的 glob 不匹配点开头的名字，`*` 也就不会展开成
/// `.env`。`last` 为假时（中间目录）只可能落在敏感目录上。
pub(super) fn glob_component_sensitivity(pattern: &str, last: bool) -> Option<Sensitivity> {
    if !pattern.starts_with('.') || !pattern.contains(['*', '?', '[', '{']) {
        return None;
    }
    let folded = fs_fold(pattern);
    let pattern = folded.as_ref();
    if SECRET_DIRS.iter().any(|name| glob_matches(pattern, name)) {
        return Some(Sensitivity::Secret);
    }
    let vcs = glob_matches(pattern, VCS_DIR);
    if last
        && (SECRET_FILES.iter().any(|name| glob_matches(pattern, name))
            || could_match_prefix(pattern, ENV_PREFIX))
    {
        return Some(Sensitivity::Secret);
    }
    vcs.then_some(Sensitivity::Vcs)
}

/// 用规则表同一套 globset 语义做单分量匹配；解析不了的模式按“可能命中”处理，
/// 宁可多问一次也不把拼不出的形状当成安全。
fn glob_matches(pattern: &str, name: &str) -> bool {
    globset::GlobBuilder::new(pattern)
        .literal_separator(/*literal_separator*/ true)
        .case_insensitive(FS_FOLDS_CASE)
        .build()
        .map_or(true, |glob| glob.compile_matcher().is_match(name))
}

/// 模式与“以 `prefix` 开头的一族名字”是否有交集。只看字面前缀不够：`.??`
/// 长度不足、`.e[ab]*` 的字符类排除了 `n`，都拼不出 `.env`；`.e*`、`.en?`、
/// `.env.p*` 才有交集。交集按模式自动机吃前缀判定。
fn could_match_prefix(pattern: &str, prefix: &str) -> bool {
    let Some(tokens) = glob_tokens(pattern) else {
        // 花括号交替、反斜杠转义这些没建模：保留字面前缀的排除判断（`.c{…}`
        // 拼不出敏感名就放行），前缀本身还可能有交集时按可能命中处理。
        return literal_prefix_may_match(pattern, prefix);
    };
    let prefix: Vec<char> = prefix.chars().collect();
    // reachable[i][j]：前 i 个模式记号能否吃掉前缀的前 j 个字符。`*` 留在原
    // 记号上继续吃，其余记号各吃一个。
    let mut reachable = vec![vec![false; prefix.len() + 1]; tokens.len() + 1];
    reachable[0][0] = true;
    for index in 0..tokens.len() {
        for consumed in 0..=prefix.len() {
            if !reachable[index][consumed] {
                continue;
            }
            match &tokens[index] {
                GlobToken::Star => {
                    reachable[index + 1][consumed] = true;
                    if consumed < prefix.len() {
                        reachable[index][consumed + 1] = true;
                    }
                }
                GlobToken::Any if consumed < prefix.len() => {
                    reachable[index + 1][consumed + 1] = true;
                }
                GlobToken::Literal(ch) if consumed < prefix.len() && prefix[consumed] == *ch => {
                    reachable[index + 1][consumed + 1] = true;
                }
                GlobToken::Class(class)
                    if consumed < prefix.len()
                        && glob_matches(class, &prefix[consumed].to_string()) =>
                {
                    reachable[index + 1][consumed + 1] = true;
                }
                _ => {}
            }
        }
    }
    // 前缀吃完后，剩下的模式总能匹配某个后缀，交集即成立。
    (0..=tokens.len()).any(|index| reachable[index][prefix.len()])
}

/// 未建模语法的保守回退：第一个元字符之前的字面前缀与 `prefix` 是否可能相交。
fn literal_prefix_may_match(pattern: &str, prefix: &str) -> bool {
    let literal: String = pattern
        .chars()
        .take_while(|ch| !matches!(ch, '*' | '?' | '[' | '{' | '\\'))
        .collect();
    prefix.starts_with(&literal) || literal.starts_with(prefix)
}

enum GlobToken {
    Literal(char),
    Any,
    Star,
    /// 原样的字符类（含 `[`/`]`），交给 globset 判单个字符。
    Class(String),
}

fn glob_tokens(pattern: &str) -> Option<Vec<GlobToken>> {
    let mut tokens = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' => tokens.push(GlobToken::Star),
            '?' => tokens.push(GlobToken::Any),
            '[' => {
                let mut class = String::from("[");
                // 可选否定符之后，第一个 `]` 仍是字符类的成员（`[!]]` 是
                // “排除 ]”），只有已经收过成员的那个 `]` 才结束字符类。
                if let Some(negation) = chars.next_if(|ch| matches!(ch, '!' | '^')) {
                    class.push(negation);
                }
                let mut member = false;
                let mut closed = false;
                for ch in chars.by_ref() {
                    class.push(ch);
                    if ch == ']' && member {
                        closed = true;
                        break;
                    }
                    member = true;
                }
                if !closed {
                    return None;
                }
                tokens.push(GlobToken::Class(class));
            }
            '{' | '}' | '\\' => return None,
            other => tokens.push(GlobToken::Literal(other)),
        }
    }
    Some(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_components_only_flag_shapes_that_can_spell_a_secret() {
        assert_eq!(
            glob_component_sensitivity(".en?", /*last*/ true),
            Some(Sensitivity::Secret)
        );
        assert_eq!(
            glob_component_sensitivity(".env.p*", /*last*/ true),
            Some(Sensitivity::Secret)
        );
        assert_eq!(
            glob_component_sensitivity(".s*", /*last*/ false),
            Some(Sensitivity::Secret)
        );
        assert_eq!(
            glob_component_sensitivity(".gi?", /*last*/ false),
            Some(Sensitivity::Vcs)
        );
        assert_eq!(
            glob_component_sensitivity(".gi?", /*last*/ true),
            Some(Sensitivity::Vcs)
        );

        // 拼不出敏感名字的形状、不以点开头的 glob、以及没有元字符的字面量
        // 都照旧放行。
        assert_eq!(
            glob_component_sensitivity(".c*", /*last*/ true),
            None,
            "cannot reach a listed name"
        );
        assert_eq!(
            glob_component_sensitivity("*.env", /*last*/ true),
            None,
            "shell globs do not match dot names"
        );
        assert_eq!(
            glob_component_sensitivity(".env", /*last*/ true),
            None,
            "literal names are handled by the path check"
        );
        // 与 `.env*` 的交集要真的存在：长度不够或字符类排除 `n` 都不算。
        assert_eq!(glob_component_sensitivity(".??", /*last*/ true), None);
        assert_eq!(glob_component_sensitivity(".e[ab]*", /*last*/ true), None);
        assert_eq!(
            glob_component_sensitivity(".e*", /*last*/ true),
            Some(Sensitivity::Secret)
        );
        // 否定字符类里第一个 `]` 是成员，`[!]]` 不能把类提前截断。
        assert_eq!(
            glob_component_sensitivity(".e[!]]v", /*last*/ true),
            Some(Sensitivity::Secret)
        );
        // 未建模的花括号仍保留字面前缀排除：`.c{ache,onfig}` 拼不出敏感名。
        assert_eq!(
            glob_component_sensitivity(".c{ache,onfig}", /*last*/ true),
            None
        );
        assert_eq!(
            glob_component_sensitivity(".env{a,b}", /*last*/ true),
            Some(Sensitivity::Secret)
        );
    }

    #[test]
    fn secret_names_are_one_list() {
        for name in SECRET_DIRS {
            assert!(dir_name_is_secret(name));
            assert!(name_is_secret(name));
        }
        for name in SECRET_FILES {
            assert!(file_name_is_secret(name));
        }
        assert!(file_name_is_secret(".env"));
        assert!(file_name_is_secret(".env.production"));
        assert!(!file_name_is_secret("environment.rs"));
        assert!(!dir_name_is_secret(".git"));
    }
}
