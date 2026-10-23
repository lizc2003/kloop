use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use super::ToolCtx;
use crate::interaction::MAX_DESCRIPTION_CHARS;
use crate::interaction::MAX_HEADER_CHARS;
use crate::interaction::MAX_LABEL_CHARS;
use crate::interaction::MAX_PREVIEW_CHARS;
use crate::interaction::MAX_QUESTION_CHARS;
use crate::interaction::MAX_SOURCE_CHARS;
use crate::interaction::MAX_TOTAL_CHARS;
use crate::interaction::QuestionAnswer;
use crate::interaction::QuestionMetadata;
use crate::interaction::QuestionOutcome;
use crate::interaction::QuestionRequest;
use kloop_protocol::ToolDef;

/// 仅修正模型工具入参；共享交互类型和前端协议仍严格使用 snake_case。
pub(super) fn normalize_arguments(input: &mut Value) {
    let Some(questions) = input.get_mut("questions").and_then(Value::as_array_mut) else {
        return;
    };
    for question in questions {
        let Some(question) = question.as_object_mut() else {
            continue;
        };
        if !question.contains_key("multi_select")
            && let Some(value) = question.remove("multiSelect")
        {
            question.insert("multi_select".into(), value);
        }
    }
}

pub(super) fn ask_user_question_def() -> ToolDef {
    ToolDef {
        name: "ask_user_question".into(),
        description: format!(
            "Ask the user one to four concrete decision questions only when their answer is genuinely required. Each question has two to four options and always permits an Other free-text answer. Use multi_select for non-exclusive choices and preview only when a visual/code comparison helps. All question text, headers, option labels/descriptions/previews and metadata.source combined must fit within {MAX_TOTAL_CHARS} characters. Do not use this for permission approval or to ask whether an implementation plan is ready."
        ),
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
                            "question": {"type": "string", "minLength": 1, "maxLength": MAX_QUESTION_CHARS, "pattern": ".*\\S.*", "description": "Non-blank full question text."},
                            "header": {"type": "string", "minLength": 1, "maxLength": MAX_HEADER_CHARS, "pattern": ".*\\S.*", "description": format!("Non-blank short display title, at most {MAX_HEADER_CHARS} characters. Put the full question in question.")},
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string", "minLength": 1, "maxLength": MAX_LABEL_CHARS, "pattern": ".*\\S.*", "description": "Non-blank option label, unique within this question."},
                                        "description": {"type": "string", "minLength": 1, "maxLength": MAX_DESCRIPTION_CHARS, "pattern": ".*\\S.*", "description": "Non-blank explanation of this option."},
                                        "preview": {"type": "string", "minLength": 1, "maxLength": MAX_PREVIEW_CHARS, "pattern": ".*\\S.*", "description": "Optional non-blank preview, only for single-select questions."}
                                    },
                                    "required": ["label", "description"],
                                    "additionalProperties": false
                                }
                            },
                            "multi_select": {"type": "boolean", "default": false}
                        },
                        "required": ["question", "header", "options", "multi_select"],
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
                    "properties": {"source": {"type": "string", "minLength": 1, "maxLength": MAX_SOURCE_CHARS, "pattern": ".*\\S.*", "description": "Optional non-blank question source."}},
                    "additionalProperties": false
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        }),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
        bail!(
            "ask_user_question: no interactive user is available in this frontend; do not retry waiting for an answer"
        )
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
                "multi_select": false
            }]
        })
    }

    #[test]
    fn question_schema_exposes_runtime_text_limits() {
        let definition = ask_user_question_def();
        for (schema_path, input_path, max) in [
            (
                "/properties/questions/items/properties/question",
                "/questions/0/question",
                1_000,
            ),
            (
                "/properties/questions/items/properties/header",
                "/questions/0/header",
                32,
            ),
            (
                "/properties/questions/items/properties/options/items/properties/label",
                "/questions/0/options/0/label",
                100,
            ),
            (
                "/properties/questions/items/properties/options/items/properties/description",
                "/questions/0/options/0/description",
                2_000,
            ),
            (
                "/properties/questions/items/properties/options/items/properties/preview",
                "/questions/0/options/0/preview",
                10_000,
            ),
            (
                "/properties/metadata/properties/source",
                "/metadata/source",
                200,
            ),
        ] {
            let schema = definition.schema.pointer(schema_path).unwrap();
            assert_eq!(schema["minLength"], 1, "{schema_path}");
            assert_eq!(schema["maxLength"], max, "{schema_path}");
            assert_eq!(schema["pattern"], ".*\\S.*", "{schema_path}");
            for unit in ["a", "界"] {
                for length in [max, max + 1] {
                    let mut value = input();
                    value["metadata"] = json!({"source": "test"});
                    *value.pointer_mut(input_path).unwrap() = json!(unit.repeat(length));
                    let request: QuestionRequest = serde_json::from_value(value).unwrap();
                    let field = input_path
                        .trim_start_matches('/')
                        .replace("/0/", "[0].")
                        .replace('/', ".");
                    let expected = if length == max {
                        Ok(())
                    } else {
                        Err(format!("{field} exceeds the {max}-character limit"))
                    };
                    assert_eq!(request.validate(), expected, "{input_path}: {unit}");
                }
            }
        }
    }

    #[test]
    fn question_schema_prompts_nonblank_text_and_runtime_rejects_whitespace() {
        let definition = ask_user_question_def();
        let validator = jsonschema::validator_for(&definition.schema).unwrap();
        for input_path in [
            "/questions/0/question",
            "/questions/0/header",
            "/questions/0/options/0/label",
            "/questions/0/options/0/description",
            "/questions/0/options/0/preview",
            "/metadata/source",
        ] {
            let field = input_path
                .trim_start_matches('/')
                .replace("/0/", "[0].")
                .replace('/', ".");
            for text in ["", " ", "\n", "\t\r\n", "答", " \n答\n "] {
                let mut value = input();
                value["metadata"] = json!({"source": "test"});
                *value.pointer_mut(input_path).unwrap() = json!(text);
                let expected_valid = !text.trim().is_empty();
                assert_eq!(
                    validator.is_valid(&value),
                    expected_valid,
                    "schema: {input_path} = {text:?}"
                );
                let request: QuestionRequest = serde_json::from_value(value).unwrap();
                let expected = if expected_valid {
                    Ok(())
                } else {
                    Err(format!("{field} must not be empty"))
                };
                assert_eq!(request.validate(), expected, "{input_path} = {text:?}");
            }
            // schema 正则和 trim 的 Unicode 空白集合不保证一致，最终由运行时裁决。
            for text in ["\u{0085}", "\u{3000}", " \u{3000}\t"] {
                let mut value = input();
                value["metadata"] = json!({"source": "test"});
                *value.pointer_mut(input_path).unwrap() = json!(text);
                let request: QuestionRequest = serde_json::from_value(value).unwrap();
                assert_eq!(
                    request.validate(),
                    Err(format!("{field} must not be empty")),
                    "{input_path} = {text:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn second_question_header_enforces_character_limit_through_dispatch() {
        let ctx = ctx_with(QuestionOutcome::Cancelled);
        for unit in ["a", "界"] {
            for length in [12, 13, 20, 32, 33] {
                let mut value = input();
                let mut second = value["questions"][0].clone();
                second["header"] = json!(unit.repeat(length));
                value["questions"].as_array_mut().unwrap().push(second);
                for (name, input) in [
                    ("ask_user_question", value.clone()),
                    (
                        "call_tool",
                        json!({"tool_name": "ask_user_question", "params": value}),
                    ),
                ] {
                    let (out, is_error) = run_tool(name, input, &ctx).await;
                    assert_eq!(is_error, length > 32, "{out}");
                    if is_error {
                        assert_eq!(
                            out,
                            "ask_user_question: questions[1].header exceeds the 32-character limit"
                        );
                    } else {
                        assert!(out.contains("cancelled"), "{out}");
                    }
                }
            }
        }
    }

    #[test]
    fn question_flag_normalization_is_scoped_and_idempotent() {
        let definition = ask_user_question_def();
        let properties = &definition.schema["properties"]["questions"]["items"]["properties"];
        assert_eq!(
            properties["multi_select"],
            json!({"type": "boolean", "default": false})
        );
        assert!(properties.get("multiSelect").is_none());
        let alias = json!({
            "questions": [
                {"multiSelect": false, "options": [{"multiSelect": true}]},
                {"multiSelect": true},
                null
            ],
            "metadata": {"multiSelect": true}
        });
        let canonical = json!({
            "questions": [
                {"multi_select": false, "options": [{"multiSelect": true}]},
                {"multi_select": true},
                null
            ],
            "metadata": {"multiSelect": true}
        });
        let calls = vec![
            ("direct".into(), "ask_user_question".into(), alias.clone()),
            (
                "wrapped".into(),
                "call_tool".into(),
                json!({"tool_name": "ask_user_question", "params": alias}),
            ),
            ("external".into(), "srv__question".into(), alias.clone()),
            ("other".into(), "edit_file".into(), alias.clone()),
            (
                "malformed".into(),
                "ask_user_question".into(),
                json!({"questions": {"multiSelect": true}}),
            ),
        ];
        let mut expected = calls.clone();
        expected[0].2 = canonical.clone();
        expected[1].1 = "ask_user_question".into();
        expected[1].2 = canonical;
        let normalized = crate::tools::normalize_tool_uses(calls);
        assert_eq!(normalized, expected);
        assert_eq!(crate::tools::normalize_tool_uses(normalized), expected);
    }

    #[tokio::test]
    async fn camel_case_flags_support_single_and_multi_select_questions() {
        let mut value = input();
        let first = value["questions"][0].as_object_mut().unwrap();
        let flag = first.remove("multi_select").unwrap();
        first.insert("multiSelect".into(), flag);
        let second = json!({
            "question": "Which colors?",
            "header": "Colors",
            "options": [
                {"label": "Red", "description": "warm"},
                {"label": "Blue", "description": "cool"}
            ],
            "multiSelect": true
        });
        value["questions"].as_array_mut().unwrap().push(second);
        assert!(serde_json::from_value::<QuestionRequest>(value.clone()).is_err());
        let ctx = ctx_with(QuestionOutcome::Answered(vec![
            QuestionAnswer {
                question_index: 0,
                selected: vec![1],
                other: None,
            },
            QuestionAnswer {
                question_index: 1,
                selected: vec![0, 1],
                other: None,
            },
        ]));
        for (name, input) in [
            ("ask_user_question", value.clone()),
            (
                "call_tool",
                json!({"tool_name": "ask_user_question", "params": value}),
            ),
        ] {
            let (out, is_error) = run_tool(name, input, &ctx).await;
            assert!(!is_error, "{out}");
            assert_eq!(
                out,
                "Your questions have been answered: \"Which layout?\"=\"Columns\" selected preview:\nCOL, \"Which colors?\"=\"Red, Blue\". You can now continue with these answers in mind."
            );
        }
    }

    #[tokio::test]
    async fn camel_case_flags_preserve_strict_input_validation() {
        let ctx = ctx_with(QuestionOutcome::Cancelled);
        for (canonical, alias, expected) in [
            (
                Some(json!(false)),
                json!(false),
                "unknown field `multiSelect`",
            ),
            (
                Some(json!(false)),
                json!(true),
                "unknown field `multiSelect`",
            ),
            (
                Some(json!(true)),
                json!(false),
                "unknown field `multiSelect`",
            ),
            (None, Value::Null, "expected a boolean"),
            (None, json!("true"), "expected a boolean"),
            (None, json!(1), "expected a boolean"),
            (None, json!(true), "cannot use previews with multi_select"),
        ] {
            let mut value = input();
            let question = value["questions"][0].as_object_mut().unwrap();
            question.remove("multi_select");
            if let Some(canonical) = canonical {
                question.insert("multi_select".into(), canonical);
            }
            question.insert("multiSelect".into(), alias);
            let (out, is_error) = run_tool("ask_user_question", value, &ctx).await;
            assert!(is_error, "{out}");
            assert!(out.contains(expected), "{out}");
        }
        let mut value = input();
        let question = value["questions"][0].as_object_mut().unwrap();
        question.remove("multi_select");
        question.insert("multiSelect".into(), json!(false));
        question.insert("multi_select_typo".into(), json!(false));
        let (out, is_error) = run_tool("ask_user_question", value, &ctx).await;
        assert!(is_error, "{out}");
        assert!(out.contains("unknown field `multi_select_typo`"), "{out}");
    }

    #[tokio::test]
    async fn answered_formats_selected_preview() {
        let ctx = ctx_with(QuestionOutcome::Answered(vec![QuestionAnswer {
            question_index: 0,
            selected: vec![1],
            other: None,
        }]));
        let (out, is_error) = run_tool("ask_user_question", input(), &ctx).await;
        assert!(!is_error, "{out}");
        assert_eq!(
            out,
            "Your questions have been answered: \"Which layout?\"=\"Columns\" selected preview:\nCOL. You can now continue with these answers in mind."
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
        }]));
        let mut value = input();
        value["answers"] = json!({"Which layout?": "Columns"});
        let (out, is_error) = run_tool("ask_user_question", value, &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("Rows"));
        assert!(!out.contains("Columns"));
    }
}
