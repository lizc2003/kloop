//! plan 283 阶段 0：离线回放，量这一版权限层在本机真实会话上会问多少次。
//!
//! 读会话文件里的工具调用，按时间顺序送进真实的权限门；批准者总选最宽的作用域，
//! 所以凡是“记住”能省掉的询问都会省掉，剩下的就是记忆省不掉的那部分。被拒后
//! 升级没法离线重跑，只能看原来那次结果里有没有沙箱拒绝的痕迹（拒绝提示、
//! 升级重跑前缀、拒绝升级的说明、已记住的升级前缀），有就替它问一次升级。
//!
//! 语料只在本机（默认 `~/.kloop/projects/v1`），不进仓库；没有语料时跳过：
//!
//!     cargo test -p kloop-core --lib approval_replay -- --ignored --nocapture
//!
//! - `KLOOP_REPLAY_DIR`：语料根，下面一层是项目分区目录。
//! - `KLOOP_REPLAY_SESSIONS`：取最近多少个根会话（子 agent 的文件并进它的根），默认 40。
//! - `KLOOP_REPLAY_OUT`：另把汇总写成 JSON，供下一阶段对比。
//! - `KLOOP_REPLAY_ASSUME_DENIED`：原来脱沙箱跑、现在会进沙箱的调用，结果里没有拒绝痕迹，
//!   离线不知道会不会被拒；设了它就按“全部被拒”替它们问升级，得到这部分询问的上界。
//!
//! 调用先过生产入口的 `normalize_tool_uses`（拆 `call_tool` 外壳、参数别名改名），权限门看到的
//! 和真实分发一样。简化：每个项目的持久规则从窗口开头的空集开始累积；全局 deny/ask 规则、
//! 权限模式都不回放（一律 manual）；fork 出来的会话按 tool_use id 去重。

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;
use serde_json::json;

use super::ApprovalAnswer;
use super::ApprovalDoor;
use super::ApprovalRecord;
use super::ApprovalScope;
use super::Approver;
use super::ConfirmRequest;
use super::Decision;
use super::GlobalPermissionPolicy;
use super::Mode;
use super::PermissionSession;
use super::Permissions;
use super::ProjectPermissionPolicy;
use super::ProjectPermissionWriter;
use super::ProjectPolicySnapshot;
use super::SandboxPlan;
use super::testutil::ScriptedWriter;
use crate::project::WorkspaceIdentity;
use crate::sandbox::DenialKind;
use crate::sandbox::ExecutionContext;
use crate::sandbox::SandboxDenial;
use crate::sandbox::SandboxPolicy;

/// Says yes with the widest scope offered, so every memory that could spare a
/// later question gets the chance to. For the network that is the session:
/// every command for the rest of it, where the project scope covers only the
/// programs it names (plan 283 stage 2b).
struct WidestScope;

impl Approver for WidestScope {
    fn confirm(&self, req: ConfirmRequest) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>> {
        let network = req
            .remember_rules
            .iter()
            .flatten()
            .any(|line| line.contains("every bash command may reach the network"));
        let order = if network {
            [
                ApprovalScope::WorkspaceSession,
                ApprovalScope::Project,
                ApprovalScope::Once,
            ]
        } else {
            [
                ApprovalScope::Project,
                ApprovalScope::WorkspaceSession,
                ApprovalScope::Once,
            ]
        };
        let scope = order
            .into_iter()
            .find(|scope| req.approval_scopes.contains(scope))
            .unwrap_or(ApprovalScope::Once);
        Box::pin(async move { Decision::Allow(scope) })
    }
}

struct Call {
    ts: u64,
    order: usize,
    depth: u8,
    cwd: PathBuf,
    id: String,
    name: String,
    input: Value,
    result: Option<String>,
}

struct SessionFile {
    stem: String,
    partition: String,
    parent: Option<String>,
    start: u64,
    calls: Vec<Call>,
}

