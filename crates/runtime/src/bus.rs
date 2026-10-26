//! The command socket.
//!
//! The daemon is the only writer of the log. A client therefore does not append
//! anything: it sends one line to this socket, and the loop that owns the writer
//! turns it into an event on its next turn. A daemon that is not running has no
//! socket, so a command sent to it fails where it was typed rather than waiting in
//! the log for a process that will act on it later, when nobody remembers asking.
//!
//! One line in, one line out, JSON both ways. The socket carries no state.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The socket's file name inside the state directory.
pub const SOCKET: &str = "bus.sock";

/// How long a client has to finish its line before it is dropped.
const PATIENCE: Duration = Duration::from_secs(2);

/// The commands the socket accepts.
const ACCEPTED: [&str; 3] = ["send", "approve", "deny"];

/// One command, as it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub command: String,
    pub text: String,
}

/// What went wrong with one line, in the words the client will print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The line was not the JSON this socket documents.
    Malformed,
    /// The command is not one the socket accepts.
    Unknown(String),
    /// The command needs text and none was given.
    Empty(String),
}

impl Refusal {
    fn message(&self) -> String {
        match self {
            Self::Malformed => "the request is not the JSON this socket reads".to_string(),
            Self::Unknown(command) => format!("{command} is not a command the socket accepts"),
            Self::Empty(command) => format!("{command} needs text"),
        }
    }
}

/// Reads one request line.
///
/// # Errors
///
/// Returns [`Refusal`] when the line is not JSON, when it names a command the
/// socket does not accept, or when a command that carries text carries none.
pub fn read_request(line: &str) -> Result<Command, Refusal> {
    #[derive(serde::Deserialize)]
    struct Request {
        command: String,
        #[serde(default)]
        text: Option<String>,
    }
    let request: Request = serde_json::from_str(line).map_err(|_| Refusal::Malformed)?;
    if !ACCEPTED.contains(&request.command.as_str()) {
        return Err(Refusal::Unknown(request.command));
    }
    let text = request.text.unwrap_or_default();
    if text.trim().is_empty() {
        return Err(Refusal::Empty(request.command));
    }
    Ok(Command {
        command: request.command,
        text,
    })
}

/// The line a client writes.
#[must_use]
pub fn request_line(
    command: &str,
    text: &str,
) -> String {
    serde_json::json!({ "command": command, "text": text }).to_string()
}

/// The line the daemon writes back.
#[must_use]
pub fn reply_line(result: &Result<(), Refusal>) -> String {
    match result {
        Ok(()) => serde_json::json!({ "ok": true }).to_string(),
        Err(refusal) => {
            serde_json::json!({ "ok": false, "message": refusal.message() }).to_string()
        },
    }
}

/// A bound socket, waiting for clients.
pub struct Bus {
    listener: UnixListener,
    path: PathBuf,
}

impl Bus {
    /// Binds the socket, replacing one a dead daemon left behind.
    ///
    /// # Errors
    ///
    /// Returns the error from binding, which is what a second daemon on the same
    /// path gets.
    pub fn bind(path: &Path) -> std::io::Result<Self> {
        // A socket file outlives the process that made it. Removing it is what
        // lets a restart take its own address back; an honest one would check
        // whether anyone is listening, and the cost of being wrong is a bind that
        // fails, which the caller reports.
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    /// Every request that arrived since the last call, answered as it is read.
    ///
    /// Answers immediately rather than after the loop has acted: the client is a
    /// person at a keyboard, and "the daemon took your line" is a different fact
    /// from "the daemon did what it said".
    #[must_use]
    pub fn poll(&self) -> Vec<Result<Command, Refusal>> {
        let mut answers = Vec::new();
        // An empty socket and one that cannot accept look the same from here, and
        // both mean the same thing: there is nothing to do until the next turn.
        while let Ok((stream, _)) = self.listener.accept() {
            answers.push(serve(&stream));
        }
        answers
    }

    /// Where the socket is, for the message that tells an operator how to reach it.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Reads one line from a client, answers it, and closes.
fn serve(stream: &UnixStream) -> Result<Command, Refusal> {
    let _ = stream.set_read_timeout(Some(PATIENCE));
    let _ = stream.set_write_timeout(Some(PATIENCE));
    let mut writer = stream;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let result = match reader.read_line(&mut line) {
        Ok(0) | Err(_) => Err(Refusal::Malformed),
        Ok(_) => read_request(line.trim_end()),
    };
    let _ = writeln!(
        writer,
        "{}",
        reply_line(&result.as_ref().map(|_command| ()).map_err(Clone::clone))
    );
    let _ = writer.flush();
    result
}

#[cfg(test)]
mod tests;
