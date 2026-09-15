//! Minimal HTML→text: drop script/style/comments, turn block boundaries
//! into newlines, strip tags, decode common entities. Good enough for docs
//! pages; deliberately not a spec-grade parser (no extra dependencies).

pub(crate) fn html_to_text(html: &str) -> String {
    let stripped = strip_container(html, "script");
    let stripped = strip_container(&stripped, "style");
    let stripped = strip_comments(&stripped);

    let mut out = String::with_capacity(stripped.len() / 2);
    let mut chars = stripped.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '<' => {
                let rest = &stripped[i + 1..];
                let Some(end) = rest.find('>') else {
                    break; // unterminated tag: drop the trailing fragment
                };
                let tag = rest[..end]
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if is_block_tag(&tag) {
                    out.push('\n');
                }
                if tag == "li" && !rest.starts_with('/') {
                    out.push_str("- ");
                }
                // Skip to '>' (chars is char-indexed; advance past the tag).
                for (_, c) in chars.by_ref() {
                    if c == '>' {
                        break;
                    }
                }
            }
            '&' => {
                let rest = &stripped[i..];
                match decode_entity(rest) {
                    Some((decoded, len)) => {
                        out.push_str(&decoded);
                        // Skip the entity body ('&' already consumed).
                        for _ in 0..len - 1 {
                            chars.next();
                        }
                    }
                    None => out.push('&'),
                }
            }
            _ => out.push(c),
        }
    }
    collapse_whitespace(&out)
}

/// Also used on search snippets, which carry highlight markup.
pub(crate) fn strip_inline_tags(text: &str) -> String {
    html_to_text(text).replace('\n', " ")
}

/// Remove `<tag ...> ... </tag>` blocks (case-insensitive), content and all.
fn strip_container(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}");
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while let Some(start) = lower[pos..].find(&open) {
        let start = pos + start;
        out.push_str(&html[pos..start]);
        let after = match lower[start..].find(&close) {
            Some(rel) => {
                let close_at = start + rel;
                match lower[close_at..].find('>') {
                    Some(gt) => close_at + gt + 1,
                    None => lower.len(),
                }
            }
            None => lower.len(),
        };
        pos = after;
    }
    out.push_str(&html[pos..]);
    out
}

fn strip_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while let Some(start) = html[pos..].find("<!--") {
        let start = pos + start;
        out.push_str(&html[pos..start]);
        pos = match html[start..].find("-->") {
            Some(rel) => start + rel + 3,
            None => html.len(),
        };
    }
    out.push_str(&html[pos..]);
    out
}

fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div"
            | "br"
            | "hr"
            | "li"
            | "ul"
            | "ol"
            | "tr"
            | "table"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "blockquote"
            | "pre"
            | "title"
    )
}

/// Decode one entity at the start of `rest` (which begins with '&').
/// Returns (decoded text, byte length consumed).
fn decode_entity(rest: &str) -> Option<(String, usize)> {
    // Entities are short and ASCII; scan only the first dozen bytes for the
    // ';' terminator (a bare '&' in prose must not reach for a distant ';').
    // Walk by chars, not a raw byte slice: `rest` can hold multibyte text
    // right after the '&', and `rest[..12]` would panic on a non-char boundary.
    let semi = rest
        .char_indices()
        .take_while(|(byte, _)| *byte < 12)
        .find(|(_, c)| *c == ';')
        .map(|(byte, _)| byte)?;
    let body = &rest[1..semi];
    let decoded = match body {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        "nbsp" => " ".to_string(),
        _ => {
            let code = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X"));
            let n = match code {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => body.strip_prefix('#')?.parse().ok()?,
            };
            char::from_u32(n)?.to_string()
        }
    };
    Some((decoded, semi + 1))
}

/// Trim trailing space per line and collapse runs of blank lines.
fn collapse_whitespace(text: &str) -> String {
    let mut out = Vec::new();
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !out.is_empty() {
                out.push(String::new());
            }
        } else {
            blank_run = 0;
            out.push(line.to_string());
        }
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_scripts_styles_and_comments() {
        let html = r#"<html><head><title>Doc</title>
            <style>body { color: red }</style>
            <script type="text/javascript">alert("hi")</script>
            </head><body>
            <!-- hidden note -->
            <h1>Title</h1>
            <p>First <b>bold</b> paragraph.</p>
            <ul><li>one</li><li>two</li></ul>
            </body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Doc"));
        assert!(text.contains("Title"));
        assert!(text.contains("First bold paragraph."));
        assert!(text.contains("- one"), "{text}");
        assert!(text.contains("- two"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("color: red"));
        assert!(!text.contains("hidden note"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn decodes_entities() {
        assert_eq!(
            html_to_text("a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#39; &#x41;&nbsp;f"),
            "a & b <c> \"d\" 'e' A f"
        );
        // Unknown / malformed entities pass through literally.
        assert_eq!(html_to_text("R&D &unknown; &#zzz;"), "R&D &unknown; &#zzz;");
    }

    #[test]
    fn bare_amp_before_multibyte_char_does_not_panic() {
        // A bare '&' followed by ~10 ASCII then a multibyte char once put the
        // 12-byte cutoff mid-character and panicked on untrusted web content.
        assert_eq!(html_to_text("&aaaaaaaaaa中文"), "&aaaaaaaaaa中文");
        // The multibyte char landing exactly on the byte-12 boundary.
        assert_eq!(html_to_text("x &bbbbbbbbbb好 y"), "x &bbbbbbbbbb好 y");
        // A real entity right before multibyte text still decodes.
        assert_eq!(html_to_text("&amp;中文"), "&中文");
    }

    #[test]
    fn block_tags_become_newlines_and_blanks_collapse() {
        let text = html_to_text("<p>a</p><div><div><div>b</div></div></div><h2>c</h2>");
        assert_eq!(text, "a\n\nb\n\nc");
    }

    #[test]
    fn strip_inline_tags_flattens_snippets() {
        assert_eq!(
            strip_inline_tags("The <strong>Rust</strong> language"),
            "The Rust language"
        );
    }

    #[test]
    fn survives_malformed_html() {
        assert_eq!(html_to_text("plain no tags"), "plain no tags");
        assert_eq!(html_to_text("broken <tag never closes"), "broken");
        assert_eq!(html_to_text("<script>never closed"), "");
    }
}
