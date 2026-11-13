//! Incremental SSE parser for streamable-HTTP responses: feed arbitrary byte
//! chunks, get complete frames. Each MCP SSE frame's `data` is one JSON-RPC
//! message. (A near-twin of provider's parser; kept local so the two crates
//! stay decoupled and can diverge.)
//!
//! Buffers raw bytes and decodes only whole frames — a multibyte UTF-8
//! character split across a chunk boundary would mangle into `U+FFFD` if each
//! chunk were decoded on its own; a complete frame is always valid UTF-8
//! because the `\n`/`\r` separators are ASCII.
//!
//! Scanning resumes where the previous feed stopped (minus a few bytes for a
//! separator split across the boundary): a large frame arriving in many chunks
//! costs one pass, not one pass per chunk.

#[derive(Default)]
pub(crate) struct SseParser {
    buf: Vec<u8>,
    /// Bytes of `buf` already searched for a frame separator.
    scanned: usize,
    #[cfg(test)]
    pub(crate) examined: usize,
}

impl SseParser {
    /// Feed a chunk; return the `data` payloads of any frames it completed.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            // A `\r\n\r\n` separator may straddle the previous scan point.
            let start = self.scanned.saturating_sub(3);
            #[cfg(test)]
            {
                self.examined += self.buf.len().saturating_sub(start);
            }
            let Some(end) = frame_end(&self.buf, start) else {
                self.scanned = self.buf.len();
                break;
            };
            let raw: Vec<u8> = self.buf.drain(..end).collect();
            self.scanned = self.scanned.saturating_sub(end);
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

    /// Bytes buffered for an unfinished frame.
    pub(crate) fn buffered_len(&self) -> usize {
        self.buf.len()
    }
}

/// Byte index just past the first blank-line frame separator (`\n\n` or
/// `\r\n\r\n`) at or after `from`, or None if no complete frame is buffered yet.
fn frame_end(buf: &[u8], from: usize) -> Option<usize> {
    for i in from..buf.len() {
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

    /// A frame split across many chunks is scanned once, not once per chunk.
    #[test]
    fn unfinished_frame_is_not_rescanned_from_the_start() {
        const CHUNK: usize = 16 * 1024;
        const CHUNKS: usize = 64; // 1 MiB, below the wire cap
        let mut p = SseParser::default();
        for _ in 0..CHUNKS {
            assert!(p.feed(&vec![b'x'; CHUNK]).is_empty());
        }
        assert_eq!(p.buffered_len(), CHUNK * CHUNKS);
        // The old parser examined the whole buffer every feed (CHUNKS/2 × the
        // buffer size, ~32 MiB); the resuming scan touches each byte about once.
        assert!(
            p.examined < 2 * CHUNK * CHUNKS,
            "examined {} bytes",
            p.examined
        );

        // A separator that straddles the boundary still closes the frame.
        assert!(p.feed(b"\n\n").is_empty());
        assert_eq!(p.buffered_len(), 0);
    }
}
