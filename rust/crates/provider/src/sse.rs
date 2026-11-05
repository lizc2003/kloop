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
    scan_from: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, crate::ProviderFailure> {
        self.buf.extend_from_slice(chunk);
        // Frames are parsed in place and the consumed prefix is dropped once at
        // the end: draining per frame memmoves the whole remainder, which goes
        // quadratic when one chunk carries thousands of small frames.
        let mut frames = Vec::new();
        let mut error = None;
        let mut consumed = 0;
        let mut scan_from = self.scan_from;
        loop {
            // Blank lines separate frames, and a stream may open with them.
            while consumed < self.buf.len() {
                if self.buf[consumed] == b'\n' {
                    consumed += 1;
                } else if self.buf[consumed..].starts_with(b"\r\n") {
                    consumed += 2;
                } else {
                    break;
                }
            }
            scan_from = scan_from.max(consumed);
            let Some(end) = frame_end(&self.buf, scan_from) else {
                break;
            };
            if end - consumed > crate::stream::STREAM_MAX_FRAME_BYTES {
                error = Some(crate::ProviderFailure::response_too_large(format!(
                    "SSE frame exceeded {} bytes",
                    crate::stream::STREAM_MAX_FRAME_BYTES
                )));
                break;
            }
            let raw = match std::str::from_utf8(&self.buf[consumed..end]) {
                Ok(raw) => raw,
                Err(invalid) => {
                    consumed = end;
                    error = Some(crate::ProviderFailure::protocol(format!(
                        "SSE frame was not valid UTF-8: {invalid}"
                    )));
                    break;
                }
            };
            frames.extend(parse_frame(raw));
            consumed = end;
            scan_from = end;
        }
        self.buf.drain(..consumed);
        self.scan_from = self.buf.len().saturating_sub(2);
        if let Some(error) = error {
            return Err(error);
        }
        if self.buf.len() > crate::stream::STREAM_MAX_FRAME_BYTES {
            return Err(crate::ProviderFailure::response_too_large(format!(
                "unterminated SSE frame exceeded {} bytes",
                crate::stream::STREAM_MAX_FRAME_BYTES
            )));
        }
        Ok(frames)
    }

    /// Validate bytes left when the HTTP body reaches EOF. Invalid UTF-8 is a
    /// non-retryable protocol violation; valid bytes without a blank-line frame
    /// terminator retain Plan 64's retryable incomplete-protocol classification.
    pub fn finish(&self) -> Result<(), crate::ProviderFailure> {
        if self.buf.is_empty() {
            return Ok(());
        }
        std::str::from_utf8(&self.buf).map_err(|error| {
            crate::ProviderFailure::protocol(format!("SSE residual was not valid UTF-8: {error}"))
        })?;
        Err(crate::ProviderFailure::incomplete_protocol(
            "SSE stream ended with an unterminated frame",
        ))
    }
}

/// One complete frame's text, parsed in place. Returns `None` for a frame with
/// no `data:` field: `event: ping\n\n` carries none, and the SSE dispatch
/// algorithm drops a frame whose data buffer stayed empty. A present-but-empty
/// `data:` line still dispatches (the buffer held a newline). Comments (":…")
/// and other fields are ignored.
fn parse_frame(raw: &str) -> Option<SseFrame> {
    let mut event = None;
    let mut data: Option<String> = None;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            match &mut data {
                Some(data) => {
                    data.push('\n');
                    data.push_str(rest);
                }
                None => data = Some(rest.to_string()),
            }
        }
    }
    data.map(|data| SseFrame { event, data })
}

