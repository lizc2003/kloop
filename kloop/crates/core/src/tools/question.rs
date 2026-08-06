use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;

use super::ToolCtx;
use crate::interaction::QuestionAnswer;
use crate::interaction::QuestionMetadata;
use crate::interaction::QuestionOutcome;
use crate::interaction::QuestionRequest;
use kloop_protocol::ToolDef;

pub(super) fn ask_user_question_def() -> ToolDef {
    ToolDef {
        name: "ask_user_question".into(),
        description: "Ask the user one to four concrete decision questions only when their answer is genuinely required. Each question has two to four options and always permits an Other free-text answer. Use multiSelect for non-exclusive choices and preview only when a visual/code comparison helps. Do not use this for permission approval or to ask whether an implementation plan is ready.".into(),
        schema: json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string"},
                            "header": {"type": "string"},
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string"},
                                        "description": {"type": "string"},
                                        "preview": {"type": "string"}
                                    },
                                    "required": ["label", "description"],
                                    "additionalProperties": false
                                }
                            },
                            "multiSelect": {"type": "boolean", "default": false}
                        },
                        "required": ["question", "header", "options", "multiSelect"],
                        "additionalProperties": false
                    }
                },
                "answers": {
                    "type": "object",
                    "additionalProperties": {"type": "string"}
                },
                "annotations": {
                    "type": "object",
                    "additionalProperties": {
                        "type": "object",
                        "properties": {
                            "preview": {"type": "string"},
                            "notes": {"type": "string"}
                        },
                        "additionalProperties": false
                    }
                },
                "metadata": {
                    "type": "object",
                    "properties": {"source": {"type": "string"}},
                    "additionalProperties": false
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        }),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QuestionToolInput {
    questions: Vec<crate::interaction::Question>,
    #[serde(default)]
    metadata: Option<QuestionMetadata>,
    // These fields exist in the observed compatible schema because Claude Code's
    // permission component backfills them. A model-provided value has no
    // authority: kloop always overwrites it with the Questioner outcome.
    #[serde(default, rename = "answers")]
    _answers: Option<Value>,
    #[serde(default, rename = "annotations")]
    _annotations: Option<Value>,
}

pub(super) async fn ask_user_question_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    if ctx.depth >= 1 {
        bail!("ask_user_question: only the top-level agent can ask the user questions");
    }
    let parsed: QuestionToolInput =
        serde_json::from_value(input.clone()).context("ask_user_question: invalid input")?;
    let request = QuestionRequest {
        questions: parsed.questions,
        metadata: parsed.metadata,
    };
    request
        .validate()
        .map_err(|error| anyhow!("ask_user_question: {error}"))?;
    let Some(questioner) = &ctx.cfg.questioner else {
        bail!("ask_user_question: no interactive user is available in this frontend; do not retry waiting for an answer")
    };
    match questioner.ask(request.clone()).await {
        QuestionOutcome::Answered(answers) => {
            request
                .validate_answers(&answers)
                .map_err(|error| anyhow!("ask_user_question: {error}"))?;
            Ok(format_answers(&request, &answers))
        }
        QuestionOutcome::Cancelled => Ok(
            "The user cancelled the questions without answering. Do not assume a choice; continue only if a safe default follows from existing requirements, otherwise explain what remains blocked."
                .into(),
        ),
        QuestionOutcome::Unavailable(reason) => bail!(
            "ask_user_question: the user interaction became unavailable ({reason}); do not retry waiting for an answer"
        ),
    }
}

