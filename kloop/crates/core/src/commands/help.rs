//! `/help` — list the available slash commands.

use super::BUILTINS;
use super::SlashResult;

pub const SUMMARY: &str = "list the slash commands";

pub fn run() -> SlashResult {
    let pad = BUILTINS.iter().map(|b| b.name.len()).max().unwrap_or(0);
    let mut output = String::from("commands:");
    for b in BUILTINS {
        output.push_str(&format!("\n  /{:pad$}  {}", b.name, b.summary, pad = pad));
    }
    SlashResult::message(output)
}
