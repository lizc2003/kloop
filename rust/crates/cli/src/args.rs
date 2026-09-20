//! CLI argument parsing (`--mock`/`--resume`/`--fork`/…) and session picking:
//! turning argv into a [`CliArgs`], and resolving a [`SessionChoice`] into an
//! opened `History` (list / continue / resume / fork). Pure front-matter that
//! runs before any UI owns the terminal.

use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;

use kloop_core::history::History;
use kloop_core::permissions::Mode;
use kloop_core::provider_route::FrozenProviderRoute;
use kloop_core::rollout::Rollout;
use kloop_core::rollout::SessionDigest;
use kloop_core::rollout::SessionOrigin;
use kloop_core::rollout::checked_session_path;
use kloop_core::rollout::first_user_snippet;
use kloop_core::rollout::fork_origin;
use kloop_core::rollout::fork_session;
use kloop_core::rollout::load_session;
use kloop_core::rollout::new_session_id;
use kloop_core::rollout::resume_session;
use kloop_core::rollout::session_digest;
use kloop_core::rollout::session_id_of;
use kloop_core::rollout::session_origin;
use kloop_core::rollout::session_path;
use kloop_core::rollout::sessions_by_recency;
use kloop_core::session_store::ProjectBucket;
use kloop_core::session_store::SessionDirs;
use kloop_core::session_store::SessionStore;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionChoice {
    New,
    /// `--continue`: the most recently modified session.
    Continue,
    /// `--resume` with no id: pick from a numbered list.
    Pick,
    Resume(String),
    /// `--fork <id>[#<seq>]`: branch a new session off an existing one at a
    /// line boundary (no seq = at the end) and continue there. Covers rewind
    /// too: fork the current session at an earlier point and take the other
    /// road — the original file is never touched.
    Fork {
        id: String,
        cut: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CliArgs {
    pub(crate) mock: bool,
    /// `-h`/`--help`: print the usage summary and exit.
    pub(crate) help: bool,
    /// `-V`/`--version`: print the build stamp and exit. `-v` stays free for a
    /// future verbose flag, which is what the lowercase letter reads as.
    pub(crate) version: bool,
    /// `--permission-mode <mode>`: the gating mode for this session. `manual`
    /// (the default when the flag is omitted — ask for anything unvouched-for),
    /// `accept-edits` (auto-approve cwd file writes), `bypass` (approve all but
    /// deny rules + safety checks), `plan` (read-only until the model's plan is
    /// approved via exit_plan_mode). `--mock` ignores it (no gate at all).
    pub(crate) permission_mode: Mode,
    pub(crate) list_sessions: bool,
    /// `--all` (`--list-sessions` only): list every project's sessions, not
    /// just this working directory's. Sessions are stored per project, so this
    /// is the machine-wide view.
    pub(crate) all: bool,
    pub(crate) plain: bool,
    pub(crate) serve: bool,
    /// `--headless`: run one turn headless (no REPL, no TUI) and exit. The
    /// prompt comes from the positional argument and/or piped stdin. No short
    /// alias — headless is not a hot path, and `-p` (cc's print) wouldn't match
    /// the `--headless` name anyway.
    pub(crate) headless: bool,
    /// `--json` (headless only): emit the run as a NDJSON event stream on
    /// stdout, reusing the server mode's notification wire shapes.
    pub(crate) json: bool,
    /// `--max-rounds <n>` (headless only): cap the number of sampling rounds
    /// (one model call + the tool calls it asks for) as a runaway guardrail for
    /// scripts. Interactive and server turns are otherwise unbounded.
    pub(crate) max_rounds: Option<usize>,
    /// The positional prompt for headless mode, if any (may be combined with
    /// piped stdin at run time).
    pub(crate) prompt: Option<String>,
    /// `--image <path>` (repeatable): local image files attached to the first
    /// user turn. Read + validated after parsing; remote URLs are refused.
    pub(crate) images: Vec<PathBuf>,
    /// `--worktree[=<name>]` (plan 35 slice 2): start the session inside an
    /// isolated git worktree named `<name>` (default `session`). `Some(name)`
    /// when requested, `None` otherwise. The `=` form avoids swallowing a
    /// headless positional prompt.
    pub(crate) worktree: Option<String>,
    pub(crate) session: SessionChoice,
}

pub(crate) fn parse_args(args: &[String]) -> Result<CliArgs> {
    let mut parsed = CliArgs {
        mock: false,
        help: false,
        version: false,
        permission_mode: Mode::Manual,
        list_sessions: false,
        all: false,
        plain: false,
        serve: false,
        headless: false,
        json: false,
        max_rounds: None,
        prompt: None,
        images: Vec::new(),
        worktree: None,
        session: SessionChoice::New,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => parsed.help = true,
            "-V" | "--version" => parsed.version = true,
            "--mock" => parsed.mock = true,
            "--permission-mode" => {
                let raw = match args.get(i + 1) {
                    Some(mode) if !mode.starts_with('-') => mode,
                    _ => bail!(
                        "--permission-mode needs a mode (manual | accept-edits | bypass | plan)"
                    ),
                };
                i += 1;
                parsed.permission_mode = match raw.as_str() {
                    "manual" => Mode::Manual,
                    "accept-edits" => Mode::AcceptEdits,
                    "bypass" => Mode::Bypass,
                    "plan" => Mode::Plan,
                    other => bail!(
                        "unknown permission mode '{other}' (manual | accept-edits | bypass | plan)"
                    ),
                };
            }
            "--list-sessions" => parsed.list_sessions = true,
            "--all" => parsed.all = true,
            "--plain" => parsed.plain = true,
            "--serve" => parsed.serve = true,
            "--worktree" => parsed.worktree = Some("session".into()),
            w if w.starts_with("--worktree=") => {
                let name = w.trim_start_matches("--worktree=");
                parsed.worktree = Some(if name.is_empty() { "session" } else { name }.to_string());
            }
            "--headless" => parsed.headless = true,
            "--json" => parsed.json = true,
            "--max-rounds" => {
                let raw = match args.get(i + 1) {
                    Some(raw) if !raw.starts_with('-') => raw,
                    _ => bail!("--max-rounds needs a positive integer"),
                };
                i += 1;
                let n: usize = raw
                    .parse()
                    .with_context(|| format!("--max-rounds: '{raw}' is not a positive integer"))?;
                if n == 0 {
                    bail!("--max-rounds must be at least 1");
                }
                parsed.max_rounds = Some(n);
            }
            "--image" => {
                let path = match args.get(i + 1) {
                    Some(path) if !path.starts_with('-') => path,
                    _ => bail!("--image needs a file path"),
                };
                i += 1;
                parsed.images.push(PathBuf::from(path));
            }
            "-c" | "--continue" => parsed.session = SessionChoice::Continue,
            "-r" | "--resume" => {
                parsed.session = match args.get(i + 1) {
                    Some(id) if !id.starts_with('-') => {
                        i += 1;
                        SessionChoice::Resume(id.clone())
                    }
                    _ => SessionChoice::Pick,
                };
            }
            "--fork" => {
                let arg = match args.get(i + 1) {
                    Some(arg) if !arg.starts_with('-') => arg,
                    _ => bail!("--fork needs a session (<id> or <id>#<seq>)"),
                };
                i += 1;
                parsed.session = match arg.split_once('#') {
                    None => SessionChoice::Fork {
                        id: arg.clone(),
                        cut: None,
                    },
                    Some((id, seq)) => SessionChoice::Fork {
                        id: id.to_string(),
                        cut: Some(seq.parse().with_context(|| {
                            format!("--fork: '{seq}' is not a line number (<id>#<seq>)")
                        })?),
                    },
                };
            }
            // A leading dash is an unknown flag; anything else is the headless
            // positional prompt (only one is allowed).
            other if other.starts_with('-') => bail!(
                "unknown argument '{other}' (-h/--help | -V/--version | --headless | --json | --max-rounds <n> | --mock | --permission-mode <mode> | --plain | --serve | --image <path> | -c/--continue | -r/--resume [id] | --fork <id>[#<seq>] | --list-sessions [--all])"
            ),
            prompt => {
                if parsed.prompt.is_some() {
                    bail!("unexpected extra argument '{prompt}' (pass a single prompt)");
                }
                parsed.prompt = Some(prompt.to_string());
            }
        }
        i += 1;
    }
    // `--json` / `--max-rounds` / a positional prompt only mean something in
    // headless mode; requiring `--headless` keeps the mode's flags cohesive.
    if !parsed.headless {
        if parsed.json {
            bail!("--json requires --headless (it streams the headless run as JSON)");
        }
        if parsed.max_rounds.is_some() {
            bail!("--max-rounds requires --headless (it is a headless guardrail)");
        }
        if let Some(prompt) = &parsed.prompt {
            bail!(
                "a prompt argument ('{prompt}') requires --headless; interactive mode takes input at its prompt"
            );
        }
    }
    if parsed.headless && parsed.serve {
        bail!("--headless and --serve are different modes; pick one");
    }
    Ok(parsed)
}

/// The `-h`/`--help` usage summary. Kept in sync by hand with the flags above
/// (the parser is small enough that a derive isn't worth a dependency).
pub(crate) fn help_text() -> &'static str {
    "kloop — a Rust coding agent\n\
     \n\
     USAGE:\n\
     \x20   kloop [OPTIONS]              start the interactive TUI (default)\n\
     \x20   kloop --headless [OPTS] [PROMPT]  run one turn headless, then exit\n\
     \x20   kloop app-server             native agent protocol server on stdio\n\
     \x20   kloop mcp login <name>       OAuth login to a remote MCP server\n\
     \n\
     MODES:\n\
     \x20       --headless        run one turn without a REPL/TUI, then exit\n\
     \x20       --plain           line-based REPL instead of the TUI\n\
     \x20       --serve           native agent protocol server over stdio\n\
     \x20                         (also as the `app-server` subcommand)\n\
     \x20       --mock            keyless scripted demo (hermetic)\n\
     \n\
     HEADLESS (--headless only):\n\
     \x20   [PROMPT]              the task; may also be piped on stdin (both combine)\n\
     \x20       --json            stream the run as NDJSON events on stdout\n\
     \x20       --max-rounds <n>  cap sampling rounds as a runaway guardrail\n\
     \n\
     SESSIONS:\n\
     \x20   -c, --continue        resume the most recent session\n\
     \x20   -r, --resume [id]     resume a session (no id = pick from a list)\n\
     \x20       --fork <id>[#<seq>]  branch a session at line <seq> (rewind)\n\
     \x20       --list-sessions   list this project's saved sessions and exit\n\
     \x20       --all             (with --list-sessions) list every project's sessions\n\
     \n\
     PERMISSIONS:\n\
     \x20       --permission-mode <mode>   manual (default) | accept-edits | bypass | plan\n\
     \n\
     INPUT:\n\
     \x20       --image <path>    attach a local image (repeatable)\n\
     \n\
     WORKTREE:\n\
     \x20       --worktree[=<name>]  run the session in an isolated git worktree\n\
     \n\
     OTHER:\n\
     \x20   -h, --help            show this help and exit\n\
     \x20   -V, --version         show the version and build commit, then exit\n\
     \n\
     All persistent runtime/provider configuration: ~/.kloop/config.toml. No environment variable overrides it.\n\
     Cwd selects the workspace; it is never an automatic config source.\n"
}

