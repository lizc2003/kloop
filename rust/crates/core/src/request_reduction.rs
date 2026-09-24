//! Request-time reduction (plan 200): old tool results go out as stubs in the
//! request, while the history, the session file, resume and fork keep every
//! byte. Offload bounds what *enters* the history; this bounds what a request
//! keeps *carrying* — a 25000-char `read_file` that offload lets through rides
//! every request until compaction folds it.
//!
//! **The prompt cache is the constraint, not a cost to weigh.** Every track
//! caches the request prefix, so changing any byte of an already-sent request
//! re-bills everything after it. Two rules follow:
//!
//! - A stub, once sent, is sent byte for byte on every later request
//!   ([`ReductionState`] keeps it by `tool_use_id`), even if the reason for it
//!   no longer holds.
//! - A *new* stub only ever lands when the cache is already cold — this model
//!   has not been asked anything in this history, or has sat idle past its
//!   cache TTL, or compaction just rewrote the history. A warm cache is never
//!   given up for a smaller request; the proposals simply wait.
//!
//! Only `ToolResult` text is ever replaced: ids, error flags, block and message
//! order are untouched, so tool_use/tool_result pairing cannot break.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::ProviderApiFamily;
use kloop_protocol::ProviderAttemptIdentity;
use kloop_protocol::Role;
use kloop_protocol::ToolResultContent;
use serde_json::Value;

use crate::file_state::normalize_absolute_path;
use crate::history::estimate_text_tokens;
use crate::history::is_offload_pointer;
use crate::tools::Builtin;

/// Above this, a stale/superseded read or an old search/shell result is worth
/// a stub. Thresholds and ages are chord's (bytes read as chars); tune from
/// dogfood, not from first principles.
const LOCAL_MIN_CHARS: usize = 3000;
/// External results (MCP, `call_tool`, web) are stubbed from a lower size.
const EXTERNAL_MIN_CHARS: usize = 1500;
/// Age = assistant messages after the result, i.e. rounds sampled since.
const LOCAL_MIN_AGE: usize = 2;
const EXTERNAL_MIN_AGE: usize = 3;
/// A failed call's output is what the model is most likely still reasoning
/// about, so it waits longer.
const ERROR_MIN_AGE: usize = 4;
const SEARCH_STUB_FILES: usize = 20;
const SHELL_STUB_LINES: usize = 20;
const SHELL_STUB_CHARS: usize = 1500;
const EXTERNAL_STUB_CHARS: usize = 500;

const QUERY_ADVICE: &str = "query it in place (bash with grep or `python3 -c '...'` over that \
                            path, printing only what you need) instead of reading it back";

/// How long a provider keeps an unused prefix. Anthropic's ephemeral cache is
/// five minutes. The OpenAI families keep theirs "up to an hour" with no
/// promise either way, so an hour is the one idle span after which a
/// reduction cannot have thrown a warm cache away.
fn cache_ttl(family: ProviderApiFamily) -> Duration {
    match family {
        ProviderApiFamily::AnthropicMessages | ProviderApiFamily::Mock => {
            Duration::from_secs(5 * 60)
        }
        ProviderApiFamily::OpenAiChatCompletions | ProviderApiFamily::OpenAiResponses => {
            Duration::from_secs(60 * 60)
        }
    }
}

/// Whose cache a request warms. The route revision is left out on purpose:
/// switching away and back lands on the same backend cache.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheScope {
    provider_id: String,
    endpoint_fingerprint: String,
    model: String,
}

