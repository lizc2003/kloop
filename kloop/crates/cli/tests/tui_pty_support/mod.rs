use std::collections::VecDeque;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use portable_pty::native_pty_system;
use portable_pty::Child;
use portable_pty::CommandBuilder;
use portable_pty::ExitStatus;
use portable_pty::MasterPty;
use portable_pty::PtySize;
use serde_json::Value;
use tempfile::TempDir;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;

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
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .insert_header("connection", "close")
            .set_body_raw(response, "text/event-stream")
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
        let server = MockServer::start().await;
        let state = Arc::new(Mutex::new(FixtureState {
            responses: responses.into(),
            requests: Vec::new(),
        }));
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(QueueResponder {
                state: Arc::clone(&state),
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

#[derive(Clone, Debug)]
pub struct FrameSnapshot {
    pub rows: u16,
    pub cols: u16,
    pub cursor: (u16, u16),
    pub text: String,
    pub bracketed_paste: bool,
    pub cpr_count: usize,
    pub raw_len: usize,
}

impl FrameSnapshot {
    pub fn contains(&self, needle: &str) -> bool {
        self.text.contains(needle)
    }

    pub fn count(&self, needle: &str) -> usize {
        self.text.matches(needle).count()
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
}

impl TerminalState {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            scanner: CprScanner::default(),
            raw: Vec::new(),
            cpr_count: 0,
            error: None,
            reader_done: false,
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
        }
    }

    fn push_raw(&mut self, byte: u8) {
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
        let sandbox = tempfile::tempdir().context("create PTY sandbox")?;
        let home = sandbox.path().join("home");
        let xdg_config = sandbox.path().join("xdg-config");
        let xdg_cache = sandbox.path().join("xdg-cache");
        let workspace = sandbox.path().join("workspace");
        for directory in [&home, &xdg_config, &xdg_cache, &workspace] {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("create {}", directory.display()))?;
        }

        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open PTY")?;
        let mut command = CommandBuilder::new(program);
        command.env_clear();
        command.cwd(&workspace);
        command.env("HOME", &home);
        command.env("XDG_CONFIG_HOME", &xdg_config);
        command.env("XDG_CACHE_HOME", &xdg_cache);
        command.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("KLOOP_NO_ANIM", "1");
        command.env("KLOOP_PROVIDER", "openai-compat");
        command.env("OPENAI_API_KEY", "synthetic-tui-pty-key");
        command.env("OPENAI_MODEL", "tui-pty-model");
        command.env("OPENAI_BASE_URL", base_url);
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
        let terminal: SharedTerminal =
            Arc::new((Mutex::new(TerminalState::new(rows, cols)), Condvar::new()));
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
        state.parser.set_size(rows, cols);
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
        let deadline = Instant::now() + scaled_timeout(timeout);
        loop {
            let (state_lock, changed) = &*self.terminal;
            let state = state_lock.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(error) = &state.error {
                bail!("{description}: {error}\n{}", state.snapshot().debug_dump());
            }
            let snapshot = state.snapshot();
            if predicate(&snapshot) {
                return Ok(snapshot);
            }
            let raw_summary = bounded_raw_summary(&state.raw);
            let (state, _) = changed
                .wait_timeout(state, Duration::from_millis(25))
                .unwrap_or_else(|error| error.into_inner());
            drop(state);

            if self.exit_status.is_none() {
                if let Some(status) = self.child.try_wait().context("poll PTY child")? {
                    self.exit_status = Some(status.clone());
                    bail!(
                        "{description}: child exited early ({})\n{}\nraw: {raw_summary}",
                        status.exit_code(),
                        snapshot.debug_dump()
                    );
                }
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

    #[test]
    fn cpr_response_is_one_based() {
        assert_eq!(cpr_response((0, 0)), b"\x1b[1;1R");
        assert_eq!(cpr_response((8, 11)), b"\x1b[9;12R");
    }
}
