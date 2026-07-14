/// Incremental SSE parser: feed arbitrary byte chunks, get complete frames.
/// Handles frames split across chunk boundaries, CRLF, multi-line data.
///
/// Buffers raw bytes and decodes only whole frames — never per chunk. A
/// multibyte UTF-8 character (a CJK glyph, an emoji) split across a chunk
/// boundary would be mangled into `U+FFFD` if each chunk were decoded on its
/// own; a complete frame is always valid UTF-8 because the `\n`/`\r`
/// separators are ASCII and can never fall inside a multibyte sequence.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(end) = frame_end(&self.buf) {
            let raw: Vec<u8> = self.buf.drain(..end).collect();
            let raw = String::from_utf8_lossy(&raw);
            let mut event = None;
            let mut data_lines = Vec::new();
            for line in raw.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event = Some(rest.trim().to_string());
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                }
                // comments (":...") and other fields ignored
            }
            if event.is_some() || !data_lines.is_empty() {
                frames.push(SseFrame {
                    event,
                    data: data_lines.join("\n"),
                });
            }
        }
        frames
    }
}

/// Byte index just past the first blank-line frame separator (`\n\n` or
/// `\r\n\r\n`), or None if no complete frame is buffered yet. A blank line is
/// an LF whose preceding line terminator is right in front of it.
fn frame_end(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len() {
        if buf[i] != b'\n' {
            continue;
        }
        // The blank line's own LF at `i`, sitting right after the previous
        // line's terminator: `\n` (LF) or `\r\n` (CRLF).
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
    fn parses_frames_split_across_chunks() {
        let mut p = SseParser::default();
        assert_eq!(p.feed(b"event: message_start\ndata: {\"a\""), vec![]);
        let frames = p.feed(b":1}\n\ndata: [DONE]\n\n");
        assert_eq!(
            frames,
            vec![
                SseFrame {
                    event: Some("message_start".into()),
                    data: "{\"a\":1}".into()
                },
                SseFrame {
                    event: None,
                    data: "[DONE]".into()
                },
            ]
        );
    }

    #[test]
    fn handles_crlf_and_comments() {
        let mut p = SseParser::default();
        let frames = p.feed(b": keepalive\r\ndata: x\r\n\r\n");
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "x".into()
            }]
        );
    }

    #[test]
    fn multibyte_char_split_across_chunks_is_not_mangled() {
        // "你好" is 6 bytes (3 each). Split the frame mid-character: the '你'
        // straddles the chunk boundary. Per-chunk decoding would corrupt it;
        // whole-frame decoding recovers it intact.
        let mut p = SseParser::default();
        let full = "data: 你好\n\n".as_bytes();
        let split = 8; // 'data: ' (6) + first byte of '你'
        assert_eq!(p.feed(&full[..split]), vec![]);
        let frames = p.feed(&full[split..]);
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "你好".into()
            }]
        );
    }
}
