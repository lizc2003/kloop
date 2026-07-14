//! Built-in `grep` and `glob` tools on ripgrep's own crates
//! (`grep-searcher`/`grep-regex`/`ignore`) — no external binary, same
//! .gitignore semantics as rg. Shapes follow cc's Grep/Glob with two
//! deliberate deviations (documented in plan 14): `glob` respects
//! .gitignore (cc's does not), and `glob` returns newest-first so the
//! 100-entry cap keeps the most recently modified files (cc slices
//! oldest-first, dropping exactly the fresh ones).

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use grep_regex::RegexMatcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::BinaryDetection;
use grep_searcher::Searcher;
use grep_searcher::SearcherBuilder;
use grep_searcher::Sink;
use grep_searcher::SinkContext;
use grep_searcher::SinkMatch;
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use serde_json::Value;

/// cc caps matched lines at 500 columns (`rg --max-columns 500`) so
/// minified/base64 blobs cannot flood the context; we truncate instead of
/// omitting the line entirely like rg does.
const MAX_LINE_CHARS: usize = 500;
/// cc's DEFAULT_HEAD_LIMIT.
const GREP_DEFAULT_LIMIT: usize = 250;
/// cc's Glob maxResults.
const GLOB_LIMIT: usize = 100;
/// cc kills rg after 20s; we stop between files and return partial results.
const SEARCH_BUDGET: Duration = Duration::from_secs(20);

#[derive(PartialEq, Clone, Copy)]
enum OutputMode {
    FilesWithMatches,
    Content,
    Count,
}

struct GrepArgs {
    pattern: String,
    root: PathBuf,
    glob: Option<String>,
    file_type: Option<String>,
    mode: OutputMode,
    case_insensitive: bool,
    line_numbers: bool,
    before: usize,
    after: usize,
    /// 0 = unlimited.
    limit: usize,
    offset: usize,
    multiline: bool,
}

impl GrepArgs {
    fn parse(input: &Value) -> Result<Self> {
        let pattern = crate::tools::str_arg(input, "pattern", "grep")?.to_string();
        let mode = match input["output_mode"].as_str().unwrap_or("files_with_matches") {
            "files_with_matches" => OutputMode::FilesWithMatches,
            "content" => OutputMode::Content,
            "count" => OutputMode::Count,
            other => bail!(
                "grep: unknown output_mode '{other}' (expected files_with_matches, content or count)"
            ),
        };
        let around = input["-C"].as_u64();
        Ok(GrepArgs {
            pattern,
            root: PathBuf::from(input["path"].as_str().unwrap_or(".")),
            glob: input["glob"].as_str().map(str::to_string),
            file_type: input["type"].as_str().map(str::to_string),
            mode,
            case_insensitive: input["-i"].as_bool().unwrap_or(false),
            line_numbers: input["-n"].as_bool().unwrap_or(true),
            before: around.or(input["-B"].as_u64()).unwrap_or(0) as usize,
            after: around.or(input["-A"].as_u64()).unwrap_or(0) as usize,
            limit: input["head_limit"]
                .as_u64()
                .unwrap_or(GREP_DEFAULT_LIMIT as u64) as usize,
            offset: input["offset"].as_u64().unwrap_or(0) as usize,
            multiline: input["multiline"].as_bool().unwrap_or(false),
        })
    }
}

pub async fn grep_tool(input: &Value) -> Result<String> {
    let args = GrepArgs::parse(input)?;
    tokio::task::spawn_blocking(move || run_grep(&args))
        .await
        .map_err(|e| anyhow!("grep: worker panicked: {e}"))?
}

pub async fn glob_tool(
    input: &Value,
    program_result: Option<&crate::tools::ProgramResultSink>,
) -> Result<String> {
    let pattern = crate::tools::str_arg(input, "pattern", "glob")?.to_string();
    let root = PathBuf::from(input["path"].as_str().unwrap_or("."));
    let (text, paths) = tokio::task::spawn_blocking(move || run_glob(&pattern, &root))
        .await
        .map_err(|e| anyhow!("glob: worker panicked: {e}"))??;
    // A program gets the path list as an array; the model gets the text.
    if let Some(slot) = program_result {
        *slot.lock().unwrap() = Some(Value::Array(paths.into_iter().map(Value::String).collect()));
    }
    Ok(text)
}

