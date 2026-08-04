use std::borrow::Cow;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use indexmap::IndexMap;
use kloop_protocol::ContentBlock;
use kloop_protocol::ToolResultContent;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::image::image_block_from_bytes;

pub(crate) const MAX_NOTEBOOK_BYTES: usize = 10 * 1024 * 1024;
const MAX_NOTEBOOK_CELLS: usize = 10_000;
const MAX_NOTEBOOK_TEXT_CHARS: usize = 7_000;
const MAX_NOTEBOOK_IMAGES: usize = 16;
const MAX_NOTEBOOK_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_RESULT_SOURCE_CHARS: usize = 200;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum OrderedValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<OrderedValue>),
    Object(IndexMap<String, OrderedValue>),
}

impl OrderedValue {
    fn as_array(&self) -> Option<&[OrderedValue]> {
        match self {
            Self::Array(values) => Some(values),
            _ => None,
        }
    }

    fn as_array_mut(&mut self) -> Option<&mut Vec<OrderedValue>> {
        match self {
            Self::Array(values) => Some(values),
            _ => None,
        }
    }

    fn as_object(&self) -> Option<&IndexMap<String, OrderedValue>> {
        match self {
            Self::Object(value) => Some(value),
            _ => None,
        }
    }

    fn as_object_mut(&mut self) -> Option<&mut IndexMap<String, OrderedValue>> {
        match self {
            Self::Object(value) => Some(value),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }
}

pub(super) struct NotebookReadOutput {
    pub content: ToolResultContent,
    pub editable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditMode {
    Replace,
    Insert,
    Delete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct NotebookEditRequest {
    cell_id: Option<String>,
    new_source: String,
    cell_type: Option<String>,
    mode: EditMode,
}

pub(super) struct NotebookMutation {
    pub bytes: Vec<u8>,
    pub content: String,
}

pub(crate) struct NotebookPreview {
    pub header: String,
    pub old_source: String,
    pub new_source: String,
}

pub(super) fn read_notebook(bytes: &[u8]) -> Result<NotebookReadOutput> {
    let notebook = parse_notebook(bytes)?;
    let cells = notebook_cells(&notebook)?;
    let mut blocks = Vec::new();
    let mut image_count = 0usize;
    let mut total_image_bytes = 0usize;

    for (index, cell) in cells.iter().enumerate() {
        let object = cell
            .as_object()
            .with_context(|| format!("Notebook cell {index} must be a JSON object"))?;
        let cell_type = object
            .get("cell_type")
            .and_then(OrderedValue::as_str)
            .with_context(|| format!("Notebook cell {index} is missing a string cell_type"))?;
        let source = object
            .get("source")
            .with_context(|| format!("Notebook cell {index} is missing source"))
            .and_then(source_text)?;
        let id = effective_cell_id(object, index);
        let escaped_id = escape_attribute(&id);
        let mut tags = String::new();
        if cell_type != "code" {
            tags.push_str(&format!("<cell_type>{cell_type}</cell_type>"));
        } else if notebook_language(&notebook) != "python" {
            tags.push_str(&format!(
                "<language>{}</language>",
                notebook_language(&notebook)
            ));
        }
        push_merged_text(
            &mut blocks,
            format!("<cell id=\"{escaped_id}\">{tags}{source}</cell id=\"{escaped_id}\">"),
        );
        if cell_type == "code" {
            if let Some(outputs) = object.get("outputs") {
                for output in outputs
                    .as_array()
                    .with_context(|| format!("Notebook code cell {id} has non-array outputs"))?
                {
                    let (text, output_images) = render_output(output, &id)?;
                    if let Some(text) = text.filter(|text| !text.is_empty()) {
                        push_merged_text(&mut blocks, format!("\n{text}"));
                    }
                    for image in output_images {
                        image_count += 1;
                        total_image_bytes = total_image_bytes.saturating_add(image.len());
                        if image_count > MAX_NOTEBOOK_IMAGES
                            || total_image_bytes > MAX_NOTEBOOK_IMAGE_BYTES
                        {
                            bail!(
                                "Notebook images exceed the bounded result limit ({MAX_NOTEBOOK_IMAGES} images or {MAX_NOTEBOOK_IMAGE_BYTES} decoded bytes)"
                            );
                        }
                        blocks.push(image_block_from_bytes(&image).with_context(|| {
                            format!("Notebook code cell {id} contains an invalid image output")
                        })?);
                    }
                }
            }
        }
    }

    let (blocks, truncated) = bound_text_blocks(blocks);
    let content = match blocks.as_slice() {
        [ContentBlock::Text { text }] => ToolResultContent::Text(text.clone()),
        _ => ToolResultContent::Blocks(blocks),
    };
    Ok(NotebookReadOutput {
        content,
        editable: !truncated,
    })
}

fn notebook_language(notebook: &OrderedValue) -> &str {
    notebook
        .as_object()
        .and_then(|root| root.get("metadata"))
        .and_then(OrderedValue::as_object)
        .and_then(|metadata| metadata.get("language_info"))
        .and_then(OrderedValue::as_object)
        .and_then(|language| language.get("name"))
        .and_then(OrderedValue::as_str)
        .unwrap_or("python")
}

fn push_merged_text(blocks: &mut Vec<ContentBlock>, text: String) {
    if let Some(ContentBlock::Text { text: previous }) = blocks.last_mut() {
        previous.push('\n');
        previous.push_str(&text);
    } else {
        blocks.push(ContentBlock::Text { text });
    }
}

fn bound_text_blocks(blocks: Vec<ContentBlock>) -> (Vec<ContentBlock>, bool) {
    let mut visible = Vec::with_capacity(blocks.len());
    let mut remaining = MAX_NOTEBOOK_TEXT_CHARS;
    let mut truncated = false;
    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                let count = text.chars().count();
                if count <= remaining {
                    remaining -= count;
                    visible.push(ContentBlock::Text { text });
                    continue;
                }
                let (mut prefix, _) = char_prefix_owned(&text, remaining);
                prefix.push_str(
                    "\n\n[notebook output truncated; remove large cell outputs or split the notebook before editing it]",
                );
                visible.push(ContentBlock::Text { text: prefix });
                truncated = true;
                break;
            }
            image @ ContentBlock::Image { .. } => visible.push(image),
            _ => unreachable!("notebook Read emits only text and image blocks"),
        }
    }
    (visible, truncated)
}

