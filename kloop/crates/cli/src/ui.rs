//! The plain terminal frontend: the blocking y/a/p/n approval prompt
//! ([`CliApprover`]) and the streaming stdout renderer ([`StdoutUi`]) the
//! `--plain`/`--mock` REPL installs, plus the ANSI diff colorizer they share.

use std::io::Write as _;

use kloop_core::agent::Ui;
use kloop_core::permissions::Approver;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::permissions::Decision;

use crate::startup::PERMISSIONS_CONFIG;

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
            let options = match &req.remember_rules {
                Some(rules) => format!(
                    "y = allow once / a = allow for this session / p = allow always (saves {} to {PERMISSIONS_CONFIG}) / n = deny",
                    rules.join(", ")
                ),
                None => "y = allow once / n = deny".to_string(),
            };
            let preview = req
                .preview
                .as_deref()
                .map(|p| format!("\n{}", color_diff(p)))
                .unwrap_or_default();
            print!("\n[approve?] {}{preview}\n  {options} > ", req.description);
            let _ = std::io::stdout().flush();
            let line = tokio::task::spawn_blocking(|| {
                let mut buf = String::new();
                std::io::stdin().read_line(&mut buf).map(|_| buf)
            })
            .await;
            match line {
                // 'a'/'p' on a non-remember-able call degrade to allow-once
                // in the gate (it ignores the remember part), matching the
                // user's evident intent to allow.
                Ok(Ok(answer)) => match answer.trim().to_lowercase().as_str() {
                    "y" | "yes" => Decision::Allow,
                    "a" | "always" => Decision::AllowSession,
                    "p" | "persist" => Decision::AllowAlways,
                    _ => Decision::Deny,
                },
                // Reader died or stdin closed: the safe answer is no.
                _ => Decision::Deny,
            }
        })
    }
}

pub(crate) struct StdoutUi;

impl Ui for StdoutUi {
    fn text_delta(&self, s: &str) {
        print!("{s}");
        let _ = std::io::stdout().flush();
    }

    fn thinking_delta(&self, s: &str) {
        // Dim gray, inline with the stream: reasoning is context, not answer.
        print!("\x1b[2m{s}\x1b[0m");
        let _ = std::io::stdout().flush();
    }

    fn note(&self, s: &str) {
        eprintln!("\x1b[2m[{s}]\x1b[0m");
    }

    fn tool_start(
        &self,
        agent: &str,
        _id: &str,
        name: &str,
        summary: &str,
        _input: &serde_json::Value,
    ) {
        // todo_write is rendered as a checklist by todo_update, not a note.
        if name == "todo_write" {
            return;
        }
        if agent.is_empty() {
            self.note(&format!("{name} {summary}"));
        } else {
            self.note(&format!("{agent} · {name} {summary}"));
        }
    }

    fn todo_update(&self, agent: &str, todos: &[kloop_core::tools::TodoItem]) {
        use kloop_core::tools::TodoStatus;
        let prefix = if agent.is_empty() {
            String::new()
        } else {
            format!("{agent} · ")
        };
        eprintln!("\x1b[2m[{prefix}todos]\x1b[0m");
        for todo in todos {
            let (mark, text) = match todo.status {
                TodoStatus::Completed => ("✓", &todo.content),
                TodoStatus::InProgress => ("▶", &todo.active_form),
                TodoStatus::Pending => ("○", &todo.content),
            };
            eprintln!("\x1b[2m  {mark} {text}\x1b[0m");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_diff_wraps_lines_by_sign() {
        assert_eq!(
            color_diff("+1  add\n-2  del\n 3  ctx"),
            "\x1b[32m+1  add\x1b[0m\n\x1b[31m-2  del\x1b[0m\n\x1b[2m 3  ctx\x1b[0m"
        );
    }
}
