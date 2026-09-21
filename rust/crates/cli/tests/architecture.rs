//! The size ratchet: a gate that stops source files from quietly growing.
//!
//! AGENTS.md carries a page of style constraints and not one of them is decided
//! by a machine, so they get bypassed silently. This is the one structural rule
//! that is: every non-test file under `rust/crates` has a code-line count, and
//! `architecture-policy.toml` says how big one may be.
//!
//! It is a ratchet, not a height bar. Splitting the twenty-three files that are
//! already over the line in one go would shred cohesive `impl`s for a number;
//! what is worth having today is that they **stop growing**. So the oversized
//! ones are frozen at their current count in `architecture-baseline.toml` and
//! may only shrink, while a file that is not on that list has to fit. Nothing is
//! ever added to the baseline — see `rewrite_the_size_baseline`.
//!
//! It lives in the top-level binary's integration tests for the same reason
//! `doc_placement.rs` does: the invariant is repository-wide and belongs to no
//! single crate. Being a test is the whole point — `make test`, `make check` and
//! all three CI platforms pick it up with no new gate configuration, so there is
//! no "CI ran it, my machine did not".
//!
//! ## What a code line is
//!
//! Three exclusions, and the gate is a perverse incentive without any one of
//! them:
//!
//! 1. **Test files are out of scope.** 72k of kloop's 135k lines are tests;
//!    counting them would put test files at the top of the list and turn this
//!    into pressure to move tests out of the crate.
//! 2. **`#[cfg(test)]` items are cut out, brace-balanced.** Truncating at the
//!    first such attribute would be easier and wrong: in `core/src/tools/mod.rs`
//!    the test module is not last, and everything after it would vanish.
//! 3. **Blank and comment-only lines do not count.** AGENTS.md asks for comments
//!    that say what the code cannot; the gate must not charge for them.
//!
//! The spread is not a rounding error: `core/src/permissions.rs` is 4177 lines
//! raw, 2233 without its tests, 1632 as code. That is why the counting rule is
//! pinned down here, in `code_lines` and the unit tests under it, rather than
//! left to whatever `wc -l` someone reaches for.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The rules. Read, never written, by anything in CI.
const POLICY: &str = "architecture-policy.toml";
/// The frozen water line for files that are already over the threshold.
const BASELINE: &str = "architecture-baseline.toml";

// ---------------------------------------------------------------------------
// the gate
// ---------------------------------------------------------------------------

#[test]
fn no_file_grows_past_the_size_ratchet() {
    let report = Report::take();
    assert!(
        report.growth.is_empty() && report.stale.is_empty(),
        "\n{}",
        report.render()
    );
}

/// Pull the baseline down to where the code actually is. It only ever lowers a
/// row or removes one: a path that is not already listed cannot be added, so
/// nobody can legalise a new oversized file by running this. Growth stays
/// something you fix in the source.
///
/// Ignored on purpose — `make arch-baseline` is the door, and moving the water
/// line is a decision that belongs in a commit message, not a side effect of
/// running the suite.
#[test]
#[ignore = "rewrites architecture-baseline.toml; run `make arch-baseline`"]
fn rewrite_the_size_baseline() {
    let report = Report::take();
    assert!(
        report.growth.is_empty(),
        "refusing to write a baseline that would bless growth:\n{}",
        report.render()
    );
    let path = workspace_root().join(BASELINE);
    let text = render_baseline(&report.tightened);
    std::fs::write(&path, text).unwrap_or_else(|error| panic!("cannot write {path:?}: {error}"));
    println!(
        "{BASELINE}: {} files on the ratchet, threshold {}",
        report.tightened.len(),
        report.threshold
    );
    for line in &report.stale {
        println!("  {line}");
    }
}

/// What one pass over the tree found.
struct Report {
    threshold: usize,
    /// Growth the gate refuses. Only editing the source clears these.
    growth: Vec<String>,
    /// The baseline is behind reality on the safe side — a file shrank, dropped
    /// under the threshold, or is gone. `make arch-baseline` clears these.
    stale: Vec<String>,
    /// What the baseline should say: every still-oversized listed path at its
    /// current count.
    tightened: BTreeMap<String, usize>,
}