pub(super) fn request_from_input(input: &Value) -> Result<NotebookEditRequest> {
    let object = input
        .as_object()
        .context("notebook_edit: input must be an object")?;
    let allowed = [
        "notebook_path",
        "cell_id",
        "new_source",
        "cell_type",
        "edit_mode",
    ];
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        bail!("notebook_edit: unexpected input field {key:?}");
    }
    let new_source = input["new_source"]
        .as_str()
        .context("notebook_edit: missing required string argument 'new_source'")?
        .to_string();
    let cell_id = optional_string(input, "cell_id", "notebook_edit")?;
    let cell_type = optional_string(input, "cell_type", "notebook_edit")?;
    if cell_type
        .as_deref()
        .is_some_and(|value| !matches!(value, "code" | "markdown"))
    {
        bail!("notebook_edit: cell_type must be code or markdown");
    }
    let mode = match input["edit_mode"].as_str().unwrap_or("replace") {
        "replace" => EditMode::Replace,
        "insert" => EditMode::Insert,
        "delete" => EditMode::Delete,
        other => bail!("notebook_edit: unsupported edit_mode {other:?}"),
    };
    match mode {
        EditMode::Insert if cell_type.is_none() => {
            bail!("Cell type is required when using edit_mode=insert.")
        }
        EditMode::Replace | EditMode::Delete if cell_id.is_none() => {
            bail!("Cell ID must be specified when not inserting a new cell.")
        }
        _ => {}
    }
    Ok(NotebookEditRequest {
        cell_id,
        new_source,
        cell_type,
        mode,
    })
}

