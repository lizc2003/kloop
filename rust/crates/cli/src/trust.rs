//! Workspace trust (plan 193): one question, asked once per project, before
//! any front-end reads the directory.
//!
//! kloop's *policy* surface is already closed to the repository — permissions,
//! hooks, MCP servers and the sandbox all come from the one private
//! `~/.kloop/config.toml`, so a hostile checkout cannot move the gate. What it
//! can still do is steer the model: `AGENTS.md` / `CLAUDE.md` / `.kloop/rules`
//! ride into the system prompt, this project's skills are model-activatable,
//! and a skill body's `` !`cmd` `` inline really executes. Since plan 192 an
//! in-cwd file write does not stop for the human either. That is what this
//! question is for, and it is the only thing it is for: it is not a permission
//! mode, and answering yes changes no layer of the gate.
//!
//! The answer is keyed by [`ProjectId`], so a Git project is trusted once for
//! all of its subdirectories and linked worktrees, and a plain directory is
//! trusted as itself.

use std::io::IsTerminal as _;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use kloop_core::project::WorkspaceIdentity;

use crate::project_store::ProjectStore;

/// Whether this launch has a human who can answer. `--serve` and `--headless`
/// are started by something that already made the call — whoever launches them
/// owns the decision (user, 2026-09-22) — `--mock` is hermetic and never
/// touches the durable store, and a non-TTY stdin has nobody behind it.
pub(crate) fn asks_for_trust(serve: bool, headless: bool, mock: bool, stdin_is_tty: bool) -> bool {
    !serve && !headless && !mock && stdin_is_tty
}

/// Only an explicit yes continues. A typo, an empty line, an EOF or `n` all
/// mean the same thing: this is not a directory the human vouched for.
pub(crate) fn accepts(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Ask unless this project was trusted in an earlier session. `false` means
/// the human declined and the caller should exit without starting anything.
pub(crate) fn ensure_trusted(store: Option<&Arc<ProjectStore>>, cwd: &Path) -> bool {
    let identity = WorkspaceIdentity::resolve(cwd);
    let project = identity.project_id().zip(store);
    if let Some((project_id, store)) = project
        && store.trusted_blocking(project_id)
    {
        return true;
    }
    if !ask(identity.cwd(), project.is_none()) {
        return false;
    }
    if let Some((project_id, store)) = project
        && let Err(error) = store.grant_trust_blocking(project_id)
    {
        eprintln!("\x1b[2m[trust was not saved ({error}); this directory will ask again]\x1b[0m");
    }
    true
}

/// The prompt itself. Blocking, and deliberately before any async stdin reader
/// exists — the REPL's own reader would otherwise buffer past this line.
fn ask(cwd: &Path, unremembered: bool) -> bool {
    let path = cwd.display();
    let scope = if unremembered {
        "\n  This directory has no stable project identity, so the answer cannot be remembered."
    } else {
        ""
    };
    print!(
        "\n\x1b[1mTrust this directory?\x1b[0m\n\n  {path}\n\n\
         kloop reads the files here, follows the instructions it finds in AGENTS.md,\n\
         CLAUDE.md and .kloop/rules, lets the model activate this project's own skills,\n\
         and edits files inside this directory without asking. Commands still run in the\n\
         OS sandbox, and writes to .git, .kloop and .env* still ask every time.\n\n\
         Continue only if you know where this directory came from.{scope}\n\n  \
         y = trust it (this project and its worktrees) · anything else = exit\n> "
    );
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    match std::io::stdin().read_line(&mut answer) {
        Ok(0) | Err(_) => false,
        Ok(_) => accepts(&answer),
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

    /// Yes is exact; everything else — including a near-miss and an empty
    /// line — exits.
    #[test]
    fn nothing_but_yes_continues() {
        for yes in ["y", "Y", "yes", "YES", " y \n"] {
            assert!(accepts(yes), "{yes:?}");
        }
        for no in ["", "\n", "n", "no", "yep", "sure", "1", "trust"] {
            assert!(!accepts(no), "{no:?}");
        }
    }
}