impl Report {
    fn take() -> Report {
        let root = workspace_root();
        let threshold = read_policy(&root);
        let baseline = read_baseline(&root);
        let measured = measure(&root);

        let mut report = Report {
            threshold,
            growth: Vec::new(),
            stale: Vec::new(),
            tightened: BTreeMap::new(),
        };
        for (path, &frozen) in &baseline {
            match measured.get(path) {
                None => report
                    .stale
                    .push(format!("{path}: baseline {frozen}, but the file is gone")),
                Some(&now) if now > frozen => report.growth.push(format!(
                    "{path}: {now} code lines, baseline {frozen}, threshold {threshold}"
                )),
                Some(&now) if now <= threshold => report.stale.push(format!(
                    "{path}: {now} code lines, baseline {frozen} — it fits the threshold {threshold} now"
                )),
                Some(&now) => {
                    if now < frozen {
                        report
                            .stale
                            .push(format!("{path}: {now} code lines, baseline {frozen}"));
                    }
                    report.tightened.insert(path.clone(), now);
                }
            }
        }
        for (path, &now) in &measured {
            if now > threshold && !baseline.contains_key(path) {
                report.growth.push(format!(
                    "{path}: {now} code lines, threshold {threshold}, not on the ratchet"
                ));
            }
        }
        report
    }

    fn render(&self) -> String {
        let mut out = String::new();
        let threshold = self.threshold;
        if !self.growth.is_empty() {
            out.push_str("a source file grew past what it is allowed to be.\n");
            let _ = writeln!(
                out,
                "split it, or move the code out. the baseline never moves up, and a file that is \
                 not on it has to fit {threshold} code lines:\n"
            );
            for line in &self.growth {
                let _ = writeln!(out, "  {line}");
            }
            out.push('\n');
        }
        if !self.stale.is_empty() {
            out.push_str("the size baseline is behind the code (these all shrank — good):\n");
            out.push_str("run `make arch-baseline` to tighten the water line, and say so in the commit message:\n\n");
            for line in &self.stale {
                let _ = writeln!(out, "  {line}");
            }
            out.push('\n');
        }
        let _ = writeln!(
            out,
            "counting rule: non-test files under rust/crates, #[cfg(test)] items cut out, blank \
             and comment-only lines not counted. see {}.",
            file!()
        );
        out
    }
}

/// The baseline file as it should be written: a flat `path = lines` table, one
/// row per still-oversized file, sorted by path so a diff reads as a list of
/// water lines moving.
fn render_baseline(tightened: &BTreeMap<String, usize>) -> String {
    let mut out = String::from(
        "# Size ratchet water line — generated by `make arch-baseline`, not hand-edited.\n\
         #\n\
         # Every path here is over `max_code_lines` in architecture-policy.toml and is frozen\n\
         # at the count it had when it was last recorded: it may shrink, never grow. Rows are\n\
         # only ever lowered or dropped — a new file over the threshold is a gate failure, not\n\
         # a new row here, and CI never writes this file.\n\
         \n\
         [file_size]\n",
    );
    for (path, lines) in tightened {
        let _ = writeln!(out, "\"{path}\" = {lines}");
    }
    out
}

// ---------------------------------------------------------------------------
// policy and baseline files
// ---------------------------------------------------------------------------

/// The cargo workspace root, `rust/`. `CARGO_MANIFEST_DIR` is `rust/crates/cli`,
/// so the gate does not care what the working directory is.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR is rust/crates/<crate>")
        .to_path_buf()
}

/// Unknown keys are rejected rather than ignored: a rule the gate silently does
/// not read is worse than no rule. A future rule (say, which crate may depend on
/// which) gets its own section here and its own field.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    file_size: FileSizePolicy,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSizePolicy {
    max_code_lines: usize,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    file_size: BTreeMap<String, usize>,
}

fn read_policy(root: &Path) -> usize {
    let path = root.join(POLICY);
    let text = read(&path);
    let policy: Policy =
        toml::from_str(&text).unwrap_or_else(|error| panic!("cannot parse {path:?}: {error}"));
    policy.file_size.max_code_lines
}

