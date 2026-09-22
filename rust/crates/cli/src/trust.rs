//! Workspace trust (plan 193): one question, asked once per project, before
//! any front-end reads the directory.
//!
//! kloop's *policy* surface is already closed to the repository — permissions,
//! hooks, MCP servers and the sandbox all come from the one private
//! `~/.kloop/config.toml`, so a hostile checkout cannot move the gate. What it
//! can still do is steer the model: `AGENTS.md`, `.kloop/rules/*.md` and
//! `AGENTS.local.md` ride into the system prompt, this project's skills are
//! model-activatable, and a skill body's `` !`cmd` `` inline really executes.
//! Since plan 192 an in-cwd file write does not stop for the human either.
//! That is what this question is for, and the only thing it is for: it is not
//! a permission mode, and answering yes changes no layer of the gate.
//!
//! The answer is keyed by [`ProjectId`](kloop_core::project::ProjectId), so a
//! Git project is trusted once for all of its subdirectories and linked
//! worktrees, and a plain directory is trusted as itself.

use std::io::IsTerminal as _;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::terminal;

use kloop_core::project::WorkspaceIdentity;

use crate::project_store::ProjectStore;

/// The two choices, in the order they are listed. `Exit` is first and starts
/// selected: the safe answer is the one a stray Enter gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Exit,
    Trust,
}

/// What one keypress does: move the selection, or settle it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Select(Answer),
    Done(Answer),
}

/// Whether this launch has a human who can answer. `--serve` and `--headless`
/// are started by something that already made the call — whoever launches them
/// owns the decision (user, 2026-09-22) — `--mock` is hermetic and never
/// touches the durable store, and a non-TTY stdin has nobody behind it.
pub(crate) fn asks_for_trust(serve: bool, headless: bool, mock: bool, stdin_is_tty: bool) -> bool {
    !serve && !headless && !mock && stdin_is_tty
}

/// Ask unless this project is already answered for. `false` means the human
/// declined and the caller should exit without starting anything.
pub(crate) fn ensure_trusted(
    store: Option<&Arc<ProjectStore>>,
    sessions: &Path,
    cwd: &Path,
) -> bool {
    let identity = WorkspaceIdentity::resolve(cwd);
    let project = identity.project_id().zip(store);
    if let Some((project_id, store)) = project
        && store.trusted_blocking(project_id)
    {
        return true;
    }
    // Sessions of this project's own are the same answer in another form: the
    // transcripts live in the private state root, never in the repository, so
    // one existing means this machine's owner has already worked here. Asking
    // now would protect nothing and would greet every project that predates
    // the question. It is not recorded as a grant — `trust.json` keeps meaning
    // "a human said yes".
    if has_prior_sessions(sessions) {
        return true;
    }
    if ask(identity.cwd(), project.is_none()) == Answer::Exit {
        return false;
    }
    if let Some((project_id, store)) = project
        && let Err(error) = store.grant_trust_blocking(project_id)
    {
        eprintln!("\x1b[2m[trust was not saved ({error}); this directory will ask again]\x1b[0m");
    }
    true
}

/// Whether this project's private session directory holds anything at all.
fn has_prior_sessions(sessions: &Path) -> bool {
    std::fs::read_dir(sessions).is_ok_and(|mut entries| entries.next().is_some())
}

/// Pure key handling, so the answer's shape is testable without a terminal.
fn step(key: KeyEvent, selected: Answer) -> Step {
    match (key.code, key.modifiers) {
        (KeyCode::Up | KeyCode::Char('k'), _) => Step::Select(Answer::Exit),
        (KeyCode::Down | KeyCode::Char('j'), _) => Step::Select(Answer::Trust),
        (KeyCode::Enter, _) => Step::Done(selected),
        (KeyCode::Char('c' | 'd'), KeyModifiers::CONTROL) | (KeyCode::Esc, _) => {
            Step::Done(Answer::Exit)
        }
        _ => Step::Select(selected),
    }
}

/// The two option lines, selected one marked and highlighted.
fn options(selected: Answer) -> [String; 2] {
    let line = |answer: Answer, text: &str| {
        if answer == selected {
            format!("\x1b[36m❯ {text}\x1b[0m")
        } else {
            format!("  {text}")
        }
    };
    [
        line(Answer::Exit, "No, exit"),
        line(Answer::Trust, "Yes, I trust this directory"),
    ]
}

