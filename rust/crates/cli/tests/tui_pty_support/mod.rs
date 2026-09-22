#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use portable_pty::Child;
use portable_pty::CommandBuilder;
use portable_pty::ExitStatus;
use portable_pty::MasterPty;
use portable_pty::PtySize;
use portable_pty::native_pty_system;
use serde_json::Value;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const CPR_QUERY: &[u8] = b"\x1b[6n";
const RAW_LIMIT: usize = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedRequest {
    pub model: String,
    pub user_texts: Vec<String>,
}

#[derive(Default)]
struct FixtureState {
    responses: VecDeque<String>,
    requests: Vec<RecordedRequest>,
}

#[derive(Clone)]
struct QueueResponder {
    state: Arc<Mutex<FixtureState>>,
    delay: Option<Duration>,
}

impl Respond for QueueResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let user_texts = body
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
            .filter_map(|message| message.get("content"))
            .map(content_text)
            .collect();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.requests.push(RecordedRequest { model, user_texts });
        let Some(response) = state.responses.pop_front() else {
            return ResponseTemplate::new(500).set_body_string("no synthetic response queued");
        };
        let template = ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .insert_header("connection", "close")
            .set_body_raw(response, "text/event-stream");
        match self.delay {
            Some(delay) => template.set_delay(delay),
            None => template,
        }
    }
}

fn content_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

pub struct ChatFixture {
    server: MockServer,
    state: Arc<Mutex<FixtureState>>,
}

impl ChatFixture {
    pub async fn start(responses: Vec<String>) -> Self {
        Self::start_with_delay(responses, None).await
    }

    pub async fn start_delayed(responses: Vec<String>, delay: Duration) -> Self {
        Self::start_with_delay(responses, Some(delay)).await
    }

    async fn start_with_delay(responses: Vec<String>, delay: Option<Duration>) -> Self {
        let server = MockServer::start().await;
        let state = Arc::new(Mutex::new(FixtureState {
            responses: responses.into(),
            requests: Vec::new(),
        }));
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(QueueResponder {
                state: Arc::clone(&state),
                delay,
            })
            .mount(&server)
            .await;
        Self { server, state }
    }

    pub fn uri(&self) -> String {
        self.server.uri()
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .requests
            .clone()
    }
}

pub fn sse_text(text: &str) -> String {
    let delta = serde_json::json!({
        "choices": [{"index": 0, "delta": {"content": text}}]
    });
    let finish = serde_json::json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    let usage = serde_json::json!({
        "choices": [],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    });
    format!("data: {delta}\n\ndata: {finish}\n\ndata: {usage}\n\ndata: [DONE]\n\n")
}

/// One SSE turn that calls a built-in tool, for scenarios that need a tool row
/// on screen. `arguments` is serialized the way a provider sends it: a JSON
/// string inside the delta, not a nested object.
pub fn sse_tool_call(id: &str, name: &str, arguments: &Value) -> String {
    let delta = serde_json::json!({
        "choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 0,
            "id": id,
            "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()},
        }]}}]
    });
    let finish = serde_json::json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
    });
    let usage = serde_json::json!({
        "choices": [],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    });
    format!("data: {delta}\n\ndata: {finish}\n\ndata: {usage}\n\ndata: [DONE]\n\n")
}

/// What a spawn varies beyond size. [`PtyHarness::spawn`] and
/// [`PtyHarness::spawn_with_args`] stay the short forms for the common cases.
#[derive(Default)]
pub struct PtyOptions<'a> {
    pub args: &'a [&'a str],
    /// Seeded into the workspace before the binary starts, as (path relative to
    /// the workspace, contents) — a tool scenario needs something to act on.
    pub files: &'a [(&'a str, &'a str)],
}

#[derive(Clone, Debug)]
pub struct FrameSnapshot {
    pub rows: u16,
    pub cols: u16,
    pub cursor: (u16, u16),
    pub text: String,
    pub bracketed_paste: bool,
    pub cpr_count: usize,
    pub raw_len: usize,
    /// Literal per-run values to replace before the frame becomes a baseline.
    redactions: Arc<Vec<(String, String)>>,
}

impl FrameSnapshot {
    pub fn contains(&self, needle: &str) -> bool {
        self.text.contains(needle)
    }

    pub fn count(&self, needle: &str) -> usize {
        self.text.matches(needle).count()
    }

