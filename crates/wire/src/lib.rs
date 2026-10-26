//! Server-sent events, as bytes in and events out.
//!
//! No socket, no async runtime, no clock. The parser is handed chunks and
//! returns whole events, so every branch it has can be driven from a test with
//! no server and no timing, and the shell that owns the connection has nothing
//! left to get wrong but the connection itself.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// The longest a single line may be before the stream is treated as broken.
///
/// A provider that never sends a newline would otherwise grow this buffer until
/// the process dies. One megabyte is far past a real frame and small enough that
/// the failure arrives while there is still memory to report it.
pub const MAX_LINE: usize = 1 << 20;

/// The longest one event may be, counting every data line it accumulated.
pub const MAX_EVENT: usize = 8 << 20;

/// Why the bytes were not a well formed event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// A line grew past [`MAX_LINE`] without ending.
    LineTooLong,
    /// One event grew past [`MAX_EVENT`].
    EventTooLong,
    /// A data field was not valid UTF-8.
    NotUtf8,
}

impl core::fmt::Display for WireError {
    fn fmt(
        &self,
        formatter: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        match self {
            Self::LineTooLong => {
                write!(formatter, "an event stream line exceeded {MAX_LINE} bytes")
            },
            Self::EventTooLong => write!(formatter, "an event exceeded {MAX_EVENT} bytes"),
            Self::NotUtf8 => write!(formatter, "an event carried bytes that are not UTF-8"),
        }
    }
}

impl core::error::Error for WireError {}

/// One complete event, which is what the stream means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The `data` field of one event, with its lines joined and the final newline removed.
    Data(String),
}

/// An incremental parser. Feed it whatever the socket gave you.
#[derive(Debug, Default)]
pub struct Parser {
    line: Vec<u8>,
    data: String,
    /// A carriage return ended the previous line, so a line feed that follows belongs to it.
    skip_lf: bool,
}

impl Parser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes a chunk and returns every event it completed.
    ///
    /// # Errors
    ///
    /// Returns [`WireError`] when a line or an event grows past its bound, or
    /// when a data field is not UTF-8.
    pub fn push(
        &mut self,
        chunk: &[u8],
    ) -> Result<Vec<Event>, WireError> {
        let mut events = Vec::new();
        for &byte in chunk {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(&mut events)?,
                b'\r' => {
                    self.end_line(&mut events)?;
                    self.skip_lf = true;
                },
                other => {
                    if self.line.len() == MAX_LINE {
                        return Err(WireError::LineTooLong);
                    }
                    self.line.push(other);
                },
            }
        }
        Ok(events)
    }

    /// Ends the stream, returning an event whose blank line never arrived.
    ///
    /// A provider that stops mid-event leaves data behind; the caller decides
    /// whether that is a truncated response or a stream that simply did not
    /// terminate its last frame.
    ///
    /// # Errors
    ///
    /// Returns [`WireError`] when the pending data is not valid UTF-8.
    pub fn finish(&mut self) -> Result<Vec<Event>, WireError> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            let line = core::mem::take(&mut self.line);
            self.field(&line)?;
        }
        self.dispatch(&mut events);
        Ok(events)
    }

    /// Ends the line in progress and handles what it held.
    fn end_line(
        &mut self,
        events: &mut Vec<Event>,
    ) -> Result<(), WireError> {
        let line = core::mem::take(&mut self.line);
        if line.is_empty() {
            self.dispatch(events);
            return Ok(());
        }
        self.field(&line)
    }

    /// Applies one `field: value` line.
    fn field(
        &mut self,
        line: &[u8],
    ) -> Result<(), WireError> {
        if line.first() == Some(&b':') {
            return Ok(());
        }
        let (name, value) = line.iter().position(|byte| *byte == b':').map_or_else(
            || (line, &line[line.len()..]),
            |colon| (&line[..colon], &line[colon + 1..]),
        );
        if name != b"data" {
            return Ok(());
        }
        let value = value.strip_prefix(b" ").unwrap_or(value);
        let text = core::str::from_utf8(value).map_err(|_| WireError::NotUtf8)?;
        // The bound covers the newline this field is about to add, so that an
        // event of exactly MAX_EVENT bytes is accepted and one byte more is not.
        let total = self.data.len().saturating_add(text.len()).saturating_add(1);
        if total > MAX_EVENT {
            return Err(WireError::EventTooLong);
        }
        self.data.push_str(text);
        self.data.push('\n');
        Ok(())
    }

    /// Emits the event the blank line just ended, if it carried data.
    fn dispatch(
        &mut self,
        events: &mut Vec<Event>,
    ) {
        if self.data.is_empty() {
            return;
        }
        let mut data = core::mem::take(&mut self.data);
        data.pop();
        events.push(Event::Data(data));
    }
}

#[cfg(test)]
mod tests;