fn run_grep(args: &GrepArgs) -> Result<String> {
    if !args.root.exists() {
        bail!("grep: path does not exist: {}", args.root.display());
    }
    let matcher = build_matcher(args)?;
    let mut searcher = build_searcher(args);

    // Content mode can stop early: collect one line past the requested
    // window to learn whether it was truncated. Files mode must see every
    // match (sorted before slicing); count mode reports exact totals.
    let stop_at = match (args.mode, args.limit) {
        (OutputMode::Content, limit) if limit > 0 => Some(args.offset + limit + 1),
        _ => None,
    };

    let mut lines: Vec<String> = Vec::new();
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    let mut counts: Vec<(String, u64)> = Vec::new();
    let started = Instant::now();
    let mut timed_out = false;

    for entry in build_walk(&args.root, args.glob.as_deref(), args.file_type.as_deref())? {
        if started.elapsed() > SEARCH_BUDGET {
            timed_out = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let display = display_path(entry.path());
        match args.mode {
            OutputMode::Content => {
                let sink = ContentSink {
                    display: &display,
                    show_numbers: args.line_numbers,
                    stop_at,
                    lines: &mut lines,
                };
                let _ = searcher.search_path(&matcher, entry.path(), sink);
                if stop_at.is_some_and(|s| lines.len() >= s) {
                    break;
                }
            }
            OutputMode::FilesWithMatches => {
                let mut found = false;
                let sink = FoundSink { found: &mut found };
                let _ = searcher.search_path(&matcher, entry.path(), sink);
                if found {
                    files.push((entry.path().to_path_buf(), display));
                }
            }
            OutputMode::Count => {
                let mut count = 0;
                let sink = CountSink { count: &mut count };
                let _ = searcher.search_path(&matcher, entry.path(), sink);
                if count > 0 {
                    counts.push((display, count));
                }
            }
        }
    }

    let mut out = match args.mode {
        OutputMode::Content => {
            let truncated = stop_at.is_some_and(|s| lines.len() >= s);
            let shown = page(lines, args.offset, args.limit);
            if shown.is_empty() {
                "No matches found".to_string()
            } else {
                let mut out = shown.join("\n");
                if truncated {
                    out.push_str(&page_note(args.limit, args.offset));
                }
                out
            }
        }
        OutputMode::FilesWithMatches => {
            // cc sorts newest-first so the freshest files survive the cap.
            files.sort_by_key(|(path, name)| (std::cmp::Reverse(mtime(path)), name.clone()));
            let total = files.len();
            let shown = page(
                files.into_iter().map(|(_, d)| d).collect(),
                args.offset,
                args.limit,
            );
            if shown.is_empty() {
                "No files found".to_string()
            } else {
                let mut out = format!("Found {}\n{}", plural(total, "file"), shown.join("\n"));
                if total > args.offset + shown.len() {
                    out.push_str(&page_note(args.limit, args.offset));
                }
                out
            }
        }
        OutputMode::Count => {
            let total_matches: u64 = counts.iter().map(|(_, n)| n).sum();
            let total_files = counts.len();
            let shown = page(
                counts.iter().map(|(d, n)| format!("{d}:{n}")).collect(),
                args.offset,
                args.limit,
            );
            if shown.is_empty() {
                "No matches found".to_string()
            } else {
                let mut out = format!(
                    "{}\n\nFound {} across {}.",
                    shown.join("\n"),
                    plural(total_matches as usize, "total occurrence"),
                    plural(total_files, "file"),
                );
                if total_files > args.offset + shown.len() {
                    out.push_str(&page_note(args.limit, args.offset));
                }
                out
            }
        }
    };
    if timed_out {
        out.push_str(
            "\n\n[search stopped after 20s; results are partial — narrow the path or pattern]",
        );
    }
    Ok(out)
}

/// Returns the model-facing text and the capped list of matched paths (the
/// array a code-mode program receives). The two share the same paths — the text
/// is just those paths joined, with truncation/timeout notices appended.
fn run_glob(pattern: &str, root: &Path) -> Result<(String, Vec<String>)> {
    if !root.is_dir() {
        bail!("glob: not a directory: {}", root.display());
    }
    let started = Instant::now();
    let mut timed_out = false;
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    for entry in build_walk(root, Some(pattern), None)? {
        if started.elapsed() > SEARCH_BUDGET {
            timed_out = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_some_and(|t| t.is_file()) {
            files.push((entry.path().to_path_buf(), display_path(entry.path())));
        }
    }
    // Newest first: the cap must keep the most recently touched files.
    files.sort_by_key(|(path, name)| (std::cmp::Reverse(mtime(path)), name.clone()));
    let total = files.len();
    let paths: Vec<String> = files.into_iter().take(GLOB_LIMIT).map(|(_, d)| d).collect();
    let mut out = if paths.is_empty() {
        "No files found".to_string()
    } else {
        paths.join("\n")
    };
    if total > GLOB_LIMIT {
        out.push_str("\n(Results are truncated. Consider using a more specific path or pattern.)");
    }
    if timed_out {
        out.push_str(
            "\n[search stopped after 20s; results are partial — narrow the path or pattern]",
        );
    }
    Ok((out, paths))
}

fn build_matcher(args: &GrepArgs) -> Result<RegexMatcher> {
    let mut builder = RegexMatcherBuilder::new();
    builder.case_insensitive(args.case_insensitive);
    if args.multiline {
        // `.` crosses lines and the pattern may contain \n (cc: rg -U
        // --multiline-dotall).
        builder.dot_matches_new_line(true);
    } else {
        // Line-oriented: a pattern that could match \n is rejected here,
        // which is what the multiline flag is for.
        builder.line_terminator(Some(b'\n'));
    }
    builder
        .build(&args.pattern)
        .with_context(|| format!("grep: invalid pattern '{}' (Rust regex syntax; set multiline:true for patterns spanning lines)", args.pattern))
}

fn build_searcher(args: &GrepArgs) -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true);
    if args.mode == OutputMode::Content {
        builder
            .before_context(args.before)
            .after_context(args.after);
    }
    if args.multiline {
        builder.multi_line(true);
    }
    builder.build()
}