fn format_answers(request: &QuestionRequest, answers: &[QuestionAnswer]) -> String {
    let mut used_free_text = false;
    let mut entries = Vec::with_capacity(answers.len());
    for answer in answers {
        let question = &request.questions[answer.question_index];
        let mut values: Vec<&str> = answer
            .selected
            .iter()
            .map(|index| question.options[*index].label.as_str())
            .collect();
        if let Some(other) = answer.other.as_deref() {
            used_free_text = true;
            values.push(other);
        }
        if answer.notes.is_some() {
            used_free_text = true;
        }
        let answer_text = if values.is_empty() {
            "(no option selected)".to_string()
        } else {
            quote(&values.join(", "))
        };
        let mut entry = format!("{}={answer_text}", quote(&question.question));
        if let Some(preview) = answer
            .selected
            .first()
            .and_then(|index| question.options[*index].preview.as_deref())
        {
            entry.push_str(" selected preview:\n");
            entry.push_str(preview);
        }
        if let Some(notes) = &answer.notes {
            entry.push_str(" notes: ");
            entry.push_str(notes);
        }
        entries.push(entry);
    }
    if used_free_text {
        format!(
            "The user answered: {}. Read the answers carefully — they may request clarification, changes, or that you not proceed — and follow what they actually say.",
            entries.join(", ")
        )
    } else {
        format!(
            "Your questions have been answered: {}. You can now continue with these answers in mind.",
            entries.join(", ")
        )
    }
}

fn quote(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    use super::*;
    use crate::interaction::Questioner;
    use crate::tools::testutil::run_tool;
    use crate::tools::testutil::test_ctx;

    struct ScriptedQuestioner(QuestionOutcome);

    impl Questioner for ScriptedQuestioner {
        fn ask(
            &self,
            _request: QuestionRequest,
        ) -> Pin<Box<dyn Future<Output = QuestionOutcome> + Send + '_>> {
            let outcome = self.0.clone();
            Box::pin(async move { outcome })
        }
    }

    fn ctx_with(outcome: QuestionOutcome) -> ToolCtx {
        let ctx = test_ctx(0, "question");
        let mut cfg = ctx.cfg.test_clone();
        cfg.questioner = Some(Arc::new(ScriptedQuestioner(outcome)));
        ToolCtx {
            cfg: Arc::new(cfg),
            ..ctx
        }
    }

    fn input() -> Value {
        json!({
            "questions": [{
                "question": "Which layout?",
                "header": "Layout",
                "options": [
                    {"label": "Rows", "description": "stack", "preview": "ROW"},
                    {"label": "Columns", "description": "split", "preview": "COL"}
                ],
                "multiSelect": false
            }]
        })
    }

    #[tokio::test]
    async fn answered_formats_selected_preview_and_notes() {
        let ctx = ctx_with(QuestionOutcome::Answered(vec![QuestionAnswer {
            question_index: 0,
            selected: vec![1],
            other: None,
            notes: Some("keep compact".into()),
        }]));
        let (out, is_error) = run_tool("ask_user_question", input(), &ctx).await;
        assert!(!is_error, "{out}");
        assert_eq!(
            out,
            "The user answered: \"Which layout?\"=\"Columns\" selected preview:\nCOL notes: keep compact. Read the answers carefully — they may request clarification, changes, or that you not proceed — and follow what they actually say."
        );
    }

    #[tokio::test]
    async fn cancellation_is_paired_but_unavailable_is_an_error() {
        let cancelled = ctx_with(QuestionOutcome::Cancelled);
        let (out, is_error) = run_tool("ask_user_question", input(), &cancelled).await;
        assert!(!is_error);
        assert!(out.contains("cancelled"));

        let unavailable = ctx_with(QuestionOutcome::Unavailable("connection closed".into()));
        let (out, is_error) = run_tool("ask_user_question", input(), &unavailable).await;
        assert!(is_error);
        assert!(out.contains("connection closed"));
    }

    #[tokio::test]
    async fn model_prefilled_answers_are_ignored() {
        let ctx = ctx_with(QuestionOutcome::Answered(vec![QuestionAnswer {
            question_index: 0,
            selected: vec![0],
            other: None,
            notes: None,
        }]));
        let mut value = input();
        value["answers"] = json!({"Which layout?": "Columns"});
        let (out, is_error) = run_tool("ask_user_question", value, &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("Rows"));
        assert!(!out.contains("Columns"));
    }
}