    /// The whole screen as one assertable string, for an `insta` baseline.
    ///
    /// Deliberately not [`Self::debug_dump`]: that one is a diagnostic for a
    /// human reading a failure, and carries `cpr_count`/`raw_len` — counters
    /// that differ every run and would make a baseline flap. This carries only
    /// what a user could see: the size, the cursor, and the screen text, with
    /// the values that are new on every run normalized away.
    pub fn stable_text(&self) -> String {
        let mut screen = self.text.clone();
        for (from, to) in self.redactions.iter() {
            screen = screen.replace(from.as_str(), to);
        }
        let mut out = format!(
            "[{}x{} cursor={},{}]\n",
            self.rows, self.cols, self.cursor.0, self.cursor.1
        );
        let mut lines: Vec<String> = screen
            .lines()
            .map(|line| normalize_elapsed(line.trim_end()))
            .collect();
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    fn debug_dump(&self) -> String {
        let mut dump = format!(
            "frame={}x{} cursor={:?} cpr={} raw={}\n",
            self.rows, self.cols, self.cursor, self.cpr_count, self.raw_len
        );
        for (row, line) in self.text.lines().enumerate() {
            dump.push_str(&format!("{row:>3} | {}\n", line.trim_end()));
        }
        dump
    }
}

/// The stand-in for any wall-clock readout in a baseline.
const ELAPSED: &str = "<elapsed>";

/// `8s`, `1m05s`, `2h03m` — the shapes `anim::format_elapsed` produces.
fn is_elapsed(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_digit())
        && text.ends_with(['s', 'm'])
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, 's' | 'm' | 'h'))
}

/// Replace an elapsed readout on one screen line with [`ELAPSED`].
///
/// Both call sites are anchored on their surrounding text rather than on a
/// bare number, so a model reply that happens to say `30s` is left alone.
///
/// The turn-end rule needs more than a substitution: it pads with `─` out to
/// the transcript width, so `0s` and `10s` differ by a dash even after the
/// number is replaced. Rebuilding it to the width it already occupies is what
/// makes the two produce the same line.
fn normalize_elapsed(line: &str) -> String {
    const RULE: &str = "── Worked for ";
    const INTERRUPT: &str = " · esc to interrupt)";

    if let Some(rest) = line.strip_prefix(RULE)
        && let Some((elapsed, fill)) = rest.split_once(' ')
        && is_elapsed(elapsed)
        && !fill.is_empty()
        && fill.chars().all(|c| c == '─')
    {
        let label = format!("{RULE}{ELAPSED} ");
        let dashes = line.chars().count().saturating_sub(label.chars().count());
        return format!("{label}{}", "─".repeat(dashes));
    }

    if let Some(tail) = line.find(INTERRUPT)
        && let Some(open) = line[..tail].rfind('(')
        && is_elapsed(&line[open + 1..tail])
    {
        return format!("{}({ELAPSED}{}", &line[..open], &line[tail..]);
    }

    line.to_string()
}

#[derive(Default)]
struct CprScanner {
    matched: usize,
}

impl CprScanner {
    fn feed(&mut self, byte: u8) -> bool {
        if byte == CPR_QUERY[self.matched] {
            self.matched += 1;
            if self.matched == CPR_QUERY.len() {
                self.matched = 0;
                return true;
            }
            return false;
        }
        self.matched = usize::from(byte == CPR_QUERY[0]);
        false
    }
}

fn cpr_response(cursor: (u16, u16)) -> Vec<u8> {
    format!("\x1b[{};{}R", cursor.0 + 1, cursor.1 + 1).into_bytes()
}

struct TerminalState {
    parser: vt100::Parser,
    scanner: CprScanner,
    raw: Vec<u8>,
    cpr_count: usize,
    error: Option<String>,
    reader_done: bool,
    redactions: Arc<Vec<(String, String)>>,
    /// When the last byte arrived, so a caller can wait for the screen to stop
    /// moving rather than for one string to appear.
    last_write: Instant,
}