pub(super) fn apply_edit(bytes: &[u8], request: &NotebookEditRequest) -> Result<NotebookMutation> {
    let mut notebook = parse_notebook(bytes)?;
    let supports_cell_ids = notebook_supports_cell_ids(&notebook);
    let cells = notebook_cells_mut(&mut notebook)?;
    let content = match request.mode {
        EditMode::Replace => {
            let requested = request.cell_id.as_deref().expect("validated cell id");
            let index = find_cell(cells, requested)?;
            let cell = cells[index]
                .as_object_mut()
                .context("Notebook cell must be a JSON object")?;
            if let Some(cell_type) = &request.cell_type {
                cell.insert(
                    "cell_type".to_string(),
                    OrderedValue::String(cell_type.clone()),
                );
            }
            cell.insert(
                "source".to_string(),
                OrderedValue::String(request.new_source.clone()),
            );
            if cell.contains_key("execution_count") {
                cell.insert("execution_count".to_string(), OrderedValue::Null);
            }
            if cell.contains_key("outputs") {
                cell.insert("outputs".to_string(), OrderedValue::Array(Vec::new()));
            }
            format!(
                "Updated cell {requested} with {}",
                result_source(&request.new_source)
            )
        }
        EditMode::Insert => {
            let insert_at = match request.cell_id.as_deref() {
                Some(requested) => find_cell(cells, requested)? + 1,
                None => 0,
            };
            let generated_id = supports_cell_ids.then(generated_cell_id).transpose()?;
            let display_id = generated_id
                .clone()
                .unwrap_or_else(|| format!("cell-{insert_at}"));
            let cell_type = request.cell_type.as_deref().expect("validated cell type");
            let mut cell = IndexMap::new();
            cell.insert(
                "cell_type".to_string(),
                OrderedValue::String(cell_type.to_string()),
            );
            if let Some(id) = generated_id {
                cell.insert("id".to_string(), OrderedValue::String(id));
            }
            cell.insert(
                "source".to_string(),
                OrderedValue::String(request.new_source.clone()),
            );
            cell.insert(
                "metadata".to_string(),
                OrderedValue::Object(IndexMap::new()),
            );
            if cell_type == "code" {
                cell.insert("execution_count".to_string(), OrderedValue::Null);
                cell.insert("outputs".to_string(), OrderedValue::Array(Vec::new()));
            }
            cells.insert(insert_at, OrderedValue::Object(cell));
            format!(
                "Inserted cell {display_id} with {}",
                result_source(&request.new_source)
            )
        }
        EditMode::Delete => {
            let requested = request.cell_id.as_deref().expect("validated cell id");
            let index = find_cell(cells, requested)?;
            cells.remove(index);
            format!("Deleted cell {requested}")
        }
    };
    let bytes = serialize_notebook(&notebook)?;
    if bytes.len() > MAX_NOTEBOOK_BYTES {
        bail!("Notebook edit result exceeds the {MAX_NOTEBOOK_BYTES} byte limit");
    }
    Ok(NotebookMutation { bytes, content })
}

pub(crate) fn change_preview(bytes: &[u8], input: &Value) -> Result<NotebookPreview> {
    let request = request_from_input(input)?;
    let notebook = parse_notebook(bytes)?;
    let cells = notebook_cells(&notebook)?;
    match request.mode {
        EditMode::Replace => {
            let requested = request.cell_id.as_deref().expect("validated cell id");
            let index = find_cell(cells, requested)?;
            let old_source = cell_source(&cells[index], requested)?;
            Ok(NotebookPreview {
                header: format!("(replace notebook cell {requested})"),
                old_source,
                new_source: request.new_source,
            })
        }
        EditMode::Insert => {
            if let Some(requested) = request.cell_id.as_deref() {
                find_cell(cells, requested)?;
            }
            let location = request
                .cell_id
                .as_deref()
                .map_or_else(|| "at start".to_string(), |id| format!("after {id}"));
            Ok(NotebookPreview {
                header: format!(
                    "(insert {} notebook cell {location})",
                    request.cell_type.as_deref().expect("validated cell type")
                ),
                old_source: String::new(),
                new_source: request.new_source,
            })
        }
        EditMode::Delete => {
            let requested = request.cell_id.as_deref().expect("validated cell id");
            let index = find_cell(cells, requested)?;
            let old_source = cell_source(&cells[index], requested)?;
            Ok(NotebookPreview {
                header: format!("(delete notebook cell {requested})"),
                old_source,
                new_source: String::new(),
            })
        }
    }
}

fn parse_notebook(bytes: &[u8]) -> Result<OrderedValue> {
    if bytes.len() > MAX_NOTEBOOK_BYTES {
        bail!("Notebook file is over the {MAX_NOTEBOOK_BYTES} byte limit");
    }
    serde_json::from_slice(bytes).map_err(|error| {
        anyhow!(
            "Notebook file is not valid JSON (it may be truncated, corrupted, or still being written): {error}"
        )
    })
}

