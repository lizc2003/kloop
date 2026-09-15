//! `/skills` — list the loaded skills, or print one's instructions.
//!
//! `/help` names them; this is where you read what one actually says. That
//! matters most for a builtin (plan 119): it lives in the binary, so there is
//! no file to open, and the only way to change it is to write a same-named
//! `SKILL.md` into a discovery root — which you cannot do without first seeing
//! what you are replacing.

use std::sync::Arc;

use super::SlashResult;
use crate::config::Config;
use crate::skills::Skill;
use crate::skills::SkillSource;

pub const SUMMARY: &str = "list the loaded skills, or print one's instructions";

/// One column-friendly line for a listing. A skill's `description` carries both
/// what it does and when to use it — that is the model's only matching signal,
/// so it stays long — but printed whole it wraps to four lines per entry and
/// buries the next one. Newlines collapse (a block-scalar description is still
/// one field), then it truncates the way `description_from_body` does: 100
/// chars with an ellipsis. The full text is one `/skills <name>` away.
pub(super) fn one_line(description: &str) -> String {
    let flat = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 100 {
        format!("{}...", flat.chars().take(97).collect::<String>())
    } else {
        flat
    }
}

/// Where an entry came from, for the listing. A disk skill shows its directory
/// (which is what distinguishes a project skill from a global one), a builtin
/// says so, and a user command is labelled — it is `/name`-invocable but the
/// model never sees it.
fn origin(skill: &Skill) -> String {
    match skill.source {
        SkillSource::Builtin => "builtin".to_string(),
        SkillSource::Command => format!("user command · {}", skill.dir),
        SkillSource::Skill => skill.dir.clone(),
    }
}

pub fn run(args: &str, cfg: &Arc<Config>) -> SlashResult {
    let name = args.trim();
    if cfg.skills.is_empty() {
        return SlashResult::message("no skills loaded");
    }
    if name.is_empty() {
        let pad = cfg.skills.iter().map(|s| s.name.len()).max().unwrap_or(0);
        let mut output = String::from("skills (invoke as /name, or let the model pick one):");
        for s in cfg.skills.iter() {
            output.push_str(&format!(
                "\n  /{:pad$}  {}\n  {:pad$}   {}",
                s.name,
                one_line(&s.description),
                "",
                origin(s),
                pad = pad
            ));
        }
        output.push_str("\n\n/skills <name> prints one skill's full instructions.");
        return SlashResult::message(output);
    }
    // A name: print that skill's body verbatim. This is the local REPL, not the
    // `skills/list` read surface — that one withholds bodies on purpose because
    // it answers a client over the wire.
    match Skill::lookup(cfg.skills.iter(), name) {
        Ok(skill) => SlashResult::message(format!(
            "/{} — {}\nsource: {}\n\n{}",
            skill.name,
            skill.description,
            origin(skill),
            skill.body
        )),
        Err(e) => SlashResult::message(e),
    }
}
