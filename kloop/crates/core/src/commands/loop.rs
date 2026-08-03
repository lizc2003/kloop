use super::SlashResult;

pub const SUMMARY: &str = "Run a prompt now and continue it on a fixed or self-paced schedule";
const AUTONOMOUS_FIXED_SENTINEL: &str = "<<autonomous-loop>>";
const AUTONOMOUS_DYNAMIC_SENTINEL: &str = "<<autonomous-loop-dynamic>>";

pub fn run(args: &str) -> SlashResult {
    let parsed = parse(args);
    match parsed {
        ParsedLoop::Error(error) => SlashResult::message(format!("/loop: {error}")),
        ParsedLoop::Fixed {
            cron,
            human,
            prompt,
        } => {
            let prompt = if prompt.is_empty() {
                AUTONOMOUS_FIXED_SENTINEL
            } else {
                prompt.as_str()
            };
            SlashResult::turn(format!(
                "You are starting a fixed-interval /loop. In this same turn, first do the requested work once now. Before finishing, call cron_create with exactly {{\"cron\":{cron:?},\"prompt\":{prompt:?},\"recurring\":true,\"durable\":false}} so later ticks repeat it. Tell the user the schedule is {human} and that recurring jobs auto-expire after seven days. The prompt text must remain unchanged.\n\nRequested work:\n{prompt}"
            ))
        }
        ParsedLoop::Dynamic { prompt } => {
            let prompt = if prompt.is_empty() {
                AUTONOMOUS_DYNAMIC_SENTINEL
            } else {
                prompt.as_str()
            };
            SlashResult::turn(format!(
                "You are starting or resuming a self-paced dynamic /loop. Do the requested work once now. If more work remains, end this turn by calling schedule_wakeup with delay_seconds, a specific reason, and prompt exactly {prompt:?}. Choose 60–3600 seconds based on when the watched state can realistically change; do not poll harness-tracked background work. If the loop is complete, call schedule_wakeup with stop=true and no other fields. Never convert this dynamic loop into a recurring cron.\n\nRequested work:\n{prompt}"
            ))
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ParsedLoop {
    Fixed {
        cron: String,
        human: String,
        prompt: String,
    },
    Dynamic {
        prompt: String,
    },
    Error(String),
}

fn parse(args: &str) -> ParsedLoop {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return ParsedLoop::Dynamic {
            prompt: String::new(),
        };
    }
    let mut words = trimmed.split_whitespace();
    let first = words.next().unwrap();
    if looks_like_interval(first) {
        return match interval_to_cron(first) {
            Ok((cron, human)) => ParsedLoop::Fixed {
                cron,
                human,
                prompt: words.collect::<Vec<_>>().join(" "),
            },
            Err(error) => ParsedLoop::Error(error),
        };
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() >= 3 && tokens[tokens.len() - 2].eq_ignore_ascii_case("every") {
        let interval = tokens[tokens.len() - 1];
        if looks_like_interval(interval) {
            return match interval_to_cron(interval) {
                Ok((cron, human)) => ParsedLoop::Fixed {
                    cron,
                    human,
                    prompt: tokens[..tokens.len() - 2].join(" "),
                },
                Err(error) => ParsedLoop::Error(error),
            };
        }
    }
    ParsedLoop::Dynamic {
        prompt: trimmed.to_string(),
    }
}

fn looks_like_interval(value: &str) -> bool {
    let Some((number, unit)) = value.split_at_checked(value.len().saturating_sub(1)) else {
        return false;
    };
    !number.is_empty()
        && number.chars().all(|value| value.is_ascii_digit())
        && matches!(unit.to_ascii_lowercase().as_str(), "s" | "m" | "h" | "d")
}

fn interval_to_cron(value: &str) -> Result<(String, String), String> {
    let split = value.len().saturating_sub(1);
    let (number, unit) = value.split_at(split);
    let amount = number
        .parse::<u32>()
        .map_err(|_| "interval must use a positive integer".to_string())?;
    if amount == 0 {
        return Err("interval must be at least 1".into());
    }
    match unit.to_ascii_lowercase().as_str() {
        "s" => {
            Err("minimum fixed interval is 1 minute; omit the interval for a dynamic loop".into())
        }
        "m" if amount <= 59 => Ok((
            if amount == 1 {
                "* * * * *".into()
            } else {
                format!("*/{amount} * * * *")
            },
            if amount == 1 {
                "every minute".into()
            } else {
                format!("every {amount} minutes")
            },
        )),
        "m" => Err("minute interval must be 1–59; use hours instead".into()),
        "h" if amount <= 23 => Ok((
            if amount == 1 {
                "0 * * * *".into()
            } else {
                format!("0 */{amount} * * *")
            },
            if amount == 1 {
                "every hour".into()
            } else {
                format!("every {amount} hours")
            },
        )),
        "h" => Err("hour interval must be 1–23; use days instead".into()),
        "d" if amount <= 28 => Ok((
            if amount == 1 {
                "0 0 * * *".into()
            } else {
                format!("0 0 */{amount} * *")
            },
            if amount == 1 {
                "every day".into()
            } else {
                format!("every {amount} days")
            },
        )),
        "d" => Err("day interval must be 1–28; use an explicit cron expression".into()),
        _ => Err("unknown interval unit".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_leading_and_trailing_fixed_intervals() {
        assert_eq!(
            parse("5m check status"),
            ParsedLoop::Fixed {
                cron: "*/5 * * * *".into(),
                human: "every 5 minutes".into(),
                prompt: "check status".into(),
            }
        );
        assert_eq!(
            parse("check status every 2h"),
            ParsedLoop::Fixed {
                cron: "0 */2 * * *".into(),
                human: "every 2 hours".into(),
                prompt: "check status".into(),
            }
        );
    }

    #[test]
    fn no_interval_is_dynamic_and_empty_uses_sentinel() {
        assert_eq!(
            parse("check deployment"),
            ParsedLoop::Dynamic {
                prompt: "check deployment".into()
            }
        );
        let result = run("");
        assert!(result
            .run_turn
            .unwrap()
            .contains(AUTONOMOUS_DYNAMIC_SENTINEL));
    }

    #[test]
    fn rejects_fixed_seconds_and_out_of_range_intervals() {
        assert!(matches!(parse("30s check"), ParsedLoop::Error(_)));
        assert!(matches!(parse("60m check"), ParsedLoop::Error(_)));
        assert!(matches!(parse("29d check"), ParsedLoop::Error(_)));
    }

    #[test]
    fn generated_prompts_use_native_scheduler_names_and_first_tick() {
        let fixed = run("5m check status").run_turn.unwrap();
        assert!(fixed.contains("first do the requested work once now"));
        assert!(fixed.contains("cron_create"));
        assert!(fixed.contains("\"*/5 * * * *\""));
        let dynamic = run("check status").run_turn.unwrap();
        assert!(dynamic.contains("schedule_wakeup"));
        assert!(dynamic.contains("delay_seconds"));
        assert!(dynamic.contains("stop=true"));
    }
}