fn read_baseline(root: &Path) -> BTreeMap<String, usize> {
    let path = root.join(BASELINE);
    let text = read(&path);
    let baseline: Baseline =
        toml::from_str(&text).unwrap_or_else(|error| panic!("cannot parse {path:?}: {error}"));
    baseline.file_size
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("cannot read {path:?}: {error}"))
}

// ---------------------------------------------------------------------------
// measuring the tree
// ---------------------------------------------------------------------------

/// Every non-test source under `rust/crates`, keyed by its slash-separated path
/// relative to the workspace root, with its code-line count.
fn measure(root: &Path) -> BTreeMap<String, usize> {
    let mut files = Vec::new();
    collect(&root.join("crates"), &mut files);
    files
        .into_iter()
        .map(|path| {
            let key = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            (key, code_lines(&read(&path)))
        })
        .collect()
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot list {dir:?}: {error}"))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for path in entries {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if name != "tests" && name != "target" && !name.starts_with('.') {
                collect(&path, out);
            }
        } else if is_production_source(name) {
            out.push(path);
        }
    }
}

/// A `.rs` file that is not itself a test file. The two naming conventions in
/// the tree are `tests.rs` (a module's own tests) and `*_tests.rs` (the parity
/// and acceptance suites); `tests/` directories are skipped by the walk.
fn is_production_source(name: &str) -> bool {
    name.ends_with(".rs") && name != "tests.rs" && !name.ends_with("_tests.rs")
}

/// Lines of this source that are neither blank, nor comment-only, nor part of a
/// `#[cfg(test)]` item.
fn code_lines(src: &str) -> usize {
    let code = scan(src);
    let excluded = test_only_lines(&code);
    (0..code.has_code.len())
        .filter(|&line| code.has_code[line] && !excluded[line])
        .count()
}

// ---------------------------------------------------------------------------
// the counting rule
// ---------------------------------------------------------------------------

/// A source file projected down to the characters that are code: comments
/// dropped, every literal collapsed to one placeholder. A brace inside a string
/// then cannot move the `#[cfg(test)]` scanner's depth, and a `//` inside one
/// cannot blank out the rest of the line — both of which happen in this tree,
/// which is full of JSON and shell fixtures.
struct Code {
    /// The projected characters, in order.
    chars: Vec<char>,
    /// `lines[i]` is the 0-based source line `chars[i]` came from.
    lines: Vec<usize>,
    /// Per source line: does it carry anything but whitespace and comments?
    has_code: Vec<bool>,
}

impl Code {
    fn push(&mut self, ch: char, line: usize) {
        self.chars.push(ch);
        self.lines.push(line);
        if !ch.is_whitespace() {
            self.has_code[line] = true;
        }
    }

    /// Mark a character that stays inside a literal and never reaches `chars`.
    fn touch(&mut self, ch: char, line: usize) {
        if !ch.is_whitespace() {
            self.has_code[line] = true;
        }
    }
}