/// What this build calls itself, for the session banner: the crate version plus
/// the commit `build.rs` stamped in (`v0.1.0 (2319ea3)`). Built outside a
/// checkout there is no commit and it degrades to `v0.1.0`. `KLOOP_VERSION`
/// replaces the whole string — the PTY screen baselines pin it, so a checked-in
/// frame does not move with every commit.
pub(crate) fn version_string() -> String {
    match std::env::var("KLOOP_VERSION") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => format_version(env!("CARGO_PKG_VERSION"), option_env!("KLOOP_BUILD_SHA")),
    }
}

fn format_version(pkg: &str, sha: Option<&str>) -> String {
    match sha.map(str::trim).filter(|s| !s.is_empty()) {
        Some(sha) => format!("v{pkg} ({sha})"),
        None => format!("v{pkg}"),
    }
}

fn session_line(path: &Path) -> String {
    let id = session_id_of(path);
    let origin = match session_origin(path) {
        Some(SessionOrigin::Fork(o)) => format!("  [forked from {o}]"),
        Some(SessionOrigin::SubAgent(o)) => format!("  [sub-agent of {o}]"),
        None => String::new(),
    };
    match load_session(path) {
        Ok(messages) => format!(
            "{id}  {} message(s)  {}{origin}",
            messages.len(),
            first_user_snippet(&messages)
        ),
        Err(e) => format!("{id}  (unreadable: {e}){origin}"),
    }
}

