use kloop_protocol::Usage;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageOperation {
    Sampling,
    Compaction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUsageRecord {
    pub model: String,
    pub operation: UsageOperation,
    pub usage: Usage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageAggregate {
    pub usage: Usage,
    pub reported_responses: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageOverflow;

impl fmt::Display for UsageOverflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("provider usage aggregate overflow")
    }
}

impl std::error::Error for UsageOverflow {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageLedger {
    records: Vec<ProviderUsageRecord>,
    aggregate: Option<Result<UsageAggregate, UsageOverflow>>,
}

impl UsageLedger {
    pub fn push(&mut self, record: ProviderUsageRecord) {
        let next = match self.aggregate {
            None => Ok(UsageAggregate {
                usage: record.usage,
                reported_responses: 1,
            }),
            Some(Ok(current)) => add_usage(current, record.usage),
            Some(Err(error)) => Err(error),
        };
        self.records.push(record);
        self.aggregate = Some(next);
    }

    pub fn records(&self) -> &[ProviderUsageRecord] {
        &self.records
    }

    pub fn aggregate(&self) -> Result<Option<UsageAggregate>, UsageOverflow> {
        self.aggregate.transpose()
    }
}

fn add_usage(current: UsageAggregate, added: Usage) -> Result<UsageAggregate, UsageOverflow> {
    Ok(UsageAggregate {
        usage: Usage {
            input_tokens: current
                .usage
                .input_tokens
                .checked_add(added.input_tokens)
                .ok_or(UsageOverflow)?,
            output_tokens: current
                .usage
                .output_tokens
                .checked_add(added.output_tokens)
                .ok_or(UsageOverflow)?,
            cache_read_input_tokens: current
                .usage
                .cache_read_input_tokens
                .checked_add(added.cache_read_input_tokens)
                .ok_or(UsageOverflow)?,
            cache_creation_input_tokens: current
                .usage
                .cache_creation_input_tokens
                .checked_add(added.cache_creation_input_tokens)
                .ok_or(UsageOverflow)?,
        },
        reported_responses: current
            .reported_responses
            .checked_add(1)
            .ok_or(UsageOverflow)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(model: &str, operation: UsageOperation, usage: Usage) -> ProviderUsageRecord {
        ProviderUsageRecord {
            model: model.into(),
            operation,
            usage,
        }
    }

    #[test]
    fn aggregate_preserves_all_categories_and_response_count() {
        let mut ledger = UsageLedger::default();
        ledger.push(record(
            "primary",
            UsageOperation::Sampling,
            Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_input_tokens: 30,
                cache_creation_input_tokens: 40,
            },
        ));
        ledger.push(record(
            "fallback",
            UsageOperation::Compaction,
            Usage {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_input_tokens: 3,
                cache_creation_input_tokens: 4,
            },
        ));

        assert_eq!(
            ledger.aggregate().unwrap(),
            Some(UsageAggregate {
                usage: Usage {
                    input_tokens: 11,
                    output_tokens: 22,
                    cache_read_input_tokens: 33,
                    cache_creation_input_tokens: 44,
                },
                reported_responses: 2,
            })
        );
        assert_eq!(ledger.records()[1].model, "fallback");
        assert_eq!(ledger.records()[1].operation, UsageOperation::Compaction);
    }

    #[test]
    fn empty_and_reported_zero_are_distinct() {
        let mut ledger = UsageLedger::default();
        assert_eq!(ledger.aggregate().unwrap(), None);

        ledger.push(record("model", UsageOperation::Sampling, Usage::default()));
        assert_eq!(
            ledger.aggregate().unwrap(),
            Some(UsageAggregate {
                usage: Usage::default(),
                reported_responses: 1,
            })
        );
    }

    #[test]
    fn aggregate_overflow_fails_closed() {
        let mut ledger = UsageLedger::default();
        ledger.push(record(
            "model",
            UsageOperation::Sampling,
            Usage {
                input_tokens: u64::MAX,
                ..Usage::default()
            },
        ));
        ledger.push(record(
            "model",
            UsageOperation::Sampling,
            Usage {
                input_tokens: 1,
                ..Usage::default()
            },
        ));

        assert_eq!(ledger.aggregate(), Err(UsageOverflow));
    }
}