/// Shared walker: honors .gitignore, includes hidden files, never descends
/// into VCS internals (cc passes the same `--hidden --glob !.git` set to rg).
/// `glob` is an rg-style whitelist filter; `file_type` selects one of the
/// ignore crate's built-in type definitions.
fn build_walk(root: &Path, glob: Option<&str>, file_type: Option<&str>) -> Result<ignore::Walk> {
    let mut over = OverrideBuilder::new(root);
    for vcs in ["!.git", "!.hg", "!.svn", "!.jj"] {
        over.add(vcs).expect("static vcs globs parse");
    }
    if let Some(glob) = glob {
        over.add(glob)
            .with_context(|| format!("invalid glob pattern '{glob}'"))?;
    }
    let mut walk = WalkBuilder::new(root);
    walk.hidden(/*skip_hidden*/ false)
        .overrides(over.build().context("building glob filter")?);
    if let Some(name) = file_type {
        let mut types = ignore::types::TypesBuilder::new();
        types.add_defaults();
        if !types.definitions().iter().any(|d| d.name() == name) {
            bail!("unknown file type '{name}'; use the glob parameter instead");
        }
        types.select(name);
        walk.types(types.build().context("building type filter")?);
    }
    Ok(walk.build())
}

/// Walker paths come back rooted at the search path; "./" noise is stripped.
fn display_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    s.strip_prefix("./").unwrap_or(&s).to_string()
}

fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn page(items: Vec<String>, offset: usize, limit: usize) -> Vec<String> {
    let it = items.into_iter().skip(offset);
    if limit == 0 {
        it.collect()
    } else {
        it.take(limit).collect()
    }
}

fn page_note(limit: usize, offset: usize) -> String {
    format!(
        "\n\n[results truncated at head_limit={limit}; pass offset={} for the next page]",
        offset + limit
    )
}

fn plural(n: usize, word: &str) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{n} {word}{s}")
}

fn clip(text: &str) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(MAX_LINE_CHARS).collect();
    if chars.next().is_none() {
        head
    } else {
        format!("{head} [line truncated]")
    }
}

fn line_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    clip(text.trim_end_matches(['\r', '\n']))
}

fn fmt_line(display: &str, n: Option<u64>, show_numbers: bool, sep: char, text: &str) -> String {
    match n.filter(|_| show_numbers) {
        Some(n) => format!("{display}{sep}{n}{sep}{text}"),
        None => format!("{display}{sep}{text}"),
    }
}

/// Content mode: match lines as `path:n:text`, context lines as
/// `path-n-text` (rg's separators).
struct ContentSink<'a> {
    display: &'a str,
    show_numbers: bool,
    stop_at: Option<usize>,
    lines: &'a mut Vec<String>,
}

impl ContentSink<'_> {
    fn push(&mut self, line: String) -> bool {
        self.lines.push(line);
        self.stop_at.is_none_or(|s| self.lines.len() < s)
    }
}