pub(crate) fn list_sessions(store: &SessionStore, cwd: &Path, all: bool) {
    if all {
        list_every_project(store);
        return;
    }
    let dirs = store.dirs(cwd);
    let sessions = sessions_by_recency(&dirs.sessions);
    if sessions.is_empty() {
        println!("no saved sessions in {}", dirs.sessions.display());
        return;
    }
    for path in sessions {
        println!("{}", session_line(&path));
    }
}

/// `--list-sessions --all`: every project partition on this machine, most
/// recently used first, labelled with the directory the partition is named
/// after (its digest is storage detail the user should never have to read).
fn list_every_project(store: &SessionStore) {
    let mut groups: Vec<(ProjectBucket, Vec<PathBuf>, SystemTime)> = store
        .buckets()
        .into_iter()
        .filter_map(|bucket| {
            let sessions = sessions_by_recency(&bucket.dirs.sessions);
            let newest = newest_mtime(sessions.first()?)?;
            Some((bucket, sessions, newest))
        })
        .collect();
    if groups.is_empty() {
        println!("no saved sessions under {}", store.root().display());
        return;
    }
    groups.sort_by_key(|(_, _, newest)| std::cmp::Reverse(*newest));
    for (index, (bucket, sessions, _)) in groups.iter().enumerate() {
        if index > 0 {
            println!();
        }
        match &bucket.anchor {
            Some(anchor) => println!("{}", anchor.display()),
            None => println!("(unlabelled project {})", bucket.project_id),
        }
        for path in sessions {
            println!("  {}", session_line(path));
        }
    }
}