impl TerminalState {
    fn new(rows: u16, cols: u16, redactions: Arc<Vec<(String, String)>>) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            scanner: CprScanner::default(),
            raw: Vec::new(),
            cpr_count: 0,
            error: None,
            reader_done: false,
            redactions,
            last_write: Instant::now(),
        }
    }

    fn snapshot(&self) -> FrameSnapshot {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        FrameSnapshot {
            rows,
            cols,
            cursor: screen.cursor_position(),
            text: screen.contents(),
            bracketed_paste: screen.bracketed_paste(),
            cpr_count: self.cpr_count,
            raw_len: self.raw.len(),
            redactions: Arc::clone(&self.redactions),
        }
    }

    fn push_raw(&mut self, byte: u8) {
        self.last_write = Instant::now();
        if self.raw.len() < RAW_LIMIT {
            self.raw.push(byte);
        } else if self.error.is_none() {
            self.error = Some(format!("raw ANSI capture exceeded {RAW_LIMIT} bytes"));
        }
    }
}

type SharedTerminal = Arc<(Mutex<TerminalState>, Condvar)>;
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

pub struct PtyHarness {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: SharedWriter,
    terminal: SharedTerminal,
    reader: Option<JoinHandle<()>>,
    exit_status: Option<ExitStatus>,
    emergency_kill: Arc<AtomicBool>,
    _sandbox: TempDir,
}

impl PtyHarness {
    pub fn spawn(program: &Path, base_url: &str, rows: u16, cols: u16) -> Result<Self> {
        Self::spawn_with_args(program, base_url, rows, cols, &[])
    }

    pub fn spawn_with_args(
        program: &Path,
        base_url: &str,
        rows: u16,
        cols: u16,
        args: &[&str],
    ) -> Result<Self> {
        Self::spawn_with_options(
            program,
            base_url,
            rows,
            cols,
            &PtyOptions {
                args,
                ..PtyOptions::default()
            },
        )
    }