fn notebook_supports_cell_ids(notebook: &OrderedValue) -> bool {
    let Some(root) = notebook.as_object() else {
        return false;
    };
    let number = |key: &str| match root.get(key) {
        Some(OrderedValue::Number(value)) => value.as_u64(),
        _ => None,
    };
    match (number("nbformat"), number("nbformat_minor")) {
        (Some(major), _) if major > 4 => true,
        (Some(4), Some(minor)) => minor >= 5,
        _ => false,
    }
}

fn notebook_cells(notebook: &OrderedValue) -> Result<&[OrderedValue]> {
    let object = notebook
        .as_object()
        .context("Notebook root must be a JSON object")?;
    let cells = object
        .get("cells")
        .context("Notebook root is missing cells")?
        .as_array()
        .context("Notebook cells must be an array")?;
    if cells.len() > MAX_NOTEBOOK_CELLS {
        bail!("Notebook has more than {MAX_NOTEBOOK_CELLS} cells");
    }
    Ok(cells)
}

fn notebook_cells_mut(notebook: &mut OrderedValue) -> Result<&mut Vec<OrderedValue>> {
    let object = notebook
        .as_object_mut()
        .context("Notebook root must be a JSON object")?;
    let cells = object
        .get_mut("cells")
        .context("Notebook root is missing cells")?
        .as_array_mut()
        .context("Notebook cells must be an array")?;
    if cells.len() > MAX_NOTEBOOK_CELLS {
        bail!("Notebook has more than {MAX_NOTEBOOK_CELLS} cells");
    }
    Ok(cells)
}

fn source_text(value: &OrderedValue) -> Result<String> {
    match value {
        OrderedValue::String(value) => Ok(value.clone()),
        OrderedValue::Array(values) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .context("Notebook source arrays may contain only strings")
            })
            .collect::<Result<Vec<_>>>()
            .map(|parts| parts.concat()),
        _ => bail!("Notebook cell source must be a string or an array of strings"),
    }
}

fn cell_source(cell: &OrderedValue, id: &str) -> Result<String> {
    cell.as_object()
        .with_context(|| format!("Notebook cell {id} must be a JSON object"))?
        .get("source")
        .with_context(|| format!("Notebook cell {id} is missing source"))
        .and_then(source_text)
}

fn effective_cell_id<'a>(cell: &'a IndexMap<String, OrderedValue>, index: usize) -> Cow<'a, str> {
    cell.get("id")
        .and_then(OrderedValue::as_str)
        .map(Cow::Borrowed)
        .unwrap_or_else(|| Cow::Owned(format!("cell-{index}")))
}

fn find_cell(cells: &[OrderedValue], requested: &str) -> Result<usize> {
    cells
        .iter()
        .enumerate()
        .find_map(|(index, cell)| {
            cell.as_object()
                .filter(|cell| effective_cell_id(cell, index) == requested)
                .map(|_| index)
        })
        .with_context(|| format!("Cell with ID \"{requested}\" not found in notebook."))
}

fn render_output(output: &OrderedValue, cell_id: &str) -> Result<(Option<String>, Vec<Vec<u8>>)> {
    let object = output
        .as_object()
        .with_context(|| format!("Notebook code cell {cell_id} has a non-object output"))?;
    let output_type = object
        .get("output_type")
        .and_then(OrderedValue::as_str)
        .unwrap_or_default();
    let mut images = Vec::new();
    let text = match output_type {
        "stream" => object.get("text").map(source_text).transpose()?,
        "execute_result" | "display_data" => {
            let data = object.get("data").and_then(OrderedValue::as_object);
            if let Some(data) = data {
                for media_type in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
                    if let Some(encoded) = data.get(media_type) {
                        let encoded = source_text(encoded)?;
                        let decoded = STANDARD.decode(encoded.trim()).with_context(|| {
                            format!(
                                "Notebook code cell {cell_id} contains invalid base64 for {media_type}"
                            )
                        })?;
                        images.push(decoded);
                    }
                }
                data.get("text/plain").map(source_text).transpose()?
            } else {
                None
            }
        }
        "error" => {
            let name = object
                .get("ename")
                .and_then(OrderedValue::as_str)
                .unwrap_or("Error");
            let value = object
                .get("evalue")
                .and_then(OrderedValue::as_str)
                .unwrap_or_default();
            let mut rendered = if value.is_empty() {
                name.to_string()
            } else {
                format!("{name}: {value}")
            };
            if let Some(traceback) = object.get("traceback") {
                let traceback = source_lines(traceback)?;
                if !traceback.is_empty() {
                    rendered.push('\n');
                    rendered.push_str(&traceback.join("\n"));
                }
            }
            Some(rendered)
        }
        _ => None,
    };
    Ok((text, images))
}

