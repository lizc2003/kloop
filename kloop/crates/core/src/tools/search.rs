//! Built-in `grep` and `glob` tools on ripgrep's own crates
//! (`grep-searcher`/`grep-regex`/`ignore`) — no external binary. Output and
//! parser shapes follow cc where that does not weaken kloop's safety boundary.
//! Deliberate differences: `glob` respects .gitignore, skips VCS internals, and
//! returns newest-first; both tools hide read-denied/sensitive paths and keep
//! model-facing text below the inline History budget.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::BinaryDetection;
use grep_searcher::Searcher;
use grep_searcher::SearcherBuilder;
use grep_searcher::Sink;
use grep_searcher::SinkContext;
use grep_searcher::SinkMatch;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use serde_json::Value;

use std::sync::Arc;

use crate::permissions::Permissions;

/// cc caps matched lines at 500 columns (`rg --max-columns 500`) so
/// minified/base64 blobs cannot flood the context; we truncate instead of
/// omitting the line entirely like rg does.
const MAX_LINE_CHARS: usize = 500;
/// cc's DEFAULT_HEAD_LIMIT.
const GREP_DEFAULT_LIMIT: usize = 250;
/// cc's Glob maxResults.
const GLOB_LIMIT: usize = 100;
/// Keep model-facing search text below History's generic 8k offload threshold.
const SEARCH_CONTENT_CHARS: usize = 7_000;
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
    display_base: PathBuf,
    absolute_paths: bool,
    glob: Option<String>,
    file_type: Option<String>,
    mode: OutputMode,
    case_insensitive: bool,
    line_numbers: bool,
    only_matching: bool,
    before: usize,
    after: usize,
    /// 0 = unlimited.
    limit: usize,
    offset: usize,
    multiline: bool,
}

impl GrepArgs {
    fn parse(input: &Value, cwd: &Path) -> Result<Self> {
        let pattern = crate::tools::str_arg(input, "pattern", "grep")?.to_string();
        let mode = match optional_string(input, "output_mode", "grep")?
            .unwrap_or("files_with_matches")
        {
            "files_with_matches" => OutputMode::FilesWithMatches,
            "content" => OutputMode::Content,
            "count" => OutputMode::Count,
            other => bail!(
                "grep: unknown output_mode '{other}' (expected files_with_matches, content or count)"
            ),
        };
        let supplied_path = optional_string(input, "path", "grep")?;
        let root = supplied_path
            .map(|path| crate::tools::resolve_path(cwd, path))
            .unwrap_or_else(|| cwd.to_path_buf());
        let around = integer_arg(input, "context", "grep")?.or(integer_arg(input, "-C", "grep")?);
        Ok(GrepArgs {
            pattern,
            root,
            display_base: cwd.to_path_buf(),
            absolute_paths: supplied_path.is_some_and(|path| Path::new(path).is_absolute()),
            glob: optional_string(input, "glob", "grep")?
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            file_type: optional_string(input, "type", "grep")?
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            mode,
            case_insensitive: optional_bool(input, "-i", "grep")?.unwrap_or(false),
            line_numbers: optional_bool(input, "-n", "grep")?.unwrap_or(true),
            only_matching: optional_bool(input, "-o", "grep")?.unwrap_or(false),
            before: around.or(integer_arg(input, "-B", "grep")?).unwrap_or(0),
            after: around.or(integer_arg(input, "-A", "grep")?).unwrap_or(0),
            limit: integer_arg(input, "head_limit", "grep")?.unwrap_or(GREP_DEFAULT_LIMIT),
            offset: integer_arg(input, "offset", "grep")?.unwrap_or(0),
            multiline: optional_bool(input, "multiline", "grep")?.unwrap_or(false),
        })
    }
}

fn optional_string<'a>(input: &'a Value, key: &str, tool: &str) -> Result<Option<&'a str>> {
    let value = &input[key];
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(Some)
        .with_context(|| format!("{tool}: {key} must be a string"))
}