    pub fn spawn_with_options(
        program: &Path,
        base_url: &str,
        rows: u16,
        cols: u16,
        options: &PtyOptions<'_>,
    ) -> Result<Self> {
        let sandbox = tempfile::tempdir().context("create PTY sandbox")?;
        // macOS hands out temp dirs under `/var`, a symlink to `/private/var`,
        // and the child's own `current_dir()` comes back resolved. An
        // unresolved `$HOME` would then not be a prefix of it, so canonicalize
        // once here and derive every path the child sees from that.
        let root = std::fs::canonicalize(sandbox.path()).context("canonicalize PTY sandbox")?;
        let home = root.join("home");
        let xdg_config = root.join("xdg-config");
        let xdg_cache = root.join("xdg-cache");
        // The workspace lives inside `$HOME` so the session banner contracts it
        // to `~/workspace`. A temp path is a different length on every run, and
        // the banner's box is sized to its widest field — left alone, the box
        // would change width from one run to the next.
        let workspace = home.join("workspace");
        for directory in [&home, &xdg_config, &xdg_cache, &workspace] {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("create {}", directory.display()))?;
        }
        for (relative, contents) in options.files {
            let file = workspace.join(relative);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            std::fs::write(&file, contents).with_context(|| format!("seed {}", file.display()))?;
        }
        // The provider comes from the config file and nothing else (plan 172),
        // so the harness writes one instead of exporting OPENAI_* — same
        // profile the environment used to build.
        let kloop_home = home.join(".kloop");
        std::fs::create_dir_all(&kloop_home).context("create PTY ~/.kloop")?;
        let config = kloop_home.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "provider = \"openai-compat\"\n\n\
                 [providers.openai-compat]\n\
                 wire_api = \"chat\"\n\
                 base_url = \"{base_url}\"\n\
                 auth_header = {{ Authorization = \"Bearer synthetic-tui-pty-key\" }}\n\
                 model = \"tui-pty-model\"\n"
            ),
        )
        .context("write PTY config.toml")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&kloop_home, std::fs::Permissions::from_mode(0o700))
                .context("chmod PTY ~/.kloop")?;
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
                .context("chmod PTY config.toml")?;
        }
        // Workspace trust (plan 193) is answered on a TTY before a front-end
        // boots, and every scenario here is about what happens after boot — so
        // the sandbox starts out already trusted, the way a returning session
        // does. The id comes from the real resolver rather than a second copy
        // of the hashing rule.
        if let Some(project_id) =
            kloop_core::project::WorkspaceIdentity::resolve(&workspace).project_id()
        {
            let mut project = kloop_home.clone();
            for component in ["projects", "v1", project_id.as_str()] {
                project.push(component);
                std::fs::create_dir_all(&project)
                    .with_context(|| format!("create {}", project.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o700))
                        .with_context(|| format!("chmod {}", project.display()))?;
                }
            }
            let trust = project.join("trust.json");
            std::fs::write(
                &trust,
                format!(
                    "{{\n  \"version\": 1,\n  \"projectId\": \"{project_id}\",\n  \"trusted\": true\n}}\n"
                ),
            )
            .context("write PTY trust.json")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&trust, std::fs::Permissions::from_mode(0o600))
                    .context("chmod PTY trust.json")?;
            }
        }
        let redactions = Arc::new(redaction_table(base_url, sandbox.path(), &root));

        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open PTY")?;
        let mut command = CommandBuilder::new(program);
        command.args(options.args);
        command.env_clear();
        command.cwd(&workspace);
        command.env("HOME", &home);
        command.env("XDG_CONFIG_HOME", &xdg_config);
        command.env("XDG_CACHE_HOME", &xdg_cache);
        command.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("KLOOP_NO_ANIM", "1");
        // The banner carries the build stamp (plan 161), which moves with every
        // commit — pin it so the checked-in frames stay a fact about layout.
        command.env("KLOOP_VERSION", "v0.0.0 (0000000)");
        command.env("NO_PROXY", "127.0.0.1,localhost,::1");
        command.env("USER", "kloop-test");

        let child = pair
            .slave
            .spawn_command(command)
            .context("spawn kloop in PTY")?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().context("clone PTY reader")?;
        let writer: SharedWriter = Arc::new(Mutex::new(
            pair.master.take_writer().context("take PTY writer")?,
        ));
        let terminal: SharedTerminal = Arc::new((
            Mutex::new(TerminalState::new(rows, cols, redactions)),
            Condvar::new(),
        ));
        let reader_terminal = Arc::clone(&terminal);
        let reader_writer = Arc::clone(&writer);
        let reader_handle = thread::Builder::new()
            .name("kloop-tui-pty-reader".into())
            .spawn(move || {
                let mut chunk = [0u8; 8192];
                loop {
                    let read = match reader.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => read,
                        Err(_) => break,
                    };
                    for &byte in &chunk[..read] {
                        let response = {
                            let (state, changed) = &*reader_terminal;
                            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                            state.push_raw(byte);
                            state.parser.process(&[byte]);
                            let response = state.scanner.feed(byte).then(|| {
                                state.cpr_count += 1;
                                cpr_response(state.parser.screen().cursor_position())
                            });
                            changed.notify_all();
                            response
                        };
                        if let Some(response) = response {
                            let result = {
                                let mut writer = reader_writer
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner());
                                writer.write_all(&response).and_then(|()| writer.flush())
                            };
                            if let Err(error) = result {
                                let (state, changed) = &*reader_terminal;
                                let mut state =
                                    state.lock().unwrap_or_else(|poison| poison.into_inner());
                                state.error = Some(format!("write CPR response: {error}"));
                                changed.notify_all();
                                break;
                            }
                        }
                    }
                }
                let (state, changed) = &*reader_terminal;
                let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                state.reader_done = true;
                changed.notify_all();
            })
            .context("spawn PTY reader")?;

        Ok(Self {
            master: pair.master,
            child,
            writer,
            terminal,
            reader: Some(reader_handle),
            exit_status: None,
            emergency_kill: Arc::new(AtomicBool::new(false)),
            _sandbox: sandbox,
        })
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        writer.write_all(bytes).context("write PTY input")?;
        writer.flush().context("flush PTY input")
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize PTY")?;
        let (state, changed) = &*self.terminal;
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        // vt100 cannot observe the PTY ioctl itself. This only resizes the ANSI
        // decoding canvas; resize cases must separately assert application layout
        // facts (for example, the composer's width-dependent cursor column).
        state.parser.screen_mut().set_size(rows, cols);
        changed.notify_all();
        Ok(())
    }

    pub fn pty_size(&self) -> Result<(u16, u16)> {
        let size = self.master.get_size().context("read PTY size")?;
        Ok((size.rows, size.cols))
    }

    pub fn snapshot(&self) -> FrameSnapshot {
        self.terminal
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot()
    }

    pub fn raw_mark(&self) -> usize {
        self.terminal
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .raw
            .len()
    }

    pub fn raw(&self) -> Vec<u8> {
        self.terminal
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .raw
            .clone()
    }

    pub fn raw_since(&self, mark: usize) -> Vec<u8> {
        let state = self
            .terminal
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.raw[mark.min(state.raw.len())..].to_vec()
    }

    pub fn wait_for(
        &mut self,
        description: &str,
        timeout: Duration,
        predicate: impl Fn(&FrameSnapshot) -> bool,
    ) -> Result<FrameSnapshot> {
        self.wait_until(description, timeout, |frame, _quiet| predicate(frame))
    }

    /// Wait until `predicate` holds *and* nothing has been written for `idle`.
    ///
    /// [`Self::wait_for`] returns the first frame where a string is present,
    /// which is the wrong frame for a baseline: the repaint that follows it is
    /// still in flight, so the screen captured is a transitional one. This
    /// waits for the screen to stop moving instead — on the same condvar the
    /// reader already notifies, not on a fixed sleep.
    pub fn wait_for_quiescent(
        &mut self,
        description: &str,
        timeout: Duration,
        idle: Duration,
        predicate: impl Fn(&FrameSnapshot) -> bool,
    ) -> Result<FrameSnapshot> {
        let idle = scaled_timeout(idle);
        self.wait_until(description, timeout, move |frame, quiet| {
            quiet >= idle && predicate(frame)
        })
    }

    /// The shared poll loop: `ready` sees each frame plus how long the screen
    /// has been still.
    fn wait_until(
        &mut self,
        description: &str,
        timeout: Duration,
        ready: impl Fn(&FrameSnapshot, Duration) -> bool,
    ) -> Result<FrameSnapshot> {
        let deadline = Instant::now() + scaled_timeout(timeout);
        loop {
            let (state_lock, changed) = &*self.terminal;
            let state = state_lock.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(error) = &state.error {
                bail!("{description}: {error}\n{}", state.snapshot().debug_dump());
            }
            let snapshot = state.snapshot();
            if ready(&snapshot, state.last_write.elapsed()) {
                return Ok(snapshot);
            }
            let raw_summary = bounded_raw_summary(&state.raw);
            let (state, _) = changed
                .wait_timeout(state, Duration::from_millis(25))
                .unwrap_or_else(|error| error.into_inner());
            drop(state);

            if self.exit_status.is_none()
                && let Some(status) = self.child.try_wait().context("poll PTY child")?
            {
                self.exit_status = Some(status.clone());
                bail!(
                    "{description}: child exited early ({})\n{}\nraw: {raw_summary}",
                    status.exit_code(),
                    snapshot.debug_dump()
                );
            }
            if Instant::now() >= deadline {
                bail!(
                    "{description}: timed out\n{}\nraw: {raw_summary}",
                    snapshot.debug_dump()
                );
            }
        }
    }

    pub fn wait_for_exit(&mut self, timeout: Duration) -> Result<ExitStatus> {
        let deadline = Instant::now() + scaled_timeout(timeout);
        loop {
            if let Some(status) = &self.exit_status {
                return Ok(status.clone());
            }
            if let Some(status) = self.child.try_wait().context("wait for PTY child")? {
                self.exit_status = Some(status.clone());
                self.join_reader(Duration::from_secs(2));
                return Ok(status);
            }
            if Instant::now() >= deadline {
                bail!("PTY child did not exit before deadline");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn emergency_killed(&self) -> bool {
        self.emergency_kill.load(Ordering::SeqCst)
    }

    fn join_reader(&mut self, timeout: Duration) {
        let Some(handle) = self.reader.take() else {
            return;
        };
        let deadline = Instant::now() + timeout;
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if handle.is_finished() {
            let _ = handle.join();
        } else {
            self.reader = Some(handle);
        }
    }
}

impl Drop for PtyHarness {
    fn drop(&mut self) {
        if self.exit_status.is_none() {
            self.emergency_kill.store(true, Ordering::SeqCst);
            let _ = self.child.kill();
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                match self.child.try_wait() {
                    Ok(Some(status)) => {
                        self.exit_status = Some(status);
                        break;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(20)),
                    Err(_) => break,
                }
            }
        }
        self.join_reader(Duration::from_secs(1));
    }
}

/// The literal values a frame carries that are new on every run: the mock
/// provider's port and the sandbox's temp path (both spellings of it on macOS,
/// resolved and not). Longest first, so `/private/var/…` is consumed before the
/// `/var/…` that is its suffix-sharing sibling.
fn redaction_table(base_url: &str, sandbox: &Path, root: &Path) -> Vec<(String, String)> {
    let mut table = vec![(base_url.to_string(), "http://mock-provider".to_string())];
    for path in [root, sandbox] {
        let text = path.display().to_string();
        if !table.iter().any(|(from, _)| *from == text) {
            table.push((text, "/sandbox".to_string()));
        }
    }
    table.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    table
}

pub fn scaled_timeout(timeout: Duration) -> Duration {
    if std::env::var_os("CI").is_some() {
        timeout.saturating_mul(4)
    } else {
        timeout
    }
}

fn bounded_raw_summary(raw: &[u8]) -> String {
    let start = raw.len().saturating_sub(2048);
    raw[start..]
        .iter()
        .map(|byte| match byte {
            b'\n' => "\\n".to_string(),
            b'\r' => "\\r".to_string(),
            b'\t' => "\\t".to_string(),
            0x1b => "\\x1b".to_string(),
            0x20..=0x7e => char::from(*byte).to_string(),
            _ => format!("\\x{byte:02x}"),
        })
        .collect()
}

pub fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    find_bytes(haystack, needle, 0).is_some()
}

