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

use agent_core::fork::Selector;
use agent_core::ids::{BranchId, EventId, TurnId};

use crate::{ROOT_BRANCH, ReactorError};

/// The socket's file name inside the state directory.
pub const SOCKET: &str = "bus.sock";

/// How long a client has to finish its line before it is dropped.
const PATIENCE: Duration = Duration::from_secs(2);

/// One command, as it arrived and as the loop reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// A person's message to one branch.
    Send { branch: BranchId, text: String },
    /// A yes to one call's question.
    Approve { branch: BranchId, id: String },
    /// A no to one call's question.
    Deny { branch: BranchId, id: String },
    /// A new branch at one point of an existing one.
    Fork(ForkRequest),
}

/// Where a fork starts, and what it is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkRequest {
    pub parent: BranchId,
    pub selector: Selector,
    pub label: Option<String>,
    pub boundary: bool,
    pub force: bool,
}

/// What the loop did with one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// The command was appended to its branch.
    Taken,
    /// A branch was opened, and this is where it starts.
    Forked { branch: BranchId, depth: u64 },
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
    /// The command is well formed but its arguments are not.
    Usage(String),
    /// The command would allow an effect the guards refuse.
    Policy(String),
    /// The store could not be read or written right now.
    Storage(String),
    /// The stored history disagrees with itself.
    Corrupt(String),
    /// The loop failed for a reason the client can only report.
    Failed(String),
}

impl Refusal {
    /// The machine name of this class, as the client's exit table reads it.
    #[must_use]
    pub const fn class(&self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::Unknown(_) => "unknown",
            Self::Empty(_) => "empty",
            Self::Usage(_) => "usage",
            Self::Policy(_) => "policy",
            Self::Storage(_) => "storage",
            Self::Corrupt(_) => "corrupt",
            Self::Failed(_) => "failed",
        }
    }

    /// What the client prints when it says why the command failed.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Malformed => "the request is not the JSON this socket reads".to_string(),
            Self::Unknown(command) => {
                format!("{command} is not a command the socket accepts")
            },
            Self::Empty(command) if command.is_empty() => "the line is empty".to_string(),
            Self::Empty(command) => format!("{command} needs text"),
            Self::Usage(message)
            | Self::Policy(message)
            | Self::Storage(message)
            | Self::Corrupt(message)
            | Self::Failed(message) => message.clone(),
        }
    }
}

/// Reads one request line.
///
/// # Errors
///
/// Returns [`Refusal::Malformed`] when the line is not JSON, [`Refusal::Empty`]
/// when it carries nothing, [`Refusal::Unknown`] when it names no command the
/// socket accepts, and [`Refusal::Usage`] when an argument is not the type or id
/// its field needs.
pub fn read_request(line: &str) -> Result<Command, Refusal> {
    #[derive(serde::Deserialize)]
    struct Request {
        command: String,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        parent: Option<String>,
        #[serde(default)]
        at: Option<String>,
        #[serde(default)]
        rewind: Option<u32>,
        #[serde(default)]
        turn: Option<String>,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        boundary: bool,
        #[serde(default)]
        force: bool,
    }

    if line.trim().is_empty() {
        return Err(Refusal::Empty(String::new()));
    }
    let request: Request = serde_json::from_str(line).map_err(|_| Refusal::Malformed)?;
    let Request {
        command,
        branch,
        text,
        id,
        parent,
        at,
        rewind,
        turn,
        label,
        boundary,
        force,
    } = request;
    match command.as_str() {
        "send" => {
            let branch = branch_of(branch.as_deref())?;
            let text = text.unwrap_or_default();
            if text.trim().is_empty() {
                return Err(Refusal::Empty("send".to_string()));
            }
            Ok(Command::Send { branch, text })
        },
        "approve" => {
            let branch = branch_of(branch.as_deref())?;
            let id = id.unwrap_or_default();
            if id.trim().is_empty() {
                return Err(Refusal::Empty("approve".to_string()));
            }
            Ok(Command::Approve { branch, id })
        },
        "deny" => {
            let branch = branch_of(branch.as_deref())?;
            let id = id.unwrap_or_default();
            if id.trim().is_empty() {
                return Err(Refusal::Empty("deny".to_string()));
            }
            Ok(Command::Deny { branch, id })
        },
        "fork" => Ok(Command::Fork(fork_request(
            parent, at, rewind, turn, label, boundary, force,
        )?)),
        other => Err(Refusal::Unknown(other.to_string())),
    }
}

/// The branch a command names, defaulting to the root for the old surface.
fn branch_of(text: Option<&str>) -> Result<BranchId, Refusal> {
    text.map_or(Ok(ROOT_BRANCH), |text| {
        text.parse()
            .map_err(|_| Refusal::Usage("branch is not a branch id".to_string()))
    })
}