impl CacheScope {
    fn of(identity: &ProviderAttemptIdentity) -> Self {
        Self {
            provider_id: identity.provider_id.clone(),
            endpoint_fingerprint: identity.endpoint_fingerprint.clone(),
            model: identity.model.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct Frozen {
    stub: String,
    saved_tokens: u64,
    /// Present when an identical later call means the stub cut too deep: the
    /// call, and the request length at which the model first saw the stub.
    recall: Option<(String, Value, usize)>,
}

/// What survives between requests. Lives as long as the history it reduces and
/// is never persisted: a resumed session re-derives its stubs, at the price of
/// a few duplicate offload files.
#[derive(Debug, Default)]
pub(crate) struct ReductionState {
    frozen: HashMap<String, Frozen>,
    /// Results never to stub: the model asked again for something a stub took.
    exempt: HashSet<String>,
    last_request: HashMap<CacheScope, SystemTime>,
    /// For a resumed session, when the session file was last written — the
    /// closest thing to a last request this process can know of. Stands in for
    /// every scope without its own entry.
    quiet_since: Option<SystemTime>,
}

impl ReductionState {
    pub(crate) fn resumed(quiet_since: Option<SystemTime>) -> Self {
        Self {
            quiet_since,
            ..Self::default()
        }
    }

    /// The history was rewritten: no stub describes it any more, and no cache
    /// holds its prefix.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// As if every cache had long expired, without waiting for it.
    #[cfg(test)]
    pub(crate) fn let_caches_expire(&mut self) {
        self.last_request.clear();
        self.quiet_since = None;
    }

    fn cache_is_cold(&self, scope: &CacheScope, now: SystemTime, ttl: Duration) -> bool {
        match self.last_request.get(scope).copied().or(self.quiet_since) {
            None => true,
            // A clock that went backwards proves nothing about idleness.
            Some(last) => now.duration_since(last).is_ok_and(|idle| idle > ttl),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReductionStats {
    pub stubbed: usize,
    pub saved_tokens: u64,
}

/// Where a stubbed result's original goes. The history implements it with its
/// offload store, so the file name — and with it the permission and sandbox
/// exemption for `off-NNNN.txt` — stays the one offload already uses.
pub(crate) trait OffloadSink {
    fn save(&mut self, content: &str) -> std::io::Result<PathBuf>;
}

/// The request being reduced: who it goes to, when, and the cwd relative
/// `read_file` paths resolve against.
pub struct RequestReduction<'a> {
    pub cwd: &'a Path,
    pub now: SystemTime,
    pub identity: &'a ProviderAttemptIdentity,
}

/// Reduce one request view in place. Frozen stubs are always applied; new ones
/// only when the cache for this identity is cold.
pub(crate) fn reduce(
    view: &mut [Message],
    state: &mut ReductionState,
    request: &RequestReduction<'_>,
    sink: &mut dyn OffloadSink,
) -> ReductionStats {
    let scope = CacheScope::of(request.identity);
    let cold = state.cache_is_cold(&scope, request.now, cache_ttl(request.identity.api_family));
    if cold {
        // Exemptions only matter to what is about to be frozen, and are judged
        // by position, so working them out here is as good as every request.
        let calls = Calls::index(view);
        note_recalls(&calls, state);
        freeze_proposals(view, &calls, state, request.cwd, sink);
    }
    state.last_request.insert(scope, request.now);
    apply_frozen(view, state)
}

/// Swap every frozen result for its stub. Also what `/context` uses to size
/// history as it is actually sent.
pub(crate) fn apply_frozen(view: &mut [Message], state: &ReductionState) -> ReductionStats {
    let mut stats = ReductionStats::default();
    for message in view.iter_mut() {
        for block in &mut message.content {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } = block
            else {
                continue;
            };
            if let Some(frozen) = state.frozen.get(tool_use_id) {
                *content = ToolResultContent::Text(frozen.stub.clone());
                stats.stubbed += 1;
                stats.saved_tokens += frozen.saved_tokens;
            }
        }
    }
    stats
}

struct Call<'a> {
    name: &'a str,
    input: &'a Value,
    message: usize,
    /// Position among all calls, for "later than".
    order: usize,
}

struct Calls<'a> {
    by_id: HashMap<&'a str, Call<'a>>,
    ordered: Vec<&'a str>,
    /// `tool_use_id` → is_error, for results present in the view.
    outcomes: HashMap<&'a str, bool>,
    /// Assistant messages after each message.
    ages: Vec<usize>,
}

impl<'a> Calls<'a> {
    fn index(view: &'a [Message]) -> Self {
        let mut by_id = HashMap::new();
        let mut ordered = Vec::new();
        let mut outcomes = HashMap::new();
        for (message_index, message) in view.iter().enumerate() {
            for block in &message.content {
                match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        by_id.insert(
                            id.as_str(),
                            Call {
                                name,
                                input,
                                message: message_index,
                                order: ordered.len(),
                            },
                        );
                        ordered.push(id.as_str());
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    } => {
                        outcomes.insert(tool_use_id.as_str(), *is_error);
                    }
                    ContentBlock::Text { .. }
                    | ContentBlock::Thinking { .. }
                    | ContentBlock::RedactedThinking { .. }
                    | ContentBlock::Image { .. } => {}
                }
            }
        }
        let mut ages = vec![0; view.len()];
        let mut after = 0;
        for (index, message) in view.iter().enumerate().rev() {
            ages[index] = after;
            if message.role == Role::Assistant {
                after += 1;
            }
        }
        Self {
            by_id,
            ordered,
            outcomes,
            ages,
        }
    }