/// The directory a partition's sessions ran in, when the session file does not
/// say (only server threads record a runtime line): the project anchor, which
/// is the Git common directory for a repository and the directory otherwise.
/// A session that ran in a linked worktree is replayed against the main
/// checkout — the same project, a different workspace.
fn partition_cwd(partition: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(partition.join("project.json")).ok()?;
    let anchor = PathBuf::from(serde_json::from_str::<Value>(&raw).ok()?["anchor"].as_str()?);
    Some(if anchor.file_name().is_some_and(|name| name == ".git") {
        anchor.parent()?.to_path_buf()
    } else {
        anchor
    })
}

fn read_session(path: &Path, partition: &str, default_cwd: Option<&Path>) -> Option<SessionFile> {
    let raw = std::fs::read_to_string(path).ok()?;
    // Stems are timestamps, unique only within a partition.
    let stem = format!("{partition}/{}", path.file_stem()?.to_string_lossy());
    let mut cwd: Option<PathBuf> = default_cwd.map(Path::to_path_buf);
    let mut parent = None;
    let mut start = None;
    let mut calls = Vec::new();
    let mut results: HashMap<String, String> = HashMap::new();
    for (order, line) in raw.lines().enumerate() {
        let Ok(line) = serde_json::from_str::<Value>(line) else {
            break;
        };
        let ts = line["ts"].as_u64().unwrap_or(0);
        start.get_or_insert(ts);
        if let Some(of) = line["subagent_of"].as_str() {
            parent = of
                .rsplit_once('#')
                .map(|(stem, _)| format!("{partition}/{stem}"));
        }
        match line["type"].as_str() {
            Some("session") => {
                if let Some(dir) = line["runtime"]["cwd"].as_str() {
                    cwd = Some(PathBuf::from(dir));
                }
            }
            Some("message") => {
                let Some(blocks) = line["content"].as_array() else {
                    continue;
                };
                for block in blocks {
                    match block["type"].as_str() {
                        Some("tool_use") => {
                            let Some(cwd) = cwd.clone() else {
                                continue;
                            };
                            calls.push(Call {
                                ts,
                                order,
                                depth: u8::from(parent.is_some()),
                                cwd,
                                id: block["id"].as_str().unwrap_or_default().to_string(),
                                name: block["name"].as_str().unwrap_or_default().to_string(),
                                input: block["input"].clone(),
                                result: None,
                            });
                        }
                        Some("tool_result") => {
                            let text = match &block["content"] {
                                Value::String(text) => text.clone(),
                                Value::Array(parts) => parts
                                    .iter()
                                    .filter_map(|part| part["text"].as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                _ => String::new(),
                            };
                            if let Some(id) = block["tool_use_id"].as_str() {
                                results.insert(id.to_string(), text);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    for call in &mut calls {
        call.result = results.remove(&call.id);
    }
    Some(SessionFile {
        stem,
        partition: partition.to_string(),
        parent,
        start: start?,
        calls,
    })
}

/// What the recorded run says the sandbox did to this bash call: `Some(None)`
/// for a denial of unknown class, `None` when nothing says it was denied.
fn recorded_denial(result: &str) -> Option<Option<SandboxDenial>> {
    if let Some(rest) = result
        .strip_prefix("[Re-ran without the sandbox")
        .and_then(|rest| rest.split_once("it had been denied (").map(|(_, r)| r))
    {
        let (label, evidence) = rest.split_once("): ")?;
        let kind = match label {
            "file write" => DenialKind::Write,
            "network" => DenialKind::Network,
            "protected read" => DenialKind::Read,
            "nested sandbox" => DenialKind::Nested,
            _ => DenialKind::Unclassified,
        };
        let evidence = evidence.lines().next().unwrap_or_default();
        return Some(Some(SandboxDenial {
            kind,
            evidence: evidence.trim_end_matches(']').to_string(),
            paths: Vec::new(),
        }));
    }
    if result.starts_with("[Ran without the sandbox: a remembered") {
        return Some(None);
    }
    // The hints are appended as the result's last line; the same words inside
    // the output (a file that documents them) are not a denial.
    let last = result.trim_end().lines().last().unwrap_or_default();
    if last.starts_with("[This command ran inside kloop's sandbox")
        || last.starts_with("[The user declined to run this outside kloop's sandbox")
    {
        return Some(crate::sandbox::classify_sandbox_denial(
            Some(1),
            result,
            /*network_disabled*/ true,
        ));
    }
    None
}

/// What the gate sees of a recorded call: the production entry point's
/// normalization (the inner tool of a `call_tool` envelope, canonical argument
/// names), without the removed `disable_sandbox` — a model sends the same call
/// without it now (plan 283) — plus whether the recording carried it.
fn replayed_call(call: &Call) -> Option<(String, Value, bool)> {
    let (_, name, mut input) = crate::tools::normalize_tool_uses(vec![(
        call.id.clone(),
        call.name.clone(),
        call.input.clone(),
    )])
    .pop()?;
    let no_sandbox = name == "bash" && input["disable_sandbox"].as_bool().unwrap_or(false);
    if let Some(fields) = input.as_object_mut() {
        fields.remove("disable_sandbox");
    }
    Some((name, input, no_sandbox))
}

#[derive(Default)]
struct Tally {
    calls: usize,
    bash: usize,
    no_sandbox: usize,
    asks: BTreeMap<String, usize>,
    remembered: BTreeMap<String, usize>,
    /// Ran uncontained in the recording, contained now: whether the sandbox
    /// would have refused it cannot be known offline.
    unknown_outcome: usize,
}

impl Tally {
    fn asked(&self) -> usize {
        self.asks.values().sum()
    }

    fn add(&mut self, other: &Tally) {
        self.calls += other.calls;
        self.bash += other.bash;
        self.no_sandbox += other.no_sandbox;
        for (key, count) in &other.asks {
            *self.asks.entry(key.clone()).or_default() += count;
        }
        for (key, count) in &other.remembered {
            *self.remembered.entry(key.clone()).or_default() += count;
        }
        self.unknown_outcome += other.unknown_outcome;
    }

    fn to_json(&self) -> Value {
        json!({
            "calls": self.calls,
            "bash": self.bash,
            "no_sandbox": self.no_sandbox,
            "asked": self.asked(),
            "asks": self.asks,
            "remembered": self.remembered,
            "unknown_outcome": self.unknown_outcome,
        })
    }
}

fn reason_label(record: &ApprovalRecord) -> String {
    let door = match record.door {
        ApprovalDoor::Gate => "gate",
        ApprovalDoor::Escalation => "escalation",
    };
    let reason = serde_json::to_value(&record.reason).unwrap_or_default();
    let mut label = format!("{door}/{}", reason["kind"].as_str().unwrap_or("?"));
    for detail in ["tag", "denial"] {
        if let Some(value) = reason[detail].as_str() {
            label.push_str(&format!(":{value}"));
        }
    }
    label
}

/// Across all sessions: how often each exact question came back, the widest
/// memory each class of question could be given, and what it was asked about.
#[derive(Default)]
struct Detail {
    repeats: HashMap<String, usize>,
    widest: BTreeMap<String, BTreeMap<&'static str, usize>>,
    examples: BTreeMap<String, BTreeMap<String, usize>>,
}

fn tally_record(
    tally: &mut Tally,
    detail: &mut Detail,
    records: impl IntoIterator<Item = ApprovalRecord>,
) {
    for record in records {
        tally_one(tally, detail, record);
    }
}

fn tally_one(tally: &mut Tally, detail: &mut Detail, record: ApprovalRecord) {
    let label = reason_label(&record);
    match record.answer {
        ApprovalAnswer::Remembered { by } => {
            *tally
                .remembered
                .entry(format!("{label} by {by:?}"))
                .or_default() += 1;
        }
        ApprovalAnswer::Allowed { .. }
        | ApprovalAnswer::Denied
        | ApprovalAnswer::Stopped
        | ApprovalAnswer::NoApprover => {
            *tally.asks.entry(label.clone()).or_default() += 1;
            let key = if record.keys.is_empty() {
                format!("{label} | (no key) {}", record.detail)
            } else {
                format!("{label} | {}", record.keys.join(" + "))
            };
            *detail.repeats.entry(key).or_default() += 1;
            let widest = if record.offered.contains(&ApprovalScope::Project) {
                "project"
            } else if record.offered.contains(&ApprovalScope::WorkspaceSession) {
                if record.keys.iter().any(|key| key.contains("-script:")) {
                    "session (exact text)"
                } else {
                    "session"
                }
            } else {
                "once"
            };
            *detail
                .widest
                .entry(label.clone())
                .or_default()
                .entry(widest)
                .or_default() += 1;
            *detail
                .examples
                .entry(label)
                .or_default()
                .entry(clip(&record.detail.replace('\n', " ⏎ "), 110))
                .or_default() += 1;
        }
    }
}

/// The questions a recorded denial leads to now (plan 283 stage 2b): a refused
/// network is asked as a capability — and not at all when a remembered consent
/// already gave the call the network — and anything else as the full
/// escalation. Recorded writes carry no paths, so they cannot be offered as a
/// directory and go to the full escalation as before.
async fn replay_escalation(
    gate: &Permissions,
    command: &str,
    denial: Option<SandboxDenial>,
    network_granted: bool,
    depth: u8,
) -> Option<ApprovalRecord> {
    match denial {
        Some(denial) if denial.kind == DenialKind::Network => {
            if network_granted {
                return None;
            }
            let programs = crate::shell::analyze_bash(command)
                .commands()
                .and_then(super::network_programs)
                .filter(|programs| !programs.is_empty());
            gate.escalate_capability(
                command,
                &denial,
                &super::CapabilityRequest::Network { programs },
                depth,
            )
            .await
            .approval
        }
        denial => {
            gate.escalate_sandbox(command, denial.as_ref(), depth)
                .await
                .approval
        }
    }
}

fn corpus_root() -> Option<PathBuf> {
    std::env::var_os("KLOOP_REPLAY_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".kloop/projects/v1")))
        .filter(|dir| dir.is_dir())
}

fn clip(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(chars).collect();
    clipped.push('…');
    clipped
}

#[tokio::test]
#[ignore = "reads the local session corpus; run by hand (plan 283)"]
async fn approval_replay() {
    let Some(root) = corpus_root() else {
        println!("skipped: no session corpus (set KLOOP_REPLAY_DIR)");
        return;
    };
    // `network` assumes those calls were refused the network; any other value,
    // an unclassified refusal.
    let assume_denied = std::env::var("KLOOP_REPLAY_ASSUME_DENIED")
        .ok()
        .map(|kind| (kind == "network").then_some(DenialKind::Network));
    let window: usize = std::env::var("KLOOP_REPLAY_SESSIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(40);

    let mut files: HashMap<String, SessionFile> = HashMap::new();
    for partition in std::fs::read_dir(&root).unwrap().flatten() {
        let partition_name = partition.file_name().to_string_lossy().into_owned();
        let default_cwd = partition_cwd(&partition.path());
        let Ok(entries) = std::fs::read_dir(partition.path()) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        if let Ok(sessions) = std::fs::read_dir(partition.path().join("sessions")) {
            paths.extend(sessions.flatten().map(|entry| entry.path()));
        }
        for path in paths {
            if path.extension().is_some_and(|ext| ext == "jsonl")
                && let Some(file) = read_session(&path, &partition_name, default_cwd.as_deref())
            {
                files.insert(file.stem.clone(), file);
            }
        }
    }
    let root_of = |stem: &str| {
        let mut stem = stem.to_string();
        for _ in 0..16 {
            match files.get(&stem).and_then(|file| file.parent.clone()) {
                Some(parent) if files.contains_key(&parent) => stem = parent,
                _ => break,
            }
        }
        stem
    };
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for stem in files.keys() {
        groups.entry(root_of(stem)).or_default().push(stem.clone());
    }
    let mut roots: Vec<String> = groups.keys().cloned().collect();
    roots.sort_by_key(|stem| (files[stem].start, stem.clone()));
    let roots: Vec<String> = roots
        .into_iter()
        .filter(|stem| groups[stem].iter().any(|s| !files[s].calls.is_empty()))
        .collect();
    let roots = &roots[roots.len().saturating_sub(window)..];

    let home = std::env::home_dir();
    let mut projects: HashMap<String, Arc<ProjectPermissionPolicy>> = HashMap::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut detail = Detail::default();
    let mut total = Tally::default();
    let mut rows = Vec::new();
    println!(
        "{:<16} {:<16} {:>5} {:>4} {:>4} {:>4}  asks by reason",
        "session", "cwd", "calls", "bash", "nosb", "ask"
    );
    for root_stem in roots {
        let partition = files[root_stem].partition.clone();
        let mut calls: Vec<&Call> = groups[root_stem]
            .iter()
            .flat_map(|stem| files[stem].calls.iter())
            .collect();
        calls.sort_by_key(|call| (call.ts, call.order));
        let Some(first) = calls.first() else {
            continue;
        };
        let project = Arc::clone(projects.entry(partition.clone()).or_insert_with(|| {
            let identity = WorkspaceIdentity::for_replay(first.cwd.clone(), &partition);
            let writer: Arc<dyn ProjectPermissionWriter> = ScriptedWriter::succeeding();
            Arc::new(ProjectPermissionPolicy::available(
                identity.project_id().unwrap().clone(),
                ProjectPolicySnapshot::empty(),
                Some(writer),
            ))
        }));
        let session = Arc::new(PermissionSession::new(
            Mode::Manual,
            Some(Arc::new(WidestScope)),
        ));
        let gate_for = |cwd: &Path| {
            Permissions::from_layers(
                Arc::new(GlobalPermissionPolicy::empty()),
                Arc::clone(&project),
                Arc::clone(&session),
                WorkspaceIdentity::for_replay(cwd.to_path_buf(), &partition),
            )
        };
        let mut tally = Tally::default();
        for call in calls {
            if !call.id.is_empty() && !seen_ids.insert(call.id.clone()) {
                continue;
            }
            tally.calls += 1;
            let gate = gate_for(&call.cwd);
            let mut policy = SandboxPolicy::workspace(&call.cwd, &[], false);
            if let Some(home) = &home {
                policy = policy.with_credential_roots(home);
            }
            let Some((name, input, no_sandbox)) = replayed_call(call) else {
                continue;
            };
            if name == "bash" {
                tally.bash += 1;
                tally.no_sandbox += usize::from(no_sandbox);
            }
            // The executor's choice of sandbox context, as dispatch makes it.
            let sandbox = (name == "bash").then_some(&policy);
            let outcome = gate
                .gate_call(
                    &name,
                    &input,
                    None,
                    None,
                    call.depth,
                    ExecutionContext::sandboxed(sandbox),
                    None,
                )
                .await;
            tally_record(&mut tally, &mut detail, outcome.approvals);
            let Ok(verdict) = outcome.result else {
                continue;
            };
            let network_granted = verdict.grants.network;
            if name != "bash" || input["background"].as_bool() == Some(true) {
                continue;
            }
            match verdict.sandbox {
                // Recorded by the gate as `escalated_before_run`.
                SandboxPlan::Escalated => {}
                SandboxPlan::Contained => {
                    let command = input["command"].as_str().unwrap_or_default();
                    let assumed = |kind: Option<DenialKind>| {
                        kind.map(|kind| SandboxDenial {
                            kind,
                            evidence: "assumed".into(),
                            paths: Vec::new(),
                        })
                    };
                    let denial = match call.result.as_deref().and_then(recorded_denial) {
                        Some(denial) => Some(denial),
                        None if no_sandbox => {
                            tally.unknown_outcome += 1;
                            assume_denied.map(assumed)
                        }
                        None => None,
                    };
                    if let Some(denial) = denial {
                        let records =
                            replay_escalation(&gate, command, denial, network_granted, call.depth)
                                .await;
                        tally_record(&mut tally, &mut detail, records);
                    }
                }
                SandboxPlan::None => {}
            }
        }
        let cwd = files[root_stem]
            .calls
            .first()
            .and_then(|call| call.cwd.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let reasons: Vec<String> = tally
            .asks
            .iter()
            .map(|(label, count)| format!("{label}×{count}"))
            .collect();
        println!(
            "{:<16} {:<16} {:>5} {:>4} {:>4} {:>4}  {}",
            clip(root_stem.rsplit('/').next().unwrap_or(root_stem), 15),
            clip(&cwd, 15),
            tally.calls,
            tally.bash,
            tally.no_sandbox,
            tally.asked(),
            reasons.join(", ")
        );
        rows.push(json!({
            "session": root_stem,
            "cwd": cwd,
            "start": files[root_stem].start,
            "tally": tally.to_json(),
        }));
        total.add(&tally);
    }

    let sessions = rows.len();
    let mut per_session: Vec<usize> = rows
        .iter()
        .map(|row| row["tally"]["asked"].as_u64().unwrap_or(0) as usize)
        .collect();
    per_session.sort_unstable();
    let median = per_session.get(per_session.len() / 2).copied().unwrap_or(0);
    println!(
        "\n{sessions} sessions, {} calls ({} bash, {} with disable_sandbox): {} questions, \
         median {median}/session, max {}",
        total.calls,
        total.bash,
        total.no_sandbox,
        total.asked(),
        per_session.last().copied().unwrap_or(0),
    );
    println!("questions by reason (widest memory offered; most frequent commands):");
    for (label, count) in &total.asks {
        let widest: Vec<String> = detail.widest[label]
            .iter()
            .map(|(scope, n)| format!("{scope} {n}"))
            .collect();
        println!("  {count:>5}  {label}  [{}]", widest.join(", "));
        let mut examples: Vec<(&String, &usize)> = detail.examples[label].iter().collect();
        examples.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (example, n) in examples.iter().take(6) {
            println!("         {n:>3}× {example}");
        }
    }
    println!("answered by a memory:");
    for (label, count) in &total.remembered {
        println!("  {count:>5}  {label}");
    }
    println!(
        "ran uncontained in the recording, contained now (outcome unknown): {}",
        total.unknown_outcome
    );
    let mut repeated: Vec<(&String, &usize)> = detail
        .repeats
        .iter()
        .filter(|(_, count)| **count > 1)
        .collect();
    repeated.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    println!("the same question asked again (memory did not cover it):");
    for (key, count) in repeated.iter().take(20) {
        println!("  {count:>5}  {}", clip(key, 140));
    }

    if let Some(out) = std::env::var_os("KLOOP_REPLAY_OUT") {
        let report = json!({
            "sessions": rows,
            "total": total.to_json(),
            "median_asked": median,
        });
        std::fs::write(out, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
}

/// The replay gates what dispatch would gate: two MCP tools behind `call_tool`
/// are two tools, not one `call_tool` permission (plan 283 review).
#[test]
fn replay_gates_the_normalized_call() {
    let call = |name: &str, input: Value| Call {
        ts: 0,
        order: 0,
        depth: 0,
        cwd: PathBuf::from("/w"),
        id: "c1".into(),
        name: name.into(),
        input,
        result: None,
    };
    let wrapped = call(
        "call_tool",
        json!({"tool_name": "mcp__notes__search", "params": {"query": "x"}}),
    );
    assert_eq!(
        replayed_call(&wrapped),
        Some((
            "mcp__notes__search".to_string(),
            json!({"query": "x"}),
            false
        ))
    );
    let escaped = call(
        "bash",
        json!({"command": "cargo test", "disable_sandbox": true}),
    );
    assert_eq!(
        replayed_call(&escaped),
        Some(("bash".to_string(), json!({"command": "cargo test"}), true))
    );
}