pub fn find_bytes(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(start.min(haystack.len()));
    }
    haystack
        .get(start..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| start + offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpr_scanner_handles_every_split_and_multiple_queries() {
        for split in 0..=CPR_QUERY.len() {
            let mut scanner = CprScanner::default();
            let first = CPR_QUERY[..split]
                .iter()
                .filter(|byte| scanner.feed(**byte))
                .count();
            let second = CPR_QUERY[split..]
                .iter()
                .filter(|byte| scanner.feed(**byte))
                .count();
            assert_eq!(first + second, 1, "split {split}");
        }

        let mut scanner = CprScanner::default();
        let stream = b"plain\x1b[31m\x1b[6nmore\x1b[6n";
        assert_eq!(stream.iter().filter(|byte| scanner.feed(**byte)).count(), 2);
    }

    /// The turn-end rule pads to the full width, so a longer elapsed eats a
    /// dash. Both spellings must normalize to the same line, at the same width.
    #[test]
    fn a_turn_end_rule_normalizes_the_same_whatever_the_elapsed() {
        let short = format!("── Worked for 0s {}", "─".repeat(63));
        let long = format!("── Worked for 12m30s {}", "─".repeat(59));
        assert_eq!(short.chars().count(), 80);
        assert_eq!(long.chars().count(), 80);
        assert_eq!(normalize_elapsed(&short), normalize_elapsed(&long));
        assert_eq!(normalize_elapsed(&short).chars().count(), 80);
    }

    /// Both replacements are anchored on the text around them, so a reply that
    /// happens to mention a duration keeps it — the point of a baseline is that
    /// the model's own words reach it unchanged.
    #[test]
    fn only_anchored_elapsed_readouts_are_replaced() {
        assert_eq!(
            normalize_elapsed("● Working (8s · esc to interrupt)"),
            "● Working (<elapsed> · esc to interrupt)"
        );
        let prose = "the retry backs off after 30s and gives up";
        assert_eq!(normalize_elapsed(prose), prose);
        let not_a_rule = "── Worked for ever ───";
        assert_eq!(normalize_elapsed(not_a_rule), not_a_rule);
    }

    #[test]
    fn the_redaction_table_consumes_the_longer_temp_path_first() {
        let table = redaction_table(
            "http://127.0.0.1:53421",
            Path::new("/var/folders/ab/T/.tmp123"),
            Path::new("/private/var/folders/ab/T/.tmp123"),
        );
        let mut text = "cwd /private/var/folders/ab/T/.tmp123/workspace via http://127.0.0.1:53421"
            .to_string();
        for (from, to) in &table {
            text = text.replace(from, to);
        }
        assert_eq!(text, "cwd /sandbox/workspace via http://mock-provider");
    }

    #[test]
    fn cpr_response_is_one_based() {
        assert_eq!(cpr_response((0, 0)), b"\x1b[1;1R");
        assert_eq!(cpr_response((8, 11)), b"\x1b[9;12R");
    }
}