fn optional_bool(input: &Value, key: &str, tool: &str) -> Result<Option<bool>> {
    let value = &input[key];
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_bool()
        .map(Some)
        .with_context(|| format!("{tool}: {key} must be a boolean"))
}

fn integer_arg(input: &Value, key: &str, tool: &str) -> Result<Option<usize>> {
    let value = &input[key];
    if value.is_null() {
        return Ok(None);
    }
    let parsed = if let Some(value) = value.as_u64() {
        value
    } else if let Some(value) = value.as_i64() {
        if value < 0 {
            bail!("{tool}: {key} must be a whole number of 0 or more, got {value}");
        }
        value as u64
    } else if let Some(value) = value.as_f64() {
        if value < 0.0 || value.fract() != 0.0 {
            bail!("{tool}: {key} must be a whole number of 0 or more, got {value}");
        }
        value as u64
    } else if let Some(value) = value.as_str() {
        value.trim().parse::<u64>().with_context(|| {
            format!("{tool}: {key} must be a whole number of 0 or more, got {value:?}")
        })?
    } else {
        bail!("{tool}: {key} must be a whole number of 0 or more");
    };
    usize::try_from(parsed)
        .map(Some)
        .with_context(|| format!("{tool}: {key} is too large"))
}

pub async fn grep_tool(input: &Value, cwd: &Path, perms: Arc<Permissions>) -> Result<String> {
    let args = GrepArgs::parse(input, cwd)?;
    tokio::task::spawn_blocking(move || run_grep(&args, &perms))
        .await
        .map_err(|e| anyhow!("grep: worker panicked: {e}"))?
}

pub async fn glob_tool(
    input: &Value,
    cwd: &Path,
    program_result: Option<&crate::tools::ProgramResultSink>,
    perms: Arc<Permissions>,
) -> Result<String> {
    let pattern = crate::tools::str_arg(input, "pattern", "glob")?.to_string();
    let supplied_path = optional_string(input, "path", "glob")?;
    let root = supplied_path
        .map(|path| crate::tools::resolve_path(cwd, path))
        .unwrap_or_else(|| cwd.to_path_buf());
    let display_base = cwd.to_path_buf();
    let absolute_paths = supplied_path.is_some_and(|path| Path::new(path).is_absolute());
    let (text, paths) = tokio::task::spawn_blocking(move || {
        run_glob(&pattern, &root, &display_base, absolute_paths, &perms)
    })
    .await
    .map_err(|e| anyhow!("glob: worker panicked: {e}"))??;
    // A program gets the count-capped path array. The model gets the same list
    // as text, with an additional character cap to keep it inline in History.
    if let Some(slot) = program_result {
        *slot.lock().unwrap() = Some(Value::Array(paths.into_iter().map(Value::String).collect()));
    }
    Ok(text)
}

fn run_grep(args: &GrepArgs, perms: &Permissions) -> Result<String> {
    run_grep_with_hook(args, perms, |_| {})
}