fn source_lines(value: &OrderedValue) -> Result<Vec<String>> {
    match value {
        OrderedValue::String(value) => Ok(vec![value.clone()]),
        OrderedValue::Array(values) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .context("Notebook text arrays may contain only strings")
            })
            .collect(),
        _ => bail!("Notebook text must be a string or an array of strings"),
    }
}

fn serialize_notebook(notebook: &OrderedValue) -> Result<Vec<u8>> {
    let writer = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut serializer = serde_json::Serializer::with_formatter(writer, formatter);
    notebook
        .serialize(&mut serializer)
        .context("Notebook could not be serialized")?;
    Ok(serializer.into_inner())
}

fn generated_cell_id() -> Result<String> {
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| anyhow!("notebook_edit: could not generate a cell id: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn optional_string(input: &Value, key: &str, tool: &str) -> Result<Option<String>> {
    match &input[key] {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value.clone())),
        _ => bail!("{tool}: {key} must be a string"),
    }
}

fn result_source(source: &str) -> String {
    let (visible, truncated) = char_prefix_owned(source, MAX_RESULT_SOURCE_CHARS);
    if truncated {
        format!("{visible}…")
    } else {
        visible
    }
}

fn char_prefix_owned(text: &str, max_chars: usize) -> (String, bool) {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => (text[..end].to_string(), true),
        None => (text.to_string(), false),
    }
}

