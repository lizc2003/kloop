use std::fmt;

use kloop_protocol::ProviderApiFamily;
use kloop_protocol::ProviderAttemptIdentity;
use kloop_protocol::ProviderAttemptKind;
use kloop_protocol::Usage;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageOperation {
    Sampling,
    Compaction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsageRecord {
    pub provider_id: String,
    pub api_family: ProviderApiFamily,
    pub route_revision: u64,
    pub model: String,
    pub attempt_kind: ProviderAttemptKind,
    pub operation: UsageOperation,
    pub usage: Usage,
}

impl ProviderUsageRecord {
    pub fn from_attempt(
        attempt: &ProviderAttemptIdentity,
        operation: UsageOperation,
        usage: Usage,
    ) -> Self {
        Self {
            provider_id: attempt.provider_id.clone(),
            api_family: attempt.api_family,
            route_revision: attempt.route_revision,
            model: attempt.model.clone(),
            attempt_kind: attempt.attempt_kind,
            operation,
            usage,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageAggregate {
    pub usage: Usage,
    pub reported_responses: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageGroup {
    pub provider_id: String,
    pub api_family: ProviderApiFamily,
    pub model: String,
    pub aggregate: UsageAggregate,
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

    pub fn grouped(&self) -> Result<Vec<UsageGroup>, UsageOverflow> {
        let mut groups: Vec<UsageGroup> = Vec::new();
        for record in &self.records {
            if let Some(group) = groups.iter_mut().find(|group| {
                group.provider_id == record.provider_id
                    && group.api_family == record.api_family
                    && group.model == record.model
            }) {
                group.aggregate = add_usage(group.aggregate, record.usage)?;
            } else {
                groups.push(UsageGroup {
                    provider_id: record.provider_id.clone(),
                    api_family: record.api_family,
                    model: record.model.clone(),
                    aggregate: UsageAggregate {
                        usage: record.usage,
                        reported_responses: 1,
                    },
                });
            }
        }
        Ok(groups)
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

    fn record(provider: &str, model: &str, usage: Usage) -> ProviderUsageRecord {
        ProviderUsageRecord {
            provider_id: provider.into(),
            api_family: ProviderApiFamily::Mock,
            route_revision: 1,
            model: model.into(),
            attempt_kind: ProviderAttemptKind::Primary,
            operation: UsageOperation::Sampling,
            usage,
        }
    }

    #[test]
    fn aggregate_preserves_categories_and_groups_provider_model() {
        let mut ledger = UsageLedger::default();
        ledger.push(record(
            "a",
            "shared",
            Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_input_tokens: 30,
                cache_creation_input_tokens: 40,
            },
        ));
        ledger.push(record(
            "b",
            "shared",
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
        let groups = ledger.grouped().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].provider_id, "a");
        assert_eq!(groups[1].provider_id, "b");
    }

    #[test]
    fn empty_and_reported_zero_are_distinct() {
        let mut ledger = UsageLedger::default();
        assert_eq!(ledger.aggregate().unwrap(), None);
        ledger.push(record("a", "model", Usage::default()));
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
            "a",
            "model",
            Usage {
                input_tokens: u64::MAX,
                ..Usage::default()
            },
        ));
        ledger.push(record(
            "a",
            "model",
            Usage {
                input_tokens: 1,
                ..Usage::default()
            },
        ));
        assert_eq!(ledger.aggregate(), Err(UsageOverflow));
        assert_eq!(ledger.records().len(), 2);
    }
}
