//! Incremental SSE parser for streamable-HTTP responses: feed arbitrary byte
//! chunks, get complete frames. Each MCP SSE frame's `data` is one JSON-RPC
//! message. (A near-twin of provider's parser; kept local so the two crates
//! stay decoupled and can diverge.)
//!
//! Buffers raw bytes and decodes only whole frames — a multibyte UTF-8
//! character split across a chunk boundary would mangle into `U+FFFD` if each
//! chunk were decoded on its own; a complete frame is always valid UTF-8
//! because the `\n`/`\r` separators are ASCII.

#[derive(Default)]
pub(crate) struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    /// Feed a chunk; return the `data` payloads of any frames it completed.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = frame_end(&self.buf) {
            let raw: Vec<u8> = self.buf.drain(..end).collect();
            let raw = String::from_utf8_lossy(&raw);
            let mut data_lines = Vec::new();
            for line in raw.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                }
                // event:/id:/retry:/comments (":...") are irrelevant here.
            }
            if !data_lines.is_empty() {
                out.push(data_lines.join("\n"));
            }
        }
        out
    }
}

/// Byte index just past the first blank-line frame separator (`\n\n` or
/// `\r\n\r\n`), or None if no complete frame is buffered yet.
fn frame_end(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len() {
        if buf[i] != b'\n' {
            continue;
        }
        let lf_lf = i >= 1 && buf[i - 1] == b'\n';
        let crlf_crlf = i >= 2 && buf[i - 1] == b'\r' && buf[i - 2] == b'\n';
        if lf_lf || crlf_crlf {
            return Some(i + 1);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_data_payloads_across_chunks() {
        let mut p = SseParser::default();
        assert_eq!(
            p.feed(b"event: message\ndata: {\"a\""),
            Vec::<String>::new()
        );
        assert_eq!(
            p.feed(b":1}\n\n: keepalive\r\ndata: two\r\n\r\n"),
            vec!["{\"a\":1}".to_string(), "two".to_string()]
        );
    }

    #[test]
    fn multibyte_split_is_not_mangled() {
        let mut p = SseParser::default();
        let full = "data: 你好\n\n".as_bytes();
        let split = 8; // 'data: ' (6) + first byte of '你'
        assert_eq!(p.feed(&full[..split]), Vec::<String>::new());
        assert_eq!(p.feed(&full[split..]), vec!["你好".to_string()]);
    }
}
