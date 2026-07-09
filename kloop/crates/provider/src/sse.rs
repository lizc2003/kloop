/// Incremental SSE parser: feed arbitrary byte chunks, get complete frames.
/// Handles frames split across chunk boundaries, CRLF, multi-line data.
#[derive(Default)]
pub struct SseParser {
    buf: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &str) -> Vec<SseFrame> {
        self.buf.push_str(&chunk.replace("\r\n", "\n"));
        let mut frames = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let raw: String = self.buf.drain(..pos + 2).collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frames_split_across_chunks() {
        let mut p = SseParser::default();
        assert_eq!(p.feed("event: message_start\ndata: {\"a\""), vec![]);
        let frames = p.feed(":1}\n\ndata: [DONE]\n\n");
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
        let frames = p.feed(": keepalive\r\ndata: x\r\n\r\n");
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "x".into()
            }]
        );
    }
}
