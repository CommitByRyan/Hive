//! Incremental Server-Sent Events framing.
//!
//! The parser accepts arbitrary byte boundaries (including a multi-byte UTF-8
//! character split across two `feed` calls), handles `\n`, `\r\n` and `\r`
//! line endings, joins multi-line `data:` fields and ignores `: comment`
//! heartbeat lines.

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SseEvent {
    /// A `: comment` line, typically a heartbeat. Callers normally ignore it.
    Comment(String),
    /// One complete frame's `data:` payload, with multiple lines joined by `\n`.
    Data(String),
    /// The OpenAI terminator `data: [DONE]`.
    Done,
}

#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    data_lines: Vec<String>,
    line_no: usize,
    done: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        if self.done {
            return Vec::new();
        }
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(line) = self.take_line(false) {
            self.line_no += 1;
            self.handle_line(&line, &mut events);
            if self.done {
                break;
            }
        }
        events
    }

    pub fn feed_str(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.feed(chunk.as_bytes())
    }

    /// Flush a trailing line and any buffered frame, e.g. when the connection
    /// closes without a final blank line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        if self.done {
            return Vec::new();
        }
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            if let Some(line) = self.take_line(true) {
                self.line_no += 1;
                self.handle_line(&line, &mut events);
            }
        }
        if !self.done {
            if let Some(event) = self.flush_frame() {
                events.push(event);
            }
        }
        self.done = true;
        events
    }

    pub fn line_no(&self) -> usize {
        self.line_no
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    fn take_line(&mut self, at_eof: bool) -> Option<Vec<u8>> {
        let mut index = 0;
        while index < self.buffer.len() {
            match self.buffer[index] {
                b'\n' => {
                    let line = self.buffer[..index].to_vec();
                    self.buffer.drain(..=index);
                    return Some(line);
                }
                b'\r' => {
                    if index + 1 < self.buffer.len() {
                        let end = if self.buffer[index + 1] == b'\n' {
                            index + 2
                        } else {
                            index + 1
                        };
                        let line = self.buffer[..index].to_vec();
                        self.buffer.drain(..end);
                        return Some(line);
                    }
                    if at_eof {
                        let line = self.buffer[..index].to_vec();
                        self.buffer.clear();
                        return Some(line);
                    }
                    // A trailing CR may be the first half of a split CRLF; wait.
                    return None;
                }
                _ => index += 1,
            }
        }
        if at_eof && !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            return Some(line);
        }
        None
    }

    fn handle_line(&mut self, raw: &[u8], events: &mut Vec<SseEvent>) {
        let line = String::from_utf8_lossy(raw);
        if line.is_empty() {
            if let Some(event) = self.flush_frame() {
                events.push(event);
            }
            return;
        }
        if let Some(comment) = line.strip_prefix(':') {
            events.push(SseEvent::Comment(comment.trim_start().to_owned()));
            return;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let value = rest.strip_prefix(' ').unwrap_or(rest);
            if value.trim() == "[DONE]" {
                if let Some(event) = self.flush_frame() {
                    events.push(event);
                }
                self.data_lines.clear();
                self.done = true;
                events.push(SseEvent::Done);
                return;
            }
            self.data_lines.push(value.to_owned());
        }
        // `event:`, `id:` and `retry:` are valid SSE fields but carry no
        // information this gateway needs, so they are ignored.
    }

    fn flush_frame(&mut self) -> Option<SseEvent> {
        if self.data_lines.is_empty() {
            return None;
        }
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        if data.trim() == "[DONE]" {
            self.done = true;
            Some(SseEvent::Done)
        } else {
            Some(SseEvent::Data(data))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_events(events: Vec<SseEvent>) -> Vec<String> {
        events
            .into_iter()
            .filter_map(|event| match event {
                SseEvent::Data(data) => Some(data),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn parses_frame_split_at_every_byte_boundary() {
        let stream = "data: {\"a\":1}\n\ndata: {\"b\":2}\n\n";
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        for byte in stream.as_bytes() {
            events.extend(parser.feed(&[*byte]));
        }
        assert_eq!(data_events(events), vec![r#"{"a":1}"#, r#"{"b":2}"#]);
    }

    #[test]
    fn joins_multiline_data() {
        let mut parser = SseParser::new();
        let events = parser.feed_str("data: line one\ndata: line two\n\n");
        assert_eq!(data_events(events), vec!["line one\nline two"]);
    }

    #[test]
    fn ignores_heartbeat_comments() {
        let mut parser = SseParser::new();
        let events = parser.feed_str(": heartbeat\ndata: {\"ok\":true}\n\n");
        assert_eq!(
            events,
            vec![
                SseEvent::Comment("heartbeat".to_owned()),
                SseEvent::Data(r#"{"ok":true}"#.to_owned()),
            ]
        );
    }

    #[test]
    fn stops_at_done_without_trailing_blank_line() {
        let mut parser = SseParser::new();
        let events = parser.feed_str("data: {\"x\":1}\n\ndata: [DONE]\n");
        assert_eq!(
            events,
            vec![SseEvent::Data(r#"{"x":1}"#.to_owned()), SseEvent::Done]
        );
        assert!(parser.is_done());
        assert!(parser.feed_str("data: ignored\n\n").is_empty());
    }

    #[test]
    fn handles_crlf_split_across_feeds() {
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        events.extend(parser.feed(b"data: {\"a\":1}\r"));
        assert!(events.is_empty(), "trailing CR must wait for the LF");
        events.extend(parser.feed(b"\ndata: [DONE]\r\n"));
        assert_eq!(
            events,
            vec![SseEvent::Data(r#"{"a":1}"#.to_owned()), SseEvent::Done]
        );
    }

    #[test]
    fn handles_utf8_split_across_feeds() {
        let text = "data: {\"text\":\"中文\"}\n\n";
        let bytes = text.as_bytes();
        let split = text.find('中').expect("multibyte char") + 1;
        let mut parser = SseParser::new();
        let mut events = parser.feed(&bytes[..split]);
        events.extend(parser.feed(&bytes[split..]));
        assert_eq!(data_events(events), vec![r#"{"text":"中文"}"#]);
    }

    #[test]
    fn finish_flushes_unterminated_frame() {
        let mut parser = SseParser::new();
        assert!(parser.feed_str("data: {\"tail\":true}").is_empty());
        let events = parser.finish();
        assert_eq!(data_events(events), vec![r#"{"tail":true}"#]);
    }
}
