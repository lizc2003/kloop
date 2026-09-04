//! `/help` — list the available slash commands.

use std::sync::Arc;

use super::BUILTINS;
use super::SlashResult;
use crate::config::Config;
use crate::skills::SkillSource;

pub const SUMMARY: &str = "list the slash commands";

pub fn run(cfg: &Arc<Config>) -> SlashResult {
    // One padding width across both sections so the two lists read as one
    // column — a skill is invoked exactly like a built-in.
    let pad = BUILTINS
        .iter()
        .map(|b| b.name.len())
        .chain(cfg.skills.iter().map(|s| s.name.len()))
        .max()
        .unwrap_or(0);
    let mut output = String::from("commands:");
    for b in BUILTINS {
        output.push_str(&format!("\n  /{:pad$}  {}", b.name, b.summary, pad = pad));
    }
    // Skills are `/name`-invocable too, so leaving them out of `/help` hid the
    // builtins from everyone (plan 119): the only listing that named them was
    // the unknown-command error.
    if !cfg.skills.is_empty() {
        output.push_str("\n\nskills:");
        for s in cfg.skills.iter() {
            let tag = match s.source {
                SkillSource::Builtin => " (builtin)",
                SkillSource::Command => " (user command)",
                SkillSource::Skill => "",
            };
            output.push_str(&format!(
                "\n  /{:pad$}  {}{tag}",
                s.name,
                super::skills::one_line(&s.description),
                pad = pad
            ));
        }
        output.push_str("\n\n/skills <name> prints one skill's full instructions.");
    }
    SlashResult::message(output)
}