fn newest_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// The project a session id belongs to, when it is not this one — so a listing
/// that spans projects can explain why an id here is not resumable rather than
/// just reporting it missing.
fn owning_project(store: &SessionStore, dirs: &SessionDirs, id: &str) -> Option<PathBuf> {
    store
        .buckets()
        .into_iter()
        .find(|bucket| {
            bucket.dirs.sessions != dirs.sessions
                && checked_session_path(&bucket.dirs.sessions, id).is_ok_and(|path| path.exists())
        })
        .map(|bucket| {
            bucket
                .anchor
                .unwrap_or_else(|| bucket.dirs.sessions.clone())
        })
}

/// A session id that exists, or an error naming the project that owns it.
/// Sessions are resumed in the current working directory, so adopting another
/// project's transcript would run the agent against the wrong repository.
fn session_in_this_project(store: &SessionStore, dirs: &SessionDirs, id: &str) -> Result<PathBuf> {
    let path = checked_session_path(&dirs.sessions, id)
        .with_context(|| format!("invalid session id '{id}'"))?;
    if path.exists() {
        return Ok(path);
    }
    match owning_project(store, dirs, id) {
        Some(project) => bail!(
            "session '{id}' belongs to {}; run kloop there to resume it",
            project.display()
        ),
        None => bail!("no session '{id}' (try --list-sessions)"),
    }
}

/// One offered session: its file plus the digest a list row is built from.
/// `error` is set when the file could not be read at all — it stays on the
/// list, saying why, because hiding it would hide the problem.
struct Resumable {
    path: PathBuf,
    digest: SessionDigest,
    error: Option<String>,
}

/// Top-level sessions the resume picker offers, most recent first: every
/// session except sub-agent transcripts, which are reachable only by explicit
/// id (they still show in `--list-sessions`). Mirrors cc hiding sidechains and
/// codex filtering by source.
///
/// Read through [`session_digest`], not `load_session`: the picker draws every
/// one of these rows before the first keystroke, and replaying dozens of
/// megabyte-sized transcripts to learn their first line is the cost that made
/// the old numbered list feel instant only because it was short.
fn resumable_sessions(sessions_dir: &Path) -> Vec<Resumable> {
    sessions_by_recency(sessions_dir)
        .into_iter()
        .map(|path| match session_digest(&path) {
            Ok(digest) => Resumable {
                path,
                digest,
                error: None,
            },
            Err(error) => Resumable {
                digest: SessionDigest {
                    id: session_id_of(&path),
                    title: String::new(),
                    origin: None,
                    modified: SystemTime::UNIX_EPOCH,
                    bytes: 0,
                    has_content: true,
                },
                path,
                error: Some(error.to_string()),
            },
        })
        .filter(|session| !matches!(session.digest.origin, Some(SessionOrigin::SubAgent(_))))
        // A session holding nothing but its opening preamble replays as an empty
        // conversation — there is nothing to continue into. A writer no longer
        // leaves one behind, but the ones already on disk (and any left by a
        // hard kill) would still win `--continue` on recency alone.
        .filter(|session| session.digest.has_content)
        .collect()
}