/// Builds a fork request from the fields, exactly one of which may select.
fn fork_request(
    parent: Option<String>,
    at: Option<String>,
    rewind: Option<u32>,
    turn: Option<String>,
    label: Option<String>,
    boundary: bool,
    force: bool,
) -> Result<ForkRequest, Refusal> {
    let parent = parent.ok_or_else(|| Refusal::Usage("fork needs a parent branch".to_string()))?;
    let parent = parent
        .parse()
        .map_err(|_| Refusal::Usage("parent is not a branch id".to_string()))?;
    let selector = match (at, rewind, turn) {
        (None, None, None) => Selector::Tip,
        (Some(at), None, None) => Selector::At(
            at.parse::<EventId>()
                .map_err(|_| Refusal::Usage("at is not an event id".to_string()))?,
        ),
        (None, Some(rewind), None) => Selector::Rewind(rewind),
        (None, None, Some(turn)) => Selector::Turn(
            turn.parse::<TurnId>()
                .map_err(|_| Refusal::Usage("turn is not a turn id".to_string()))?,
        ),
        _ => {
            return Err(Refusal::Usage(
                "fork takes one of --at, --rewind, or --turn".to_string(),
            ));
        },
    };
    Ok(ForkRequest {
        parent,
        selector,
        label,
        boundary,
        force,
    })
}

/// The line the daemon writes back.
#[must_use]
pub fn reply_line(result: &Result<Reply, Refusal>) -> String {
    match result {
        Ok(Reply::Taken) => r#"{"ok":true}"#.to_string(),
        Ok(Reply::Forked { branch, depth }) => {
            format!(r#"{{"ok":true,"branch":"{branch}","depth":{depth}}}"#)
        },
        Err(refusal) => {
            // The message is the one field a client cannot predict, so it is the
            // one that needs the JSON encoder rather than the fixed shape around it.
            let message = serde_json::json!(refusal.message());
            format!(
                r#"{{"ok":false,"class":"{}","message":{message}}}"#,
                refusal.class()
            )
        },
    }
}

/// The class and message a reactor failure becomes on the wire.
#[must_use]
pub fn refusal(error: &ReactorError) -> Refusal {
    match error {
        ReactorError::NoSuchBranch(_) | ReactorError::Fork(_) => Refusal::Usage(error.to_string()),
        ReactorError::ForkStranded { .. } => Refusal::Policy(error.to_string()),
        ReactorError::Store(agent_store::StoreError::BranchExists(_))
        | ReactorError::NoModel
        | ReactorError::Time(_)
        | ReactorError::NoSuchCall(_) => Refusal::Failed(error.to_string()),
        ReactorError::Store(store) if store.is_corrupt() => Refusal::Corrupt(error.to_string()),
        ReactorError::Store(_) => Refusal::Storage(error.to_string()),
        ReactorError::Fold(_) => Refusal::Corrupt(error.to_string()),
    }
}

/// A client whose line was read, waiting for the loop to say what it did.
pub struct Pending {
    stream: UnixStream,
    request: Result<Command, Refusal>,
}

impl Pending {
    /// The command that arrived, or why it could not be read.
    ///
    /// # Errors
    ///
    /// Returns the [`Refusal`] the line earned, which the loop answers exactly
    /// as it answers a failure of its own.
    pub const fn command(&self) -> Result<&Command, &Refusal> {
        self.request.as_ref()
    }

    /// Writes the one line the client is waiting for, and closes.
    pub fn answer(
        self,
        reply: &Result<Reply, Refusal>,
    ) {
        let mut stream = self.stream;
        let _ = writeln!(stream, "{}", reply_line(reply));
        let _ = stream.flush();
    }

    /// Reads one line, and does not answer it.
    fn read(stream: UnixStream) -> Self {
        // On a platform that lets an accepted socket inherit the listener's
        // nonblocking mode, the read timeout would never apply, and a client that
        // stops mid-line would block the loop forever.
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(PATIENCE));
        let _ = stream.set_write_timeout(Some(PATIENCE));
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let request = match reader.read_line(&mut line) {
            Ok(0) | Err(_) => Err(Refusal::Malformed),
            Ok(_) => read_request(line.trim_end()),
        };
        Self {
            stream: reader.into_inner(),
            request,
        }
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

    /// Every request that arrived since the last call, read but not answered.
    ///
    /// The answer is written by [`Pending::answer`], after the loop has acted:
    /// "the daemon took your line" and "the daemon did what it said" are the same
    /// fact, and the second is the one the client asked for.
    #[must_use]
    pub fn poll(&self) -> Vec<Pending> {
        let mut pending = Vec::new();
        // An empty socket and one that cannot accept look the same from here, and
        // both mean the same thing: there is nothing to do until the next turn.
        while let Ok((stream, _)) = self.listener.accept() {
            pending.push(Pending::read(stream));
        }
        pending
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

#[cfg(test)]
mod tests;