    fn succeeded(&self, id: &str) -> bool {
        self.outcomes.get(id) == Some(&false)
    }

    fn later(&self, call: &Call<'_>) -> impl Iterator<Item = (&str, &Call<'a>)> {
        self.ordered[call.order + 1..]
            .iter()
            .filter_map(|id| self.by_id.get(id).map(|later| (*id, later)))
    }
}

/// An identical call issued after its stub was sent means the stub took
/// something still needed: that new result is never stubbed. The old stub
/// stays — changing it would rewrite an already-sent prefix.
fn note_recalls(calls: &Calls<'_>, state: &mut ReductionState) {
    let recallable: Vec<&(String, Value, usize)> = state
        .frozen
        .values()
        .filter_map(|frozen| frozen.recall.as_ref())
        .collect();
    if recallable.is_empty() {
        return;
    }
    let mut recalled = Vec::new();
    for (id, call) in &calls.by_id {
        if state.frozen.contains_key(*id) || state.exempt.contains(*id) {
            continue;
        }
        if recallable.iter().any(|(name, input, seen_from)| {
            call.message >= *seen_from && name == call.name && input == call.input
        }) {
            recalled.push((*id).to_string());
        }
    }
    state.exempt.extend(recalled);
}

fn freeze_proposals(
    view: &[Message],
    calls: &Calls<'_>,
    state: &mut ReductionState,
    cwd: &Path,
    sink: &mut dyn OffloadSink,
) {
    for (message_index, message) in view.iter().enumerate() {
        for block in &message.content {
            let ContentBlock::ToolResult {
                tool_use_id,
                content: ToolResultContent::Text(text),
                is_error,
            } = block
            else {
                continue;
            };
            if state.frozen.contains_key(tool_use_id) || state.exempt.contains(tool_use_id) {
                continue;
            }
            let Some(call) = calls.by_id.get(tool_use_id.as_str()) else {
                continue;
            };
            let age = calls.ages[message_index];
            let Some(proposal) = propose(call, text, *is_error, age, calls, cwd) else {
                continue;
            };
            // A stub points at the original; without it on disk the stub would
            // destroy content to save context, which is the worse trade.
            let Ok(saved) = sink.save(text) else {
                continue;
            };
            let stub = proposal.render(&saved.display().to_string());
            let saved_tokens =
                estimate_text_tokens(text).saturating_sub(estimate_text_tokens(&stub));
            let recall = proposal
                .recallable()
                .then(|| (call.name.to_string(), call.input.clone(), view.len()));
            state.frozen.insert(
                tool_use_id.clone(),
                Frozen {
                    stub,
                    saved_tokens,
                    recall,
                },
            );
        }
    }
}

enum Kind {
    Read,
    Search,
    Shell,
    External,
    Never,
}

fn kind(name: &str) -> Kind {
    let Some(builtin) = Builtin::from_name(name) else {
        return match name {
            crate::structured_output::TOOL_NAME => Kind::Never,
            // web tools and every MCP tool a source advertises inline.
            _ => Kind::External,
        };
    };
    match builtin {
        Builtin::ReadFile => Kind::Read,
        Builtin::Grep | Builtin::Glob => Kind::Search,
        Builtin::Bash | Builtin::BashOutput | Builtin::PowerShell => Kind::Shell,
        Builtin::CallTool => Kind::External,
        // Instructions (skill), answers (ask_user_question), change evidence
        // (edits), sub-agent reports, and small control results: not data a
        // later round can do without.
        Builtin::StopBash
        | Builtin::WriteFile
        | Builtin::EditFile
        | Builtin::NotebookEdit
        | Builtin::TodoWrite
        | Builtin::SendMessage
        | Builtin::ListAgents
        | Builtin::RunAgent
        | Builtin::WaitForActivity
        | Builtin::StopAgent
        | Builtin::ToolSearch
        | Builtin::RunProgram
        | Builtin::StopProgram
        | Builtin::CronCreate
        | Builtin::CronDelete
        | Builtin::CronList
        | Builtin::ScheduleWakeup
        | Builtin::AskUserQuestion
        | Builtin::EnterPlanMode
        | Builtin::ExitPlanMode
        | Builtin::Workflow
        | Builtin::StopWorkflow
        | Builtin::EnterWorktree
        | Builtin::ExitWorktree
        | Builtin::Skill => Kind::Never,
    }
}