impl Sink for ContentSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        // A multiline match spans several lines; number them from the first.
        let mut n = m.line_number();
        for line in m.lines() {
            let out = fmt_line(self.display, n, self.show_numbers, ':', &line_text(line));
            if !self.push(out) {
                return Ok(false);
            }
            n = n.map(|v| v + 1);
        }
        Ok(true)
    }

    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> Result<bool, Self::Error> {
        let out = fmt_line(
            self.display,
            c.line_number(),
            self.show_numbers,
            '-',
            &line_text(c.bytes()),
        );
        Ok(self.push(out))
    }
}

/// files_with_matches mode: stop at the first hit.
struct FoundSink<'a> {
    found: &'a mut bool,
}

impl Sink for FoundSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, _: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        *self.found = true;
        Ok(false)
    }
}

/// count mode: matching lines per file (rg -c semantics).
struct CountSink<'a> {
    count: &'a mut u64,
}

impl Sink for CountSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, _: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        *self.count += 1;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A scratch tree with a `.git` marker (so .gitignore is honored) —
    /// removed on drop.
    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(tag: &str, files: &[(&str, &str)]) -> Tree {
            let root =
                std::env::temp_dir().join(format!("kloop-search-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(".git")).unwrap();
            let tree = Tree { root };
            for (path, content) in files {
                tree.write(path, content);
            }
            tree
        }

        fn write(&self, path: &str, content: &str) {
            let full = self.root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
        }

        fn path(&self) -> &str {
            self.root.to_str().unwrap()
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    async fn grep(input: Value) -> Result<String> {
        grep_tool(&input).await
    }

    async fn glob(input: Value) -> Result<String> {
        glob_tool(&input, None).await
    }

    /// Strip the tree root prefix so assertions read relative.
    fn rel(out: &str, tree: &Tree) -> String {
        out.replace(&format!("{}/", tree.path()), "")
    }

    #[tokio::test]
    async fn content_mode_formats_path_line_and_text() {
        let t = Tree::new("content", &[("src/a.rs", "one\nneedle here\nthree\n")]);
        let out = grep(json!({"pattern": "needle", "path": t.path(), "output_mode": "content"}))
            .await
            .unwrap();
        assert_eq!(rel(&out, &t), "src/a.rs:2:needle here");
    }

    #[tokio::test]
    async fn content_mode_line_numbers_off() {
        let t = Tree::new("no-n", &[("a.txt", "needle\n")]);
        let out = grep(
            json!({"pattern": "needle", "path": t.path(), "output_mode": "content", "-n": false}),
        )
        .await
        .unwrap();
        assert_eq!(rel(&out, &t), "a.txt:needle");
    }

    #[tokio::test]
    async fn content_mode_context_lines_use_dash_separator() {
        let t = Tree::new("ctx", &[("a.txt", "before\nneedle\nafter\ntail\n")]);
        let out =
            grep(json!({"pattern": "needle", "path": t.path(), "output_mode": "content", "-C": 1}))
                .await
                .unwrap();
        assert_eq!(
            rel(&out, &t),
            "a.txt-1-before\na.txt:2:needle\na.txt-3-after"
        );

        // -A alone: only trailing context.
        let out =
            grep(json!({"pattern": "needle", "path": t.path(), "output_mode": "content", "-A": 1}))
                .await
                .unwrap();
        assert_eq!(rel(&out, &t), "a.txt:2:needle\na.txt-3-after");
    }

    #[tokio::test]
    async fn files_mode_is_default_with_count_header() {
        let t = Tree::new("files", &[("a.txt", "needle\n"), ("b/c.txt", "needle\n")]);
        let out = grep(json!({"pattern": "needle", "path": t.path()}))
            .await
            .unwrap();
        let out = rel(&out, &t);
        assert!(out.starts_with("Found 2 files\n"), "got: {out}");
        assert!(out.contains("a.txt"));
        assert!(out.contains("b/c.txt"));
    }

    #[tokio::test]
    async fn count_mode_reports_per_file_and_totals() {
        let t = Tree::new(
            "count",
            &[("a.txt", "x\nx\n"), ("b.txt", "x\n"), ("c.txt", "y\n")],
        );
        let out = grep(json!({"pattern": "x", "path": t.path(), "output_mode": "count"}))
            .await
            .unwrap();
        let out = rel(&out, &t);
        assert!(out.contains("a.txt:2"), "got: {out}");
        assert!(out.contains("b.txt:1"));
        assert!(!out.contains("c.txt"));
        assert!(
            out.ends_with("Found 3 total occurrences across 2 files."),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn no_matches_messages_per_mode() {
        let t = Tree::new("empty", &[("a.txt", "nothing\n")]);
        let content = grep(json!({"pattern": "zzz", "path": t.path(), "output_mode": "content"}))
            .await
            .unwrap();
        assert_eq!(content, "No matches found");
        let files = grep(json!({"pattern": "zzz", "path": t.path()}))
            .await
            .unwrap();
        assert_eq!(files, "No files found");
        let count = grep(json!({"pattern": "zzz", "path": t.path(), "output_mode": "count"}))
            .await
            .unwrap();
        assert_eq!(count, "No matches found");
    }

    #[tokio::test]
    async fn respects_gitignore_and_skips_git_dir_but_searches_hidden() {
        let t = Tree::new("ignore", &[("kept.txt", "needle\n")]);
        t.write(".gitignore", "ignored/\n");
        t.write("ignored/skip.txt", "needle\n");
        t.write(".hidden/h.txt", "needle\n");
        t.write(".git/config", "needle\n");
        let out = grep(json!({"pattern": "needle", "path": t.path()}))
            .await
            .unwrap();
        let out = rel(&out, &t);
        assert!(out.contains("kept.txt"));
        assert!(
            out.contains(".hidden/h.txt"),
            "hidden files are searched: {out}"
        );
        assert!(!out.contains("ignored/"), ".gitignore respected: {out}");
        assert!(!out.contains(".git/"), ".git never searched: {out}");
    }

    #[tokio::test]
    async fn glob_and_type_filters() {
        let t = Tree::new(
            "filter",
            &[
                ("a.rs", "needle\n"),
                ("b.py", "needle\n"),
                ("c.txt", "needle\n"),
            ],
        );
        let out = grep(json!({"pattern": "needle", "path": t.path(), "glob": "*.rs"}))
            .await
            .unwrap();
        let out = rel(&out, &t);
        assert!(out.contains("a.rs") && !out.contains("b.py") && !out.contains("c.txt"));

        let out = grep(json!({"pattern": "needle", "path": t.path(), "type": "py"}))
            .await
            .unwrap();
        let out = rel(&out, &t);
        assert!(out.contains("b.py") && !out.contains("a.rs"));

        let err = grep(json!({"pattern": "x", "path": t.path(), "type": "no-such-type"}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("unknown file type"));
    }

    #[tokio::test]
    async fn case_insensitive_flag() {
        let t = Tree::new("case", &[("a.txt", "NeEdLe\n")]);
        let miss = grep(json!({"pattern": "needle", "path": t.path(), "output_mode": "content"}))
            .await
            .unwrap();
        assert_eq!(miss, "No matches found");
        let hit = grep(
            json!({"pattern": "needle", "path": t.path(), "output_mode": "content", "-i": true}),
        )
        .await
        .unwrap();
        assert!(hit.contains("NeEdLe"));
    }

    #[tokio::test]
    async fn multiline_spans_lines_only_when_enabled() {
        let t = Tree::new("multi", &[("a.txt", "start\nfinish\n")]);
        let single = json!({"pattern": "start.finish", "path": t.path(), "output_mode": "content"});
        assert_eq!(grep(single).await.unwrap(), "No matches found");
        let out = grep(json!({"pattern": "start.finish", "path": t.path(), "output_mode": "content", "multiline": true}))
            .await
            .unwrap();
        let out = rel(&out, &t);
        assert_eq!(out, "a.txt:1:start\na.txt:2:finish");
    }

    #[tokio::test]
    async fn head_limit_and_offset_paginate_content() {
        let body: String = (1..=10).map(|i| format!("needle {i}\n")).collect();
        let t = Tree::new("page", &[("a.txt", &body)]);
        let out = grep(json!({"pattern": "needle", "path": t.path(), "output_mode": "content", "head_limit": 3}))
            .await
            .unwrap();
        let out = rel(&out, &t);
        assert!(out.contains("a.txt:1:needle 1"));
        assert!(out.contains("a.txt:3:needle 3"));
        assert!(!out.contains("needle 4"));
        assert!(
            out.contains("[results truncated at head_limit=3; pass offset=3 for the next page]"),
            "got: {out}"
        );

        let out = grep(json!({"pattern": "needle", "path": t.path(), "output_mode": "content", "head_limit": 3, "offset": 3}))
            .await
            .unwrap();
        assert!(out.contains("needle 4") && out.contains("needle 6") && !out.contains("needle 7"));

        // head_limit 0 = unlimited, no truncation note.
        let out = grep(json!({"pattern": "needle", "path": t.path(), "output_mode": "content", "head_limit": 0}))
            .await
            .unwrap();
        assert!(out.contains("needle 10") && !out.contains("truncated"));
    }

    #[tokio::test]
    async fn long_lines_are_clipped_not_omitted() {
        let long = format!("needle {}\n", "x".repeat(2000));
        let t = Tree::new("clip", &[("a.txt", &long)]);
        let out = grep(json!({"pattern": "needle", "path": t.path(), "output_mode": "content"}))
            .await
            .unwrap();
        assert!(
            out.ends_with(" [line truncated]"),
            "got tail: …{}",
            &out[out.len() - 40..]
        );
        // path + separators + 500 chars + marker stays far below the raw line.
        assert!(out.len() < 700, "got {} chars", out.len());
    }

    #[tokio::test]
    async fn binary_files_are_skipped() {
        let t = Tree::new("bin", &[("a.txt", "needle\n")]);
        std::fs::write(t.root.join("blob.bin"), b"needle\x00\x01\x02").unwrap();
        let out = grep(json!({"pattern": "needle", "path": t.path()}))
            .await
            .unwrap();
        let out = rel(&out, &t);
        assert!(
            out.contains("a.txt") && !out.contains("blob.bin"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn invalid_pattern_and_missing_path_error() {
        let err = grep(json!({"pattern": "(unclosed"})).await.unwrap_err();
        assert!(format!("{err:#}").contains("invalid pattern"));

        let err = grep(json!({"pattern": "x", "path": "/nonexistent/kloop-search"}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("does not exist"));

        let err = grep(json!({})).await.unwrap_err();
        assert!(format!("{err:#}").contains("missing required string argument 'pattern'"));

        let err = grep(json!({"pattern": "x", "output_mode": "nope"}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("unknown output_mode"));
    }

    #[tokio::test]
    async fn glob_lists_matches_newest_first_with_cap() {
        let t = Tree::new(
            "glob",
            &[("old.rs", "1"), ("sub/mid.rs", "2"), ("skip.txt", "3")],
        );
        std::thread::sleep(std::time::Duration::from_millis(30));
        t.write("new.rs", "4");
        let out = glob(json!({"pattern": "**/*.rs", "path": t.path()}))
            .await
            .unwrap();
        let out = rel(&out, &t);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.first(), Some(&"new.rs"), "newest first: {out}");
        assert_eq!(lines.len(), 3);
        assert!(!out.contains("skip.txt"));

        // Bare basename patterns match at any depth (gitignore semantics).
        let out = glob(json!({"pattern": "*.rs", "path": t.path()}))
            .await
            .unwrap();
        assert_eq!(rel(&out, &t).lines().count(), 3);
    }

    #[tokio::test]
    async fn glob_respects_gitignore_and_reports_empty() {
        let t = Tree::new("glob-ignore", &[("kept.rs", "x")]);
        t.write(".gitignore", "target/\n");
        t.write("target/gen.rs", "x");
        let out = glob(json!({"pattern": "*.rs", "path": t.path()}))
            .await
            .unwrap();
        let out = rel(&out, &t);
        assert!(
            out.contains("kept.rs") && !out.contains("target/"),
            "got: {out}"
        );

        let out = glob(json!({"pattern": "*.zig", "path": t.path()}))
            .await
            .unwrap();
        assert_eq!(out, "No files found");
    }

    #[tokio::test]
    async fn glob_truncates_past_100_files() {
        let t = Tree::new("glob-cap", &[]);
        for i in 0..105 {
            t.write(&format!("f{i:03}.txt"), "x");
        }
        let out = glob(json!({"pattern": "*.txt", "path": t.path()}))
            .await
            .unwrap();
        assert_eq!(out.lines().count(), 101, "100 paths + truncation notice");
        assert!(out
            .ends_with("(Results are truncated. Consider using a more specific path or pattern.)"));
    }

    #[tokio::test]
    async fn glob_errors() {
        let err = glob(json!({"pattern": "*.rs", "path": "/nonexistent/kloop-glob"}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("not a directory"));

        let t = Tree::new("glob-bad", &[]);
        let err = glob(json!({"pattern": "{unclosed", "path": t.path()}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("invalid glob pattern"));

        let err = glob(json!({})).await.unwrap_err();
        assert!(format!("{err:#}").contains("missing required string argument 'pattern'"));
    }
}