/// The row text for one session, shared by the picker and the stdio fallback.
fn session_title(session: &Resumable) -> String {
    match (&session.error, session.digest.title.as_str()) {
        (Some(error), _) => format!("(unreadable: {error})"),
        (None, "") => "(no prompt yet)".to_string(),
        (None, title) => title.to_string(),
    }
}

fn session_badge(session: &Resumable) -> Option<String> {
    match &session.digest.origin {
        Some(SessionOrigin::Fork(origin)) => Some(format!("forked from {origin}")),
        Some(SessionOrigin::SubAgent(origin)) => Some(format!("sub-agent of {origin}")),
        None => None,
    }
}

/// `--resume` with no id. On a terminal this is the full-screen picker
/// (plan 162); anywhere else — a pipe, a test harness, a terminal that refuses
/// raw mode — it falls back to the numbered list this used to be.
/// Runs before any UI starts, so plain blocking stdio is fine.
fn pick_session(sessions_dir: &Path) -> Result<Option<PathBuf>> {
    use std::io::IsTerminal as _;

    let sessions = resumable_sessions(sessions_dir);
    if sessions.is_empty() {
        bail!("no saved sessions to resume");
    }
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let now = SystemTime::now();
        let entries = sessions
            .iter()
            .map(|session| kloop_tui::SessionEntry {
                id: session.digest.id.clone(),
                title: session_title(session),
                age: now
                    .duration_since(session.digest.modified)
                    .unwrap_or_default(),
                bytes: session.digest.bytes,
                badge: session_badge(session),
            })
            .collect();
        let project = std::env::current_dir()
            .ok()
            .and_then(|cwd| {
                cwd.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        return Ok(
            kloop_tui::pick_session(entries, &project)?.map(|index| sessions[index].path.clone())
        );
    }
    println!("saved sessions (most recent first):");
    for (i, session) in sessions.iter().enumerate() {
        println!(
            "{:>3}. {}  {}{}",
            i + 1,
            session.digest.id,
            session_title(session),
            session_badge(session)
                .map(|badge| format!("  [{badge}]"))
                .unwrap_or_default()
        );
    }
    print!("resume which? [1-{}, empty = 1] > ", sessions.len());
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let index = pick_index(&line, sessions.len())?;
    Ok(Some(sessions[index].path.clone()))
}

/// 1-based selection, empty input = the first (most recent) entry.
fn pick_index(input: &str, len: usize) -> Result<usize> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(0);
    }
    match input.parse::<usize>() {
        Ok(n) if (1..=len).contains(&n) => Ok(n - 1),
        _ => bail!("invalid selection '{input}' (expected 1-{len})"),
    }
}

