//! Agent-facing contracts for the web tools.
//!
//! Core owns the names, descriptions, and input schemas the model sees. The
//! network implementation remains in `kloop-web` and is attached through the
//! ordinary [`super::ToolSource`] seam by the CLI.

use kloop_protocol::ToolDef;
use serde_json::json;

pub const WEB_FETCH: &str = "web_fetch";
pub const WEB_SEARCH: &str = "web_search";

/// Build the web definitions exposed to the model. Fetch is always present;
/// search is present only when the CLI configured a usable backend.
pub fn tool_defs(search_backend: Option<&str>) -> Vec<ToolDef> {
    let mut defs = vec![ToolDef {
        name: WEB_FETCH.into(),
        description: "Fetch a URL and return its content as plain text (HTML is converted, tags stripped). HTTP is upgraded to HTTPS. Same-host redirects are followed; a cross-host redirect is reported back so you can fetch the new URL explicitly. Refuses private/internal addresses. Long pages are truncated.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Full URL to fetch (http or https)"}
            },
            "required": ["url"]
        }),
    }];
    if let Some(backend) = search_backend {
        defs.push(ToolDef {
            name: WEB_SEARCH.into(),
            description: format!(
                "Search the web (via {backend}). Returns the top results as title, URL and snippet; fetch a result with web_fetch for the full page."
            ),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "The search query"},
                    "max_results": {"type": "integer", "description": "Number of results (1-10, default 5)"}
                },
                "required": ["query"]
            }),
        });
    }
    defs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_follow_backend_availability() {
        assert_eq!(
            tool_defs(None),
            vec![ToolDef {
                name: "web_fetch".into(),
                description: "Fetch a URL and return its content as plain text (HTML is converted, tags stripped). HTTP is upgraded to HTTPS. Same-host redirects are followed; a cross-host redirect is reported back so you can fetch the new URL explicitly. Refuses private/internal addresses. Long pages are truncated.".into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "Full URL to fetch (http or https)"}
                    },
                    "required": ["url"]
                }),
            }]
        );

        let defs = tool_defs(Some("tavily"));
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[1].name, "web_search");
        assert!(defs[1].description.contains("via tavily"));
        assert_eq!(defs[1].schema["required"], json!(["query"]));
    }
}
