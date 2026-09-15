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
        description: "Fetch a URL and return its bounded plain-text content (HTML is converted, tags stripped). HTTP is upgraded to HTTPS. Same-site redirects are followed; a cross-host redirect is reported back so you can fetch the new URL explicitly. Refuses private/internal addresses and embedded credentials. This tool returns page content directly; it does not run a second model prompt over the page. The whole body is returned — there is no text cap — so a large target (a spec, an OpenAPI document, a full page dump) is offloaded to a file whose path the result names. Do not read it back: query it in place with bash (`python3 -c '...'` printing just the fields you need), and do not fetch it a second time.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "format": "uri",
                    "description": "Full URL to fetch (http or https)"
                }
            },
            "required": ["url"],
            "additionalProperties": false
        }),
    }];
    if let Some(backend) = search_backend {
        defs.push(ToolDef {
            name: WEB_SEARCH.into(),
            description: format!(
                "Search the web (via {backend}). Returns bounded results as title, URL and snippet; fetch a result with web_fetch for the full page. Optional domain lists restrict the returned URLs."
            ),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "minLength": 2,
                        "description": "The search query"
                    },
                    "allowed_domains": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Only include results from these domains"
                    },
                    "blocked_domains": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Never include results from these domains"
                    }
                },
                "required": ["query"],
                "additionalProperties": false
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
        let fetch = ToolDef {
            name: "web_fetch".into(),
            description: "Fetch a URL and return its bounded plain-text content (HTML is converted, tags stripped). HTTP is upgraded to HTTPS. Same-site redirects are followed; a cross-host redirect is reported back so you can fetch the new URL explicitly. Refuses private/internal addresses and embedded credentials. This tool returns page content directly; it does not run a second model prompt over the page. The whole body is returned — there is no text cap — so a large target (a spec, an OpenAPI document, a full page dump) is offloaded to a file whose path the result names. Do not read it back: query it in place with bash (`python3 -c '...'` printing just the fields you need), and do not fetch it a second time.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "format": "uri",
                        "description": "Full URL to fetch (http or https)"
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        };
        assert_eq!(tool_defs(None), vec![fetch.clone()]);

        assert_eq!(
            tool_defs(Some("tavily")),
            vec![
                fetch,
                ToolDef {
                    name: "web_search".into(),
                    description: "Search the web (via tavily). Returns bounded results as title, URL and snippet; fetch a result with web_fetch for the full page. Optional domain lists restrict the returned URLs.".into(),
                    schema: json!({
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "minLength": 2,
                                "description": "The search query"
                            },
                            "allowed_domains": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "Only include results from these domains"
                            },
                            "blocked_domains": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "Never include results from these domains"
                            }
                        },
                        "required": ["query"],
                        "additionalProperties": false
                    }),
                }
            ]
        );
    }
}