fn scan(src: &str) -> Code {
    let s: Vec<char> = src.chars().collect();
    let mut code = Code {
        chars: Vec::with_capacity(s.len()),
        lines: Vec::with_capacity(s.len()),
        has_code: vec![false; src.matches('\n').count() + 1],
    };
    let mut i = 0;
    let mut line = 0;
    while i < s.len() {
        let c = s[i];
        let next = s.get(i + 1).copied();
        if c == '\n' {
            code.push(' ', line);
            line += 1;
            i += 1;
        } else if c == '/' && next == Some('/') {
            while i < s.len() && s[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            // Rust block comments nest.
            i += 2;
            let mut depth = 1usize;
            while i < s.len() && depth > 0 {
                let here = s[i];
                let after = s.get(i + 1).copied();
                if here == '/' && after == Some('*') {
                    depth += 1;
                    i += 2;
                } else if here == '*' && after == Some('/') {
                    depth -= 1;
                    i += 2;
                } else {
                    if here == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
        } else if let Some((body, hashes)) = raw_string_open(&s, i) {
            code.push('"', line);
            i = body;
            while i < s.len() {
                if s[i] == '"' && s[i + 1..].iter().take_while(|&&h| h == '#').count() >= hashes {
                    i += 1 + hashes;
                    break;
                }
                if s[i] == '\n' {
                    line += 1;
                } else {
                    code.touch(s[i], line);
                }
                i += 1;
            }
        } else if c == '"' {
            code.push('"', line);
            i += 1;
            while i < s.len() {
                let ch = s[i];
                if ch == '\\' {
                    if s.get(i + 1) == Some(&'\n') {
                        line += 1;
                    }
                    i += 2;
                } else if ch == '"' {
                    i += 1;
                    break;
                } else {
                    if ch == '\n' {
                        line += 1;
                    } else {
                        code.touch(ch, line);
                    }
                    i += 1;
                }
            }
        } else if c == '\'' {
            // `'a` (a lifetime or a loop label) against `'x'` and `'\n'`.
            code.push('\'', line);
            let escaped = next == Some('\\');
            if escaped {
                i += 3;
                while i < s.len() && s[i] != '\'' {
                    i += 1;
                }
                i += 1;
            } else if s.get(i + 2) == Some(&'\'') {
                i += 3;
            } else {
                i += 1;
            }
        } else {
            code.push(c, line);
            i += 1;
        }
    }
    code
}

/// `Some((body index, hash count))` when `s[i]` opens a raw string — `r"`, `r#"`,
/// `br##"`. Raw strings are the one literal an ordinary string scanner gets
/// wrong: `r"a\"` ends at that quote, where an escape would swallow it and run
/// on to the next one. `b"` and `c"` need no special case, their prefix is just
/// an identifier character followed by an ordinary string.
fn raw_string_open(s: &[char], i: usize) -> Option<(usize, usize)> {
    if i > 0 && is_ident(s[i - 1]) {
        return None;
    }
    let mut j = i;
    if s[j] == 'b' {
        j += 1;
    }
    if s.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let hashes = s[j..].iter().take_while(|&&c| c == '#').count();
    j += hashes;
    (s.get(j) == Some(&'"')).then_some((j + 1, hashes))
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Per source line: is it inside an item that only exists under `cfg(test)`?
fn test_only_lines(code: &Code) -> Vec<bool> {
    let mut excluded = vec![false; code.has_code.len()];
    let chars = &code.chars;
    let mut k = 0;
    while k + 1 < chars.len() {
        if chars[k] != '#' || chars[k + 1] != '[' {
            k += 1;
            continue;
        }
        let Some(close) = matching(chars, k + 1, '[', ']') else {
            break;
        };
        let body: String = chars[k + 2..close].iter().collect();
        if cfg_predicate(&body).is_some_and(is_test_only) {
            let end = item_end(chars, close + 1);
            let (first, last) = (code.lines[k], code.lines[end]);
            excluded[first..=last].fill(true);
            k = end + 1;
        } else {
            k = close + 1;
        }
    }
    excluded
}

/// The index of the bracket closing the one at `open`.
fn matching(chars: &[char], open: usize, opener: char, closer: char) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, &c) in chars[open..].iter().enumerate() {
        if c == opener {
            depth += 1;
        } else if c == closer {
            depth -= 1;
            if depth == 0 {
                return Some(open + offset);
            }
        }
    }
    None
}

/// The predicate inside an attribute body, if the attribute is a `cfg`.
fn cfg_predicate(body: &str) -> Option<&str> {
    body.trim()
        .strip_prefix("cfg(")
        .and_then(|rest| rest.strip_suffix(')'))
}

/// Does this predicate hold only when `test` is on? `all(test, windows)` does;
/// `not(test)` and `not(all(test, windows))` mark code that is *absent* under
/// test, so they must not be cut. Anything unrecognised counts as production —
/// the safe direction, because an over-count fails loudly instead of quietly
/// loosening the gate.
fn is_test_only(predicate: &str) -> bool {
    let predicate = predicate.trim();
    if predicate == "test" {
        return true;
    }
    if let Some(inner) = call_args(predicate, "all") {
        return inner.iter().any(|arg| is_test_only(arg));
    }
    if let Some(inner) = call_args(predicate, "any") {
        return !inner.is_empty() && inner.iter().all(|arg| is_test_only(arg));
    }
    false
}

/// The comma-separated arguments of `name(..)`, split at paren depth zero.
fn call_args<'a>(predicate: &'a str, name: &str) -> Option<Vec<&'a str>> {
    let inner = predicate
        .strip_prefix(name)?
        .trim_start()
        .strip_prefix('(')?
        .strip_suffix(')')?;
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (offset, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                args.push(inner[start..offset].trim());
                start = offset + 1;
            }
            _ => {}
        }
    }
    let tail = inner[start..].trim();
    if !tail.is_empty() {
        args.push(tail);
    }
    Some(args)
}