enum Proposal<'a> {
    Read {
        path: &'a str,
        range: String,
        chars: usize,
        why: String,
    },
    Search {
        tool: &'a str,
        lines: usize,
        chars: usize,
        files: Vec<&'a str>,
    },
    Shell {
        tool: &'a str,
        chars: usize,
        tail: String,
    },
    External {
        tool: &'a str,
        chars: usize,
        head: &'a str,
    },
}

fn propose<'a>(
    call: &Call<'a>,
    text: &'a str,
    is_error: bool,
    age: usize,
    calls: &Calls<'a>,
    cwd: &Path,
) -> Option<Proposal<'a>> {
    // An offloaded result is already a preview plus a pointer.
    if is_offload_pointer(text) {
        return None;
    }
    let chars = text.chars().count();
    let old_enough = |min_age: usize| age >= if is_error { ERROR_MIN_AGE } else { min_age };
    match kind(call.name) {
        Kind::Read => {
            if is_error || chars <= LOCAL_MIN_CHARS {
                return None;
            }
            let read = ReadSpan::of(call.input, cwd)?;
            let why = read_invalidated_by(call, &read, calls, cwd)?;
            Some(Proposal::Read {
                path: call.input.get("path")?.as_str()?,
                range: read.describe(),
                chars,
                why,
            })
        }
        Kind::Search if old_enough(LOCAL_MIN_AGE) && chars > LOCAL_MIN_CHARS => {
            Some(Proposal::Search {
                tool: call.name,
                lines: text.lines().count(),
                chars,
                files: named_files(call, text),
            })
        }
        Kind::Shell if old_enough(LOCAL_MIN_AGE) && chars > LOCAL_MIN_CHARS => {
            Some(Proposal::Shell {
                tool: call.name,
                chars,
                tail: shell_tail(text),
            })
        }
        Kind::External if old_enough(EXTERNAL_MIN_AGE) && chars > EXTERNAL_MIN_CHARS => {
            let (head, _) = crate::tools::char_prefix(text, EXTERNAL_STUB_CHARS);
            Some(Proposal::External {
                tool: call.name,
                chars,
                head,
            })
        }
        Kind::Search | Kind::Shell | Kind::External | Kind::Never => None,
    }
}

impl Proposal<'_> {
    /// Re-running a shell command is ordinary (a test suite, a build), not a
    /// sign the stub cut too deep; a still-valid read is never stubbed at all.
    fn recallable(&self) -> bool {
        match self {
            Self::Search { .. } | Self::External { .. } => true,
            Self::Read { .. } | Self::Shell { .. } => false,
        }
    }

    /// A pure function of the result and the call: the same stub must come out
    /// every time, so nothing here may depend on when it is rendered.
    fn render(&self, saved: &str) -> String {
        match self {
            Self::Read {
                path,
                range,
                chars,
                why,
            } => format!(
                "[read_file {path} ({range}, {chars} chars) removed from this request: {why}. \
                 The original result is saved to {saved}; {QUERY_ADVICE}. For the file as it is \
                 now, read_file it again.]"
            ),
            Self::Search {
                tool,
                lines,
                chars,
                files,
            } => {
                let listed = match files.is_empty() {
                    true => String::new(),
                    false => format!(
                        "\nFiles it named (first {}):\n{}",
                        files.len(),
                        files.join("\n")
                    ),
                };
                format!(
                    "[{tool} result removed from this request: {lines} lines, {chars} chars.{listed}\n\
                     The full result is saved to {saved}; {QUERY_ADVICE}.]"
                )
            }
            Self::Shell { tool, chars, tail } => format!(
                "[{tool} output trimmed from this request: {chars} chars, only its end is kept \
                 below. The full output is saved to {saved}; {QUERY_ADVICE}.]\n{tail}"
            ),
            Self::External { tool, chars, head } => format!(
                "{head}\n…[{tool} result trimmed from this request: {chars} chars, only its start \
                 is kept above. The full result is saved to {saved}; {QUERY_ADVICE}.]"
            ),
        }
    }
}