fn run_grep_with_hook(
    args: &GrepArgs,
    perms: &Permissions,
    mut after_open: impl FnMut(&Path),
) -> Result<String> {
    if !args.root.exists() {
        bail!("grep: path does not exist: {}", args.root.display());
    }
    let matcher = build_matcher(args)?;
    let mut searcher = build_searcher(args);
    let single_file = args.root.is_file();

    // Content mode can stop early: collect one line past the requested
    // window to learn whether it was truncated. Files mode must see every
    // match (sorted before slicing); count mode reports exact totals.
    let stop_at = match (args.mode, args.limit) {
        (OutputMode::Content, limit) if limit > 0 => {
            Some(args.offset.saturating_add(limit).saturating_add(1))
        }
        _ => None,
    };

    let mut lines: Vec<String> = Vec::new();
    let mut files: Vec<(SystemTime, String)> = Vec::new();
    let mut counts: Vec<(String, u64)> = Vec::new();
    let started = Instant::now();
    let mut timed_out = false;
    let mut hidden = 0usize;

    for entry in build_walk(&args.root, args.glob.as_deref(), args.file_type.as_deref())? {
        if started.elapsed() > SEARCH_BUDGET {
            timed_out = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let display = if single_file && args.mode == OutputMode::Content {
            String::new()
        } else {
            output_path(&args.display_base, args.absolute_paths, entry.path())
        };
        let prepared = match super::fs::prepare_read_target(entry.path(), &display) {
            Ok(prepared) => prepared,
            Err(error) => {
                if error.downcast_ref::<super::fs::UnsafeHardLink>().is_some() {
                    hidden += 1;
                }
                continue;
            }
        };
        let (resolved, file) = prepared.into_parts();
        after_open(entry.path());
        // Permission and search consume facts bound to the same opened inode.
        // A leaf/parent swap after the walker cannot redirect search_reader.
        if perms.read_path_blocked_with_resolved_path(entry.path(), Some(&resolved)) {
            hidden += 1;
            continue;
        }
        let modified = file
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        match args.mode {
            OutputMode::Content => {
                let sink = ContentSink {
                    display: &display,
                    matcher: &matcher,
                    only_matching: args.only_matching,
                    show_numbers: args.line_numbers,
                    stop_at,
                    lines: &mut lines,
                };
                let _ = searcher.search_reader(&matcher, file, sink);
                if stop_at.is_some_and(|s| lines.len() >= s) {
                    break;
                }
            }
            OutputMode::FilesWithMatches => {
                let mut found = false;
                let sink = FoundSink { found: &mut found };
                let _ = searcher.search_reader(&matcher, file, sink);
                if found {
                    files.push((modified, display));
                }
            }
            OutputMode::Count => {
                let mut count = 0;
                let sink = CountSink { count: &mut count };
                let _ = searcher.search_reader(&matcher, file, sink);
                if count > 0 {
                    counts.push((display, count));
                }
            }
        }
    }

    let mut out = match args.mode {
        OutputMode::Content => {
            let total = lines.len();
            let truncated = stop_at.is_some_and(|stop| total >= stop);
            let shown = page(lines, args.offset, args.limit);
            let pagination = pagination(
                truncated.then_some(args.limit),
                (args.offset > 0).then_some(args.offset),
            );
            let base = if shown.is_empty() {
                if args.offset > 0 && total > 0 {
                    "No entries at this offset".to_string()
                } else {
                    "No matches found".to_string()
                }
            } else {
                shown.join("\n")
            };
            append_content_pagination(base, pagination.as_deref())
        }
        OutputMode::FilesWithMatches => {
            // cc sorts newest-first so the freshest files survive the cap.
            // Cached: the key does an mtime() syscall + name clone, so compute
            // it once per file, not O(n log n) times.
            files.sort_by_cached_key(|(modified, name)| {
                (std::cmp::Reverse(*modified), name.clone())
            });
            let total = files.len();
            let shown = page(
                files.into_iter().map(|(_, display)| display).collect(),
                args.offset,
                args.limit,
            );
            let truncated = args.limit > 0 && total > args.offset.saturating_add(shown.len());
            let pagination = pagination(
                truncated.then_some(args.limit),
                (args.offset > 0).then_some(args.offset),
            );
            if shown.is_empty() {
                if args.offset > 0 && total > 0 {
                    format!(
                        "No entries at this offset. [Showing results with pagination = {}]",
                        pagination.unwrap_or_else(|| format!("offset: {}", args.offset))
                    )
                } else {
                    "No files found".to_string()
                }
            } else {
                let detail = pagination
                    .as_deref()
                    .map(|value| format!(" {value}"))
                    .unwrap_or_default();
                format!(
                    "Found {}{detail}\n{}",
                    plural(shown.len(), "file"),
                    shown.join("\n")
                )
            }
        }
        OutputMode::Count => {
            let total_matches: u64 = counts.iter().map(|(_, count)| count).sum();
            let total_files = counts.len();
            let shown = page(
                counts
                    .iter()
                    .map(|(display, count)| format!("{display}:{count}"))
                    .collect(),
                args.offset,
                args.limit,
            );
            let truncated = args.limit > 0 && total_files > args.offset.saturating_add(shown.len());
            let pagination = pagination(
                truncated.then_some(args.limit),
                (args.offset > 0).then_some(args.offset),
            );
            let base = if shown.is_empty() {
                if total_matches > 0 {
                    "No entries at this offset".to_string()
                } else {
                    "No matches found".to_string()
                }
            } else {
                shown.join("\n")
            };
            let suffix = pagination
                .map(|value| format!(" with pagination = {value}"))
                .unwrap_or_default();
            format!(
                "{base}\n\nFound {} across {}.{suffix}",
                plural(total_matches as usize, "total occurrence"),
                plural(total_files, "file"),
            )
        }
    };
    out = cap_search_output(out);
    if hidden > 0 {
        out.push_str(&hidden_note(hidden));
    }
    if timed_out {
        out.push_str(
            "\n\n[search stopped after 20s; results are partial — narrow the path or pattern]",
        );
    }
    Ok(out)
}

/// Appended when read-blocked files were skipped, so the model knows the
/// result is filtered rather than empty (never silently pretend nothing exists).
fn hidden_note(n: usize) -> String {
    format!("\n\n[{} hidden by deny/sensitive rules]", plural(n, "path"))
}

/// Returns the character-capped model text and the 100-entry path list a
/// code-mode program receives.
fn run_glob(
    pattern: &str,
    root: &Path,
    display_base: &Path,
    absolute_paths: bool,
    perms: &Permissions,
) -> Result<(String, Vec<String>)> {
    if !root.is_dir() {
        bail!("glob: not a directory: {}", root.display());
    }
    let started = Instant::now();
    let mut timed_out = false;
    let mut hidden = 0usize;
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    let glob = (!pattern.is_empty()).then_some(pattern);
    for entry in build_walk(root, glob, None)? {
        if started.elapsed() > SEARCH_BUDGET {
            timed_out = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_some_and(|t| t.is_file()) {
            // Same read-accessibility filter as grep: a listed path is a read.
            if perms.read_path_blocked(entry.path()) {
                hidden += 1;
                continue;
            }
            files.push((
                entry.path().to_path_buf(),
                output_path(display_base, absolute_paths, entry.path()),
            ));
        }
    }
    // Newest first: the cap must keep the most recently touched files.
    // Cached so the mtime() syscall + name clone runs once per file.
    files.sort_by_cached_key(|(path, name)| (std::cmp::Reverse(mtime(path)), name.clone()));
    let total = files.len();
    let paths: Vec<String> = files.into_iter().take(GLOB_LIMIT).map(|(_, d)| d).collect();
    let shown_in_text = complete_items_in_capped_output(&paths);
    let mut out = if paths.is_empty() {
        "No files found".to_string()
    } else {
        paths.join("\n")
    };
    out = cap_search_output(out);
    if total > GLOB_LIMIT {
        let more = total - shown_in_text;
        out.push_str(&format!(
            "\n(Showing {shown_in_text} of {total} matching files; {more} more are not listed. Narrow the pattern or path to see the rest.)"
        ));
    }
    if hidden > 0 {
        out.push_str(&hidden_note(hidden));
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
        for glob in expand_glob_filters(glob) {
            over.add(glob)
                .with_context(|| format!("invalid glob pattern '{glob}'"))?;
        }
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

/// Match rg's path presentation: an explicitly absolute search path yields
/// absolute results; omitted or relative paths stay relative to the agent cwd.
fn expand_glob_filters(glob: &str) -> Vec<&str> {
    glob.split_whitespace()
        .flat_map(|part| {
            if part.contains('{') && part.contains('}') {
                vec![part]
            } else {
                part.split(',').filter(|value| !value.is_empty()).collect()
            }
        })
        .collect()
}

fn output_path(display_base: &Path, absolute_paths: bool, path: &Path) -> String {
    if absolute_paths {
        return path.to_string_lossy().to_string();
    }
    let relative = path.strip_prefix(display_base).unwrap_or(path);
    let display = relative.to_string_lossy();
    display.strip_prefix("./").unwrap_or(&display).to_string()
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

fn pagination(limit: Option<usize>, offset: Option<usize>) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(limit) = limit {
        parts.push(format!("limit: {limit}"));
    }
    if let Some(offset) = offset {
        parts.push(format!("offset: {offset}"));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn append_content_pagination(mut content: String, pagination: Option<&str>) -> String {
    if let Some(pagination) = pagination {
        content.push_str(&format!(
            "\n\n[Showing results with pagination = {pagination}]"
        ));
    }
    content
}

fn plural(n: usize, word: &str) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{n} {word}{s}")
}

fn complete_items_in_capped_output(items: &[String]) -> usize {
    let total_chars = items
        .iter()
        .enumerate()
        .fold(0usize, |total, (index, item)| {
            total
                .saturating_add(usize::from(index > 0))
                .saturating_add(item.chars().count())
        });
    if total_chars <= SEARCH_CONTENT_CHARS {
        return items.len();
    }

    let mut chars = 0usize;
    let mut complete = 0usize;
    for (index, item) in items.iter().enumerate() {
        chars = chars
            .saturating_add(usize::from(index > 0))
            .saturating_add(item.chars().count());
        // cap_search_output drops the final line whenever the prefix itself is
        // truncated, so a path counts only if its following newline fits too.
        if chars >= SEARCH_CONTENT_CHARS {
            break;
        }
        complete += 1;
    }
    complete
}

fn cap_search_output(output: String) -> String {
    let (prefix, truncated) = crate::tools::char_prefix(&output, SEARCH_CONTENT_CHARS);
    if truncated {
        let prefix = prefix
            .rfind('\n')
            .map(|end| &prefix[..end])
            .unwrap_or(prefix);
        format!(
            "{prefix}\n\n[search output truncated at {SEARCH_CONTENT_CHARS} characters; narrow the path or pattern]"
        )
    } else {
        output
    }
}

fn clip(text: &str) -> String {
    let (head, truncated) = crate::tools::char_prefix(text, MAX_LINE_CHARS);
    if truncated {
        format!("{head} [line truncated]")
    } else {
        head.to_string()
    }
}

fn line_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    clip(text.trim_end_matches(['\r', '\n']))
}

fn fmt_line(display: &str, n: Option<u64>, show_numbers: bool, sep: char, text: &str) -> String {
    match (display.is_empty(), n.filter(|_| show_numbers)) {
        (true, Some(n)) => format!("{n}{sep}{text}"),
        (true, None) => text.to_string(),
        (false, Some(n)) => format!("{display}{sep}{n}{sep}{text}"),
        (false, None) => format!("{display}{sep}{text}"),
    }
}

/// Content mode: match lines as `path:n:text`, context lines as
/// `path-n-text` (rg's separators).
struct ContentSink<'a> {
    display: &'a str,
    matcher: &'a RegexMatcher,
    only_matching: bool,
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

    fn matched(&mut self, _: &Searcher, matched: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.only_matching {
            let mut keep_searching = true;
            let line_number = matched.line_number();
            let _ = self.matcher.find_iter(matched.bytes(), |found| {
                if found.start() == found.end() {
                    return true;
                }
                let text = line_text(&matched.bytes()[found.start()..found.end()]);
                let out = fmt_line(self.display, line_number, self.show_numbers, ':', &text);
                keep_searching = self.push(out);
                keep_searching
            });
            return Ok(keep_searching);
        }

        // A multiline match spans several lines; number them from the first.
        let mut line_number = matched.line_number();
        for line in matched.lines() {
            let out = fmt_line(
                self.display,
                line_number,
                self.show_numbers,
                ':',
                &line_text(line),
            );
            if !self.push(out) {
                return Ok(false);
            }
            line_number = line_number.map(|value| value + 1);
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

    /// No gating: `allow_all` filters nothing, so every existing case sees
    /// unfiltered output.
    fn no_gate() -> Arc<Permissions> {
        Arc::new(Permissions::allow_all())
    }

    async fn grep(input: Value) -> Result<String> {
        grep_tool(&input, Path::new("."), no_gate()).await
    }

    async fn glob(input: Value) -> Result<String> {
        glob_tool(&input, Path::new("."), None, no_gate()).await
    }

    /// Strip the tree root prefix so assertions read relative.
    fn rel(out: &str, tree: &Tree) -> String {
        out.replace(&format!("{}\\", tree.path()), "")
            .replace(&format!("{}/", tree.path()), "")
            .replace('\\', "/")
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
    async fn grep_contract_supports_context_alias_only_matching_and_single_file_paths() {
        let t = Tree::new("grep-contract", &[("a.txt", "alpha\nBeta alpha\ngamma\n")]);
        let file = t.root.join("a.txt");

        let out = grep(json!({
            "pattern": "alpha",
            "path": file,
            "output_mode": "content",
            "context": 1,
            "-n": true
        }))
        .await
        .unwrap();
        assert_eq!(out, "1:alpha\n2:Beta alpha\n3-gamma");

        let out = grep(json!({
            "pattern": "alpha",
            "path": t.root.join("a.txt"),
            "output_mode": "content",
            "-o": true
        }))
        .await
        .unwrap();
        assert_eq!(out, "1:alpha\n2:alpha");
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
        assert_eq!(
            count,
            "No matches found\n\nFound 0 total occurrences across 0 files."
        );
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

        let out = grep(json!({"pattern": "needle", "path": t.path(), "glob": "*.rs,*.py"}))
            .await
            .unwrap();
        let out = rel(&out, &t);
        assert!(out.contains("a.rs") && out.contains("b.py") && !out.contains("c.txt"));

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

    #[test]
    fn empty_grep_filters_are_omitted_without_trimming() {
        let empty = GrepArgs::parse(
            &json!({"pattern": "needle", "glob": "", "type": ""}),
            Path::new("/workspace"),
        )
        .unwrap();
        assert_eq!(empty.glob, None);
        assert_eq!(empty.file_type, None);

        let whitespace = GrepArgs::parse(
            &json!({"pattern": "needle", "glob": " ", "type": " "}),
            Path::new("/workspace"),
        )
        .unwrap();
        assert_eq!(whitespace.glob.as_deref(), Some(" "));
        assert_eq!(whitespace.file_type.as_deref(), Some(" "));
    }

    #[tokio::test]
    async fn empty_grep_filters_match_omitted_filters() {
        let t = Tree::new(
            "empty-filter",
            &[
                ("a.rs", "needle\n"),
                ("b.py", "needle\n"),
                ("c.txt", "needle\n"),
            ],
        );
        let unfiltered = grep(json!({"pattern": "needle", "path": t.path()}))
            .await
            .unwrap();
        let empty_glob = grep(json!({"pattern": "needle", "path": t.path(), "glob": ""}))
            .await
            .unwrap();
        let empty_type = grep(json!({"pattern": "needle", "path": t.path(), "type": ""}))
            .await
            .unwrap();

        assert_eq!(empty_glob, unfiltered);
        assert_eq!(empty_type, unfiltered);
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
            out.contains("[Showing results with pagination = limit: 3]"),
            "got: {out}"
        );

        let out = grep(json!({"pattern": "needle", "path": t.path(), "output_mode": "content", "head_limit": 3, "offset": 3}))
            .await
            .unwrap();
        assert!(out.contains("needle 4") && out.contains("needle 6") && !out.contains("needle 7"));
        assert!(out.contains("[Showing results with pagination = limit: 3, offset: 3]"));

        // head_limit 0 = unlimited, no truncation note.
        let out = grep(json!({"pattern": "needle", "path": t.path(), "output_mode": "content", "head_limit": 0}))
            .await
            .unwrap();
        assert!(out.contains("needle 10") && !out.contains("truncated"));

        let max = usize::MAX.to_string();
        let out = grep(json!({
            "pattern": "needle",
            "path": t.path(),
            "output_mode": "content",
            "head_limit": max,
            "offset": usize::MAX.to_string()
        }))
        .await
        .unwrap();
        assert!(out.starts_with("No entries at this offset"), "{out}");
        assert!(out.contains(&format!("offset: {}", usize::MAX)), "{out}");
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

        for input in [
            json!({"pattern": "x", "head_limit": -1}),
            json!({"pattern": "x", "offset": 1.5}),
            json!({"pattern": "x", "-i": "true"}),
            json!({"pattern": "x", "glob": 7}),
        ] {
            let err = grep(input).await.unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains("must be a whole number")
                    || message.contains("must be a boolean")
                    || message.contains("must be a string"),
                "{message}"
            );
        }
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
        assert!(
            out.lines().all(|line| line.starts_with(t.path())),
            "an absolute root yields absolute paths: {out}"
        );
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

        let out = glob(json!({"pattern": "", "path": t.path()}))
            .await
            .unwrap();
        let out = rel(&out, &t);
        assert!(out.contains("new.rs") && out.contains("skip.txt") && out.contains("sub/mid.rs"));
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
            t.write(&format!("long-search-result-name-{i:03}.txt"), "x");
        }
        let out = glob(json!({"pattern": "*.txt", "path": t.path()}))
            .await
            .unwrap();
        assert!(out.lines().count() <= 103, "bounded path list plus notices");
        assert!(out.chars().count() < 8_000);
        assert!(out.contains("[search output truncated at 7000 characters"));
        let listed = out
            .split_once("\n\n[search output truncated")
            .map(|(listed, _)| listed)
            .unwrap();
        let shown = listed.lines().count();
        assert!(shown < 100, "character cap should hide some paths: {shown}");
        assert!(out.ends_with(&format!(
            "(Showing {shown} of 105 matching files; {} more are not listed. Narrow the pattern or path to see the rest.)",
            105 - shown
        )));
    }

    #[tokio::test]
    async fn glob_model_text_is_character_bounded_without_shrinking_program_array() {
        let t = Tree::new("glob-char-cap", &[]);
        let segment = "x".repeat(180);
        for index in 0..40 {
            t.write(&format!("{segment}/file-{index:03}.txt"), "x");
        }
        let sink: crate::tools::ProgramResultSink = Arc::new(std::sync::Mutex::new(None));
        let out = glob_tool(
            &json!({"pattern": "*.txt", "path": t.path()}),
            Path::new("."),
            Some(&sink),
            no_gate(),
        )
        .await
        .unwrap();

        assert!(out.chars().count() < 8_000);
        assert!(out.contains("[search output truncated at 7000 characters"));
        assert_eq!(
            sink.lock()
                .unwrap()
                .as_ref()
                .and_then(Value::as_array)
                .unwrap()
                .len(),
            40
        );
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

    /// A gate rooted at the tree so cwd-relative deny globs and the
    /// sensitive-path list resolve against the scratch files.
    fn gated(cwd: &str, deny: &[&str]) -> Arc<Permissions> {
        let rules = crate::permissions::PermissionRules {
            allow: vec![],
            deny: deny.iter().map(|s| s.to_string()).collect(),
            ask: vec![],
        };
        Arc::new(
            Permissions::new(
                crate::permissions::Mode::Manual,
                &rules,
                PathBuf::from(cwd),
                None,
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn grep_hides_read_deny_files() {
        let t = Tree::new(
            "grep-deny",
            &[
                ("src/a.rs", "token needle\n"),
                ("certs/server.pem", "needle key\n"),
                ("secret.pem", "needle key\n"),
            ],
        );
        let perms = gated(t.path(), &["read_file(**/*.pem)"]);
        let out = grep_tool(
            &json!({"pattern": "needle", "path": t.path(), "output_mode": "content"}),
            Path::new("."),
            perms,
        )
        .await
        .unwrap();
        let out = rel(&out, &t);
        assert!(out.contains("src/a.rs:1:token needle"), "got: {out}");
        assert!(!out.contains(".pem"), "deny hides both .pem files: {out}");
        assert!(
            out.contains("[2 paths hidden by deny/sensitive rules]"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn grep_hides_sensitive_files() {
        let t = Tree::new(
            "grep-sensitive",
            &[("app.rs", "needle here\n"), (".env", "API=needle\n")],
        );
        let perms = gated(t.path(), &[]);
        let out = grep_tool(
            &json!({"pattern": "needle", "path": t.path()}),
            Path::new("."),
            perms,
        )
        .await
        .unwrap();
        let out = rel(&out, &t);
        assert!(out.contains("app.rs"), "got: {out}");
        assert!(!out.contains(".env"), ".env content stays hidden: {out}");
        assert!(
            out.contains("[1 path hidden by deny/sensitive rules]"),
            "got: {out}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn grep_searches_opened_inode_when_leaf_changes_after_filter_boundary() {
        let t = Tree::new(
            "grep-open-binding",
            &[
                ("search/safe.txt", "needle-safe\n"),
                (".kloop/config.toml", "needle-secret\n"),
            ],
        );
        let search_root = t.root.join("search");
        let safe = search_root.join("safe.txt");
        let secret = t.root.join(".kloop/config.toml");
        let args = GrepArgs::parse(
            &json!({
                "pattern": "needle",
                "path": search_root,
                "output_mode": "content"
            }),
            &t.root,
        )
        .unwrap();
        let permissions = Permissions::new(
            crate::permissions::Mode::Bypass,
            &Default::default(),
            t.root.clone(),
            None,
        )
        .unwrap();
        let mut swapped = false;
        let out = run_grep_with_hook(&args, &permissions, |path| {
            if !swapped && path == safe {
                std::fs::remove_file(&safe).unwrap();
                std::os::unix::fs::symlink(&secret, &safe).unwrap();
                swapped = true;
            }
        })
        .unwrap();

        assert!(swapped, "fault hook reached the opened safe candidate");
        assert!(out.contains("needle-safe"), "{out}");
        assert!(!out.contains("needle-secret"), "{out}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_hides_hard_link_aliases_to_sensitive_content() {
        let t = Tree::new("grep-hardlink", &[(".env", "needle-secret\n")]);
        let search_root = t.root.join("search");
        std::fs::create_dir_all(&search_root).unwrap();
        std::fs::hard_link(t.root.join(".env"), search_root.join("safe.txt")).unwrap();
        let out = grep_tool(
            &json!({
                "pattern": "needle",
                "path": search_root,
                "output_mode": "content"
            }),
            &t.root,
            gated(t.path(), &[]),
        )
        .await
        .unwrap();

        assert!(!out.contains("needle-secret"), "{out}");
        assert!(
            out.contains("[1 path hidden by deny/sensitive rules]"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn grep_without_rules_is_unfiltered() {
        let t = Tree::new("grep-open", &[("a.rs", "needle\n"), ("b.pem", "needle\n")]);
        let perms = gated(t.path(), &[]);
        let out = grep_tool(
            &json!({"pattern": "needle", "path": t.path()}),
            Path::new("."),
            perms,
        )
        .await
        .unwrap();
        let out = rel(&out, &t);
        // .pem is only blocked when a read_file deny covers it; here nothing does.
        assert!(out.contains("a.rs") && out.contains("b.pem"), "got: {out}");
        assert!(!out.contains("hidden by deny"), "no note when nothing hid");
    }

    #[tokio::test]
    async fn glob_hides_read_deny_and_sensitive_paths() {
        let t = Tree::new(
            "glob-deny",
            &[
                ("keep.rs", "x"),
                ("certs/server.pem", "x"),
                (".env.local", "x"),
            ],
        );
        let perms = gated(t.path(), &["read_file(**/*.pem)"]);
        let out = glob_tool(
            &json!({"pattern": "**", "path": t.path()}),
            Path::new("."),
            None,
            perms,
        )
        .await
        .unwrap();
        let out = rel(&out, &t);
        assert!(out.contains("keep.rs"), "got: {out}");
        assert!(!out.contains(".pem"), "deny hides .pem: {out}");
        assert!(!out.contains(".env"), "sensitive hides .env.local: {out}");
        assert!(
            out.contains("[2 paths hidden by deny/sensitive rules]"),
            "got: {out}"
        );
    }
}
