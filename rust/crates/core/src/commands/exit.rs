//! `/exit` — quit the interactive session (TUI or plain REPL). The server
//! ignores the quit signal and only relays the message: ending one thread must
//! not stop the multi-session process.

use super::SlashResult;

pub const SUMMARY: &str = "quit kloop";

pub fn run() -> SlashResult {
    SlashResult::quit("bye")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_signals_quit() {
        let result = run();
        assert!(result.quit);
        assert_eq!(result.output, "bye");
        assert!(!result.new_session);
        assert_eq!(result.run_turn, None);
    }
}