/// Byte index just past the first blank-line frame separator (`\n\n` or
/// `\r\n\r\n`), or None if no complete frame is buffered yet. A blank line is
/// an LF whose preceding line terminator is right in front of it.
fn frame_end(buf: &[u8], scan_from: usize) -> Option<usize> {
    for i in scan_from..buf.len() {
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
    fn parses_thousands_of_frames_in_one_chunk() {
        let mut parser = SseParser::default();
        let mut chunk = String::new();
        for index in 0..5_000 {
            chunk.push_str(&format!("event: e{index}\ndata: {index}\n\n"));
        }
        let frames = parser.feed(chunk.as_bytes()).unwrap();
        assert_eq!(frames.len(), 5_000);
        assert_eq!(
            frames.first().unwrap(),
            &SseFrame {
                event: Some("e0".into()),
                data: "0".into(),
            }
        );
        assert_eq!(
            frames.last().unwrap(),
            &SseFrame {
                event: Some("e4999".into()),
                data: "4999".into(),
            }
        );
        parser.finish().unwrap();
    }

    #[test]
    fn complete_frames_are_delivered_before_an_incomplete_tail() {
        let mut parser = SseParser::default();
        let frames = parser.feed(b"data: one\n\ndata: two\n\ndata: thr").unwrap();
        assert_eq!(
            frames,
            vec![
                SseFrame {
                    event: None,
                    data: "one".into()
                },
                SseFrame {
                    event: None,
                    data: "two".into()
                },
            ]
        );
        let frames = parser.feed(b"ee\n\n").unwrap();
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "three".into()
            }]
        );
    }

    #[test]
    fn parses_frames_split_across_chunks() {
        let mut p = SseParser::default();
        assert_eq!(
            p.feed(b"event: message_start\ndata: {\"a\"").unwrap(),
            vec![]
        );
        let frames = p.feed(b":1}\n\ndata: [DONE]\n\n").unwrap();
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
        let frames = p.feed(b": keepalive\r\ndata: x\r\n\r\n").unwrap();
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "x".into()
            }]
        );
    }

    #[test]
    fn ignores_extra_blank_lines_between_and_after_frames() {
        let mut parser = SseParser::default();
        let frames = parser
            .feed(b"data: one\n\n\ndata: two\r\n\r\n\r\n")
            .unwrap();
        assert_eq!(
            frames,
            vec![
                SseFrame {
                    event: None,
                    data: "one".into(),
                },
                SseFrame {
                    event: None,
                    data: "two".into(),
                },
            ]
        );
        parser.finish().unwrap();
    }

    #[test]
    fn event_value_removes_only_one_optional_space() {
        let mut parser = SseParser::default();
        let frames = parser
            .feed(
                b"event: message_stop\ndata: one\n\nevent:  message_stop\ndata: two\n\nevent: message_stop \ndata: three\n\nevent:\tmessage_stop\ndata: four\n\n",
            )
            .unwrap();
        assert_eq!(
            frames
                .into_iter()
                .map(|frame| frame.event.unwrap())
                .collect::<Vec<_>>(),
            vec![
                "message_stop",
                " message_stop",
                "message_stop ",
                "\tmessage_stop",
            ]
        );
    }

    #[test]
    fn event_only_frames_are_not_dispatched() {
        let mut parser = SseParser::default();
        assert_eq!(
            parser
                .feed(b"event: ping\n\nevent: message_start\ndata: one\n\n")
                .unwrap(),
            vec![SseFrame {
                event: Some("message_start".into()),
                data: "one".into(),
            }]
        );

        // A `data:` line is a field even when its value is empty; the frame
        // still dispatches with an empty payload.
        assert_eq!(
            parser.feed(b"data:\n\n").unwrap(),
            vec![SseFrame {
                event: None,
                data: String::new(),
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
        assert_eq!(p.feed(&full[..split]).unwrap(), vec![]);
        let frames = p.feed(&full[split..]).unwrap();
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "你好".into()
            }]
        );
    }

    #[test]
    fn one_byte_chunks_preserve_cross_chunk_separator_detection() {
        let mut parser = SseParser::default();
        let mut frames = Vec::new();
        for byte in b"data: ok\r\n\r\n" {
            frames.extend(parser.feed(&[*byte]).unwrap());
        }
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "ok".into(),
            }]
        );
    }

    #[test]
    fn rejects_invalid_utf8_in_complete_frame_and_eof_residual() {
        let mut complete = SseParser::default();
        let error = complete.feed(b"data: \xff\n\n").unwrap_err();
        assert_eq!(error.kind(), &crate::ProviderFailureKind::Protocol);
        assert!(!error.is_retryable());
        assert!(error.to_string().contains("not valid UTF-8"));

        let mut residual = SseParser::default();
        assert!(residual.feed(b"data: \xf0\x9f").unwrap().is_empty());
        let error = residual.finish().unwrap_err();
        assert_eq!(error.kind(), &crate::ProviderFailureKind::Protocol);
        assert!(!error.is_retryable());
        assert!(error.to_string().contains("residual"));
    }

    #[test]
    fn eof_distinguishes_empty_and_valid_unterminated_residual() {
        let mut complete = SseParser::default();
        complete.feed(b"data: ok\n\n").unwrap();
        complete.finish().unwrap();

        let mut residual = SseParser::default();
        residual.feed("data: 你好".as_bytes()).unwrap();
        let error = residual.finish().unwrap_err();
        assert_eq!(error.kind(), &crate::ProviderFailureKind::Protocol);
        assert!(error.is_retryable());
        assert!(error.to_string().contains("unterminated frame"));
    }

    #[test]
    fn rejects_complete_and_unterminated_oversized_frames() {
        let mut complete = SseParser::default();
        let complete_frame = format!(
            "data: {}\n\n",
            "x".repeat(crate::stream::STREAM_MAX_FRAME_BYTES)
        );
        let error = complete.feed(complete_frame.as_bytes()).unwrap_err();
        assert_eq!(error.kind(), &crate::ProviderFailureKind::ResponseTooLarge);

        let mut open = SseParser::default();
        let open_frame = vec![b'x'; crate::stream::STREAM_MAX_FRAME_BYTES + 1];
        let error = open.feed(&open_frame).unwrap_err();
        assert_eq!(error.kind(), &crate::ProviderFailureKind::ResponseTooLarge);
    }
}