/// Build the History for this run: a fresh persisted session by default, or
/// one replayed from disk for `--resume`. `Ok(None)` means the picker was
/// cancelled — the user asked for nothing, so kloop exits quietly instead of
/// starting a session they did not choose.
pub(crate) fn open_history(
    store: &SessionStore,
    dirs: &SessionDirs,
    choice: &SessionChoice,
    initial_route: &FrozenProviderRoute,
) -> Result<Option<(History, String)>> {
    let sessions_dir = dirs.sessions.as_path();
    let offload_dir = dirs.offload.clone();
    let resume_path = match choice {
        SessionChoice::New => {
            let id = new_session_id(sessions_dir);
            let mut history = History::new(offload_dir);
            history.attach_rollout(Rollout::new_with_initial_route(
                session_path(sessions_dir, &id),
                initial_route,
            )?);
            return Ok(Some((history, id)));
        }
        SessionChoice::Resume(id) => session_in_this_project(store, dirs, id)?,
        SessionChoice::Continue => {
            resumable_sessions(sessions_dir)
                .into_iter()
                .next()
                .context("no saved sessions to continue")?
                .path
        }
        SessionChoice::Pick => match pick_session(sessions_dir)? {
            Some(path) => path,
            None => return Ok(None),
        },
        SessionChoice::Fork { id, cut } => {
            let src = session_in_this_project(store, dirs, id)?;
            let path = fork_session(&src, *cut, sessions_dir)
                .with_context(|| format!("cannot fork session '{id}'"))?;
            println!(
                "[forked {id}#{cut} → {fork_id}]",
                cut = fork_origin(&path)
                    .and_then(|origin| origin.rsplit('#').next().map(str::to_string))
                    .unwrap_or_default(),
                fork_id = session_id_of(&path),
            );
            path
        }
    };
    let id = session_id_of(&resume_path);
    let resumed = resume_session(&resume_path)
        .with_context(|| format!("cannot read session file {}", resume_path.display()))?;
    println!(
        "[resumed session {id}: {} message(s)]",
        resumed.messages.len()
    );
    let history = History::resume(offload_dir, resumed);
    Ok(Some((history, id)))
}
#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// The build stamp (plan 161): the commit rides in parentheses after the
    /// crate version, and a build with no commit to name says just the version
    /// instead of `v0.1.0 ()`.
    #[test]
    fn version_names_the_commit_only_when_the_build_stamped_one() {
        assert_eq!(format_version("0.1.0", Some("2319ea3")), "v0.1.0 (2319ea3)");
        assert_eq!(format_version("0.1.0", None), "v0.1.0");
        assert_eq!(format_version("0.1.0", Some("  ")), "v0.1.0");
    }

    /// The all-false / all-None default, so each case below asserts the whole
    /// object while spelling out only what that flag changes.
    fn base() -> CliArgs {
        CliArgs {
            mock: false,
            help: false,
            version: false,
            permission_mode: Mode::Manual,
            list_sessions: false,
            all: false,
            plain: false,
            serve: false,
            headless: false,
            json: false,
            max_rounds: None,
            prompt: None,
            images: vec![],
            worktree: None,
            session: SessionChoice::New,
        }
    }

    #[test]
    fn parse_args_covers_all_flags() {
        assert_eq!(parse_args(&[]).unwrap(), base());
        assert_eq!(
            parse_args(&strings(&["--mock", "--resume"])).unwrap(),
            CliArgs {
                mock: true,
                session: SessionChoice::Pick,
                ..base()
            }
        );
        assert_eq!(
            parse_args(&strings(&["--continue"])).unwrap(),
            CliArgs {
                session: SessionChoice::Continue,
                ..base()
            }
        );
        // Short options mirror cc: -c = --continue, -r = --resume.
        assert_eq!(
            parse_args(&strings(&["-c"])).unwrap(),
            CliArgs {
                session: SessionChoice::Continue,
                ..base()
            }
        );
        assert_eq!(
            parse_args(&strings(&["-r", "20260709-120000"])).unwrap(),
            CliArgs {
                session: SessionChoice::Resume("20260709-120000".into()),
                ..base()
            }
        );
        assert_eq!(
            parse_args(&strings(&["-r"])).unwrap(),
            CliArgs {
                session: SessionChoice::Pick,
                ..base()
            }
        );
        assert_eq!(
            parse_args(&strings(&["--serve"])).unwrap(),
            CliArgs {
                serve: true,
                ..base()
            }
        );
        assert_eq!(
            parse_args(&strings(&["--resume", "20260709-120000", "--plain"])).unwrap(),
            CliArgs {
                plain: true,
                session: SessionChoice::Resume("20260709-120000".into()),
                ..base()
            }
        );
        assert_eq!(
            parse_args(&strings(&["--list-sessions"])).unwrap(),
            CliArgs {
                list_sessions: true,
                ..base()
            }
        );
        // --permission-mode is the unified gate control: one flag, four modes.
        assert_eq!(
            parse_args(&strings(&["--permission-mode", "bypass"]))
                .unwrap()
                .permission_mode,
            Mode::Bypass
        );
        assert_eq!(
            parse_args(&strings(&["--permission-mode", "accept-edits"]))
                .unwrap()
                .permission_mode,
            Mode::AcceptEdits
        );
        assert_eq!(
            parse_args(&strings(&["--permission-mode", "manual"]))
                .unwrap()
                .permission_mode,
            Mode::Manual
        );
        // No flag at all is manual (the omit-the-flag default).
        assert_eq!(parse_args(&[]).unwrap().permission_mode, Mode::Manual);
        // `default` is gone — it is no longer an accepted value.
        assert!(parse_args(&strings(&["--permission-mode", "default"])).is_err());
        assert_eq!(
            parse_args(&strings(&["--permission-mode", "plan"]))
                .unwrap()
                .permission_mode,
            Mode::Plan
        );
        assert!(
            parse_args(&strings(&["--permission-mode", "wild"])).is_err(),
            "unknown mode rejected"
        );
        assert!(
            parse_args(&strings(&["--permission-mode"])).is_err(),
            "mode value required"
        );
        assert_eq!(
            parse_args(&strings(&["--fork", "20260709-120000#4"])).unwrap(),
            CliArgs {
                session: SessionChoice::Fork {
                    id: "20260709-120000".into(),
                    cut: Some(4),
                },
                ..base()
            }
        );
        assert_eq!(
            parse_args(&strings(&["--fork", "20260709-120000"]))
                .unwrap()
                .session,
            SessionChoice::Fork {
                id: "20260709-120000".into(),
                cut: None,
            }
        );
        // --image is repeatable and collects local paths in order.
        assert_eq!(
            parse_args(&strings(&["--image", "a.png", "--image", "b.jpg"]))
                .unwrap()
                .images,
            vec![PathBuf::from("a.png"), PathBuf::from("b.jpg")]
        );
        assert!(
            parse_args(&strings(&["--image"])).is_err(),
            "--image needs a path"
        );
        assert!(parse_args(&strings(&["--fork"])).is_err(), "id required");
        assert!(
            parse_args(&strings(&["--fork", "id#notanumber"])).is_err(),
            "seq must parse"
        );
        assert!(parse_args(&strings(&["--bogus"])).is_err());
    }

    #[test]
    fn parse_args_headless_flags() {
        // `--headless` is the switch (no short alias); a positional becomes the
        // prompt; --json and --max-rounds ride along.
        assert_eq!(
            parse_args(&strings(&["--headless", "fix the bug"])).unwrap(),
            CliArgs {
                headless: true,
                prompt: Some("fix the bug".into()),
                ..base()
            }
        );
        assert_eq!(
            parse_args(&strings(&[
                "--headless",
                "--json",
                "--max-rounds",
                "5",
                "do it"
            ]))
            .unwrap(),
            CliArgs {
                headless: true,
                json: true,
                max_rounds: Some(5),
                prompt: Some("do it".into()),
                ..base()
            }
        );
        // Headless composes with session selection (resume + run headless).
        assert_eq!(
            parse_args(&strings(&[
                "-r",
                "20260709-120000",
                "--headless",
                "continue"
            ]))
            .unwrap(),
            CliArgs {
                headless: true,
                prompt: Some("continue".into()),
                session: SessionChoice::Resume("20260709-120000".into()),
                ..base()
            }
        );
        // --headless with no prompt is legal (stdin supplies it at run time).
        assert_eq!(
            parse_args(&strings(&["--headless"])).unwrap(),
            CliArgs {
                headless: true,
                ..base()
            }
        );

        // Headless-only flags without --headless are rejected.
        assert!(
            parse_args(&strings(&["--json"])).is_err(),
            "--json needs --headless"
        );
        assert!(
            parse_args(&strings(&["--max-rounds", "3"])).is_err(),
            "--max-rounds needs --headless"
        );
        assert!(
            parse_args(&strings(&["a prompt"])).is_err(),
            "a bare prompt needs --headless"
        );
        // Two positionals, a bad round count, and mode conflicts are errors.
        assert!(
            parse_args(&strings(&["--headless", "one", "two"])).is_err(),
            "one prompt only"
        );
        assert!(
            parse_args(&strings(&["--headless", "--max-rounds", "0", "go"])).is_err(),
            "max-rounds >= 1"
        );
        assert!(
            parse_args(&strings(&["--headless", "--max-rounds", "x", "go"])).is_err(),
            "max-rounds must parse"
        );
        assert!(
            parse_args(&strings(&["--headless", "--serve"])).is_err(),
            "headless and serve conflict"
        );
    }

    /// `-V`/`--version` is its own exit path, and neither spelling touches any
    /// other field. `-v` is NOT it: the lowercase letter stays unparsed so a
    /// later verbose flag can have it.
    #[test]
    fn parse_args_version_flag() {
        for spelling in ["-V", "--version"] {
            assert_eq!(
                parse_args(&strings(&[spelling])).unwrap(),
                CliArgs {
                    version: true,
                    ..base()
                }
            );
        }
        assert!(
            parse_args(&strings(&["-v"])).is_err(),
            "-v is not --version"
        );
    }

    #[test]
    fn parse_args_help_flag() {
        assert_eq!(
            parse_args(&strings(&["-h"])).unwrap(),
            CliArgs {
                help: true,
                ..base()
            }
        );
        assert_eq!(
            parse_args(&strings(&["--help"])).unwrap(),
            CliArgs {
                help: true,
                ..base()
            }
        );
        // The usage text names every mode and the top-level flags.
        let help = help_text();
        for needle in [
            "--headless",
            "--json",
            "--max-rounds",
            "--permission-mode",
            "--serve",
            "-c, --continue",
            "-r, --resume",
            "-V, --version",
        ] {
            assert!(help.contains(needle), "help missing {needle}");
        }
        // Plan 172 took the environment out of provider configuration. The help
        // text outlived it by three plans, still telling people a `KLOOP_*` name
        // could override the file; nothing in the product reads one any more.
        assert!(
            !help.contains("KLOOP_"),
            "help still offers environment overrides"
        );
    }

    /// Two partitions under one root, each holding one transcript.
    fn two_project_store(tag: &str) -> (SessionStore, SessionDirs, SessionDirs) {
        let root = std::env::temp_dir().join(format!("kloop-args-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut dirs = Vec::new();
        for (id, anchor, session) in [
            ("p1_a", "/work/here", "20260101-000001"),
            ("p1_b", "/work/there", "20260202-000002"),
        ] {
            let partition = root.join("projects/v1").join(id);
            std::fs::create_dir_all(partition.join("sessions")).unwrap();
            std::fs::write(
                partition.join("project.json"),
                format!("{{\"version\":1,\"projectId\":\"{id}\",\"anchor\":\"{anchor}\"}}"),
            )
            .unwrap();
            std::fs::write(
                partition.join("sessions").join(format!("{session}.jsonl")),
                "",
            )
            .unwrap();
            dirs.push(SessionDirs {
                sessions: partition.join("sessions"),
                offload: partition.join("offload"),
            });
        }
        let second = dirs.pop().unwrap();
        (SessionStore::global(root), dirs.pop().unwrap(), second)
    }

    /// A session listed by `--list-sessions --all` is not silently adopted into
    /// the wrong repository: the error names the project that owns it.
    #[test]
    fn resuming_another_projects_session_names_that_project() {
        let (store, here, _there) = two_project_store("owning");

        assert!(session_in_this_project(&store, &here, "20260101-000001").is_ok());
        let error = session_in_this_project(&store, &here, "20260202-000002")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "session '20260202-000002' belongs to /work/there; run kloop there to resume it"
        );
        let missing = session_in_this_project(&store, &here, "20260303-000003")
            .unwrap_err()
            .to_string();
        assert_eq!(
            missing,
            "no session '20260303-000003' (try --list-sessions)"
        );
    }

    /// A shell holding only its preamble replays as an empty conversation. Its
    /// writer removes it on the way out, but a hard kill (and every one already
    /// on disk from before that) leaves one behind — and it must not win
    /// `--continue` by being the most recently touched file.
    #[test]
    fn an_empty_shell_never_wins_the_continue_pick() {
        use kloop_protocol::Message;

        let dir = std::env::temp_dir().join(format!("kloop-args-shell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let real = dir.join("20260101-000001.jsonl");
        let mut rollout = Rollout::new(real.clone());
        rollout.append_message(&Message::user_text("hi")).unwrap();
        drop(rollout);

        // A hard kill gives the writer no chance to remove its own file.
        let shell = dir.join("20260202-000002.jsonl");
        std::mem::forget(Rollout::new(shell.clone()));
        assert!(
            shell.exists(),
            "the shell is on disk, and it is the newer file"
        );

        let offered: Vec<PathBuf> = resumable_sessions(&dir)
            .into_iter()
            .map(|session| session.path)
            .collect();
        assert_eq!(offered, vec![real]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pick_index_covers_empty_valid_and_garbage() {
        assert_eq!(pick_index("", 5).unwrap(), 0);
        assert_eq!(pick_index("  \n", 5).unwrap(), 0);
        assert_eq!(pick_index("1", 5).unwrap(), 0);
        assert_eq!(pick_index(" 5 \n", 5).unwrap(), 4);
        assert!(pick_index("0", 5).is_err());
        assert!(pick_index("6", 5).is_err());
        assert!(pick_index("abc", 5).is_err());
    }
}