fn escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloop_protocol::ImageSource;
    use serde_json::json;

    const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    fn notebook() -> Vec<u8> {
        br##"{
 "nbformat": 4,
 "nbformat_minor": 5,
 "metadata": {"keep": true},
 "cells": [
  {"cell_type":"markdown","id":"m","metadata":{"tag":"keep"},"source":["# H\n","body\n"],"unknown":1},
  {"cell_type":"code","id":"c","metadata":{"collapsed":false},"source":"print(1)\n","execution_count":7,"outputs":[{"output_type":"stream","text":["hi\n"]}],"unknown":2},
  {"cell_type":"raw","metadata":{},"source":"raw\n"}
 ],
 "unknown_top": {"keep": true}
}
"##
        .to_vec()
    }

    #[test]
    fn renders_cells_outputs_and_fallback_ids() {
        let output = read_notebook(&notebook()).unwrap();
        assert!(output.editable);
        assert_eq!(
            output.content,
            ToolResultContent::Text(
                "<cell id=\"m\"><cell_type>markdown</cell_type># H\nbody\n</cell id=\"m\">\n<cell id=\"c\">print(1)\n</cell id=\"c\">\n\nhi\n\n<cell id=\"cell-2\"><cell_type>raw</cell_type>raw\n</cell id=\"cell-2\">".into()
            )
        );
    }

    #[test]
    fn returns_code_output_images_but_ignores_markdown_attachments() {
        let bytes = format!(
            r#"{{"cells":[{{"cell_type":"markdown","source":"m","attachments":{{"x":{{"image/png":"{PNG_BASE64}"}}}}}},{{"cell_type":"code","source":"c","outputs":[{{"output_type":"display_data","data":{{"image/png":"{PNG_BASE64}"}}}}]}}]}}"#
        );
        let output = read_notebook(bytes.as_bytes()).unwrap();
        let ToolResultContent::Blocks(blocks) = output.content else {
            panic!("image output was not returned as blocks");
        };
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[0], ContentBlock::Text { .. }));
        assert!(matches!(
            &blocks[1],
            ContentBlock::Image {
                source: ImageSource::Base64 { media_type, .. }
            } if media_type == "image/png"
        ));
    }

    #[test]
    fn replace_preserves_unknown_fields_and_clears_execution_state() {
        let request = request_from_input(&json!({
            "notebook_path": "/tmp/a.ipynb",
            "cell_id": "c",
            "new_source": "x = 2\n"
        }))
        .unwrap();
        let result = apply_edit(&notebook(), &request).unwrap();
        assert_eq!(result.content, "Updated cell c with x = 2\n");
        let value: Value = serde_json::from_slice(&result.bytes).unwrap();
        assert_eq!(value["cells"][1]["source"], "x = 2\n");
        assert_eq!(value["cells"][1]["execution_count"], Value::Null);
        assert_eq!(value["cells"][1]["outputs"], json!([]));
        assert_eq!(value["cells"][1]["unknown"], 2);
        assert_eq!(value["unknown_top"]["keep"], true);
        let serialized = String::from_utf8(result.bytes.clone()).unwrap();
        assert!(
            serialized.starts_with("{\n \"nbformat\": 4,\n \"nbformat_minor\": 5,\n \"metadata\":")
        );
        assert!(
            serialized.find("\"cells\"").unwrap() < serialized.find("\"unknown_top\"").unwrap()
        );
        assert!(!result.bytes.ends_with(b"\n"));
    }

    #[test]
    fn insert_and_delete_use_exact_cell_positions() {
        let insert = request_from_input(&json!({
            "notebook_path": "/tmp/a.ipynb",
            "cell_id": "m",
            "new_source": "inserted\n",
            "cell_type": "code",
            "edit_mode": "insert"
        }))
        .unwrap();
        let inserted = apply_edit(&notebook(), &insert).unwrap();
        let value: Value = serde_json::from_slice(&inserted.bytes).unwrap();
        let id = value["cells"][1]["id"].as_str().unwrap();
        assert_eq!(id.len(), 8);
        assert!(id
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));
        assert_eq!(value["cells"][1]["execution_count"], Value::Null);
        assert_eq!(value["cells"][1]["outputs"], json!([]));

        let delete = request_from_input(&json!({
            "notebook_path": "/tmp/a.ipynb",
            "cell_id": "cell-2",
            "new_source": "",
            "edit_mode": "delete"
        }))
        .unwrap();
        let deleted = apply_edit(&notebook(), &delete).unwrap();
        let value: Value = serde_json::from_slice(&deleted.bytes).unwrap();
        assert_eq!(value["cells"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn fallback_id_replace_does_not_persist_an_id() {
        let request = request_from_input(&json!({
            "notebook_path": "/tmp/a.ipynb",
            "cell_id": "cell-2",
            "new_source": "changed"
        }))
        .unwrap();
        let result = apply_edit(&notebook(), &request).unwrap();
        let value: Value = serde_json::from_slice(&result.bytes).unwrap();
        assert_eq!(value["cells"][2]["source"], "changed");
        assert!(value["cells"][2].get("id").is_none());
    }

    #[test]
    fn old_nbformat_insert_uses_a_fallback_id_without_persisting_one() {
        let bytes = br#"{"nbformat":4,"nbformat_minor":4,"cells":[]}"#;
        let request = request_from_input(&json!({
            "notebook_path": "/tmp/a.ipynb",
            "new_source": "old format",
            "cell_type": "markdown",
            "edit_mode": "insert"
        }))
        .unwrap();
        let result = apply_edit(bytes, &request).unwrap();
        assert_eq!(result.content, "Inserted cell cell-0 with old format");
        let value: Value = serde_json::from_slice(&result.bytes).unwrap();
        assert!(value["cells"][0].get("id").is_none());
    }

    #[test]
    fn truncated_read_does_not_grant_edit_qualification() {
        let source = "x".repeat(MAX_NOTEBOOK_TEXT_CHARS + 1);
        let bytes = serde_json::to_vec(&json!({
            "cells": [{"cell_type": "markdown", "id": "m", "source": source}]
        }))
        .unwrap();
        let output = read_notebook(&bytes).unwrap();
        assert!(!output.editable);
        assert!(output
            .content
            .as_text()
            .contains("[notebook output truncated"));
    }

    #[test]
    fn rejects_invalid_contracts_without_mutating_bytes() {
        let original = notebook();
        for input in [
            json!({"notebook_path":"/tmp/a.ipynb","new_source":"x","edit_mode":"insert"}),
            json!({"notebook_path":"/tmp/a.ipynb","new_source":"x"}),
            json!({"notebook_path":"/tmp/a.ipynb","new_source":"x","cell_id":"missing"}),
            json!({"notebook_path":"/tmp/a.ipynb","new_source":"x","cell_id":"c","extra":true}),
        ] {
            let outcome =
                request_from_input(&input).and_then(|request| apply_edit(&original, &request));
            assert!(outcome.is_err());
            assert_eq!(original, notebook());
        }
    }
}