/// The lines a `read_file` call asked for, and the file, as history recorded
/// the call — never the disk: the judgement must come out the same on resume.
struct ReadSpan {
    path: PathBuf,
    offset: u64,
    limit: Option<u64>,
}

impl ReadSpan {
    fn of(input: &Value, cwd: &Path) -> Option<Self> {
        let path = input.get("path")?.as_str()?;
        Some(Self {
            path: normalize_absolute_path(cwd, Path::new(path)),
            offset: input.get("offset").and_then(Value::as_u64).unwrap_or(1),
            limit: input.get("limit").and_then(Value::as_u64),
        })
    }

    fn covers(&self, other: &Self) -> bool {
        self.path == other.path
            && self.offset <= other.offset
            && match (self.limit, other.limit) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(limit), Some(other_limit)) => {
                    self.offset + limit >= other.offset + other_limit
                }
            }
    }

    fn describe(&self) -> String {
        match self.limit {
            Some(limit) => format!(
                "lines {}-{}",
                self.offset,
                self.offset + limit.saturating_sub(1)
            ),
            None => format!("lines {}-end", self.offset),
        }
    }
}

/// Why an old read no longer describes the file, from history alone: a later
/// successful edit of the same file makes it stale, a later successful read
/// covering its lines supersedes it. `None` while it is still the model's
/// current view of those lines — such a read is never stubbed.
fn read_invalidated_by(
    call: &Call<'_>,
    read: &ReadSpan,
    calls: &Calls<'_>,
    cwd: &Path,
) -> Option<String> {
    for (id, later) in calls.later(call) {
        if !calls.succeeded(id) {
            continue;
        }
        let path_key = match Builtin::from_name(later.name) {
            Some(Builtin::EditFile | Builtin::WriteFile) => "path",
            Some(Builtin::NotebookEdit) => "notebook_path",
            Some(Builtin::ReadFile) => {
                if ReadSpan::of(later.input, cwd).is_some_and(|span| span.covers(read)) {
                    return Some("superseded by a later read_file of the same lines".into());
                }
                continue;
            }
            _ => continue,
        };
        let Some(path) = later.input.get(path_key).and_then(Value::as_str) else {
            continue;
        };
        if normalize_absolute_path(cwd, Path::new(path)) == read.path {
            return Some(format!("stale, changed by a later {}", later.name));
        }
    }
    None
}

/// The distinct files a search result names, first-seen order, at most
/// [`SEARCH_STUB_FILES`]. `glob` and grep's default mode list one path per
/// line; grep's content and count modes prefix each line with `path:`.
fn named_files<'a>(call: &Call<'_>, text: &'a str) -> Vec<&'a str> {
    let listing = call.name == Builtin::Glob.name()
        || matches!(
            call.input.get("output_mode").and_then(Value::as_str),
            None | Some("files_with_matches")
        );
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for line in text.lines() {
        let candidate = match listing {
            true => line.trim(),
            false => prefixed_path(line),
        };
        let is_path = !candidate.is_empty()
            && !candidate.starts_with("Found ")
            && !candidate.starts_with("No ")
            && !candidate.starts_with('(')
            && !candidate.starts_with('[')
            && !candidate.chars().all(|c| c.is_ascii_digit());
        if is_path && seen.insert(candidate) {
            files.push(candidate);
            if files.len() == SEARCH_STUB_FILES {
                break;
            }
        }
    }
    files
}

/// `path:rest` → `path`, stepping over a Windows drive letter. A line with no
/// `:` (a context line, a separator, the summary) names nothing.
fn prefixed_path(line: &str) -> &str {
    let skip = match line.as_bytes() {
        [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic() => 2,
        _ => 0,
    };
    match line[skip..].find(':') {
        Some(end) => &line[..skip + end],
        None => "",
    }
}

/// The last lines of a shell output — where the exit status, the failing test,
/// the final error live — bounded by lines and by chars.
fn shell_tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(SHELL_STUB_LINES)..].join("\n");
    let chars = tail.chars().count();
    if chars <= SHELL_STUB_CHARS {
        return tail;
    }
    tail.chars().skip(chars - SHELL_STUB_CHARS).collect()
}

#[cfg(test)]
mod tests;