/// Where the item an attribute is attached to ends: the brace that closes it, or
/// the semicolon of a brace-less item such as `#[cfg(test)] use super::*;`. The
/// bracket and paren counters keep the `;` in `const X: [u8; 3]` from ending the
/// item early.
fn item_end(chars: &[char], from: usize) -> usize {
    let (mut brace, mut bracket, mut paren) = (0usize, 0usize, 0usize);
    let mut opened = false;
    for (offset, &c) in chars[from..].iter().enumerate() {
        let here = from + offset;
        match c {
            '{' => {
                brace += 1;
                opened = true;
            }
            '}' if brace > 0 => {
                brace -= 1;
                if opened && brace == 0 {
                    return here;
                }
            }
            '[' => bracket += 1,
            ']' if bracket > 0 => bracket -= 1,
            '(' => paren += 1,
            ')' if paren > 0 => paren -= 1,
            // A closer nothing here opened. The attribute was on an element of
            // an enclosing list or call — `vec![#[cfg(test)] x,]` — so the item
            // is whatever came before it, not the line that closes the list.
            '}' | ']' | ')' => return here.saturating_sub(1).max(from),
            ';' if !opened && bracket == 0 && paren == 0 => return here,
            _ => {}
        }
    }
    chars.len() - 1
}

// ---------------------------------------------------------------------------
// the counting rule, tested
// ---------------------------------------------------------------------------

#[test]
fn blank_and_comment_only_lines_do_not_count() {
    let src = "\
fn a() {}

// a line comment
/// a doc comment
/* a block
   comment */
fn b() {} // trailing comment
";
    assert_eq!(code_lines(src), 2);
}

#[test]
fn a_cfg_test_module_is_cut_out_and_the_code_after_it_is_not() {
    let src = "\
fn before() {}

#[cfg(test)]
mod tests {
    #[test]
    fn t() {
        assert_eq!(\"}\", \"}\");
    }
}

fn after() {}
";
    assert_eq!(code_lines(src), 2);
}

#[test]
fn a_brace_inside_a_string_does_not_end_the_test_module() {
    let src = "\
#[cfg(test)]
mod tests {
    const JSON: &str = r#\"{\"a\": 1}\"#;
    const HALF: &str = \"{\";
}
fn after() {}
";
    assert_eq!(code_lines(src), 1);
}

#[test]
fn a_slash_slash_inside_a_string_is_not_a_comment() {
    let src = "\
let url = \"https://example.com\";
let raw = r\"C:\\\\a\\\";
";
    assert_eq!(code_lines(src), 2);
}

#[test]
fn platform_gated_test_items_are_cut_and_not_test_items_are_kept() {
    let src = "\
#[cfg(all(test, windows))]
fn only_in_windows_tests() {}
#[cfg(not(test))]
fn never_in_tests() {}
#[cfg(not(all(test, windows)))]
fn also_production() {}
#[cfg(unix)]
fn production() {}
";
    assert_eq!(code_lines(src), 6);
}

#[test]
fn a_brace_less_cfg_test_item_ends_at_its_semicolon() {
    let src = "\
#[cfg(test)]
use super::*;
fn after() {}
";
    assert_eq!(code_lines(src), 1);
}

#[test]
fn a_lifetime_is_not_a_character_literal() {
    let src = "\
fn a<'x>(v: &'x str) -> char { '}' }
fn after() {}
";
    assert_eq!(code_lines(src), 2);
}

#[test]
fn test_files_are_out_of_scope() {
    assert!(is_production_source("mod.rs"));
    assert!(is_production_source("build.rs"));
    assert!(!is_production_source("tests.rs"));
    assert!(!is_production_source("plan49_parity_tests.rs"));
    assert!(!is_production_source("README.md"));
}
