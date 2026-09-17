//! A small Server-Sent Events parser for provider streams.
//!
//! It feeds on raw bytes and yields complete `(event, data)` records at blank
//! lines, per the SSE specification. The buffer is capped so a misbehaving
//! provider cannot grow it without bound.

use crate::provider::ProviderError;

/// The largest unconsumed SSE buffer, in bytes.
const MAX_BUFFER: usize = 8 * 1024 * 1024;

/// One parsed SSE record.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` name, if the record had one.
    pub event: Option<String>,
    /// The joined `data:` fields.
    pub data: String,
}

/// An incremental SSE parser.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    /// Creates an empty parser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds `chunk` and returns the records it completed.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Failed`] if the buffer exceeds [`MAX_BUFFER`]
    /// without a record boundary.
    pub fn push(
        &mut self,
        chunk: &[u8],
    ) -> Result<Vec<SseEvent>, ProviderError> {
        if self.buffer.len().saturating_add(chunk.len()) > MAX_BUFFER {
            return Err(ProviderError::Failed(format!(
                "the provider sent more than {MAX_BUFFER} bytes without an event boundary"
            )));
        }
        self.buffer.extend_from_slice(chunk);
        let mut buffer = std::mem::take(&mut self.buffer);
        let mut events = Vec::new();
        let mut start = 0;
        while let Some(newline) = buffer[start..].iter().position(|&byte| byte == b'\n') {
            let end = start + newline;
            let line = buffer[start..end]
                .strip_suffix(b"\r")
                .unwrap_or_else(|| &buffer[start..end]);
            self.line(std::str::from_utf8(line).unwrap_or(""), &mut events);
            start = end + 1;
        }
        buffer.drain(..start);
        self.buffer = buffer;
        Ok(events)
    }

    /// Flushes a trailing record that had no closing blank line.
    #[must_use]
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let line: Vec<u8> = std::mem::take(&mut self.buffer);
            let mut events = Vec::new();
            self.line(std::str::from_utf8(&line).unwrap_or(""), &mut events);
        }
        self.take_event()
    }

    /// Applies one line of SSE input.
    fn line(
        &mut self,
        line: &str,
        events: &mut Vec<SseEvent>,
    ) {
        if line.is_empty() {
            if let Some(event) = self.take_event() {
                events.push(event);
            }
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {},
        }
    }

    /// Builds and clears the pending record, if it has data.
    fn take_event(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() {
            self.event = None;
            return None;
        }
        let data = self.data.join("\n");
        self.data.clear();
        Some(SseEvent {
            event: self.event.take(),
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_BUFFER, SseEvent, SseParser};

    fn event(
        event: Option<&str>,
        data: &str,
    ) -> SseEvent {
        SseEvent {
            event: event.map(String::from),
            data: String::from(data),
        }
    }

    #[test]
    fn parses_records_split_across_chunks() {
        let mut parser = SseParser::new();

        assert!(parser.push(b"data: {\"a\"").expect("push").is_empty());
        let events = parser
            .push(b":1}\n\ndata: [DONE]\n\n")
            .expect("push should succeed");

        assert_eq!(
            events,
            vec![event(None, "{\"a\":1}"), event(None, "[DONE]"),]
        );
    }

    #[test]
    fn parses_event_names_and_multiline_data() {
        let mut parser = SseParser::new();
        let events = parser
            .push(b"event: content_block_delta\ndata: one\ndata: two\n\n")
            .expect("push should succeed");

        assert_eq!(events, vec![event(Some("content_block_delta"), "one\ntwo")]);
    }

    #[test]
    fn ignores_comments_and_carriage_returns() {
        let mut parser = SseParser::new();
        let events = parser
            .push(b": keep-alive\r\ndata: value\r\n\r\n")
            .expect("push should succeed");

        assert_eq!(events, vec![event(None, "value")]);
    }

    #[test]
    fn finish_flushes_a_trailing_record() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: last").expect("push").is_empty());

        assert_eq!(parser.finish(), Some(event(None, "last")));
        assert_eq!(parser.finish(), None);
    }

    #[test]
    fn retains_an_incomplete_line_across_chunks() {
        let mut parser = SseParser::new();

        assert!(
            parser
                .push(b"data: one\ndata: tw")
                .expect("push")
                .is_empty()
        );
        let events = parser.push(b"o\n\n").expect("push should succeed");

        assert_eq!(events, vec![event(None, "one\ntwo")]);
    }

    #[test]
    fn rejects_a_buffer_that_exceeds_the_cap() {
        let mut parser = SseParser::new();

        // A single chunk past the cap is refused before it is buffered.
        let oversized = vec![b'a'; MAX_BUFFER + 1];
        assert!(parser.push(&oversized).is_err());

        // A parser that has already retained bytes refuses the next chunk that
        // would cross the cap without a boundary.
        let mut parser = SseParser::new();
        assert!(parser.push(&vec![b'a'; MAX_BUFFER]).is_ok());
        assert!(parser.push(b"b").is_err());
    }
}
