//! General user questions, deliberately separate from permission approval.
//!
//! An [`Approver`](crate::permissions::Approver) answers whether an already
//! specified action may run. A [`Questioner`] instead collects product choices
//! the model cannot infer. Keeping distinct traits prevents a frontend or server
//! protocol from accidentally treating an arbitrary answer as permission.

use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;
use serde::Serialize;

const MAX_TOTAL_CHARS: usize = 50_000;
const MAX_QUESTION_CHARS: usize = 1_000;
const MAX_DESCRIPTION_CHARS: usize = 2_000;
const MAX_PREVIEW_CHARS: usize = 10_000;
const MAX_FREE_TEXT_CHARS: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionRequest {
    pub questions: Vec<Question>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<QuestionMetadata>,
}

impl QuestionRequest {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=4).contains(&self.questions.len()) {
            return Err("questions must contain between 1 and 4 items".into());
        }
        let mut total = 0usize;
        for (question_index, question) in self.questions.iter().enumerate() {
            validate_text(
                &question.question,
                MAX_QUESTION_CHARS,
                &format!("questions[{question_index}].question"),
            )?;
            validate_text(
                &question.header,
                12,
                &format!("questions[{question_index}].header"),
            )?;
            if !(2..=4).contains(&question.options.len()) {
                return Err(format!(
                    "questions[{question_index}].options must contain between 2 and 4 items"
                ));
            }
            let mut labels = std::collections::HashSet::new();
            for (option_index, option) in question.options.iter().enumerate() {
                validate_text(
                    &option.label,
                    100,
                    &format!("questions[{question_index}].options[{option_index}].label"),
                )?;
                validate_text(
                    &option.description,
                    MAX_DESCRIPTION_CHARS,
                    &format!("questions[{question_index}].options[{option_index}].description"),
                )?;
                if !labels.insert(option.label.as_str()) {
                    return Err(format!(
                        "questions[{question_index}].options contains duplicate label {:?}",
                        option.label
                    ));
                }
                if let Some(preview) = &option.preview {
                    validate_text(
                        preview,
                        MAX_PREVIEW_CHARS,
                        &format!("questions[{question_index}].options[{option_index}].preview"),
                    )?;
                    if question.multi_select {
                        return Err(format!(
                            "questions[{question_index}] cannot use previews with multiSelect"
                        ));
                    }
                    total = total.saturating_add(preview.chars().count());
                }
                total = total
                    .saturating_add(option.label.chars().count())
                    .saturating_add(option.description.chars().count());
            }
            total = total
                .saturating_add(question.question.chars().count())
                .saturating_add(question.header.chars().count());
        }
        if let Some(source) = self.metadata.as_ref().and_then(|m| m.source.as_ref()) {
            validate_text(source, 200, "metadata.source")?;
            total = total.saturating_add(source.chars().count());
        }
        if total > MAX_TOTAL_CHARS {
            return Err(format!(
                "question payload exceeds the {MAX_TOTAL_CHARS}-character limit"
            ));
        }
        Ok(())
    }

    pub fn validate_answers(&self, answers: &[QuestionAnswer]) -> Result<(), String> {
        if answers.len() != self.questions.len() {
            return Err(format!(
                "questioner returned {} answers for {} questions",
                answers.len(),
                self.questions.len()
            ));
        }
        for (expected_index, answer) in answers.iter().enumerate() {
            if answer.question_index != expected_index {
                return Err(format!(
                    "questioner answer order drift at index {expected_index}"
                ));
            }
            let question = &self.questions[expected_index];
            let mut selected = std::collections::HashSet::new();
            for option_index in &answer.selected {
                if *option_index >= question.options.len() {
                    return Err(format!(
                        "questioner selected an unknown option for question {expected_index}"
                    ));
                }
                if !selected.insert(*option_index) {
                    return Err(format!(
                        "questioner selected option {option_index} twice for question {expected_index}"
                    ));
                }
            }
            if !question.multi_select && answer.selected.len() > 1 {
                return Err(format!(
                    "questioner selected multiple options for single-select question {expected_index}"
                ));
            }
            if let Some(other) = &answer.other {
                validate_text(
                    other,
                    MAX_FREE_TEXT_CHARS,
                    &format!("answers[{expected_index}].other"),
                )?;
            }
            if let Some(notes) = &answer.notes {
                validate_text(
                    notes,
                    MAX_FREE_TEXT_CHARS,
                    &format!("answers[{expected_index}].notes"),
                )?;
            }
            if answer.selected.is_empty() && answer.other.is_none() && answer.notes.is_none() {
                return Err(format!(
                    "questioner returned no selection, Other text, or notes for question {expected_index}"
                ));
            }
        }
        Ok(())
    }
}

fn validate_text(value: &str, max: usize, field: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.chars().count() > max {
        return Err(format!("{field} exceeds the {max}-character limit"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionAnswer {
    pub question_index: usize,
    #[serde(default)]
    pub selected: Vec<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuestionOutcome {
    Answered(Vec<QuestionAnswer>),
    Cancelled,
    Unavailable(String),
}

pub trait Questioner: Send + Sync {
    fn ask(
        &self,
        request: QuestionRequest,
    ) -> Pin<Box<dyn Future<Output = QuestionOutcome> + Send + '_>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(multi_select: bool, preview: Option<&str>) -> QuestionRequest {
        QuestionRequest {
            questions: vec![Question {
                question: "Which option?".into(),
                header: "Choice".into(),
                options: vec![
                    QuestionOption {
                        label: "A".into(),
                        description: "first".into(),
                        preview: preview.map(str::to_string),
                    },
                    QuestionOption {
                        label: "B".into(),
                        description: "second".into(),
                        preview: None,
                    },
                ],
                multi_select,
            }],
            metadata: None,
        }
    }

    #[test]
    fn validates_request_and_answer_contract() {
        let req = request(false, Some("A preview"));
        assert_eq!(req.validate(), Ok(()));
        assert_eq!(
            req.validate_answers(&[QuestionAnswer {
                question_index: 0,
                selected: vec![0],
                other: None,
                notes: Some("ship it".into()),
            }]),
            Ok(())
        );
    }

    #[test]
    fn rejects_preview_on_multi_select_and_bad_answer_indices() {
        let req = request(true, Some("preview"));
        assert!(req.validate().unwrap_err().contains("previews"));

        let req = request(false, None);
        assert!(
            req.validate_answers(&[QuestionAnswer {
                question_index: 0,
                selected: vec![9],
                other: None,
                notes: None,
            }])
            .unwrap_err()
            .contains("unknown option")
        );
    }
}
