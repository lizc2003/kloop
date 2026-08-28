//! The plain terminal frontend: the blocking y/a/p/n approval prompt
//! ([`CliApprover`]) and the streaming stdout renderer ([`StdoutUi`]) the
//! `--plain`/`--mock` REPL installs, plus the ANSI diff colorizer they share.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::Mutex;

use kloop_core::agent::Ui;
use kloop_core::event::Delta;
use kloop_core::event::Event;
use kloop_core::event::Item;
use kloop_core::interaction::QuestionAnswer;
use kloop_core::interaction::QuestionOutcome;
use kloop_core::interaction::QuestionRequest;
use kloop_core::interaction::Questioner;
use kloop_core::permissions::ApprovalScope;
use kloop_core::permissions::Approver;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::permissions::Decision;

/// ANSI-color a diff preview for the plain REPL: additions green, deletions
/// red, everything else (context, hunk gaps, markers) dim.
fn color_diff(preview: &str) -> String {
    preview
        .lines()
        .map(|line| match line.chars().next() {
            Some('+') => format!("\x1b[32m{line}\x1b[0m"),
            Some('-') => format!("\x1b[31m{line}\x1b[0m"),
            _ => format!("\x1b[2m{line}\x1b[0m"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Interactive y/a/p/n prompt on the terminal. The REPL's own stdin reader
/// is idle while a turn runs, so a direct blocking read is safe; if the turn
/// is Ctrl+C-interrupted mid-prompt, the orphaned read may swallow one
/// subsequent input line — accepted edge for a line-based REPL.
#[derive(Default)]
pub(crate) struct CliApprover {
    /// Parallel sub-agents ask concurrently; one prompt owns the terminal at
    /// a time, the rest wait here (the TUI gets the same via its queue).
    prompting: tokio::sync::Mutex<()>,
}

impl Approver for CliApprover {
    fn confirm(
        &self,
        req: ConfirmRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Decision> + Send + '_>> {
        Box::pin(async move {
            let _one_at_a_time = self.prompting.lock().await;
            let options = approval_options(&req);
            let preview = req
                .preview
                .as_deref()
                .map(|p| format!("\n{}", color_diff(p)))
                .unwrap_or_default();
            // Same layering as the TUI panel, flattened for a line-oriented
            // terminal: what kind of action, what it acts on, why it is asked.
            let heading = req.title.as_deref().unwrap_or("approve?");
            let subject = match (&req.title, &req.detail) {
                (Some(_), Some(detail)) => detail.as_str(),
                _ => req.description.as_str(),
            };
            let notice = req
                .notice
                .as_deref()
                .map(|notice| format!("\n  ⚠ {notice}"))
                .unwrap_or_default();
            print!("\n[{heading}] {subject}{notice}{preview}\n  {options} > ");
            let _ = std::io::stdout().flush();
            let line = tokio::task::spawn_blocking(|| {
                let mut buf = String::new();
                std::io::stdin().read_line(&mut buf).map(|_| buf)
            })
            .await;
            match line {
                Ok(Ok(answer)) => approval_decision(answer.trim(), &req),
                // Reader died or stdin closed: the safe answer is no.
                _ => Decision::Deny,
            }
        })
    }
}

fn approval_options(request: &ConfirmRequest) -> String {
    let mut options = Vec::new();
    if request.approval_scopes.contains(&ApprovalScope::Once) {
        options.push("y = allow once");
    }
    if request
        .approval_scopes
        .contains(&ApprovalScope::WorkspaceSession)
    {
        options.push("a = allow for this workspace session");
    }
    if request.approval_scopes.contains(&ApprovalScope::Project) {
        options.push("p = allow for this project across sessions and linked worktrees");
    }
    options.push("n = deny");
    options.join(" / ")
}

fn approval_decision(answer: &str, request: &ConfirmRequest) -> Decision {
    let scope = match answer.to_ascii_lowercase().as_str() {
        "y" => Some(ApprovalScope::Once),
        "a" => Some(ApprovalScope::WorkspaceSession),
        "p" => Some(ApprovalScope::Project),
        _ => None,
    };
    match scope.filter(|scope| request.approval_scopes.contains(scope)) {
        Some(scope) => Decision::Allow(scope),
        None => Decision::Deny,
    }
}

impl Questioner for CliApprover {
    fn ask(
        &self,
        request: QuestionRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = QuestionOutcome> + Send + '_>> {
        Box::pin(async move {
            let _one_at_a_time = self.prompting.lock().await;
            let prompt = tokio::task::spawn_blocking(move || prompt_questions(&request)).await;
            match prompt {
                Ok(outcome) => outcome,
                Err(error) => QuestionOutcome::Unavailable(format!(
                    "terminal question reader panicked: {error}"
                )),
            }
        })
    }
}

fn prompt_questions(request: &QuestionRequest) -> QuestionOutcome {
    let mut answers = Vec::with_capacity(request.questions.len());
    for (question_index, question) in request.questions.iter().enumerate() {
        loop {
            println!(
                "\n[question {}/{} · {}] {}",
                question_index + 1,
                request.questions.len(),
                question.header,
                question.question
            );
            for (option_index, option) in question.options.iter().enumerate() {
                println!(
                    "  {}. {} — {}",
                    option_index + 1,
                    option.label,
                    option.description
                );
                if let Some(preview) = &option.preview {
                    for line in preview.lines() {
                        println!("     | {line}");
                    }
                }
            }
            let other_index = question.options.len() + 1;
            println!("  {other_index}. Other (type a custom answer)");
            let hint = if question.multi_select {
                "comma-separated numbers"
            } else {
                "one number"
            };
            print!("  {hint}; c = cancel > ");
            let _ = std::io::stdout().flush();
            let line = match read_terminal_line() {
                Ok(Some(line)) => line,
                Ok(None) => {
                    return QuestionOutcome::Unavailable("terminal input reached EOF".into());
                }
                Err(error) => {
                    return QuestionOutcome::Unavailable(format!(
                        "cannot read terminal input: {error}"
                    ));
                }
            };
            if matches!(line.trim().to_ascii_lowercase().as_str(), "c" | "cancel") {
                return QuestionOutcome::Cancelled;
            }
            let Some(mut selected) =
                parse_selection(&line, question.options.len(), question.multi_select)
            else {
                eprintln!("[invalid selection — choose the displayed number(s)]");
                continue;
            };
            let wants_other = selected
                .iter()
                .position(|index| *index == question.options.len())
                .map(|position| {
                    selected.remove(position);
                })
                .is_some();
            let other = if wants_other {
                print!("  Other answer > ");
                let _ = std::io::stdout().flush();
                match read_terminal_line() {
                    Ok(Some(value)) if !value.trim().is_empty() => Some(value.trim().to_string()),
                    Ok(Some(_)) => {
                        eprintln!("[Other answer cannot be empty]");
                        continue;
                    }
                    Ok(None) => {
                        return QuestionOutcome::Unavailable(
                            "terminal input reached EOF while entering Other".into(),
                        );
                    }
                    Err(error) => {
                        return QuestionOutcome::Unavailable(format!(
                            "cannot read Other answer: {error}"
                        ));
                    }
                }
            } else {
                None
            };
            let notes = if question
                .options
                .iter()
                .any(|option| option.preview.is_some())
            {
                print!("  Notes (optional; Enter to skip) > ");
                let _ = std::io::stdout().flush();
                match read_terminal_line() {
                    Ok(Some(value)) => {
                        let value = value.trim();
                        (!value.is_empty()).then(|| value.to_string())
                    }
                    Ok(None) => {
                        return QuestionOutcome::Unavailable(
                            "terminal input reached EOF while entering notes".into(),
                        );
                    }
                    Err(error) => {
                        return QuestionOutcome::Unavailable(format!("cannot read notes: {error}"));
                    }
                }
            } else {
                None
            };
            answers.push(QuestionAnswer {
                question_index,
                selected,
                other,
                notes,
            });
            break;
        }
    }
    QuestionOutcome::Answered(answers)
}

fn read_terminal_line() -> std::io::Result<Option<String>> {
    let mut line = String::new();
    let bytes = std::io::stdin().read_line(&mut line)?;
    Ok((bytes != 0).then_some(line))
}

fn parse_selection(input: &str, option_count: usize, multi_select: bool) -> Option<Vec<usize>> {
    let mut selected = Vec::new();
    for token in input
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        let number = token.parse::<usize>().ok()?;
        if !(1..=option_count + 1).contains(&number) {
            return None;
        }
        let index = number - 1;
        if !selected.contains(&index) {
            selected.push(index);
        }
    }
    if selected.is_empty() || (!multi_select && selected.len() != 1) {
        return None;
    }
    Some(selected)
}

#[derive(Clone, Copy)]
enum PlainItemKind {
    Text,
    Reasoning,
}

struct PlainItem {
    kind: PlainItemKind,
    printed: String,
}

#[derive(Default)]
pub(crate) struct StdoutUi {
    items: Mutex<HashMap<String, PlainItem>>,
}

impl StdoutUi {
    fn print_piece(kind: PlainItemKind, text: &str) {
        match kind {
            PlainItemKind::Text => print!("{text}"),
            PlainItemKind::Reasoning => print!("\x1b[2m{text}\x1b[0m"),
        }
        let _ = std::io::stdout().flush();
    }

    fn start_item(&self, id: &str, kind: PlainItemKind, text: &str) {
        let mut items = self.items.lock().unwrap_or_else(|error| error.into_inner());
        let entry = items.entry(id.to_string()).or_insert_with(|| PlainItem {
            kind,
            printed: String::new(),
        });
        if entry.printed.is_empty() && !text.is_empty() {
            Self::print_piece(kind, text);
            entry.printed.push_str(text);
        }
    }

    fn append_item(&self, id: &str, kind: PlainItemKind, text: &str) {
        let mut items = self.items.lock().unwrap_or_else(|error| error.into_inner());
        let entry = items.entry(id.to_string()).or_insert_with(|| PlainItem {
            kind,
            printed: String::new(),
        });
        Self::print_piece(entry.kind, text);
        entry.printed.push_str(text);
    }

    fn finish_item(&self, id: &str, kind: PlainItemKind, text: &str) {
        let previous = self
            .items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(id)
            .map(|item| item.printed)
            .unwrap_or_default();
        if previous == text {
            return;
        }
        if let Some(suffix) = text.strip_prefix(&previous) {
            Self::print_piece(kind, suffix);
        } else {
            Self::print_piece(kind, text);
        }
    }
}

impl Ui for StdoutUi {
    fn emit(&self, ev: &Event) {
        match ev {
            Event::ItemStarted {
                id,
                item: Item::AssistantMessage { text, .. },
            } => self.start_item(id, PlainItemKind::Text, text),
            Event::ItemStarted {
                id,
                item: Item::Reasoning { text, .. },
            } => self.start_item(id, PlainItemKind::Reasoning, text),
            Event::ItemDelta {
                id,
                delta: Delta::Text(text),
            } => self.append_item(id, PlainItemKind::Text, text),
            Event::ItemDelta {
                id,
                delta: Delta::Reasoning(text),
            } => self.append_item(id, PlainItemKind::Reasoning, text),
            Event::ItemCompleted {
                id,
                item: Item::AssistantMessage { text, .. },
            } => self.finish_item(id, PlainItemKind::Text, text),
            Event::ItemCompleted {
                id,
                item: Item::Reasoning { text, .. },
            } => self.finish_item(id, PlainItemKind::Reasoning, text),
            // Tool starts, sub-agent lifecycle, cwd/mode changes, and notes all
            // reduce to the one-line dim note they showed before plan 39.
            other => {
                if let Some(note) = other.as_note() {
                    eprintln!("\x1b[2m[{note}]\x1b[0m");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_graph_snapshot_has_no_plain_projection() {
        let ui = StdoutUi::default();
        let event = Event::TaskGraphUpdated(kloop_core::tools::TaskGraphSnapshot {
            revision: 1,
            tasks: vec![kloop_core::tools::TaskGraphTask {
                id: "1".into(),
                subject: "Do not print me".into(),
                status: kloop_core::tools::TaskStatus::Pending,
                blocked_by: Vec::new(),
                blocks: Vec::new(),
            }],
        });
        assert_eq!(event.as_note(), None);
        ui.emit(&event);
        assert!(ui.items.lock().unwrap().is_empty());
    }

    #[test]
    fn color_diff_wraps_lines_by_sign() {
        assert_eq!(
            color_diff("+1  add\n-2  del\n 3  ctx"),
            "\x1b[32m+1  add\x1b[0m\n\x1b[31m-2  del\x1b[0m\n\x1b[2m 3  ctx\x1b[0m"
        );
    }

    #[test]
    fn approval_prompt_and_answers_follow_advertised_scopes() {
        let once = ConfirmRequest {
            description: "x".into(),
            approval_scopes: vec![ApprovalScope::Once],
            remember_rules: Some(vec!["write_file(src/**)".into()]),
            preview: None,
            ..Default::default()
        };
        assert_eq!(approval_options(&once), "y = allow once / n = deny");
        assert_eq!(approval_decision("a", &once), Decision::Deny);
        assert_eq!(
            approval_decision("y", &once),
            Decision::Allow(ApprovalScope::Once)
        );

        let all = ConfirmRequest {
            approval_scopes: vec![
                ApprovalScope::Once,
                ApprovalScope::WorkspaceSession,
                ApprovalScope::Project,
            ],
            ..once
        };
        assert_eq!(
            approval_options(&all),
            "y = allow once / a = allow for this workspace session / p = allow for this project across sessions and linked worktrees / n = deny"
        );
        assert_eq!(
            approval_decision("p", &all),
            Decision::Allow(ApprovalScope::Project)
        );
        assert_eq!(approval_decision("always", &all), Decision::Deny);
    }
    #[test]
    fn selection_parser_handles_single_multi_other_and_rejects_bad_input() {
        assert_eq!(parse_selection("2", 2, false), Some(vec![1]));
        assert_eq!(parse_selection("1, 3, 1", 2, true), Some(vec![0, 2]));
        assert_eq!(parse_selection("1,2", 2, false), None);
        assert_eq!(parse_selection("0", 2, true), None);
        assert_eq!(parse_selection("", 2, true), None);
    }
}