/// The prompt. Raw mode, arrow keys, `Exit` preselected. Runs before any
/// front-end owns the terminal and before any async stdin reader exists.
fn ask(cwd: &Path, unremembered: bool) -> Answer {
    let path = cwd.display();
    let note = if unremembered {
        "\r\n\x1b[2mNo stable project identity here, so this answer is not remembered.\x1b[0m\r\n"
    } else {
        ""
    };
    print!(
        "\r\n\x1b[1;33mAccessing workspace:\x1b[0m\r\n\r\n  \x1b[1m{path}\x1b[0m\r\n\r\n\
         Is this a directory you created, or one you trust? kloop will read, edit and\r\n\
         run files here, and follow instructions it finds in them.\r\n{note}\r\n"
    );
    if terminal::enable_raw_mode().is_err() {
        return ask_by_line();
    }
    let mut selected = Answer::Exit;
    let answer = loop {
        let [first, second] = options(selected);
        print!(
            "{first}\r\n{second}\r\n\r\n\x1b[2mEnter to confirm · ↑↓ to move · Esc to cancel\x1b[0m\r\n"
        );
        let _ = std::io::stdout().flush();
        let key = match crossterm::event::read() {
            Ok(Event::Key(key)) if key.is_press() => key,
            Ok(_) => {
                print!("\x1b[4A\x1b[J");
                continue;
            }
            Err(_) => break Answer::Exit,
        };
        // Redraw in place: the four lines just printed are the whole widget.
        print!("\x1b[4A\x1b[J");
        match step(key, selected) {
            Step::Select(next) => selected = next,
            Step::Done(answer) => break answer,
        }
    };
    let [first, second] = options(answer);
    print!("{first}\r\n{second}\r\n");
    let _ = std::io::stdout().flush();
    let _ = terminal::disable_raw_mode();
    println!();
    answer
}

/// Fallback for a terminal that will not go raw: one line, same default.
fn ask_by_line() -> Answer {
    print!("  Trust this directory? [y = yes, anything else = exit] > ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    match std::io::stdin().read_line(&mut answer) {
        Ok(read) if read > 0 && matches!(answer.trim(), "y" | "Y") => Answer::Trust,
        _ => Answer::Exit,
    }
}

/// `std::io::stdin().is_terminal()`, named where it is used so the decision
/// above stays a pure function.
pub(crate) fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// The ruling this module exists to encode: only an interactive launch
    /// asks. Everything else is somebody else's decision, already made.
    #[test]
    fn only_an_interactive_launch_asks() {
        assert!(asks_for_trust(
            /*serve*/ false, /*headless*/ false, /*mock*/ false, /*tty*/ true
        ));
        for (serve, headless, mock, tty) in [
            (true, false, false, true),
            (false, true, false, true),
            (false, false, true, true),
            (false, false, false, false),
        ] {
            assert!(
                !asks_for_trust(serve, headless, mock, tty),
                "serve={serve} headless={headless} mock={mock} tty={tty}"
            );
        }
    }

    /// Exit is preselected and is what every way out gives: Enter on the
    /// default, Esc, Ctrl+C, Ctrl+D, and an unreadable terminal. Only a
    /// deliberate move down and Enter trusts.
    #[test]
    fn every_exit_leads_to_exit_and_only_a_move_down_trusts() {
        assert_eq!(
            step(key(KeyCode::Enter), Answer::Exit),
            Step::Done(Answer::Exit)
        );
        assert_eq!(
            step(key(KeyCode::Esc), Answer::Trust),
            Step::Done(Answer::Exit)
        );
        assert_eq!(
            step(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                Answer::Trust
            ),
            Step::Done(Answer::Exit)
        );
        assert_eq!(
            step(key(KeyCode::Down), Answer::Exit),
            Step::Select(Answer::Trust)
        );
        assert_eq!(
            step(key(KeyCode::Up), Answer::Trust),
            Step::Select(Answer::Exit)
        );
        assert_eq!(
            step(key(KeyCode::Enter), Answer::Trust),
            Step::Done(Answer::Trust)
        );
        // An unknown key changes nothing rather than settling the question.
        assert_eq!(
            step(key(KeyCode::Char('x')), Answer::Exit),
            Step::Select(Answer::Exit)
        );
    }

    /// A project with transcripts of its own has been worked in before, so
    /// the question is already answered; an empty or absent directory is not.
    #[test]
    fn prior_sessions_answer_the_question() {
        let root = std::env::temp_dir().join(format!("kloop-trust-prior-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert!(!has_prior_sessions(&root), "no directory at all");
        std::fs::create_dir_all(&root).unwrap();
        assert!(
            !has_prior_sessions(&root),
            "an empty directory is not a session"
        );
        std::fs::write(root.join("20260922-000000.jsonl"), b"{}\n").unwrap();
        assert!(has_prior_sessions(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// One marker, on the selected line only.
    #[test]
    fn only_the_selected_option_is_marked() {
        let [exit, trust] = options(Answer::Exit);
        assert!(exit.contains("❯ No, exit"));
        assert!(trust.starts_with("  Yes,"));
        let [exit, trust] = options(Answer::Trust);
        assert!(exit.starts_with("  No,"));
        assert!(trust.contains("❯ Yes, I trust this directory"));
    }
}
